//! One-line message summaries and event rendering (PLAN.md §12).
//!
//! `summarize` turns a parsed message into its short right-hand description;
//! `render_event` turns a session [`Event`] into the terminal line for it. The
//! REPL printer (milestone 2) and milestone-1 `main` share this code.

use std::time::Instant;

use bitcoin::p2p::message::NetworkMessage;
use bitcoin::p2p::ServiceFlags;

use crate::net::transport::FrameKind;
use crate::session::events::{Direction, Event};

/// Short, human description of a message's contents (the part after the command
/// name). Empty for payload-less messages.
pub fn summarize(msg: &NetworkMessage) -> String {
    use NetworkMessage::*;
    match msg {
        Version(v) => format!(
            "{} {} {} h={} relay={}",
            v.version,
            v.user_agent,
            services_short(&v.services),
            v.start_height,
            v.relay
        ),
        Ping(n) | Pong(n) => format!("nonce=0x{n:016x}"),
        FeeFilter(f) => format!("{f} sat/kvB"),
        SendCmpct(s) => format!("hb={} version={}", s.send_compact, s.version),
        Inv(items) => format!("{} items", items.len()),
        GetData(items) => format!("{} items", items.len()),
        NotFound(items) => format!("{} items", items.len()),
        Addr(a) => format!("{} addrs", a.len()),
        AddrV2(a) => format!("{} addrs", a.len()),
        Headers(h) => format!("{} headers", h.len()),
        GetHeaders(g) => format!("{} locators", g.locator_hashes.len()),
        GetBlocks(g) => format!("{} locators", g.locator_hashes.len()),
        Tx(_) => "1 tx".to_string(),
        Block(_) => "1 block".to_string(),
        Unknown { command, payload } => format!("{command} ({} bytes)", payload.len()),
        _ => String::new(),
    }
}

/// Render `ServiceFlags` without the `ServiceFlags(...)` wrapper.
pub fn services_short(flags: &ServiceFlags) -> String {
    let s = flags.to_string();
    s.strip_prefix("ServiceFlags(")
        .and_then(|s| s.strip_suffix(')'))
        .map(|s| s.to_string())
        .unwrap_or(s)
}

/// Human-readable byte count.
pub fn human_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    if n >= MIB {
        format!("{:.1} MiB", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.1} KiB", n as f64 / KIB as f64)
    } else {
        format!("{n} B")
    }
}

/// Minimal ANSI styling, gated on `color`.
struct Style {
    color: bool,
}

impl Style {
    fn bold<'a>(&self, s: &'a str) -> std::borrow::Cow<'a, str> {
        if self.color {
            format!("\x1b[1m{s}\x1b[0m").into()
        } else {
            s.into()
        }
    }
    fn dim(&self, s: &str) -> String {
        if self.color {
            format!("\x1b[2m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
}

/// Render one event to a printable line, or `None` if it produces no output.
/// `t0` is the reference instant (connect time) for relative timestamps.
pub fn render_event(ev: &Event, t0: Instant, color: bool) -> Option<String> {
    let st = Style { color };
    match ev {
        Event::Connecting { addr, transport } => {
            Some(format!("connecting to {addr} ({transport})"))
        }
        Event::Connected {
            addr,
            transport,
            v2_session_id,
        } => {
            let sid = v2_session_id
                .map(|id| format!("  session-id {}", short_hex(&id)))
                .unwrap_or_default();
            Some(format!("connected to {addr}  transport {transport}{sid}"))
        }
        Event::FellBackToV1 { reason } => {
            Some(st.dim(&format!("fell back to v1: {reason}")))
        }
        Event::Sent {
            seq,
            wire,
            at,
            tag,
        } => {
            let cmd = wire.command();
            let summary = wire
                .msg
                .as_ref()
                .map(summarize)
                .unwrap_or_else(|| raw_note(&wire.frame));
            let tag = tag
                .as_ref()
                .map(|t| st.dim(&format!("  ({t})")))
                .unwrap_or_default();
            Some(format!(
                "#{seq:<4} {} {} {:<12} {summary}{tag}",
                rel_time(*at, t0),
                arrow(Direction::Sent, &st),
                st.bold(&cmd),
            ))
        }
        Event::Recv { seq, wire, at } => {
            let cmd = wire.command();
            let summary = wire
                .msg
                .as_ref()
                .map(summarize)
                .unwrap_or_else(|| raw_note(&wire.frame));
            let bad = matches!(&wire.frame, FrameKind::V1 { checksum_ok: false, .. });
            let extra = if bad { st.dim("  ⚠ bad checksum") } else { String::new() };
            Some(format!(
                "#{seq:<4} {} {} {:<12} {summary}{extra}",
                rel_time(*at, t0),
                arrow(Direction::Recv, &st),
                st.bold(&cmd),
            ))
        }
        Event::DecodeWarning {
            seq,
            command,
            error,
        } => Some(st.dim(&format!(
            "#{seq:<4}          ⚠ {command} payload decode failed: {error}"
        ))),
        Event::Ready => Some(st.bold("     ↪ handshake ready").into_owned()),
        Event::SpamEnded { sent, elapsed } => {
            let secs = elapsed.as_secs_f64();
            let rate = if secs > 0.0 { *sent as f64 / secs } else { 0.0 };
            Some(format!(
                "     spam ended: {sent} sent in {secs:.1}s (≈{:.0}/s)",
                rate
            ))
        }
        Event::Disconnected { reason, stats } => {
            let dur = stats
                .connected_at
                .map(|c| format!("{:.1}s", c.elapsed().as_secs_f64()))
                .unwrap_or_else(|| "0s".to_string());
            Some(format!(
                "     ✖ disconnected: {reason}  after {dur} — in {} msgs / {}, out {} msgs / {}",
                stats.msgs_in,
                human_bytes(stats.bytes_in),
                stats.msgs_out,
                human_bytes(stats.bytes_out),
            ))
        }
        Event::Error(msg) => Some(format!("error: {msg}")),
    }
}

fn raw_note(frame: &FrameKind) -> String {
    match frame {
        FrameKind::V1 { len, .. } => format!("raw, {len} bytes"),
        FrameKind::V2 {
            decoy: true,
            cipher_len,
            ..
        } => format!("decoy ({cipher_len} bytes)"),
        FrameKind::V2 { cipher_len, .. } => format!("raw, {cipher_len} bytes"),
    }
}

/// First and last 4 bytes of a 32-byte id as hex, e.g. `3f9a…c21e`.
fn short_hex(id: &[u8; 32]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}…{:02x}{:02x}{:02x}{:02x}",
        id[0], id[1], id[2], id[3], id[28], id[29], id[30], id[31]
    )
}

fn arrow(dir: Direction, st: &Style) -> String {
    let a = match dir {
        Direction::Sent => "→",
        Direction::Recv => "←",
    };
    if st.color {
        let c = match dir {
            Direction::Sent => "\x1b[36m", // cyan
            Direction::Recv => "\x1b[32m", // green
        };
        format!("{c}{a}\x1b[0m")
    } else {
        a.to_string()
    }
}

fn rel_time(at: Instant, t0: Instant) -> String {
    let secs = at.saturating_duration_since(t0).as_secs_f64();
    format!("+{secs:>7.3}s")
}
