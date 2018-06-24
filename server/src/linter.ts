import { conf, connection, documents } from './server'
import './global'
import { TextDocument, Diagnostic, DiagnosticSeverity, Range } from 'vscode-languageserver'
import { exec } from 'child_process'
import * as path from 'path'
import { readFileSync } from 'fs';

const reDiag = /^(ERROR|WARNING): ([^?<>:*|"]+?):(\d+): (?:'.*?' : )?(.+)$/
const reVersion = /#version [\d]{3}/
const reInclude = /^(?:\s)*?(?:#include) "((?:\/?[^?<>:*|"]+?)+?\.(?:[a-zA-Z]+?))"$/
const include = '#extension GL_GOOGLE_include_directive : require'

const filters = [
  /(No code generated)/,
  /(compilation terminated)/,
  /Could not process include directive for header name:/
]

const files: {[uri: string]: number} = {}

const ext = {
  '.fsh': 'frag',
  '.gsh': 'geom',
  '.vsh': 'vert',
  //'.glsl': 'frag' //excluding non standard files, need to be treated differently
}

const tokens: {[key: string]: string} = {
  'SEMICOLON': ';',
  'COMMA': ',',
  'COLON': ':',
  'EQUAL': '=',
  'LEFT_PAREN': '(',
  'RIGHT_PAREN': ')',
  'DOT': '.',
  'BANG': '!',
  'DASH': '-',
  'TILDE': '~',
  'PLUS': '+',
  'STAR': '*',
  'SLASH': '/',
  'PERCENT': '%',
  'LEFT_ANGEL': '<',
  'RIGHT_ANGEL': '>',
  'VERICAL_BAR': '|',
  'CARET': '^',
  'AMPERSAND': '&',
  'QUESTION': '?',
  'LEFT_BRACKET': '[',
  'RIGHT_BRACKET': ']',
  'LEFT_BRACE': '{',
  'RIGHT_BRACE': '}'
}

export function preprocess(document: TextDocument, topLevel: boolean, incStack: string[]) {
  const lines = document.getText().split('\n')
  const docURI = formatURI(document.uri)
  if (topLevel) {
    let inComment = false
    for (let i = 0; i < lines.length; i++) {
      const line = lines[i]
      if (line.includes('/*')) inComment = true
      if (line.includes('*/')) inComment = false
      if (line.trim().startsWith('//')) continue
      if (!inComment && reVersion.test(line)) {
        lines.splice(i + 1, 0, include)
        break
      }
      if (i === lines.length - 1) lines.splice(0, 0, include)
    }
  }

  const includes = getIncludes(lines)
  if (includes.length > 0) {
    includes.forEach((inc) => {
      const incPath = absPath(docURI, inc.match[1])
      const data = readFileSync(incPath)
      lines[inc.lineNum] = `#line 0 "${incPath}"`
      lines.splice(inc.lineNum + 1, 0, `#line ${inc.lineNum} "${docURI}"`)
      const dataLines = data.toString().split('\n')
      lines.splice(inc.lineNum + 1, 0, ...dataLines)
      //console.log(lines.join('\n'))
    })
  }

  lint(docURI, lines.join('\n'), includes)
}

const formatURI = (uri: string) => uri.replace(/^file:\/\//, '')

const getIncludes = (lines: string[])  => lines
    .map((line, i) => ({num: i, line}))
    .filter((obj) => reInclude.test(obj.line))
    .map((obj) => ({lineNum: obj.num, match: obj.line.match(reInclude)}))

function absPath(currFile: string, includeFile: string): string {
  if (!currFile.startsWith(conf.shaderpacksPath)) {
    connection.window.showErrorMessage(`Shaderpacks path may not be correct. Current file is in ${currFile} but the path is set to ${conf.shaderpacksPath}`)
    return
  }

  if (includeFile.charAt(0) === '/') {
    const shaderPath = currFile.replace(conf.shaderpacksPath, '').split('/').slice(0, 3).join('/')
    return path.join(conf.shaderpacksPath, shaderPath, includeFile)
  } else {
    const shaderPath = path.dirname(currFile)
    return path.join(shaderPath, includeFile)
  }
}

function lint(uri: string, text: string, includes: {lineNum: number, match: RegExpMatchArray}[]) {
  const child = exec(`${conf.glslangPath} --stdin -S ${ext[path.extname(uri)]}`, (error, out) => {
    const diagnostics: {[uri: string]: Diagnostic[]} = {}
    diagnostics[uri] = []
    includes.forEach(obj => {
      diagnostics[absPath(uri, obj.match[1])] = []
    })
    const matches = filterMatches(out) as RegExpMatchArray[]
    matches.forEach((match) => {
      const [type, file, line, msg] = match.slice(1)
      const diag = {
        severity: type === 'ERROR' ? DiagnosticSeverity.Error : DiagnosticSeverity.Warning,
        range: calcRange(parseInt(line) - 1, uri),
        message: replaceWord(msg),
        source: 'mc-glsl'
      }
      diagnostics[file].push(diag)
    })
    console.log(JSON.stringify(daigsArray(diagnostics), null, 2))
    daigsArray(diagnostics).forEach((d) => {
      console.log(d.uri, d.diag.length)
      connection.sendDiagnostics({uri: 'file://' + d.uri, diagnostics: d.diag})
    })
  })
  child.stdin.write(text)
  child.stdin.end()
}

const daigsArray = (diags: {[uri: string]: Diagnostic[]}) => Object.keys(diags).map(uri => ({uri, diag: diags[uri]}))

const filterMatches = (output: string) => output
  .split('\n')
  .filter(s => s.length > 1 && !filters.some(reg => reg.test(s)))
  .map(s => s.match(reDiag))
  .filter(match => match && match.length === 5)

const replaceWord = (msg: string) => {
  for (const token of Object.keys(tokens)) {
    if (msg.includes(token)) {
      msg = msg.replace(token, tokens[token])
    }
  }
  return msg
}

function calcRange(lineNum: number, uri: string): Range {
  //TODO lineNum needs to be subtracted based off includes
  const lines = documents.get('file://' + uri).getText().split('\n')
  const line = lines[lineNum]
  const startOfLine = line.length - line.leftTrim().length
  const endOfLine = line.slice(0, line.indexOf('//')).rightTrim().length + 1
  return Range.create(lineNum, startOfLine, lineNum, endOfLine)
}