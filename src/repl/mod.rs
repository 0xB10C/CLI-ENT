//! The REPL (PLAN.md §8).
//!
//! Runs on the main thread: rustyline reads a line, we parse and dispatch it.
//! Session events print above the prompt via [`Printer`] from a background task
//! ([`event_loop`]). `status` and `show` read [`SessionView`] directly.

pub mod commands;
pub mod completer;
pub mod printer;

use std::sync::{Arc, Mutex};
use std::time::Instant;


use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use rustyline::Editor;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::messages::samples::SampleData;
use crate::messages::{presets, summary::human_bytes, summary::render_event, summary::services_short};
use crate::session::automations::{AutoKind, AutoState};
use crate::session::events::{Command, Event, HandshakeState};
use crate::session::view::SessionView;

use self::commands::{parse, Action, ShowTarget};
use self::completer::ReplHelper;
use self::printer::Printer;

/// The static prompt (PLAN.md §8: state lives in events/`status`, not the prompt).
const PROMPT: &str = "cli-ent> ";

/// Consume session events and print them above the prompt. Relative timestamps
/// reset at each new connection.
pub async fn event_loop(mut events: UnboundedReceiver<Event>, printer: Printer, color: bool) {
    let mut t0 = Instant::now();
    while let Some(ev) = events.recv().await {
        if matches!(ev, Event::Connecting { .. }) {
            t0 = Instant::now();
        }
        if let Some(line) = render_event(&ev, t0, color) {
            printer.line(&line);
        }
    }
}

/// Whether the loop should keep reading input.
enum Flow {
    Continue,
    Quit,
}

/// Parse and act on one input line, shared by the interactive and plain loops.
fn dispatch(
    line: &str,
    view: &Arc<Mutex<SessionView>>,
    commands: &UnboundedSender<Command>,
    printer: &Printer,
    samples: &Arc<SampleData>,
) -> Flow {
    match parse(line, samples) {
        Action::Nothing => Flow::Continue,
        Action::Quit => {
            let _ = commands.send(Command::Quit);
            Flow::Quit
        }
        Action::ToSession(cmd) => {
            let _ = commands.send(cmd);
            Flow::Continue
        }
        Action::Status => {
            print_status(view, printer);
            Flow::Continue
        }
        Action::Show { target, hex } => {
            print_show(view, printer, target, hex);
            Flow::Continue
        }
        Action::AutoList => {
            print_auto(view, printer);
            Flow::Continue
        }
        Action::PresetList => {
            for (name, desc) in presets::list() {
                printer.line(&format!("  {name:<22} {desc}"));
            }
            Flow::Continue
        }
        Action::Preset(name) => {
            match presets::build(&name, samples) {
                Some(msgs) => {
                    for msg in msgs {
                        let _ = commands.send(Command::Send(msg));
                    }
                }
                None => printer.line(&format!("unknown preset {name:?}; try `preset list`")),
            }
            Flow::Continue
        }
        Action::Help(topic) => {
            print_help(printer, topic.as_deref());
            Flow::Continue
        }
        Action::Usage(u) => {
            printer.line(&u);
            Flow::Continue
        }
    }
}

/// Run the interactive rustyline loop until the user quits (`quit`/Ctrl-D).
pub fn run(
    view: Arc<Mutex<SessionView>>,
    commands: UnboundedSender<Command>,
    printer: Printer,
    samples: Arc<SampleData>,
    mut editor: Editor<ReplHelper, DefaultHistory>,
) {
    loop {
        match editor.readline(PROMPT) {
            Ok(line) => {
                let _ = editor.add_history_entry(line.as_str());
                if let Flow::Quit = dispatch(&line, &view, &commands, &printer, &samples) {
                    break;
                }
            }
            Err(ReadlineError::Interrupted) => {
                printer.line("(ctrl-c — use `quit` or ctrl-d to exit)");
            }
            Err(ReadlineError::Eof) => {
                let _ = commands.send(Command::Quit);
                break;
            }
            Err(e) => {
                printer.line(&format!("readline error: {e}"));
                break;
            }
        }
    }
}

/// Run a non-interactive loop reading commands from `input` line by line (pipes,
/// redirects). No line editing or prompt; each line is dispatched in order.
pub fn run_plain<R: std::io::BufRead>(
    input: R,
    view: Arc<Mutex<SessionView>>,
    commands: UnboundedSender<Command>,
    printer: Printer,
    samples: Arc<SampleData>,
) {
    for line in input.lines() {
        let Ok(line) = line else { break };
        if let Flow::Quit = dispatch(&line, &view, &commands, &printer, &samples) {
            return;
        }
    }
    // EOF with no explicit quit: end the session cleanly.
    let _ = commands.send(Command::Quit);
}

