# Prettier for Flareite

Formats JavaScript, TypeScript, CSS, SCSS, Less, HTML, Vue, JSON, Markdown, YAML, GraphQL, and
Handlebars with [Prettier](https://prettier.io): the version your project depends on, with its
config and plugins.

- **Format Document** (Shift+Alt+F), **Format Selection**, and format on save
  (`editor.format_on_save`).
- **Choosing Prettier.** Where a language server formats too (TypeScript, HTML, CSS), Flareite
  asks which formatter to use the first time you format, and keeps the answer in
  `[editor.formatters]`. **Format Document With…** formats once with another, and changes the
  default.
- **Your project's settings:** `.prettierrc` (or any config Prettier reads), `.prettierignore`,
  and `.editorconfig`. Without a Prettier config, the editor's indentation is used.
- **Status bar:** how Prettier treats the file in use (formatted, ignored, or an error). Click it
  for Prettier's log.
- **Problems:** when Prettier can't parse a file, the error is in Problems until it formats.
- **Format All Files** and **Check Formatting**, in the command palette or on a folder in the
  explorer. Checking lists unformatted files in Problems.

## Requirements

- [Node.js](https://nodejs.org) on the `PATH`, or set where it is in `settings.toml`:

  ```toml
  [extensions.programs]
  "flareite.prettier.node" = "C:/Program Files/nodejs/node.exe"
  ```

- Prettier, installed in the project (`npm install --save-dev prettier`), or globally
  (`npm install -g prettier`). The project's copy comes first. Prettier 2 and 3 both work.
- A trusted folder: Prettier loads the project's configs and plugins, which are code.

## Settings

```toml
[extensions.settings."flareite.prettier"]
require_config = false          # format only files with a Prettier config
ignore_path = ".prettierignore" # relative to the open folder
use_editorconfig = true         # .editorconfig sets options a Prettier config doesn't
```

To make Prettier the formatter for a language without being asked:

```toml
[editor.formatters]
typescript = "flareite.prettier"
typescriptreact = "flareite.prettier"
css = "flareite.prettier"
```

## How it works

The extension keeps one Node.js process running (`src/server.cjs`, built into the extension and
started with `node -e`). It loads each Prettier once and formats on request, so formatting takes
milliseconds rather than the half second a fresh `prettier` run takes. Formatting waits for the
answer with `processes::read-output` (extension API 2.5). The server is restarted when
`package.json` changes, and forgets Prettier's configs when one changes.

The editor keeps documents with `\n` line breaks, so documents are formatted with
`endOfLine: "lf"`, and saving writes the file's own line endings. Format All Files does the same.

## Building

```sh
cargo build --release
```

Tests: `cargo test --target x86_64-pc-windows-msvc` (or your machine's target).

## License

MIT or Apache-2.0.
