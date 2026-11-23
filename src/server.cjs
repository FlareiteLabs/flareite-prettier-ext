// The Prettier extension's server. The extension runs it with `node -e`,
// keeps it running for the open folder, and talks to it in JSON lines: a
// request on standard input, `{ id, method, ... }`, and answers on standard
// output with the same `id`. Requests are handled at once, side by side, so
// formatting a file doesn't wait for a folder being checked. Prettier is the
// one each file's folder resolves (the project's), or else a global install;
// each copy is loaded once.
'use strict'

const fs = require('fs')
const path = require('path')
const Module = require('module')
const readline = require('readline')
const { spawnSync } = require('child_process')

const send = message => process.stdout.write(JSON.stringify(message) + '\n')

/** Prettier modules loaded, by their folder. */
const loaded = new Map()

/** Folders global npm packages may be in: the usual ones first, then npm's answer. */
let globalRoots = null
function globalFolders() {
  if (globalRoots) {
    return globalRoots
  }
  const node = path.dirname(process.execPath)
  globalRoots =
    process.platform === 'win32'
      ? [path.join(node, 'node_modules')]
      : [path.join(node, '..', 'lib', 'node_modules')]
  if (process.env.APPDATA) {
    globalRoots.push(path.join(process.env.APPDATA, 'npm', 'node_modules'))
  }
  try {
    const windows = process.platform === 'win32'
    const result = spawnSync(windows ? 'npm.cmd root -g' : 'npm', windows ? [] : ['root', '-g'], {
      encoding: 'utf8',
      shell: windows,
      timeout: 10000,
      windowsHide: true,
    })
    const root = result.status === 0 ? result.stdout.trim() : ''
    if (root && !globalRoots.includes(root)) {
      globalRoots.push(root)
    }
  } catch {
    // npm isn't there; the usual folders will do.
  }
  return globalRoots
}

/** Walks up from a file `prettier` resolved to, to the package's folder. */
function packageFolder(resolved) {
  let folder = path.dirname(resolved)
  for (;;) {
    const manifest = path.join(folder, 'package.json')
    try {
      if (JSON.parse(fs.readFileSync(manifest, 'utf8')).name === 'prettier') {
        return folder
      }
    } catch {
      // Not here.
    }
    const parent = path.dirname(folder)
    if (parent === folder) {
      return null
    }
    folder = parent
  }
}

/** The folder of the Prettier `file` is formatted with, and whether it's a global install. */
function findPrettier(file) {
  const requireFrom = Module.createRequire(file)
  for (const request of ['prettier/package.json', 'prettier']) {
    try {
      const folder = packageFolder(requireFrom.resolve(request))
      if (folder) {
        return { folder, global: false }
      }
    } catch {
      // Not installed for this file.
    }
  }
  if (globalPrettier === undefined) {
    globalPrettier = null
    for (const root of globalFolders()) {
      const folder = path.join(root, 'prettier')
      if (fs.existsSync(path.join(folder, 'package.json'))) {
        globalPrettier = { folder, global: true }
        break
      }
    }
  }
  return globalPrettier
}
/** The global install, once looked for: `null` without one. */
let globalPrettier

function load(found) {
  let entry = loaded.get(found.folder)
  if (!entry) {
    const manifest = JSON.parse(fs.readFileSync(path.join(found.folder, 'package.json'), 'utf8'))
    entry = {
      prettier: require(found.folder),
      version: manifest.version,
      folder: found.folder,
      global: found.global,
    }
    loaded.set(found.folder, entry)
  }
  return entry
}

/**
 * What formatting `request.path` takes: Prettier and its options, or why it isn't formatted
 * (`missing`, `ignored`, `noConfig`, `unsupported`).
 */
async function prepare(request) {
  const file = request.path
  const found = findPrettier(file)
  if (!found) {
    return { missing: true }
  }
  const { prettier, version, global } = load(found)
  const base = { version, global }
  const ignorePath = request.ignorePath
    ? path.resolve(request.folder || path.dirname(file), request.ignorePath)
    : undefined
  const info = await prettier.getFileInfo(file, { ignorePath, withNodeModules: false })
  if (info.ignored) {
    return { ...base, ignored: true }
  }
  const configFile = await prettier.resolveConfigFile(file)
  if (request.requireConfig && !configFile) {
    return { ...base, noConfig: true }
  }
  const config =
    (await prettier.resolveConfig(file, { editorconfig: request.useEditorconfig !== false })) || {}
  if (!info.inferredParser && !config.parser) {
    return { ...base, unsupported: true }
  }
  const options = { ...config, filepath: file }
  // The editor's indentation, where neither a Prettier config nor .editorconfig says.
  if (!configFile) {
    if (options.tabWidth === undefined && request.tabWidth) {
      options.tabWidth = request.tabWidth
    }
    if (options.useTabs === undefined && typeof request.useTabs === 'boolean') {
      options.useTabs = request.useTabs
    }
  }
  return { ...base, prettier, options, configFile: configFile || null }
}

