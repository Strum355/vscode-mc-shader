use rust_lsp::jsonrpc::{*, method_types::*};
use rust_lsp::lsp::*;
use rust_lsp::lsp_types::{*, notification::*};

use petgraph::stable_graph::NodeIndex;

use serde_json::Value;
use url_norm::FromUrl;
use walkdir::WalkDir;

use std::{cell::RefCell, path::PathBuf, str::FromStr};
use std::collections::{HashMap, HashSet};
use std::collections::hash_map::RandomState;
use std::convert::TryFrom;
use std::fmt::{Display, Formatter, Debug};
use std::io::{stdin, stdout, BufRead, BufReader};
use std::rc::Rc;
use std::fs;
use std::iter::{Extend, FromIterator};

use path_slash::PathBufExt;

use anyhow::{Result, anyhow};

use chan::WaitGroup;

use regex::Regex;

use lazy_static::lazy_static;

mod graph;
mod commands;
mod lsp_ext;
mod dfs;
mod merge_views;
mod consts;
mod opengl;
mod url_norm;

#[cfg(test)]
mod test;

lazy_static! {
    static ref RE_DIAGNOSTIC: Regex = Regex::new(r#"^(?P<filepath>[^?<>*|"]+)\((?P<linenum>\d+)\) : (?P<severity>error|warning) [A-C]\d+: (?P<output>.+)"#).unwrap();
    static ref RE_VERSION: Regex = Regex::new(r#"#version [\d]{3}"#).unwrap();
    static ref RE_INCLUDE: Regex = Regex::new(r#"^(?:\s)*?(?:#include) "(.+)"\r?"#).unwrap();
    static ref RE_INCLUDE_EXTENSION: Regex = Regex::new(r#"#extension GL_GOOGLE_include_directive ?: ?require"#).unwrap();
    pub static ref RE_CRLF: Regex = Regex::new(r#"\r\n"#).unwrap();
}

fn main() {
    let stdin = stdin();

    let endpoint_output = LSPEndpoint::create_lsp_output_with_output_stream(stdout);

    let cache_graph = graph::CachedStableGraph::new();

    let mut langserver = MinecraftShaderLanguageServer {
        endpoint: endpoint_output.clone(),
        graph: Rc::new(RefCell::new(cache_graph)),
        wait: WaitGroup::new(),
        root: "".into(),
        command_provider: None,
        opengl_context: Rc::new(opengl::OpenGLContext::new())
    };

    langserver.command_provider = Some(commands::CustomCommandProvider::new(vec![
        (
            "graphDot",
            Box::new(commands::GraphDotCommand {
                graph: Rc::clone(&langserver.graph),
            }),
        ),
        (
            "virtualMerge",
            Box::new(commands::VirtualMergedDocument{
                graph: Rc::clone(&langserver.graph)
            })
        )
    ]));

    LSPEndpoint::run_server_from_input(&mut stdin.lock(), endpoint_output, langserver);
}

struct MinecraftShaderLanguageServer {
    endpoint: Endpoint,
    graph: Rc<RefCell<graph::CachedStableGraph>>,
    wait: WaitGroup,
    root: PathBuf,
    command_provider: Option<commands::CustomCommandProvider>,
    opengl_context: Rc<dyn opengl::ShaderValidator>
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct IncludePosition {
    line: usize,
    start: usize,
    end: usize,
}

impl Debug for IncludePosition {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{{line: {}}}", self.line)
    }
}

impl Display for IncludePosition {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "{{line: {}}}", self.line)
    }
}

pub enum TreeType {
    Fragment, Vertex, Geometry, Compute
}

impl MinecraftShaderLanguageServer {
    pub fn error_not_available<DATA>(data: DATA) -> MethodError<DATA> {
        let msg = "Functionality not implemented.".to_string();
        MethodError::<DATA> {
            code: 1,
            message: msg,
            data,
        }
    }

    pub fn gen_initial_graph(&self) {
        eprintln!("root of project is {:?}", self.root);

        // filter directories and files not ending in any of the 3 extensions
        WalkDir::new(&self.root).into_iter().filter_map(|entry| {
                if entry.is_err() {
                    return None;
                }

                let entry = entry.unwrap();
                let path = entry.path();
                if path.is_dir() {
                    return None;
                }

                let ext = match path.extension() {
                    Some(e) => e,
                None => return None,
                };

                if ext != "vsh" && ext != "fsh" && ext != "glsl" && ext != "inc" {
                    return None;
                }

                Some(entry.into_path())
        }).for_each(|path| {
            // iterate all valid found files, search for includes, add a node into the graph for each
            // file and add a file->includes KV into the map
            self.add_file_and_includes_to_graph(&path);
        });

        eprintln!("finished building project include graph");
    }

    fn add_file_and_includes_to_graph(&self, path: &PathBuf) {
        let includes = self.find_includes(path);

        let idx = self.graph.borrow_mut().add_node(&path);

        //eprintln!("adding {:?} with {:?}", path, includes);
        for include in includes {
            self.add_include(include, idx);
        }
    }

    fn add_include(&self, include: (PathBuf, IncludePosition), node: NodeIndex) {
        let child = self.graph.borrow_mut().add_node(&include.0);
        self.graph.borrow_mut().add_edge(node, child, include.1);
    }

    pub fn find_includes(&self, file: &PathBuf) -> Vec<(PathBuf, IncludePosition)> {
        let mut includes = Vec::default();

        let buf = BufReader::new(std::fs::File::open(file).unwrap());
        buf.lines()
            .enumerate()
            .filter_map(|line| match line.1 {
                Ok(t) => Some((line.0, t)),
                Err(_e) => None,
            })
            .filter(|line| RE_INCLUDE.is_match(line.1.as_str()))
            .for_each(|line| {
                let cap = RE_INCLUDE
                    .captures(line.1.as_str())
                    .unwrap()
                    .get(1)
                    .unwrap();

                let start = cap.start();
                let end = cap.end();
                let mut path: String = cap.as_str().into();

                // TODO: difference between / and not
                let full_include = if path.starts_with('/') {
                    path = path.strip_prefix('/').unwrap().to_string();
                    self.root.join("shaders").join(PathBuf::from_slash(&path))
                } else {
                    file.parent().unwrap().join(PathBuf::from_slash(&path))
                };

                includes.push((
                    full_include,
                    IncludePosition {
                        line: line.0,
                        start,
                        end,
                    }
                ));
            });

        includes
    }

    fn update_includes(&self, file: &PathBuf) {
        let includes = self.find_includes(file);

        eprintln!("updating {:?} with {:?}", file, includes);

        let idx = match self.graph.borrow_mut().find_node(&file) {
            None => {
                return
            },
            Some(n) => n,
        };

        let prev_children: HashSet<_, RandomState> = HashSet::from_iter(self.graph.borrow().child_node_meta(idx));
        let new_children: HashSet<_, RandomState> = HashSet::from_iter(includes.iter().map(|e| e.clone()));

        let to_be_added = new_children.difference(&prev_children);
        let to_be_removed = prev_children.difference(&new_children);

        eprintln!("removing:\n\t{:?}\nadding:\n\t{:?}", to_be_removed, to_be_added);

        for removal in to_be_removed {
            let child = self.graph.borrow_mut().find_node(&removal.0).unwrap();
            self.graph.borrow_mut().remove_edge(idx, child);
        }

        for insertion in to_be_added {
            self.add_include(includes.iter().find(|f| f.0 == *insertion.0).unwrap().clone(), idx);
        }
    }

    pub fn lint(&self, uri: &PathBuf) -> Result<HashMap<Url, Vec<Diagnostic>>> {
        // get all top level ancestors of this file
        let file_ancestors = match self.get_file_toplevel_ancestors(uri) {
            Ok(opt) => match opt {
                Some(ancestors) => ancestors,
                None => vec![],
            },
            Err(e) => return Err(e),
        };
        
        eprintln!("ancestors for {:?}:\n\t{:?}", uri, file_ancestors.iter().map(|e| PathBuf::from_str(&self.graph.borrow().graph.node_weight(*e).unwrap().clone()).unwrap()).collect::<Vec<PathBuf>>());

        // the set of all filepath->content. TODO: change to Url?
        let mut all_sources: HashMap<PathBuf, String> = HashMap::new();
        // the set of filepath->list of diagnostics to report
        let mut diagnostics: HashMap<Url, Vec<Diagnostic>> = HashMap::new();

        // we want to backfill the diagnostics map with all linked sources 
        let back_fill = |all_sources, diagnostics: &mut HashMap<Url, Vec<Diagnostic>>| {
            for (path, _) in all_sources {
                diagnostics.entry(Url::from_file_path(path).unwrap()).or_default();
            }
        };

        // if we are a top-level file (this has to be one of the set defined by Optifine, right?)
        if file_ancestors.is_empty() {
            // gather the list of all descendants 
            let root = self.graph.borrow_mut().find_node(&uri).unwrap();
            let tree = match self.get_dfs_for_node(root) {
                Ok(tree) => tree,
                Err(e) => {
                    diagnostics.insert(Url::from_file_path(uri).unwrap(), vec![e.into()]);
                    return Ok(diagnostics);
                }
            };

            all_sources.extend( self.load_sources(&tree)?);

            let view = {
            let graph = self.graph.borrow();
                merge_views::generate_merge_list(&tree, &all_sources, &graph)
            };

            let root_path = self.graph.borrow().get_node(root);
            let ext = match root_path.extension() {
                Some(ext) => ext,
                None => {
                    back_fill(&all_sources, &mut diagnostics);
                    return Ok(diagnostics)
                },
            };
            let tree_type = if ext == "fsh" {
                TreeType::Fragment
            } else if ext == "vsh" {
                TreeType::Vertex
            } else if ext == "gsh" {
                TreeType::Geometry
            } else if ext == "csh" {
                TreeType::Compute
            } else {
                eprintln!("got a non fsh|vsh|gsh|csh ({:?}) as a file root ancestor: {:?}", ext, root_path);
                back_fill(&all_sources, &mut diagnostics);
                return Ok(diagnostics)
            };

            let stdout = match self.opengl_context.clone().validate(tree_type, view) {
                Some(s) => s,
                None => {
                    back_fill(&all_sources, &mut diagnostics);
                    return Ok(diagnostics)
                },
            };
            diagnostics.extend(self.parse_validator_stdout(uri, stdout, ""));
        } else {
            let mut all_trees: Vec<(TreeType, Vec<(NodeIndex, Option<_>)>)> = Vec::new();

            for root in &file_ancestors {
                let nodes = match self.get_dfs_for_node(*root) {
                    Ok(nodes) => nodes,
                    Err(e) => {
                        diagnostics.insert(Url::from_file_path(uri).unwrap(), vec![e.into()]);
                        back_fill(&all_sources, &mut diagnostics); // TODO: confirm
                        return Ok(diagnostics);
                    }
                };

                let root_path = self.graph.borrow().get_node(*root).clone();
                let ext = match root_path.extension() {
                    Some(ext) => ext,
                    None => continue
                };
                let tree_type = if ext == "fsh" {
                    TreeType::Fragment
                } else if ext == "vsh" {
                    TreeType::Vertex
                } else if ext == "gsh" {
                    TreeType::Geometry
                } else if ext == "csh" {
                    TreeType::Compute
                } else {
                    eprintln!("got a non fsh|vsh|gsh|csh ({:?}) as a file root ancestor: {:?}", ext, root_path);
                    continue;
                };

                let sources = self.load_sources(&nodes)?;
                all_trees.push((tree_type, nodes));
                all_sources.extend(sources);
            }

            for tree in all_trees {
                let view = {
                let graph = self.graph.borrow();
                    merge_views::generate_merge_list(&tree.1, &all_sources, &graph)
                };

                let stdout = match self.opengl_context.clone().validate(tree.0, view) {
                    Some(s) => s,
                    None => continue,
                };
                diagnostics.extend(self.parse_validator_stdout(uri, stdout, ""));
            }
        };

        back_fill(&all_sources, &mut diagnostics);
        Ok(diagnostics)
    }

    fn parse_validator_stdout(&self, uri: &PathBuf, stdout: String, _source: &str) -> HashMap<Url, Vec<Diagnostic>> {
        let stdout_lines = stdout.split('\n');
        let mut diagnostics: HashMap<Url, Vec<Diagnostic>> = HashMap::with_capacity(stdout_lines.count());
        let stdout_lines = stdout.split('\n');
        
        for line in stdout_lines {
            let diagnostic_capture = match RE_DIAGNOSTIC.captures(line) {
                Some(d) => d,
                None => continue
            };

            eprintln!("match {:?}", diagnostic_capture);
            
            let msg = diagnostic_capture.name("output").unwrap().as_str();

            let line = match diagnostic_capture.name("linenum") {
                Some(c) => match c.as_str().parse::<u32>() {
                    Ok(i) => i,
                    Err(_) => 0,
                },
                None => 0,
            } - 2;

            // TODO: line matching maybe
            /* let line_text = source_lines[line as usize];
            let leading_whitespace = line_text.len() - line_text.trim_start().len(); */

            let severity = match diagnostic_capture.name("severity") {
                Some(c) => match c.as_str() {
                    "error" => DiagnosticSeverity::Error,
                    "warning" => DiagnosticSeverity::Warning,
                    _ => DiagnosticSeverity::Information,
                }
                _ => DiagnosticSeverity::Information,
            };

            let origin = match diagnostic_capture.name("filepath") {
                Some(o) => {
                    if o.as_str().to_string() == "0" {
                        uri.to_str().unwrap().to_string()
                    } else {
                        o.as_str().to_string()
                    }
                },
                None => uri.to_str().unwrap().to_string(),
            };

            let diagnostic = Diagnostic {
                range: Range::new(
                    /* Position::new(line, leading_whitespace as u64),
                    Position::new(line, line_text.len() as u64) */
                    Position::new(line, 0),
                    Position::new(line, 1000),
                ),
                code: None,
                severity: Some(severity),
                source: Some(consts::SOURCE.into()),
                message: msg.trim().into(),
                related_information: None,
                tags: None,
                code_description: Option::None,
                data: Option::None,
            };

            let origin_url = Url::from_file_path(origin).unwrap();
            match diagnostics.get_mut(&origin_url) {
                Some(d) => d.push(diagnostic),
                None => {
                    diagnostics.insert(origin_url, vec![diagnostic]);
                },
            };
        }
        diagnostics
    }

    pub fn get_dfs_for_node(&self, root: NodeIndex) -> Result<Vec<(NodeIndex, Option<NodeIndex>)>, dfs::error::CycleError> {
        let graph_ref = self.graph.borrow();

        let dfs = dfs::Dfs::new(&graph_ref, root);

        dfs.collect::<Result<Vec<_>, _>>()
    }

    pub fn load_sources(&self, nodes: &[(NodeIndex, Option<NodeIndex>)]) -> Result<HashMap<PathBuf, String>> {
        let mut sources = HashMap::new();

        for node in nodes {
            let graph = self.graph.borrow();
            let path = graph.get_node(node.0);

            if sources.contains_key(&path) {
                continue;
            }

            let source = match fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) => return Err(anyhow!("error reading {:?}: {}", path, e))
            };
            let source = RE_CRLF.replace_all(&source, "\n").to_string();
            sources.insert(path.clone(), source);
        }

        Ok(sources)
    }

    fn get_file_toplevel_ancestors(&self, uri: &PathBuf) -> Result<Option<Vec<petgraph::stable_graph::NodeIndex>>> {
        let curr_node = match self.graph.borrow_mut().find_node(uri) {
            Some(n) => n,
            None => return Err(anyhow!("node not found {:?}", uri)),
        };
        let roots = self.graph.borrow().collect_root_ancestors(curr_node);
        if roots.is_empty() {
            return Ok(None);
        }
        Ok(Some(roots))
    }

    pub fn publish_diagnostic(&self, diagnostics: HashMap<Url, Vec<Diagnostic>>, document_version: Option<i32>) {
        eprintln!("DIAGNOSTICS:\n{:?}", diagnostics);
        for (uri, diagnostics) in diagnostics {
            self.endpoint.send_notification(PublishDiagnostics::METHOD, PublishDiagnosticsParams {
                uri,
                diagnostics,
                version: document_version,
            }).expect("failed to publish diagnostics");
        }
    }

    fn set_status(&self, status: impl Into<String>, message: impl Into<String>, icon: impl Into<String>) {
        self.endpoint.send_notification(lsp_ext::Status::METHOD, lsp_ext::StatusParams {
            status: status.into(),
            message: Some(message.into()),
            icon: Some(icon.into()),
        }).unwrap_or(());
    }
}

impl LanguageServerHandling for MinecraftShaderLanguageServer {
    fn initialize(&mut self, params: InitializeParams, completable: MethodCompletable<InitializeResult, InitializeError>) {
        self.wait.add(1);

        let mut capabilities = ServerCapabilities::default();
        capabilities.hover_provider = None;
        capabilities.document_link_provider = Some(DocumentLinkOptions {
            resolve_provider: None,
            work_done_progress_options: WorkDoneProgressOptions {
                work_done_progress: None,
            },
        });
        capabilities.execute_command_provider = Some(ExecuteCommandOptions {
            commands: vec!["graphDot".into()],
            work_done_progress_options: WorkDoneProgressOptions {
                work_done_progress: None,
            },
        });
        capabilities.text_document_sync = Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                will_save: None,
                will_save_wait_until: None,
                change: Some(TextDocumentSyncKind::Full),
                save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                    include_text: Some(true),
                }))
            },
        ));

        let root = match params.root_uri {
            Some(uri) => PathBuf::from_url(uri),
            None => {
                completable.complete(Err(MethodError {
                    code: 42069,
                    message: "Must be in workspace".into(),
                    data: InitializeError {
                        retry: false,
                    },
                }));
                return;
            }
        };

        completable.complete(Ok(InitializeResult {
            capabilities,
            server_info: None,
        }));

        self.set_status("loading", "Building dependency graph...", "$(loading~spin)");

        self.root = root;

        self.gen_initial_graph();

        self.set_status("ready", "Project initialized", "$(check)");
    }

    fn shutdown(&mut self, _: (), completable: LSCompletable<()>) {
        eprintln!("shutting down language server...");
        completable.complete(Ok(()));
    }

    fn exit(&mut self, _: ()) {
        self.endpoint.request_shutdown();
    }

    fn workspace_change_configuration(&mut self, params: DidChangeConfigurationParams) {
        //let config = params.settings.as_object().unwrap().get("mcglsl").unwrap();

        eprintln!("{:?}", params.settings.as_object().unwrap());

        self.wait.done();
    }

    fn did_open_text_document(&mut self, params: DidOpenTextDocumentParams) {
        //eprintln!("opened doc {}", params.text_document.uri);
        let path = PathBuf::from_url(params.text_document.uri);
        if !path.starts_with(&self.root) {
            return
        }
        if self.graph.borrow_mut().find_node(&path) == None {
            self.add_file_and_includes_to_graph(&path);
        }
        match self.lint(&path) {
            Ok(diagnostics) => self.publish_diagnostic(diagnostics, None),
            Err(e) => eprintln!("error linting: {}", e),
        }
    }

    fn did_change_text_document(&mut self, _: DidChangeTextDocumentParams) {}

    fn did_close_text_document(&mut self, _: DidCloseTextDocumentParams) {}

    fn did_save_text_document(&mut self, params: DidSaveTextDocumentParams) {
        //eprintln!("saved doc {}", params.text_document.uri);

        let path = PathBuf::from_url(params.text_document.uri);
        if !path.starts_with(&self.root) {
            return
        }
        self.update_includes(&path);
        
        match self.lint(&path) {
            Ok(diagnostics) => self.publish_diagnostic(diagnostics, None),
            Err(e) => eprintln!("error linting: {}", e),
        }
    }

    fn did_change_watched_files(&mut self, _: DidChangeWatchedFilesParams) {}

    fn completion(&mut self, _: TextDocumentPositionParams, completable: LSCompletable<CompletionList>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn resolve_completion_item(&mut self, _: CompletionItem, completable: LSCompletable<CompletionItem>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn hover(&mut self, _: TextDocumentPositionParams, _: LSCompletable<Hover>) {
        self.wait.wait();
        /* completable.complete(Ok(Hover{
            contents: HoverContents::Markup(MarkupContent{
                kind: MarkupKind::Markdown,
                value: String::from("# Hello World"),
            }),
            range: None,
        })); */
    }

    fn execute_command(&mut self, params: ExecuteCommandParams, completable: LSCompletable<Option<Value>>) {
        match self.command_provider.as_ref().unwrap().execute(&params.command, params.arguments, &self.root) {
            Ok(resp) => {
                eprintln!("executed {} successfully", params.command);
                self.endpoint.send_notification(ShowMessage::METHOD, ShowMessageParams {
                    typ: MessageType::Info,
                    message: format!("Command {} executed successfully.", params.command),
                }).expect("failed to send popup/show message notification");
                completable.complete(Ok(Some(resp)))
            },
            Err(err) => {
                self.endpoint.send_notification(ShowMessage::METHOD, ShowMessageParams {
                    typ: MessageType::Error,
                    message: format!("Failed to execute `{}`. Reason: {}", params.command, err),
                }).expect("failed to send popup/show message notification");
                eprintln!("failed to execute {}: {}", params.command, err);
                completable.complete(Err(MethodError::new(32420, err.to_string(), ())))
            },
        }
    }

    fn signature_help(&mut self, _: TextDocumentPositionParams, completable: LSCompletable<SignatureHelp>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn goto_definition(&mut self, _: TextDocumentPositionParams, completable: LSCompletable<Vec<Location>>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn references(&mut self, _: ReferenceParams, completable: LSCompletable<Vec<Location>>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn document_highlight(&mut self, _: TextDocumentPositionParams, completable: LSCompletable<Vec<DocumentHighlight>>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn document_symbols(&mut self, _: DocumentSymbolParams, completable: LSCompletable<Vec<SymbolInformation>>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn workspace_symbols(&mut self, _: WorkspaceSymbolParams, completable: LSCompletable<Vec<SymbolInformation>>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn code_action(&mut self, _: CodeActionParams, completable: LSCompletable<Vec<Command>>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn code_lens(&mut self, _: CodeLensParams, completable: LSCompletable<Vec<CodeLens>>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn code_lens_resolve(&mut self, _: CodeLens, completable: LSCompletable<CodeLens>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn document_link(&mut self, params: DocumentLinkParams, completable: LSCompletable<Vec<DocumentLink>>) {
        eprintln!("document link file: {:?}", params.text_document.uri.to_file_path().unwrap());
        // node for current document
        let curr_doc = params
            .text_document
            .uri
            .to_file_path()
            .unwrap();
        let node = match self.graph.borrow_mut().find_node(&curr_doc) {
            Some(n) => n,
            None => {
                completable.complete(Ok(vec![]));
                return
            },
        };

        let edges: Vec<DocumentLink> = self
            .graph
            .borrow()
            .child_node_indexes(node)
            .into_iter()
            .filter_map(|child| {
                let graph = self.graph.borrow();
                let value = graph.get_edge_meta(node, child);
                let path = graph.get_node(child);
                let url = match Url::from_file_path(&path) {
                    Ok(url) => url,
                    Err(e) => {
                        eprintln!("error converting {:?} into url: {:?}", path, e);
                        return None;
                    }
                };

                Some(DocumentLink {
                    range: Range::new(
                        Position::new(
                            u32::try_from(value.line).unwrap(),
                            u32::try_from(value.start).unwrap()),
                        Position::new(
                            u32::try_from(value.line).unwrap(),
                            u32::try_from(value.end).unwrap()),
                    ),
                    target: Some(url),
                    //tooltip: Some(url.path().to_string().strip_prefix(self.root.clone().unwrap().as_str()).unwrap().to_string()),
                    tooltip: None,
                    data: None,
                })
            }).collect();
        eprintln!("links: {:?}", edges);
        completable.complete(Ok(edges));
    }

    fn document_link_resolve(&mut self, _: DocumentLink, completable: LSCompletable<DocumentLink>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn formatting(&mut self, _: DocumentFormattingParams, completable: LSCompletable<Vec<TextEdit>>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn range_formatting(&mut self, _: DocumentRangeFormattingParams, completable: LSCompletable<Vec<TextEdit>>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn on_type_formatting(&mut self, _: DocumentOnTypeFormattingParams, completable: LSCompletable<Vec<TextEdit>>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn rename(&mut self, _: RenameParams, completable: LSCompletable<WorkspaceEdit>) {
        completable.complete(Err(Self::error_not_available(())));
    }
}
