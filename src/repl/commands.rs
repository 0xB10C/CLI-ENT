//! REPL command grammar and parsing (PLAN.md §8).
//!
//! Milestone 2 covers the interactive essentials: connect/disconnect/status/quit,
//! `show`, and `send` for the no-payload messages plus ping/pong/inv/getdata/
//! getheaders/feefilter/sendcmpct. Later milestones extend `send` (full DSL, §9),
//! and add preset/auto/misbehave/craft/spam/hold (§8).

use std::str::FromStr;

use bitcoin::p2p::message::NetworkMessage;
use bitcoin::p2p::message_blockdata::{GetBlocksMessage, GetHeadersMessage, Inventory};
use bitcoin::p2p::message_compact_blocks::SendCmpct;
use bitcoin::{BlockHash, Txid, Wtxid};

use crate::cli::Transport as TransportPref;
use crate::session::events::Command;

/// What the REPL should do with a parsed line.
pub enum Action {
    /// Nothing (blank line or comment).
    Nothing,
    /// Forward a command to the session task.
    ToSession(Command),
    /// Print peer/session status.
    Status,
    /// Show a ring-buffer entry.
    Show { target: ShowTarget, hex: bool },
    /// Print help (optionally for one topic).
    Help(Option<String>),
    /// Leave the REPL.
    Quit,
    /// A parse error: print this one-line usage.
    Usage(String),
}

/// Which message `show` should display.
pub enum ShowTarget {
    Last,
    Seq(u64),
}

/// Parse one REPL line into an [`Action`].
pub fn parse(line: &str) -> Action {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Action::Nothing;
    }
    let mut it = line.split_whitespace();
    let cmd = it.next().unwrap();
    let rest: Vec<&str> = it.collect();

    match cmd {
        "connect" => parse_connect(&rest),
        "disconnect" => Action::ToSession(Command::Disconnect),
        "status" => Action::Status,
        "show" => parse_show(&rest),
        "send" => parse_send(&rest),
        "help" => Action::Help(rest.first().map(|s| s.to_string())),
        "quit" | "exit" => Action::Quit,
        other => Action::Usage(format!("unknown command {other:?}; try `help`")),
    }
}

fn parse_connect(args: &[&str]) -> Action {
    let mut spec: Option<&str> = None;
    let mut transport: Option<TransportPref> = None;
    for a in args {
        match *a {
            "--v1" => transport = Some(TransportPref::V1),
            "--v2" => transport = Some(TransportPref::V2),
            "--auto" => transport = Some(TransportPref::Auto),
            s if s.starts_with("--") => {
                return Action::Usage(format!("connect: unknown option {s:?}"))
            }
            s => spec = Some(s),
        }
    }
    match spec {
        Some(addr) => Action::ToSession(Command::Connect {
            addr: addr.to_string(),
            transport,
        }),
        None => Action::Usage("usage: connect <host:port> [--v1|--v2|--auto]".to_string()),
    }
}

fn parse_show(args: &[&str]) -> Action {
    let mut target = ShowTarget::Last;
    let mut hex = false;
    for a in args {
        match *a {
            "hex" => hex = true,
            "last" => target = ShowTarget::Last,
            n => match n.parse::<u64>() {
                Ok(seq) => target = ShowTarget::Seq(seq),
                Err(_) => return Action::Usage("usage: show [last | <seq>] [hex]".to_string()),
            },
        }
    }
    Action::Show { target, hex }
}

fn parse_send(args: &[&str]) -> Action {
    let Some((name, fields)) = args.split_first() else {
        return Action::Usage("usage: send <message> [field=value ...]".to_string());
    };
    match build_message(name, fields) {
        Ok(msg) => Action::ToSession(Command::Send(msg)),
        Err(usage) => Action::Usage(usage),
    }
}

/// Build a `NetworkMessage` from a message name and field tokens.
fn build_message(name: &str, fields: &[&str]) -> Result<NetworkMessage, String> {
    use NetworkMessage as M;
    match name {
        // No-payload messages.
        "verack" => Ok(M::Verack),
        "getaddr" => Ok(M::GetAddr),
        "mempool" => Ok(M::MemPool),
        "sendheaders" => Ok(M::SendHeaders),
        "wtxidrelay" => Ok(M::WtxidRelay),
        "sendaddrv2" => Ok(M::SendAddrV2),
        "filterclear" => Ok(M::FilterClear),

        "ping" => {
            let nonce = match field(fields, "nonce") {
                Some(v) => parse_u64(v).map_err(|_| usage_ping())?,
                None => rand::random(),
            };
            Ok(M::Ping(nonce))
        }
        "pong" => {
            let nonce = field(fields, "nonce")
                .ok_or_else(|| "usage: send pong nonce=<u64>".to_string())
                .and_then(|v| parse_u64(v).map_err(|_| "usage: send pong nonce=<u64>".to_string()))?;
            Ok(M::Pong(nonce))
        }
        "feefilter" => {
            let v = fields
                .first()
                .ok_or_else(|| "usage: send feefilter <sat/kvB>".to_string())?;
            let f: i64 = v
                .parse()
                .map_err(|_| "usage: send feefilter <sat/kvB>".to_string())?;
            Ok(M::FeeFilter(f))
        }
        "sendcmpct" => {
            let hb = field(fields, "hb")
                .ok_or_else(usage_sendcmpct)
                .and_then(|v| parse_bool(v).map_err(|_| usage_sendcmpct()))?;
            let version = field(fields, "version")
                .ok_or_else(usage_sendcmpct)
                .and_then(|v| parse_u64(v).map_err(|_| usage_sendcmpct()))?;
            Ok(M::SendCmpct(SendCmpct {
                send_compact: hb,
                version,
            }))
        }
        "inv" => Ok(M::Inv(parse_inv(fields)?)),
        "getdata" => Ok(M::GetData(parse_inv(fields)?)),
        "notfound" => Ok(M::NotFound(parse_inv(fields)?)),
        "getheaders" => {
            let (locator_hashes, stop_hash) = parse_locator(fields)?;
            Ok(M::GetHeaders(GetHeadersMessage {
                version: 70016,
                locator_hashes,
                stop_hash,
            }))
        }
        "getblocks" => {
            let (locator_hashes, stop_hash) = parse_locator(fields)?;
            Ok(M::GetBlocks(GetBlocksMessage {
                version: 70016,
                locator_hashes,
                stop_hash,
            }))
        }

        other => Err(format!(
            "send: message {other:?} not supported yet (the full DSL and hex: path arrive in later milestones)"
        )),
    }
}

