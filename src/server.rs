//! The Node.js server that runs Prettier (`server.cjs`), started with
//! `node -e` and kept running, and the JSON lines the extension and it talk
//! in: requests with an `id` and a `method`, and answers with the `id`.

use flareite_extension_api::processes::{self, OutputLine, ProcessError, ProcessOptions};
use serde_json::{Map, Value};

/// The server's code.
const SCRIPT: &str = include_str!("server.cjs");

/// How long a call waits for an answer that doesn't come. Flareite waits
/// five seconds for formatting.
const ANSWER_WAIT_MS: u32 = 4_500;

/// Why the server couldn't be started or didn't answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
  /// Node.js isn't installed, or isn't on the PATH; why.
  NoNode(String),
  /// Programs don't run in the open folder until it's trusted.
  Untrusted,
  /// The server had ended.
  Stopped,
  /// No answer came in time.
  TimedOut,
  Other(String),
}

impl Failure {
  /// What went wrong, for the user.
  pub fn describe(&self) -> String {
    match self {
      Self::NoNode(why) => format!(
        "Node.js, which runs Prettier, wasn't found ({why}). Install it from nodejs.org, or set \
         where it is: [extensions.programs] \"flareite.prettier.node\" in settings.toml."
      ),
      Self::Untrusted => {
        "The project's configs and plugins are code, so Prettier runs only once the folder is \
         trusted."
          .into()
      }
      Self::Stopped => "The server running Prettier stopped; the log may say why.".into(),
      Self::TimedOut => "No answer from Prettier in time.".into(),
      Self::Other(why) => format!("Node.js couldn't be started: {why}"),
    }
  }
}

fn failure(error: ProcessError) -> Failure {
  match error {
    ProcessError::NotFound(why) => Failure::NoNode(why),
    ProcessError::Untrusted => Failure::Untrusted,
    ProcessError::InvalidArgument(_) => Failure::Stopped,
    other => Failure::Other(format!("{other:?}")),
  }
}

/// The running server.
#[derive(Debug)]
pub struct Server {
  /// Its program, as `spawn-interactive` returned.
  pub process: u64,
  /// The last request's id.
  last: u64,
}

impl Server {
  /// Starts the server in `cwd` (the open folder when `None`).
  pub fn start(cwd: Option<String>) -> Result<Self, Failure> {
    let options = ProcessOptions {
      cwd,
      env: Vec::new(),
      stdin: None,
      timeout_ms: None,
    };
    let process = processes::spawn_interactive("node", &["-e".into(), SCRIPT.into()], &options)
      .map_err(failure)?;
    Ok(Self { process, last: 0 })
  }

  /// Sends `request` (an object) as a call of `method`: its id.
  pub fn send(&mut self, method: &str, mut request: Map<String, Value>) -> Result<u64, Failure> {
    self.last += 1;
    request.insert("id".into(), self.last.into());
    request.insert("method".into(), method.into());
    let mut line = Value::Object(request).to_string();
    line.push('\n');
    processes::write_input(self.process, line.as_bytes()).map_err(failure)?;
    Ok(self.last)
  }

  /// Waits for the answer to request `id`, putting the lines that came
  /// before or with it, for other requests, in `others`, in order.
  pub fn wait(&self, id: u64, others: &mut Vec<OutputLine>) -> Result<Map<String, Value>, Failure> {
    loop {
      let lines = processes::read_output(self.process, ANSWER_WAIT_MS).map_err(failure)?;
      if lines.is_empty() {
        return Err(Failure::TimedOut);
      }
      let mut answer = None;
      for line in lines {
        if answer.is_none()
          && let Some(message) = answer_to(&line, id)
        {
          answer = Some(message);
          continue;
        }
        others.push(line);
      }
      if let Some(answer) = answer {
        return Ok(answer);
      }
    }
  }

  /// Stops the server.
  pub fn stop(&self) {
    processes::kill(self.process);
  }
}

/// `line`, if it's the answer to request `id` (not its progress).
fn answer_to(line: &OutputLine, id: u64) -> Option<Map<String, Value>> {
  if line.stderr {
    return None;
  }
  let message = parse(&line.text)?;
  (message.get("id").and_then(Value::as_u64) == Some(id) && !message.contains_key("progress"))
    .then_some(message)
}

/// A line the server wrote, if it's a JSON object.
pub fn parse(text: &str) -> Option<Map<String, Value>> {
  match serde_json::from_str(text) {
    Ok(Value::Object(message)) => Some(message),
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn line(text: &str, stderr: bool) -> OutputLine {
    OutputLine {
      stderr,
      text: text.into(),
    }
  }

  #[test]
  fn answers_are_told_apart_from_progress_and_other_requests() {
    assert!(answer_to(&line(r#"{"id":3,"formatted":"x"}"#, false), 3).is_some());
    assert!(answer_to(&line(r#"{"id":3,"formatted":"x"}"#, false), 4).is_none());
    assert!(answer_to(&line(r#"{"id":3,"progress":25}"#, false), 3).is_none());
    assert!(answer_to(&line(r#"{"id":3}"#, true), 3).is_none());
    assert!(answer_to(&line("not json", false), 3).is_none());
  }

  #[test]
  fn the_script_is_small_enough_for_a_command_line() {
    // Windows' command lines are at most 32,767 characters.
    assert!(SCRIPT.encode_utf16().count() < 24_000, "{}", SCRIPT.len());
  }
}
