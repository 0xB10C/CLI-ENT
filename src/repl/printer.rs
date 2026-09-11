//! Output sink (PLAN.md §8, §12).
//!
//! Wraps rustyline's [`ExternalPrinter`] so lines print *above* the prompt while
//! the user is typing. Shared (cloneable `Arc`) between the event task and the
//! REPL thread so all output is serialised through one lock. Optionally mirrors
//! every line to a `--log` file with ANSI colour stripped (PLAN.md §12; absolute
//! timestamps arrive in milestone 10).

use std::fs::File;
use std::io::Write;
use std::sync::{Arc, Mutex};

use rustyline::ExternalPrinter;

/// Where lines go: rustyline's external printer (prints above the live prompt on
/// a TTY) or plain stdout (non-interactive: pipes, redirects, script mode).
enum Backend {
    External(Box<dyn ExternalPrinter + Send>),
    Plain,
}

struct Inner {
    backend: Backend,
    file: Option<File>,
}

/// A cloneable handle to the terminal (and optional log file).
#[derive(Clone)]
pub struct Printer {
    inner: Arc<Mutex<Inner>>,
}

impl Printer {
    /// Interactive printer that writes above the rustyline prompt.
    pub fn external(ext: Box<dyn ExternalPrinter + Send>, file: Option<File>) -> Self {
        Self::wrap(Backend::External(ext), file)
    }

    /// Non-interactive printer that writes plain lines to stdout.
    pub fn plain(file: Option<File>) -> Self {
        Self::wrap(Backend::Plain, file)
    }

    fn wrap(backend: Backend, file: Option<File>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner { backend, file })),
        }
    }

    /// Print one line to the terminal and, if configured, the log file.
    pub fn line(&self, s: &str) {
        let mut inner = self.inner.lock().unwrap();
        match &mut inner.backend {
            Backend::External(ext) => {
                let _ = ext.print(format!("{s}\n"));
            }
            Backend::Plain => {
                let mut out = std::io::stdout().lock();
                let _ = writeln!(out, "{s}");
            }
        }
        if let Some(f) = inner.file.as_mut() {
            let _ = writeln!(f, "{}", strip_ansi(s));
        }
    }
}

/// Remove ANSI SGR escape sequences (`ESC [ … m`) for the log file.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip until the terminating letter of the escape sequence.
            for e in chars.by_ref() {
                if e.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::strip_ansi;

    #[test]
    fn strips_color_codes() {
        assert_eq!(strip_ansi("\x1b[1mhi\x1b[0m there"), "hi there");
        assert_eq!(strip_ansi("plain"), "plain");
    }
}
