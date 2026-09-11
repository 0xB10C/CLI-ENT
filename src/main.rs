//! Thin binary entry point (PLAN.md §16, milestone 1).
//!
//! Parses CLI args and, given a peer, connects and runs the session, printing
//! every message in and out until the peer disconnects or Ctrl-C. The interactive
//! REPL arrives in milestone 2; script mode in milestone 10.

use std::io::IsTerminal;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use bitcoin::p2p::{Magic, ServiceFlags};
use clap::Parser;

use cli_ent::cli::{Args, Transport as TransportPref, Verack};
use cli_ent::messages::summary::render_event;
use cli_ent::net::resolve::resolve;
use cli_ent::session::events::{Command, Event};
use cli_ent::session::handshake::{Handshaker, Profile, VersionConfig};
use cli_ent::session::view::SessionView;
use cli_ent::session::Session;

fn main() -> Result<()> {
    let args = Args::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;
    rt.block_on(run(args))
}

async fn run(args: Args) -> Result<()> {
    let color = use_color(args.no_color);

    let Some(peer_spec) = args.peer.clone() else {
        eprintln!(
            "cli-ent {}: no peer given.\n\
             The interactive REPL arrives in milestone 2; for now pass a peer, e.g.\n\
             \x20   cli-ent 127.0.0.1:18444 --network regtest --transport v1",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    };

    // --magic is v1-only (bip324 takes a Network, not raw magic; PLAN §5).
    if args.magic.is_some() && !matches!(args.transport, TransportPref::V1) {
        bail!("--magic is only valid with --transport v1");
    }

    let magic = match &args.magic {
        Some(hex) => parse_magic(hex)?,
        None => args.network.magic(),
    };
    let addr = resolve(&peer_spec, args.network.default_port())
        .with_context(|| format!("resolving peer {peer_spec:?}"))?;
    let timeout = parse_duration(&args.timeout).context("parsing --timeout")?;
    let handshaker = build_handshaker(&args)?;

    let view = Arc::new(Mutex::new(SessionView::new()));
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();

    // t0 anchors the relative timestamps to just before connecting.
    let t0 = Instant::now();
    let printer = tokio::spawn(async move {
        while let Some(ev) = ev_rx.recv().await {
            let done = matches!(ev, Event::Disconnected { .. });
            if let Some(line) = render_event(&ev, t0, color) {
                println!("{line}");
            }
            if done {
                break;
            }
        }
    });

    let session = Session::connect(
        peer_spec, addr, args.transport, magic, timeout, handshaker, view, ev_tx, cmd_rx,
    )
    .await?;

    // Ctrl-C requests a clean disconnect.
    let cmd_tx_sig = cmd_tx.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = cmd_tx_sig.send(Command::Quit);
        }
    });

    session.run().await;
    drop(cmd_tx);
    let _ = printer.await;
    Ok(())
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
