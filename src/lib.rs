//! Prettier for Flareite. Formatting a document asks a Node.js server the
//! extension keeps running for the open folder (`server.cjs`), which loads
//! the Prettier each file resolves (the project's, or else a global install)
//! once and formats with it. The answer is read during the formatting call
//! (`processes::read-output`, API 2.5), so formatting takes milliseconds.
//!
//! Besides formatting: a status bar item says how Prettier treats the file in
//! use; a document Prettier can't parse gets the error in Problems until it
//! formats; and Format All Files and Check Formatting go through a folder,
//! writing files or listing those that aren't formatted in Problems.

mod server;
mod text;

use std::collections::{HashMap, HashSet};

use flareite_extension_api::events::Event;
use flareite_extension_api::language_features::FormattingRequest;
use flareite_extension_api::processes::OutputLine;
use flareite_extension_api::types::{
  Activation, CommandContext, ConfigValue, Diagnostic, DocumentId, LogLevel, MessageLevel,
  Position, Range, Severity, StatusItem, TextEdit,
};
use flareite_extension_api::{Extension, files, host, register, workbench, workspace};
use serde_json::{Map, Value};

use server::{Failure, Server};

/// The status bar item's id.
const STATUS: &str = "status";
/// What clicking the status bar item runs.
const SHOW_LOG: &str = "flareite.prettier.showLog";

/// The file extensions Prettier formats without plugins.
const EXTENSIONS: &[&str] = &[
  "js",
  "cjs",
  "mjs",
  "jsx",
  "ts",
  "cts",
  "mts",
  "tsx",
  "json",
  "jsonc",
  "json5",
  "css",
  "scss",
  "less",
  "html",
  "htm",
  "vue",
  "md",
  "markdown",
  "mdx",
  "yaml",
  "yml",
  "graphql",
  "gql",
  "hbs",
  "handlebars",
];

/// A file's name: what's after its last slash.
fn file_name(path: &str) -> &str {
  path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Whether Prettier formats files with this path's extension.
fn formats_path(path: &str) -> bool {
  file_name(path)
    .rsplit_once('.')
    .is_some_and(|(_, extension)| {
      EXTENSIONS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(extension))
    })
}

/// The folder a file is in.
fn folder_of(path: &str) -> &str {
  path.rfind(['/', '\\']).map_or(path, |index| &path[..index])
}

/// Whether a change to this file can change how Prettier formats.
fn is_config(path: &str) -> bool {
  let name = file_name(path);
  name.starts_with(".prettierrc")
    || name.starts_with("prettier.config.")
    || name == ".prettierignore"
    || name == ".editorconfig"
    || name == "package.json"
}

/// A name for an untitled document of `language`, for Prettier to choose
/// its parser by.
fn untitled_name(language: &str) -> Option<&'static str> {
  Some(match language {
    "javascript" => "untitled.js",
    "javascriptreact" => "untitled.jsx",
    "typescript" => "untitled.ts",
    "typescriptreact" => "untitled.tsx",
    "json" => "untitled.json",
    "jsonc" => "untitled.jsonc",
    "json5" => "untitled.json5",
    "css" => "untitled.css",
    "scss" => "untitled.scss",
    "less" => "untitled.less",
    "html" => "untitled.html",
    "vue" => "untitled.vue",
    "markdown" => "untitled.md",
    "mdx" => "untitled.mdx",
    "yaml" => "untitled.yaml",
    "graphql" => "untitled.graphql",
    "handlebars" => "untitled.hbs",
    _ => return None,
  })
}

/// A document's text at `version`: its lines joined with `\n`, as the
/// editor keeps them.
fn read_document(id: DocumentId, version: u64) -> Result<String, String> {
  let slice = host::read_lines(id, 0, u32::MAX).map_err(|error| format!("{error:?}"))?;
  if slice.version != version {
    return Err("the document changed while it was being read".into());
  }
  Ok(slice.lines.join("\n"))
}

