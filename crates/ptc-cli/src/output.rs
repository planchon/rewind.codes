//! Where a command writes: its result on stdout, everything else on stderr. With `--json`,
//! the result is one JSON document, and an error is a problem (`application/problem+json`) on
//! stderr.

use std::io::{self, Write};
use std::sync::{Arc, Mutex, PoisonError};

use serde::Serialize;

use crate::error::CliError;

/// The output streams of a command.
pub struct Console {
    out: Box<dyn Write + Send>,
    err: Box<dyn Write + Send>,
    json: bool,
}

impl std::fmt::Debug for Console {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Console")
            .field("json", &self.json)
            .finish_non_exhaustive()
    }
}

impl Console {
    /// The process's stdout and stderr.
    pub fn stdio() -> Self {
        Self::new(io::stdout(), io::stderr())
    }

    pub fn new(out: impl Write + Send + 'static, err: impl Write + Send + 'static) -> Self {
        Self {
            out: Box::new(out),
            err: Box::new(err),
            json: false,
        }
    }

    /// Two in-memory buffers, for tests: what the command printed on stdout and stderr.
    pub fn captured() -> (Self, Captured) {
        let captured = Captured::default();
        let console = Self::new(captured.out.clone(), captured.err.clone());
        (console, captured)
    }

    pub(crate) fn set_json(&mut self, json: bool) {
        self.json = json;
    }

    pub(crate) fn json(&self) -> bool {
        self.json
    }

    /// A line of the result.
    pub(crate) fn line(&mut self, text: impl AsRef<str>) {
        // A closed stdout (`ptc runs | head`) is not an error of the command.
        let _ = writeln!(self.out, "{}", text.as_ref());
    }

    /// The result, as text written as it is.
    pub(crate) fn text(&mut self, text: &str) {
        let _ = self.out.write_all(text.as_bytes());
        if !text.ends_with('\n') && !text.is_empty() {
            let _ = self.out.write_all(b"\n");
        }
    }

    /// A line of information on stderr: progress, hints.
    pub(crate) fn note(&mut self, text: impl AsRef<str>) {
        let _ = writeln!(self.err, "{}", text.as_ref());
    }

    /// The result as one JSON document.
    pub(crate) fn print_json(&mut self, value: &impl Serialize) -> Result<(), CliError> {
        let text = serde_json::to_string_pretty(value)
            .map_err(|error| CliError::internal(format!("cannot write JSON: {error}")))?;
        self.line(text);
        Ok(())
    }

    /// An error: its message and diagnostics, or its problem with `--json`.
    pub(crate) fn error(&mut self, error: &CliError) {
        if self.json {
            let problem = error.problem();
            let text = serde_json::to_string(&problem).unwrap_or_else(|_| problem.detail.clone());
            let _ = writeln!(self.err, "{text}");
            return;
        }
        // A problem's own text holds its diagnostics: they are written once, below.
        match error {
            CliError::Api(ptc_api::Error::Problem(problem)) => {
                let _ = writeln!(self.err, "error: {}: {}", problem.title, problem.detail);
            }
            other => {
                let _ = writeln!(self.err, "error: {other}");
            }
        }
        for diagnostic in error.diagnostics() {
            let _ = writeln!(self.err, "{diagnostic}");
        }
    }

    pub(crate) fn flush(&mut self) {
        let _ = self.out.flush();
        let _ = self.err.flush();
    }
}

/// What a command printed, captured by [`Console::captured`].
#[derive(Debug, Clone, Default)]
pub struct Captured {
    out: SharedBuffer,
    err: SharedBuffer,
}

impl Captured {
    pub fn stdout(&self) -> String {
        self.out.text()
    }

    pub fn stderr(&self) -> String {
        self.err.text()
    }
}

#[derive(Debug, Clone, Default)]
struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl SharedBuffer {
    fn text(&self) -> String {
        let bytes = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl Write for SharedBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
