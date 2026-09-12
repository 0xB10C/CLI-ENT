//! Field-by-field annotation of a captured message (PLAN.md §12).
//!
//! We compute byte spans by serializing each field and accumulating lengths,
//! then render those spans against the raw bytes. For v1 the raw bytes are the
//! full frame (24-byte header + payload); for v2 they are the plaintext content
//! (message-id byte(s) + payload). Messages we could not parse, and field
//! layouts we don't model, fall back to a single `payload` span.

use bitcoin::consensus::serialize;
use bitcoin::p2p::message::NetworkMessage;

use crate::net::transport::{FrameKind, Wire};
use crate::session::events::Direction;
use crate::session::view::RingEntry;

/// One labelled byte range within the raw bytes.
struct Span {
    start: usize,
    len: usize,
    name: String,
    note: String,
}

impl Span {
    fn new(start: usize, len: usize, name: impl Into<String>, note: impl Into<String>) -> Self {
        Span {
            start,
            len,
            name: name.into(),
            note: note.into(),
        }
    }
}

/// Render an annotated view of a captured message.
pub fn annotate(entry: &RingEntry) -> Vec<String> {
    let arrow = match entry.dir {
        Direction::Sent => "→",
        Direction::Recv => "←",
    };
    let raw = &entry.wire.raw;
    let mut out = Vec::new();

    let spans = match &entry.wire.frame {
        FrameKind::V1 { checksum_ok, .. } => {
            out.push(format!(
                "#{} {} {}   v1 frame, {} bytes",
                entry.seq,
                arrow,
                entry.wire.command(),
                raw.len()
            ));
            v1_spans(&entry.wire, *checksum_ok)
        }
        FrameKind::V2 {
            cipher_len,
            decoy,
            short_id,
        } => {
            out.push(format!(
                "#{} {} {}   v2 plaintext, {} bytes (ciphertext {}{})",
                entry.seq,
                arrow,
                entry.wire.command(),
                raw.len(),
                cipher_len,
                if *decoy { ", decoy" } else { "" }
            ));
            v2_spans(&entry.wire, *short_id)
        }
    };

    for span in spans {
        out.push(render_span(raw, &span));
    }
    out
}

/// Spans for a v1 frame: the 24-byte header plus the payload.
fn v1_spans(wire: &Wire, checksum_ok: bool) -> Vec<Span> {
    let mut spans = vec![
        Span::new(0, 4, "magic", String::new()),
        Span::new(4, 12, "command", String::new()),
        Span::new(16, 4, "length", String::new()),
        Span::new(
            20,
            4,
            "checksum",
            if checksum_ok { "ok" } else { "BAD" }.to_string(),
        ),
    ];
    let payload_start = 24.min(wire.raw.len());
    spans.extend(payload_spans(wire, payload_start));
    spans
}

/// Spans for a v2 plaintext: the message-id byte(s) plus the payload.
fn v2_spans(wire: &Wire, short_id: Option<u8>) -> Vec<Span> {
    let id_len = if short_id.is_some() { 1 } else { 13 };
    let id_len = id_len.min(wire.raw.len());
    let mut spans = vec![Span::new(
        0,
        id_len,
        "message-id",
        match short_id {
            Some(id) => format!("short id {id}"),
            None => "long (0x00 + 12-byte command)".to_string(),
        },
    )];
    spans.extend(payload_spans(wire, id_len));
    spans
}

/// Field spans for the payload, offset to `base`. Falls back to one span.
fn payload_spans(wire: &Wire, base: usize) -> Vec<Span> {
    let payload_len = wire.raw.len().saturating_sub(base);
    let Some(msg) = &wire.msg else {
        return vec![Span::new(base, payload_len, "payload", String::new())];
    };

    // Field lengths within the payload; a `None` return means "unmodelled".
    let Some(fields) = msg_fields(msg) else {
        return vec![Span::new(base, payload_len, "payload", String::new())];
    };

    let mut spans = Vec::new();
    let mut off = base;
    for (name, len, note) in fields {
        spans.push(Span::new(off, len, name, note));
        off += len;
    }
    // Any trailing bytes we didn't model.
    if off < base + payload_len {
        spans.push(Span::new(off, base + payload_len - off, "…rest", String::new()));
    }
    spans
}

/// Modelled `(name, length, note)` field layout of a payload, or `None` to fall
/// back to a single span.
fn msg_fields(msg: &NetworkMessage) -> Option<Vec<(String, usize, String)>> {
    use NetworkMessage as M;
    let f = |n: &str, l: usize| (n.to_string(), l, String::new());
    match msg {
        M::Ping(_) | M::Pong(_) => Some(vec![f("nonce", 8)]),
        M::FeeFilter(_) => Some(vec![f("feerate", 8)]),
        M::SendCmpct(_) => Some(vec![f("high_bandwidth", 1), f("version", 8)]),
        M::Version(v) => Some(vec![
            f("version", 4),
            f("services", 8),
            f("timestamp", 8),
            f("addr_recv", 26),
            f("addr_from", 26),
            f("nonce", 8),
            (
                "user_agent".to_string(),
                serialize(&v.user_agent).len(),
                format!("{:?}", v.user_agent),
            ),
            f("start_height", 4),
            f("relay", 1),
        ]),
        M::Inv(items) | M::GetData(items) | M::NotFound(items) => {
            let mut out = vec![(
                "count".to_string(),
                varint_len(items.len() as u64),
                format!("{} items", items.len()),
            )];
            for (i, _) in items.iter().enumerate().take(8) {
                out.push(f(&format!("inv[{i}]"), 36));
            }
            Some(out)
        }
        M::GetHeaders(g) => {
            let mut out = vec![
                f("version", 4),
                (
                    "count".to_string(),
                    varint_len(g.locator_hashes.len() as u64),
                    format!("{} locators", g.locator_hashes.len()),
                ),
            ];
            for i in 0..g.locator_hashes.len().min(8) {
                out.push(f(&format!("locator[{i}]"), 32));
            }
            out.push(f("stop_hash", 32));
            Some(out)
        }
        M::GetBlocks(g) => {
            let mut out = vec![
                f("version", 4),
                (
                    "count".to_string(),
                    varint_len(g.locator_hashes.len() as u64),
                    format!("{} locators", g.locator_hashes.len()),
                ),
            ];
            for i in 0..g.locator_hashes.len().min(8) {
                out.push(f(&format!("locator[{i}]"), 32));
            }
            out.push(f("stop_hash", 32));
            Some(out)
        }
        M::Headers(hs) => {
            let mut out = vec![(
                "count".to_string(),
                varint_len(hs.len() as u64),
                format!("{} headers", hs.len()),
            )];
            for i in 0..hs.len().min(8) {
                // Each header is 80 bytes + a CompactSize(0) tx count = 81 bytes.
                out.push(f(&format!("header[{i}]"), 81));
            }
            Some(out)
        }
        _ => None,
    }
}

/// The CompactSize encoding length for a count.
fn varint_len(n: u64) -> usize {
    if n < 0xfd {
        1
    } else if n <= 0xffff {
        3
    } else if n <= 0xffff_ffff {
        5
    } else {
        9
    }
}

/// Render one span: offset, up to 12 hex bytes (or an elision), name, note.
fn render_span(raw: &[u8], span: &Span) -> String {
    let end = (span.start + span.len).min(raw.len());
    let bytes = &raw[span.start.min(raw.len())..end];
    let hex = if bytes.len() > 12 {
        format!("‥{} bytes‥", span.len)
    } else {
        bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    format!(
        "{:04x}  {:<38}  {:<14} {}",
        span.start, hex, span.name, span.note
    )
}