fn boolean(key: &str, default: bool) -> bool {
  match host::configuration(key) {
    Some(ConfigValue::Boolean(value)) => value,
    _ => default,
  }
}

fn string(key: &str, default: &str) -> String {
  match host::configuration(key) {
    Some(ConfigValue::Text(value)) => value,
    _ => default.into(),
  }
}

fn show_message(level: MessageLevel, message: &str) {
  host::show_message(level, message);
}

fn log(level: LogLevel, message: &str) {
  host::log(level, message);
}

/// A field of an answer, as text.
fn text_of<'a>(message: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
  message.get(key).and_then(Value::as_str)
}

fn flag(message: &Map<String, Value>, key: &str) -> bool {
  message.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn number(message: &Map<String, Value>, key: &str) -> Option<u32> {
  message
    .get(key)
    .and_then(Value::as_u64)
    .and_then(|value| u32::try_from(value).ok())
}

/// An error the server described: its message, and where it is (1-based).
struct Located {
  message: String,
  at: Option<(u32, u32)>,
  end: Option<(u32, u32)>,
}

fn located(error: &Value) -> Located {
  let Some(error) = error.as_object() else {
    return Located {
      message: error.to_string(),
      at: None,
      end: None,
    };
  };
  let pair = |line: &str, column: &str| Some((number(error, line)?, number(error, column)?));
  Located {
    message: text_of(error, "message").unwrap_or("failed").to_owned(),
    at: pair("line", "column"),
    end: pair("endLine", "endColumn"),
  }
}

/// A folder Format All Files or Check Formatting is going through.
#[derive(Debug)]
struct Job {
  /// The request's id.
  id: u64,
  /// Checking, rather than formatting.
  check: bool,
  folder: String,
  total: usize,
  done: usize,
  /// Files written, and those that couldn't be.
  written: usize,
  unwritten: usize,
}

#[derive(Default)]
struct Prettier {
  server: Option<Server>,
  /// How the last start failed, so that it's said once.
  start_failure: Option<Failure>,
  /// Times the server ended without being stopped; past a few, it isn't
  /// started again just to fill in the status bar.
  crashes: u32,
  /// Whether the user was told Prettier isn't installed, since the server
  /// started.
  told_missing: bool,
  /// Status bar requests waiting for their answers: the file each is for.
  infos: HashMap<u64, String>,
  job: Option<Job>,
  /// The file in use.
  active: Option<String>,
  /// Files the last format failed for, and why, for the status bar.
  errors: HashMap<String, String>,
  /// Documents given a problem because Prettier couldn't parse them.
  flagged: HashSet<DocumentId>,
  /// Files Check Formatting listed in Problems.
  listed: HashSet<String>,
  /// The open folder's watch, for configs changing.
  watch: Option<u64>,
}

impl Prettier {
  /// The request's fields every file needs: the folder, and the settings.
  fn base(path: &str) -> Map<String, Value> {
    let folder = workspace::folder().map_or_else(|| folder_of(path).to_owned(), |f| f.path);
    let mut message = Map::new();
    message.insert("path".into(), path.into());
    message.insert("folder".into(), folder.into());
    let ignore = string("ignore_path", ".prettierignore");
    if !ignore.trim().is_empty() {
      message.insert("ignorePath".into(), ignore.into());
    }
    message.insert(
      "requireConfig".into(),
      boolean("require_config", false).into(),
    );
    message.insert(
      "useEditorconfig".into(),
      boolean("use_editorconfig", true).into(),
    );
    message
  }

  /// The server, started if it isn't running. `near` is a file it's for,
  /// whose folder it runs in when none is open. `loud` says a failure to
  /// start once; otherwise only the status bar does.
  fn server(&mut self, near: &str, loud: bool) -> Result<&mut Server, Failure> {
    if self.server.is_none() {
      let cwd = workspace::folder()
        .is_none()
        .then(|| folder_of(near).to_owned());
      match Server::start(cwd) {
        Ok(server) => {
          log(LogLevel::Info, "Started Prettier's server.");
          self.server = Some(server);
          self.start_failure = None;
          self.told_missing = false;
        }
        Err(failure) => {
          if loud && self.start_failure.as_ref() != Some(&failure) {
            let level = if failure == Failure::Untrusted {
              MessageLevel::Info
            } else {
              MessageLevel::Error
            };
            show_message(level, &failure.describe());
            self.start_failure = Some(failure.clone());
          }
          log(LogLevel::Warn, &failure.describe());
          return Err(failure);
        }
      }
    }
    Ok(self.server.as_mut().expect("started above"))
  }

  /// Sends `message` as a call of `method` and waits for its answer,
  /// starting the server again once if it had stopped. Lines that came
  /// before it, for other requests, are handled after it.
  fn ask(
    &mut self,
    method: &str,
    message: &Map<String, Value>,
    near: &str,
  ) -> Result<Map<String, Value>, Failure> {
    let mut retried = false;
    loop {
      let server = self.server(near, true)?;
      let mut others = Vec::new();
      let answer = server
        .send(method, message.clone())
        .and_then(|id| server.wait(id, &mut others));
      for line in &others {
        self.handle_line(line);
      }
      match answer {
        Err(Failure::Stopped) if !retried => {
          self.server = None;
          retried = true;
        }
        Err(Failure::Stopped) => {
          self.server = None;
          return Err(Failure::Stopped);
        }
        answer => return answer,
      }
    }
  }

  fn stop_server(&mut self) {
    if let Some(server) = self.server.take() {
      server.stop();
    }
    self.infos.clear();
    if let Some(job) = self.job.take() {
      show_message(
        MessageLevel::Warning,
        &format!("Stopped going through {}.", job.folder),
      );
    }
  }

  fn set_status(text: &str, tooltip: &str) {
    let item = StatusItem {
      text: text.into(),
      tooltip: Some(tooltip.into()),
      command: Some(SHOW_LOG.into()),
    };
    let _ = workbench::set_status_item(STATUS, Some(&item));
  }

  fn hide_status() {
    let _ = workbench::set_status_item(STATUS, None);
  }

  /// Shows what Prettier does with the file in use, asking the server.
  fn refresh_status(&mut self) {
    if let Some(job) = &self.job {
      let verb = if job.check { "Checking" } else { "Formatting" };
      Self::set_status(
        &format!("Prettier: {}/{}", job.done, job.total),
        &format!(
          "{verb} the files in {}. Click for Prettier's log.",
          job.folder
        ),
      );
      return;
    }
    let Some(path) = self.active.clone().filter(|path| formats_path(path)) else {
      Self::hide_status();
      return;
    };
    if let Some(error) = self.errors.get(&path) {
      Self::set_status("⚠ Prettier", &format!("{error}\nClick for Prettier's log."));
      return;
    }
    if self.server.is_none() && self.crashes >= 3 {
      Self::set_status(
        "⚠ Prettier",
        "Prettier's server keeps stopping; its log may say why. Prettier: Restart tries again.",
      );
      return;
    }
    let message = Self::base(&path);
    let sent = self
      .server(&path, false)
      .and_then(|server| server.send("info", message));
    match sent {
      Ok(id) => {
        self.infos.insert(id, path);
      }
      Err(Failure::Untrusted) => Self::set_status(
        "Prettier (untrusted)",
        "Prettier runs the project's code, so it runs only once the folder is trusted.",
      ),
      Err(Failure::Stopped) => self.server = None,
      Err(failure) => Self::set_status("⚠ Prettier", &failure.describe()),
    }
  }

  /// Shows the server's answer about `path`, if it's still the file in use.
  fn show_info(&self, path: &str, answer: &Map<String, Value>) {
    if self.active.as_deref() != Some(path) || self.job.is_some() {
      return;
    }
    if let Some(error) = answer.get("error") {
      Self::set_status("⚠ Prettier", &located(error).message);
    } else if flag(answer, "ignored") {
      let ignore = string("ignore_path", ".prettierignore");
      Self::set_status(
        "Prettier (ignored)",
        &format!("Prettier leaves this file alone: {ignore} lists it."),
      );
    } else if flag(answer, "missing") || flag(answer, "noConfig") || flag(answer, "unsupported") {
      Self::hide_status();
    } else {
      let version = text_of(answer, "version").unwrap_or("?");
      let global = if flag(answer, "global") {
        ", installed globally,"
      } else {
        ""
      };
      let config = text_of(answer, "configFile")
        .map(|file| format!(" as {} says", file_name(file)))
        .unwrap_or_default();
      Self::set_status(
        "✓ Prettier",
        &format!("Prettier {version}{global} formats this file{config}. Click for its log."),
      );
    }
  }

  /// Handles a line the server wrote.
  fn handle_line(&mut self, line: &OutputLine) {
    if line.stderr {
      log(LogLevel::Warn, &line.text);
      return;
    }
    let Some(message) = server::parse(&line.text) else {
      log(LogLevel::Debug, &line.text);
      return;
    };
    if flag(&message, "ready") {
      let node = text_of(&message, "node").unwrap_or("?");
      log(
        LogLevel::Info,
        &format!("Prettier's server is ready (Node.js {node})."),
      );
      return;
    }
    let Some(id) = message.get("id").and_then(Value::as_u64) else {
      return;
    };
    if let Some(path) = self.infos.remove(&id) {
      self.show_info(&path, &message);
    } else if self.job.as_ref().is_some_and(|job| job.id == id) {
      self.handle_job(&message);
    }
  }

  /// Handles what the server says about the folder it's going through.
  fn handle_job(&mut self, message: &Map<String, Value>) {
    let Some(job) = self.job.as_mut() else {
      return;
    };
    if let Some(done) = message.get("progress").and_then(Value::as_u64) {
      job.done = usize::try_from(done).unwrap_or(job.done);
      self.refresh_status();
      return;
    }
    if let Some(file) = text_of(message, "file") {
      if let Some(formatted) = text_of(message, "formatted") {
        match files::write_file(file, formatted) {
          Ok(()) => job.written += 1,
          Err(error) => {
            job.unwritten += 1;
            log(LogLevel::Warn, &format!("Couldn't write {file}: {error:?}"));
          }
        }
      } else if flag(message, "unformatted") {
        let at_start = Position { line: 0, column: 0 };
        let problem = Diagnostic {
          range: Range {
            start: at_start,
            end: at_start,
          },
          severity: Severity::Warning,
          message: "Not formatted as Prettier formats it (Prettier: Format All Files does).".into(),
          code: Some("prettier".into()),
        };
        let _ = host::publish_file_diagnostics(file, &[problem]);
        self.listed.insert(file.to_owned());
      } else if let Some(error) = message.get("error") {
        let error = located(error);
        log(LogLevel::Warn, &format!("{file}: {}", error.message));
        // The file's text isn't at hand, so columns count as bytes.
        let (line, column) = error.at.unwrap_or((1, 1));
        let start = Position {
          line: line.saturating_sub(1),
          column: column.saturating_sub(1),
        };
        let problem = Diagnostic {
          range: Range {
            start,
            end: Position {
              line: start.line,
              column: start.column + 1,
            },
          },
          severity: Severity::Error,
          message: format!("Prettier couldn't format this: {}", error.message),
          code: Some("prettier".into()),
        };
        let _ = host::publish_file_diagnostics(file, &[problem]);
        self.listed.insert(file.to_owned());
      }
      return;
    }
    if flag(message, "done") {
      let job = self.job.take().expect("checked above");
      let count = |key: &str| number(message, key).unwrap_or(0);
      let (checked, unformatted, errors) = (count("files"), count("unformatted"), count("errors"));
      let mut summary = if job.check {
        match unformatted {
          0 => format!("All {checked} files in {} are formatted.", job.folder),
          1 => format!("1 of {checked} files isn't formatted; it's in Problems."),
          n => format!("{n} of {checked} files aren't formatted; they're in Problems."),
        }
      } else {
        format!(
          "Formatted {} of {checked} files in {}.",
          job.written, job.folder
        )
      };
      match errors {
        0 => {}
        1 => summary.push_str(" 1 couldn't be formatted; it's in Problems."),
        n => summary.push_str(&format!(" {n} couldn't be formatted; they're in Problems.")),
      }
      if job.unwritten > 0 {
        summary.push_str(&format!(
          " {} couldn't be written; the log says why.",
          job.unwritten
        ));
      }
      if count("missing") > 0 && checked == 0 {
        summary = "Not installed for these files. Add it to the project (npm install --save-dev \
                   prettier), or install it globally (npm install -g prettier)."
          .into();
      }
      log(LogLevel::Info, &summary);
      show_message(MessageLevel::Info, &summary);
      self.refresh_status();
    }
  }

  /// Format All Files (`check` false) or Check Formatting on `folder`.
  fn go_through(&mut self, check: bool, folder: Option<String>) -> Result<(), String> {
    let folder = folder
      .or_else(|| workspace::folder().map(|folder| folder.path))
      .ok_or("Open a folder first, or choose one in the explorer.")?;
    if let Some(job) = &self.job {
      return Err(format!(
        "Still going through {}; wait for it to finish.",
        job.folder
      ));
    }
    let list = files::list_files(&folder).map_err(|error| format!("{error:?}"))?;
    if list.truncated {
      show_message(
        MessageLevel::Warning,
        "The folder has more than 10,000 files; Prettier goes through the first 10,000.",
      );
    }
    let paths: Vec<String> = list
      .paths
      .into_iter()
      .filter(|path| formats_path(path))
      .collect();
    if paths.is_empty() {
      show_message(
        MessageLevel::Info,
        &format!("{folder} has no files Prettier formats."),
      );
      return Ok(());
    }
    // Documents with unsaved changes, as the user sees them.
    let wanted: HashSet<&str> = paths.iter().map(String::as_str).collect();
    let mut texts = Map::new();
    for document in host::documents().unwrap_or_default() {
      if let Some(path) = document
        .path
        .filter(|path| document.dirty && wanted.contains(path.as_str()))
        && let Ok(text) = read_document(document.id, document.version)
      {
        texts.insert(path, text.into());
      }
    }
    // What the last check or Format All found is out of date either way.
    host::clear_file_diagnostics();
    self.listed.clear();
    let mut message = Self::base(&folder);
    message.remove("path");
    message.insert("mode".into(), if check { "check" } else { "write" }.into());
    message.insert("files".into(), paths.clone().into());
    message.insert("texts".into(), Value::Object(texts));
    let id = self
      .server(&folder, true)
      .and_then(|server| server.send("files", message))
      .map_err(|failure| failure.describe())?;
    log(
      LogLevel::Info,
      &format!(
        "{} {} files in {folder}.",
        if check { "Checking" } else { "Formatting" },
        paths.len()
      ),
    );
    self.job = Some(Job {
      id,
      check,
      folder,
      total: paths.len(),
      done: 0,
      written: 0,
      unwritten: 0,
    });
    self.refresh_status();
    Ok(())
  }

  /// Watches the open folder for Prettier's configs changing.
  fn watch_folder(&mut self) {
    if let Some(watch) = self.watch.take() {
      workspace::unwatch(watch);
    }
    if let Some(folder) = workspace::folder() {
      self.watch = workspace::watch(&folder.path).ok();
    }
  }

  /// Gives a document the problem Prettier couldn't parse it for, or
  /// takes it away (`None`).
  fn flag_document(&mut self, document: DocumentId, version: u64, problem: Option<Diagnostic>) {
    match problem {
      Some(problem) => {
        if host::publish_diagnostics(document, version, &[problem]).is_ok() {
          self.flagged.insert(document);
        }
      }
      None => {
        if self.flagged.remove(&document) {
          let _ = host::publish_diagnostics(document, version, &[]);
        }
      }
    }
  }
}

impl Extension for Prettier {
  fn activate(_context: Activation) -> Result<Self, String> {
    let mut prettier = Self::default();
    prettier.watch_folder();
    prettier.active = workbench::active_file();
    prettier.refresh_status();
    Ok(prettier)
  }

  fn deactivate(&mut self) {
    self.stop_server();
  }

  fn run_command(&mut self, command: &str, context: CommandContext) -> Result<(), String> {
    match command {
      "flareite.prettier.formatAll" => self.go_through(false, context.argument),
      "flareite.prettier.check" => self.go_through(true, context.argument),
      "flareite.prettier.clearProblems" => {
        host::clear_file_diagnostics();
        self.listed.clear();
        for document in std::mem::take(&mut self.flagged) {
          if let Ok(info) = host::document(document) {
            let _ = host::publish_diagnostics(document, info.version, &[]);
          }
        }
        Ok(())
      }
      "flareite.prettier.restart" => {
        self.stop_server();
        self.crashes = 0;
        self.start_failure = None;
        self.errors.clear();
        self.refresh_status();
        Ok(())
      }
      SHOW_LOG => {
        workbench::show_log();
        Ok(())
      }
      _ => Err(format!("{command} isn't one of Prettier's commands")),
    }
  }

  fn handle_event(&mut self, event: Event) -> Result<(), String> {
    let server = self.server.as_ref().map(|server| server.process);
    match event {
      Event::ProcessOutput(output) if Some(output.process) == server => {
        for line in &output.lines {
          self.handle_line(line);
        }
      }
      Event::ProcessFinished(finished) if Some(finished.process) == server => {
        self.server = None;
        self.crashes += 1;
        let how = match &finished.outcome {
          Ok(output) => format!("exit code {:?}", output.exit_code),
          Err(error) => format!("{error:?}"),
        };
        log(
          LogLevel::Warn,
          &format!("Prettier's server stopped ({how})."),
        );
        self.infos.clear();
        if let Some(job) = self.job.take() {
          show_message(
            MessageLevel::Error,
            &format!(
              "The server running Prettier stopped while going through {}; the log may say why.",
              job.folder
            ),
          );
        }
        self.refresh_status();
      }
      Event::ActiveFileChanged(path) => {
        self.active = path;
        self.refresh_status();
      }
      Event::FolderChanged => {
        self.stop_server();
        self.crashes = 0;
        self.start_failure = None;
        self.errors.clear();
        self.listed.clear();
        self.watch_folder();
        self.refresh_status();
      }
      Event::FilesChanged(changes) if Some(changes.watch) == self.watch => {
        let changed: Vec<&String> = changes
          .paths
          .iter()
          .filter(|path| is_config(path))
          .collect();
        if changes.overflow || changed.iter().any(|path| file_name(path) == "package.json") {
          // Prettier may have been installed, upgraded, or removed.
          self.stop_server();
          self.errors.clear();
          self.refresh_status();
        } else if !changed.is_empty() {
          if let Some(server) = self.server.as_mut() {
            let _ = server.send("clearCache", Map::new());
          }
          self.errors.clear();
          self.refresh_status();
        }
      }
      Event::ConfigurationChanged => {
        self.errors.clear();
        self.refresh_status();
      }
      Event::DocumentClosed(document) => {
        self.flagged.remove(&document);
      }
      _ => {}
    }
    Ok(())
  }

  fn provide_formatting(&mut self, request: FormattingRequest) -> Result<Vec<TextEdit>, String> {
    let document = host::document(request.document).map_err(|error| format!("{error:?}"))?;
    let text = read_document(request.document, request.version)?;
    let path = match &document.path {
      Some(path) => path.clone(),
      None => {
        let name = document
          .language
          .as_deref()
          .and_then(untitled_name)
          .ok_or("Prettier can't tell what this document is: save it first.")?;
        let folder = workspace::folder().map_or_else(|| "/".to_owned(), |folder| folder.path);
        format!("{folder}/{name}")
      }
    };
    let mut message = Self::base(&path);
    message.insert("text".into(), text.clone().into());
    message.insert("tabWidth".into(), request.tab_size.into());
    message.insert("useTabs".into(), (!request.insert_spaces).into());
    if let Some(range) = request.range {
      let start = text::offset(&text, range.start);
      let end = text::offset(&text, range.end);
      message.insert("rangeStart".into(), text::utf16(&text, start).into());
      message.insert("rangeEnd".into(), text::utf16(&text, end).into());
    }
    let answer = match self.ask("format", &message, &path) {
      Ok(answer) => answer,
      Err(failure) => {
        let why = failure.describe();
        self.errors.insert(path, why.clone());
        self.refresh_status();
        return Err(why);
      }
    };
    self.crashes = 0;
    if let Some(formatted) = text_of(&answer, "formatted") {
      self.flag_document(request.document, request.version, None);
      if self.listed.remove(&path) {
        let _ = host::publish_file_diagnostics(&path, &[]);
      }
      if self.errors.remove(&path).is_some() || self.active.as_deref() == Some(&path) {
        self.show_info(&path, &answer);
      }
      return Ok(text::edit(&text, formatted).into_iter().collect());
    }
    if let Some(error) = answer.get("error") {
      let error = located(error);
      let name = file_name(&path).to_owned();
      log(LogLevel::Warn, &format!("{name}: {}", error.message));
      if let Some((line, column)) = error.at {
        let problem = Diagnostic {
          range: text::error_range(&text, line, column, error.end),
          severity: Severity::Error,
          message: format!("Prettier couldn't format this: {}", error.message),
          code: Some("prettier".into()),
        };
        self.flag_document(request.document, request.version, Some(problem));
      }
      self.errors.insert(
        path,
        format!("Prettier couldn't format {name}: {}", error.message),
      );
      self.refresh_status();
    } else if flag(&answer, "missing") {
      log(
        LogLevel::Info,
        &format!("Prettier isn't installed for {path}."),
      );
      if !self.told_missing {
        self.told_missing = true;
        show_message(
          MessageLevel::Info,
          "Not installed for this file. Add it to the project (npm install --save-dev \
           prettier), or install it globally (npm install -g prettier).",
        );
      }
    } else if flag(&answer, "ignored") {
      log(LogLevel::Info, &format!("{path} is ignored."));
    } else if flag(&answer, "noConfig") {
      log(
        LogLevel::Info,
        &format!("{path} has no Prettier config, which require_config asks for."),
      );
    } else if flag(&answer, "unsupported") {
      log(
        LogLevel::Info,
        &format!("Prettier has no parser for {path}."),
      );
    }
    Ok(Vec::new())
  }
}

register!(Prettier);

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn knows_the_files_prettier_formats() {
    assert!(formats_path("C:\\p\\src\\main.TSX"));
    assert!(formats_path("/p/.github/workflows/ci.yml"));
    assert!(formats_path("/p/README.md"));
    assert!(!formats_path("/p/Cargo.toml"));
    assert!(!formats_path("/p/Makefile"));
    assert!(!formats_path("/p/.prettierrc"));
  }

  #[test]
  fn knows_prettiers_configs() {
    for path in [
      "/p/.prettierrc",
      "/p/.prettierrc.json",
      "C:\\p\\prettier.config.mjs",
      "/p/.prettierignore",
      "/p/sub/.editorconfig",
      "/p/package.json",
    ] {
      assert!(is_config(path), "{path}");
    }
    assert!(!is_config("/p/src/prettier.ts"));
  }

  #[test]
  fn files_are_named_and_found_in_folders() {
    assert_eq!(file_name("C:\\p\\a.ts"), "a.ts");
    assert_eq!(folder_of("C:\\p\\a.ts"), "C:\\p");
    assert_eq!(folder_of("/p/q/a.ts"), "/p/q");
  }
}