fn print_status(view: &Arc<Mutex<SessionView>>, printer: &Printer) {
    let v = view.lock().unwrap();
    if !v.is_connected() {
        printer.line("not connected");
        return;
    }
    let mut lines = Vec::new();
    if let Some(addr) = v.peer.addr {
        let from = v
            .peer
            .resolved_from
            .as_ref()
            .map(|s| format!("  (resolved from {s})"))
            .unwrap_or_default();
        lines.push(format!("peer          {addr}{from}"));
    }
    if let Some(t) = v.peer.transport {
        lines.push(format!("transport     {t}"));
    }
    if let Some(c) = v.stats.connected_at {
        lines.push(format!("connected     {:.1}s ago", c.elapsed().as_secs_f64()));
    }
    lines.push(format!("handshake     {}", handshake_str(&v.peer.handshake)));
    if let Some(pv) = &v.peer.peer_version {
        lines.push(format!(
            "peer version  {} {} services {} height {} relay {}",
            pv.version,
            pv.user_agent,
            services_short(&pv.services),
            pv.start_height,
            pv.relay
        ));
    }
    let n = &v.peer.negotiated;
    let sendcmpct = n
        .sendcmpct
        .map(|(hb, ver)| format!("sendcmpct v{ver} hb={hb}  "))
        .unwrap_or_default();
    let feefilter = n
        .feefilter
        .map(|f| format!("feefilter {f} sat/kvB"))
        .unwrap_or_default();
    lines.push(format!(
        "negotiated    wtxidrelay {}  addrv2 {}  sendheaders {}  {sendcmpct}{feefilter}",
        yn(n.wtxidrelay),
        yn(n.addrv2),
        yn(n.sendheaders),
    ));
    lines.push(format!(
        "traffic       in {} msgs / {}   out {} msgs / {}",
        v.stats.msgs_in,
        human_bytes(v.stats.bytes_in),
        v.stats.msgs_out,
        human_bytes(v.stats.bytes_out),
    ));
    lines.push(format!("automations   {}", auto_line(&v.automations)));
    for l in lines {
        printer.line(&l);
    }
}

fn yn(b: bool) -> &'static str {
    if b {
        "✓"
    } else {
        "✗"
    }
}

fn auto_line(a: &AutoState) -> String {
    AutoKind::ALL
        .iter()
        .map(|k| format!("{} {}", k.name(), yn(a.get(*k))))
        .collect::<Vec<_>>()
        .join("  ")
}

fn print_auto(view: &Arc<Mutex<SessionView>>, printer: &Printer) {
    let a = view.lock().unwrap().automations;
    printer.line(&auto_line(&a));
}

fn handshake_str(hs: &HandshakeState) -> String {
    if hs.is_ready() {
        return "Ready".to_string();
    }
    let m = |b: bool| if b { "✓" } else { "✗" };
    format!(
        "version {}→ {}←  verack {}→ {}←",
        m(hs.version_sent),
        m(hs.version_received),
        m(hs.verack_sent),
        m(hs.verack_received),
    )
}

fn print_show(view: &Arc<Mutex<SessionView>>, printer: &Printer, target: ShowTarget, hex: bool) {
    let v = view.lock().unwrap();
    let entry = match target {
        ShowTarget::Last => v.last(),
        ShowTarget::Seq(seq) => v.get(seq),
    };
    let Some(entry) = entry else {
        printer.line("no such message in the buffer");
        return;
    };

    if hex {
        let dir = match entry.dir {
            crate::session::events::Direction::Sent => "→",
            crate::session::events::Direction::Recv => "←",
        };
        printer.line(&format!(
            "#{} {} {}   {} bytes",
            entry.seq,
            dir,
            entry.wire.command(),
            entry.wire.raw.len()
        ));
        for row in hexdump(&entry.wire.raw) {
            printer.line(&row);
        }
    } else {
        // Default: field-by-field annotation (PLAN §12).
        for row in crate::messages::annotate::annotate(entry) {
            printer.line(&row);
        }
    }
}

/// 16-bytes-per-row hex dump with offset and ASCII gutter (PLAN.md §12).
fn hexdump(bytes: &[u8]) -> Vec<String> {
    let mut rows = Vec::new();
    for (i, chunk) in bytes.chunks(16).enumerate() {
        let mut hex = String::new();
        let mut ascii = String::new();
        for (j, b) in chunk.iter().enumerate() {
            hex.push_str(&format!("{b:02x} "));
            if j == 7 {
                hex.push(' ');
            }
            ascii.push(if b.is_ascii_graphic() || *b == b' ' {
                *b as char
            } else {
                '.'
            });
        }
        rows.push(format!("{:04x}  {:<49} {}", i * 16, hex, ascii));
    }
    if rows.is_empty() {
        rows.push("(empty payload)".to_string());
    }
    rows
}

fn print_help(printer: &Printer, topic: Option<&str>) {
    match topic {
        None => {
            for l in [
                "commands:",
                "  connect <host:port> [--v1|--v2|--auto]   connect to a peer",
                "  disconnect                               close the connection",
                "  status                                   show peer/session state",
                "  send <message> [field=value ...]         send a message",
                "  show [last | <seq>] [hex]                dump a captured message",
                "  help [<command>]                         this help",
                "  quit | exit                              leave",
                "",
                "messages: verack getaddr mempool sendheaders wtxidrelay sendaddrv2",
                "          ping pong feefilter sendcmpct inv getdata notfound getheaders getblocks",
            ] {
                printer.line(l);
            }
        }
        Some("send") => {
            printer.line("send <message> [field=value ...]");
            printer.line("  ping [nonce=<u64>] | pong nonce=<u64> | feefilter <sat/kvB>");
            printer.line("  sendcmpct hb=<bool> version=<u64>");
            printer.line("  inv|getdata|notfound tx=<txid>,… block=<hash>,… wtx=<wtxid>,… cmpct=<hash>,…");
            printer.line("  getheaders|getblocks locator=<hash>[,…] [stop=<hash>]");
        }
        Some(other) => printer.line(&format!("no extended help for {other:?}")),
    }
}