/** An error as the extension shows it: its first line, and where it is (1-based) if known. */
function describe(error) {
  const message = String((error && error.message) || error)
    .split('\n')[0]
    .trim()
  const start = error && error.loc && error.loc.start
  const end = error && error.loc && error.loc.end
  return {
    message,
    line: start && typeof start.line === 'number' ? start.line : null,
    column: start && typeof start.column === 'number' ? start.column : null,
    endLine: end && typeof end.line === 'number' ? end.line : null,
    endColumn: end && typeof end.column === 'number' ? end.column : null,
  }
}

/** Without what `prepare` adds for formatting, for answers. */
function outcome(prepared) {
  const { prettier, options, ...rest } = prepared
  void prettier
  return { ...rest, parser: options ? options.parser || null : null }
}

/** Formats an open document's text (lines joined with `\n`), or a range of it. */
async function format(request) {
  const prepared = await prepare(request)
  if (!prepared.options) {
    return outcome(prepared)
  }
  const options = { ...prepared.options, endOfLine: 'lf' }
  if (typeof request.rangeStart === 'number') {
    options.rangeStart = request.rangeStart
    options.rangeEnd = request.rangeEnd
  }
  try {
    const formatted = await prepared.prettier.format(request.text, options)
    return { ...outcome(prepared), formatted }
  } catch (error) {
    return { ...outcome(prepared), error: describe(error) }
  }
}

/** Whether and how `request.path` would be formatted, for the status bar. */
async function info(request) {
  return outcome(await prepare(request))
}

/**
 * Checks or formats files (`mode`: `check` or `write`), answering for each that isn't
 * formatted or fails, then `done` with the counts. `texts` has the text of files whose
 * documents have unsaved changes (lines joined with `\n`).
 */
async function files(request) {
  const { id, mode } = request
  const texts = request.texts || {}
  const counts = { files: 0, unformatted: 0, errors: 0, skipped: 0, missing: 0 }
  for (const [index, file] of request.files.entries()) {
    if (index % 25 === 0) {
      send({ id, progress: index, total: request.files.length })
      // Lets other requests in between.
      await new Promise(resolve => setImmediate(resolve))
    }
    let prepared
    try {
      prepared = await prepare({ ...request, path: file })
    } catch (error) {
      counts.errors++
      send({ id, file, error: describe(error) })
      continue
    }
    if (prepared.missing) {
      counts.missing++
      continue
    }
    if (!prepared.options) {
      counts.skipped++
      continue
    }
    counts.files++
    const open = Object.prototype.hasOwnProperty.call(texts, file)
    try {
      const text = open ? texts[file] : fs.readFileSync(file, 'utf8')
      const options = open ? { ...prepared.options, endOfLine: 'lf' } : prepared.options
      if (mode === 'check') {
        if (!(await prepared.prettier.check(text, options))) {
          counts.unformatted++
          send({ id, file, unformatted: true })
        }
      } else {
        const formatted = await prepared.prettier.format(text, options)
        if (formatted !== text) {
          counts.unformatted++
          // The editor writes it with the file's own line endings.
          send({ id, file, formatted: formatted.replace(/\r\n/g, '\n') })
        }
      }
    } catch (error) {
      counts.errors++
      send({ id, file, error: describe(error) })
    }
  }
  return { done: true, ...counts }
}

/** Forgets the configs Prettier read, after one changed. */
async function clearCache() {
  globalPrettier = undefined
  for (const { prettier } of loaded.values()) {
    await prettier.clearConfigCache()
  }
  return {}
}

const methods = { format, info, files, clearCache }

readline.createInterface({ input: process.stdin, crlfDelay: Infinity }).on('line', line => {
  if (!line.trim()) {
    return
  }
  let request
  try {
    request = JSON.parse(line)
  } catch (error) {
    console.error(`Prettier's server got a line that isn't JSON: ${error.message}`)
    return
  }
  const method = methods[request.method]
  if (!method) {
    send({ id: request.id, error: { message: `unknown method ${request.method}` } })
    return
  }
  Promise.resolve()
    .then(() => method(request))
    .then(
      answer => send({ id: request.id, ...answer }),
      error => send({ id: request.id, error: describe(error) }),
    )
})

// When its input ends, Node exits once the requests in hand are answered.
send({ ready: true, node: process.version })
