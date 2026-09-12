//! Script-file driver (PLAN.md §13).
//!
//! Runs one REPL command per line after the handshake is ready (or, under the
//! manual profile, as soon as the transport is up). Blank lines and `#` comments
//! are ignored; `wait <dur>` pauses; `--script-delay` pauses between every line.
//! Returns a process exit code for `--exit-after-script` (0 ok; 1 if the peer
//! disconnected first, the file could not be read, or readiness timed out).

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;

use crate::messages::samples::SampleData;
use crate::repl::exec;
use crate::repl::printer::Printer;
use crate::session::events::Command;
use crate::session::view::SessionView;

/// How long to wait for the handshake before giving up.
const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Run the script. `wait_ready` waits for a full handshake; when false (manual
/// profile) it waits only for the transport to be up.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    path: &Path,
    delay: Option<Duration>,
    wait_ready: bool,
    view: Arc<Mutex<SessionView>>,
    commands: UnboundedSender<Command>,
    printer: Printer,
    samples: Arc<SampleData>,
) -> i32 {
    // Wait until we're ready to drive the peer.
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let (connected, ready) = {
            let v = view.lock().unwrap();
            (v.is_connected(), v.peer.handshake.is_ready())
        };
        if (wait_ready && ready) || (!wait_ready && connected) {
            break;
        }
        if Instant::now() > deadline {
            printer.line("script: timed out waiting for the handshake");
            return 1;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            printer.line(&format!("script: cannot read {}: {e}", path.display()));
            return 1;
        }
    };

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(dur) = line.strip_prefix("wait ") {
            match humantime::parse_duration(dur.trim()) {
                Ok(d) => tokio::time::sleep(d).await,
                Err(_) => {
                    printer.line(&format!("script: bad wait duration {dur:?}"));
                    return 1;
                }
            }
            continue;
        }

        if exec(line, &view, &commands, &printer, &samples) {
            break; // `quit`
        }

        // Give the session a moment to act, then check the peer is still there.
        tokio::time::sleep(Duration::from_millis(20)).await;
        if !view.lock().unwrap().is_connected() {
            printer.line("script: peer disconnected before the script finished");
            return 1;
        }
        if let Some(d) = delay {
            tokio::time::sleep(d).await;
        }
    }

    0
}
