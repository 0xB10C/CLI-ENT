//! Thin binary entry point (PLAN.md §16, milestones 1–2).
//!
//! Parses CLI args, starts the session actor and the event-printer task on a
//! tokio runtime, and runs the rustyline REPL on the main thread. With a peer
//! argument it issues an initial `connect`; otherwise it starts idle.

use std::fs::OpenOptions;
use std::io::IsTerminal;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use bitcoin::p2p::{Magic, ServiceFlags};
use clap::Parser;
use rustyline::history::DefaultHistory;
use rustyline::{Config, CompletionType, Editor};

use cli_ent::cli::{Args, Transport as TransportPref, Verack};
use cli_ent::repl::completer::ReplHelper;
use cli_ent::repl::printer::Printer;
use cli_ent::repl::{event_loop, run as run_repl, run_plain};
use cli_ent::session::events::Command;
use cli_ent::session::handshake::{Handshaker, Profile, VersionConfig};
use cli_ent::session::view::SessionView;
use cli_ent::session::{Session, SessionConfig};

fn main() -> Result<()> {
    let args = Args::parse();

    // --magic is v1-only (bip324 takes a Network, not raw magic; PLAN §5).
    if args.magic.is_some() && !matches!(args.transport, TransportPref::V1) {
        bail!("--magic is only valid with --transport v1");
    }

    let magic = match &args.magic {
        Some(hex) => parse_magic(hex)?,
        None => args.network.magic(),
    };
    let timeout = parse_duration(&args.timeout).context("parsing --timeout")?;
    let handshaker = build_handshaker(&args)?;
    let color = use_color(args.no_color);

    let config = SessionConfig {
        default_port: args.network.default_port(),
        magic,
        default_transport: args.transport,
        timeout,
        handshaker,
    };

    // Shared state and channels.
    let view = Arc::new(Mutex::new(SessionView::new()));
    let (ev_tx, ev_rx) = tokio::sync::mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();

    let log_file = match &args.log {
        Some(path) => Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("opening log file {}", path.display()))?,
        ),
        None => None,
    };

    // The external printer (which prints above the live prompt) needs a TTY.
    // Non-interactive stdin/stdout (pipes, redirects) use a plain line loop.
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    let session = Session::new(config, view.clone(), ev_tx, cmd_rx);

    if interactive {
        let editor_config = Config::builder()
            .completion_type(CompletionType::List)
            .auto_add_history(false)
            .build();
        let mut editor: Editor<ReplHelper, DefaultHistory> =
            Editor::with_config(editor_config).context("initialising rustyline")?;
        editor.set_helper(Some(ReplHelper));
        let ext = editor
            .create_external_printer()
            .context("creating external printer")?;
        let printer = Printer::external(Box::new(ext), log_file);

        rt.spawn(session.run());
        rt.spawn(event_loop(ev_rx, printer.clone(), color));
        initial_connect(&cmd_tx, &args);

        run_repl(view, cmd_tx, printer, editor);
    } else {
        let printer = Printer::plain(log_file);

        rt.spawn(session.run());
        rt.spawn(event_loop(ev_rx, printer.clone(), color));
        initial_connect(&cmd_tx, &args);

        let stdin = std::io::stdin();
        run_plain(stdin.lock(), view, cmd_tx, printer);
    }

    // Give in-flight teardown a moment, then stop the runtime.
    rt.shutdown_timeout(Duration::from_millis(300));
    Ok(())
}

/// Issue the initial `connect` when a peer was given on the command line.
fn initial_connect(cmd_tx: &tokio::sync::mpsc::UnboundedSender<Command>, args: &Args) {
    if let Some(peer) = &args.peer {
        let _ = cmd_tx.send(Command::Connect {
            addr: peer.clone(),
            transport: Some(args.transport),
        });
    }
}

/// Whether to emit ANSI colour: not disabled, `NO_COLOR` unset, stdout a TTY.
fn use_color(no_color_flag: bool) -> bool {
    !no_color_flag && std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

/// Parse a 4-byte network magic given as 8 hex characters.
fn parse_magic(hex: &str) -> Result<Magic> {
    let hex = hex.strip_prefix("0x").unwrap_or(hex);
    if hex.len() != 8 {
        bail!("--magic must be 8 hex characters (4 bytes), got {:?}", hex);
    }
    let mut bytes = [0u8; 4];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|_| anyhow!("invalid hex in --magic: {hex:?}"))?;
    }
    Ok(Magic::from_bytes(bytes))
}

fn parse_duration(s: &str) -> Result<Duration> {
    humantime::parse_duration(s).map_err(|e| anyhow!("invalid duration {s:?}: {e}"))
}

/// Build the handshaker from CLI flags (defaults per PLAN §6).
fn build_handshaker(args: &Args) -> Result<Handshaker> {
    use cli_ent::cli::Handshake as H;
    let profile = match args.handshake {
        H::Core => Profile::Core,
        H::Minimal => Profile::Minimal,
        H::Manual => Profile::Manual,
    };

    let mut version = VersionConfig::default();
    if let Some(ua) = &args.user_agent {
        version.user_agent = ua.clone();
    }
    if let Some(pv) = args.protocol_version {
        version.protocol_version = pv;
    }
    if let Some(s) = &args.services {
        version.services = parse_services(s).context("parsing --services")?;
    }
    if let Some(h) = args.start_height {
        version.start_height = h;
    }
    if let Some(r) = args.relay {
        version.relay = r;
    }

    Ok(Handshaker {
        profile,
        verack_manual: matches!(args.verack, Verack::Manual),
        version,
    })
}

/// Parse service flags: an integer (`0x…`/decimal) or `|`-joined names.
fn parse_services(s: &str) -> Result<ServiceFlags> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x") {
        let n = u64::from_str_radix(hex, 16).map_err(|_| anyhow!("invalid hex mask {s:?}"))?;
        return Ok(ServiceFlags::from(n));
    }
    if let Ok(n) = s.parse::<u64>() {
        return Ok(ServiceFlags::from(n));
    }
    let mut flags = ServiceFlags::NONE;
    for name in s.split('|') {
        let f = match name.trim().to_ascii_uppercase().as_str() {
            "NONE" => ServiceFlags::NONE,
            "NETWORK" => ServiceFlags::NETWORK,
            "GETUTXO" => ServiceFlags::GETUTXO,
            "BLOOM" => ServiceFlags::BLOOM,
            "WITNESS" => ServiceFlags::WITNESS,
            "COMPACT_FILTERS" => ServiceFlags::COMPACT_FILTERS,
            "NETWORK_LIMITED" => ServiceFlags::NETWORK_LIMITED,
            "P2P_V2" => ServiceFlags::P2P_V2,
            other => bail!("unknown service flag {other:?}"),
        };
        flags |= f;
    }
    Ok(flags)
}