/// Parse `tx=<txid>,… block=<hash>,… wtx=<wtxid>,… cmpct=<hash>,…` into inventory.
fn parse_inv(fields: &[&str]) -> Result<Vec<Inventory>, String> {
    let mut out = Vec::new();
    for f in fields {
        let (key, list) = f
            .split_once('=')
            .ok_or_else(usage_inv)?;
        for h in list.split(',').filter(|s| !s.is_empty()) {
            let inv = match key {
                "tx" => Inventory::Transaction(Txid::from_str(h).map_err(|_| bad_hash(h))?),
                "block" => Inventory::Block(BlockHash::from_str(h).map_err(|_| bad_hash(h))?),
                "wtx" => Inventory::WTx(Wtxid::from_str(h).map_err(|_| bad_hash(h))?),
                "cmpct" => Inventory::CompactBlock(BlockHash::from_str(h).map_err(|_| bad_hash(h))?),
                _ => return Err(usage_inv()),
            };
            out.push(inv);
        }
    }
    if out.is_empty() {
        return Err(usage_inv());
    }
    Ok(out)
}

/// Parse `locator=<hash>[,…] [stop=<hash>]` into locator hashes and a stop hash
/// (shared by getheaders and getblocks).
fn parse_locator(fields: &[&str]) -> Result<(Vec<BlockHash>, BlockHash), String> {
    let usage =
        || "usage: send getheaders|getblocks locator=<hash>[,…] [stop=<hash>]".to_string();
    let loc = field(fields, "locator").ok_or_else(usage)?;
    let mut locator_hashes = Vec::new();
    for h in loc.split(',').filter(|s| !s.is_empty()) {
        locator_hashes.push(BlockHash::from_str(h).map_err(|_| bad_hash(h))?);
    }
    if locator_hashes.is_empty() {
        return Err(usage());
    }
    let stop_hash = match field(fields, "stop") {
        Some(h) => BlockHash::from_str(h).map_err(|_| bad_hash(h))?,
        None => zero_block_hash(),
    };
    Ok((locator_hashes, stop_hash))
}

fn zero_block_hash() -> BlockHash {
    use bitcoin::hashes::Hash;
    BlockHash::all_zeros()
}

// --- small helpers ---

fn field<'a>(fields: &[&'a str], key: &str) -> Option<&'a str> {
    fields.iter().find_map(|f| {
        f.split_once('=')
            .filter(|(k, _)| *k == key)
            .map(|(_, v)| v)
    })
}

fn parse_u64(s: &str) -> Result<u64, ()> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).map_err(|_| ())
    } else {
        s.parse().map_err(|_| ())
    }
}

fn parse_bool(s: &str) -> Result<bool, ()> {
    match s.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => Err(()),
    }
}

fn bad_hash(h: &str) -> String {
    format!("invalid hash {h:?} (expect 64 hex chars)")
}

fn usage_ping() -> String {
    "usage: send ping [nonce=<u64>]".to_string()
}
fn usage_sendcmpct() -> String {
    "usage: send sendcmpct hb=<bool> version=<u64>".to_string()
}
fn usage_inv() -> String {
    "usage: send inv tx=<txid>,… block=<hash>,… wtx=<wtxid>,… cmpct=<hash>,…".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ping_with_nonce() {
        match parse("send ping nonce=0x2a") {
            Action::ToSession(Command::Send(NetworkMessage::Ping(n))) => assert_eq!(n, 42),
            _ => panic!("expected ping"),
        }
    }

    #[test]
    fn parses_connect_with_transport() {
        match parse("connect 127.0.0.1:18444 --v1") {
            Action::ToSession(Command::Connect { addr, transport }) => {
                assert_eq!(addr, "127.0.0.1:18444");
                assert!(matches!(transport, Some(TransportPref::V1)));
            }
            _ => panic!("expected connect"),
        }
    }

    #[test]
    fn parses_inv_multiple_types() {
        let h = "00".repeat(32);
        let line = format!("send inv tx={h},{h} block={h}");
        match parse(&line) {
            Action::ToSession(Command::Send(NetworkMessage::Inv(items))) => {
                assert_eq!(items.len(), 3);
            }
            _ => panic!("expected inv"),
        }
    }

    #[test]
    fn blank_and_comment_are_nothing() {
        assert!(matches!(parse("   "), Action::Nothing));
        assert!(matches!(parse("# hi"), Action::Nothing));
    }

    #[test]
    fn unknown_message_is_usage() {
        assert!(matches!(parse("send frobnicate"), Action::Usage(_)));
    }
}
