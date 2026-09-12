//! REPL command grammar and parsing (PLAN.md §8).
//!
//! The typed `send` field forms live in [`crate::messages::dsl`]; this module
//! handles the command grammar, the generic `hex:` and `send raw` paths, and the
//! non-send commands (connect/status/show/auto/preset/…).

use bitcoin::hex::FromHex;
use bitcoin::p2p::message::{CommandString, NetworkMessage};

use crate::cli::Transport as TransportPref;
use crate::messages::dsl;
use crate::messages::samples::SampleData;
use crate::session::automations::AutoKind;
use crate::session::events::Command;

/// What the REPL should do with a parsed line.
pub enum Action {
    Nothing,
    ToSession(Command),
    Status,
    Show { target: ShowTarget, hex: bool },
    AutoList,
    PresetList,
    Preset(String),
    Help(Option<String>),
    Quit,
    Usage(String),
}

/// Which message `show` should display.
pub enum ShowTarget {
    Last,
    Seq(u64),
}

/// Parse one REPL line into an [`Action`]. `samples` backs the `send … sample`
/// forms.
pub fn parse(line: &str, samples: &SampleData) -> Action {
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
        "send" => parse_send(&rest, samples),
        "auto" => parse_auto(&rest),
        "preset" => match rest.split_first() {
            None => Action::PresetList,
            Some((&"list", _)) => Action::PresetList,
            Some((name, _)) => Action::Preset(name.to_string()),
        },
        "misbehave" => parse_misbehave(&rest, samples),
        "craft" => parse_craft(&rest),
        "spam" => parse_spam(&rest, samples),
        "stop" => Action::ToSession(Command::StopSpam),
        "hold" => parse_hold(&rest, samples),
        "release" => Action::ToSession(Command::Release),
        "drop" => Action::ToSession(Command::Drop),
        "pause-reads" => Action::ToSession(Command::PauseReads(true)),
        "resume-reads" => Action::ToSession(Command::PauseReads(false)),
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

fn parse_auto(args: &[&str]) -> Action {
    use std::str::FromStr;
    match args {
        [] | ["list"] => Action::AutoList,
        ["on", name] => match AutoKind::from_str(name) {
            Ok(kind) => Action::ToSession(Command::SetAuto { kind, on: true }),
            Err(e) => Action::Usage(e),
        },
        ["off", name] => match AutoKind::from_str(name) {
            Ok(kind) => Action::ToSession(Command::SetAuto { kind, on: false }),
            Err(e) => Action::Usage(e),
        },
        _ => Action::Usage("usage: auto [list | on <name> | off <name>]".to_string()),
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

fn parse_send(args: &[&str], samples: &SampleData) -> Action {
    let Some((name, fields)) = args.split_first() else {
        return Action::Usage("usage: send <message> [field=value ...]".to_string());
    };

    // `send raw <command> <hex>`: an arbitrary command + payload.
    if *name == "raw" {
        return parse_send_raw(fields);
    }

    // `send <name> hex:<payload>`: any variant, payload as hex.
    if let Some(first) = fields.first() {
        if let Some(hex) = first.strip_prefix("hex:") {
            return match build_from_hex(name, hex) {
                Ok(msg) => Action::ToSession(Command::Send(msg)),
                Err(e) => Action::Usage(e),
            };
        }
    }

    match dsl::build_message(name, fields, samples) {
        Ok(msg) => Action::ToSession(Command::Send(msg)),
        Err(usage) => Action::Usage(usage),
    }
}

/// `send <name> hex:<payload>`: synthesize a v1 frame for `<name>` with the given
/// payload and decode it into a typed `NetworkMessage`.
fn build_from_hex(name: &str, hex: &str) -> Result<NetworkMessage, String> {
    let payload = Vec::<u8>::from_hex(hex).map_err(|_| format!("invalid hex payload {hex:?}"))?;
    match crate::net::v1::decode_payload(bitcoin::p2p::Magic::BITCOIN, name, &payload) {
        (Some(msg), _) => Ok(msg),
        (None, Some(e)) => Err(format!("cannot decode {name} payload: {e}")),
        (None, None) => Err(format!("cannot decode {name} payload")),
    }
}

/// `send raw <command> <hex>`: send an arbitrary 12-byte command with a hex
/// payload as `Unknown` (the transport frames/encrypts it normally).
fn parse_send_raw(fields: &[&str]) -> Action {
    let usage = || "usage: send raw <command> <hex>".to_string();
    let Some((cmd, rest)) = fields.split_first() else {
        return Action::Usage(usage());
    };
    let payload = match rest.first() {
        Some(h) => match Vec::<u8>::from_hex(h) {
            Ok(p) => p,
            Err(_) => return Action::Usage(format!("invalid hex payload {h:?}")),
        },
        None => Vec::new(),
    };
    match CommandString::try_from(cmd.to_string()) {
        Ok(command) => Action::ToSession(Command::Send(NetworkMessage::Unknown { command, payload })),
        Err(_) => Action::Usage(format!("invalid command {cmd:?} (max 12 ascii bytes)")),
    }
}

// --- misbehaviour parsing (PLAN.md §11) ---

use crate::misbehave::{self, MangleKind};

/// Build a message from `<name> [fields...]` tokens via the DSL.
fn build_spec(tokens: &[&str], samples: &SampleData) -> Result<NetworkMessage, String> {
    let (name, fields) = tokens
        .split_first()
        .ok_or_else(|| "expected a message".to_string())?;
    dsl::build_message(name, fields, samples)
}

fn parse_misbehave(rest: &[&str], samples: &SampleData) -> Action {
    let Some((kind, args)) = rest.split_first() else {
        return Action::Usage(
            "usage: misbehave <bad-checksum|bad-magic|wrong-length|corrupt|oversize|unknown-cmd> …"
                .to_string(),
        );
    };
    let mangle = |msg_toks: &[&str], k: MangleKind| match build_spec(msg_toks, samples) {
        Ok(msg) => Action::ToSession(Command::Mangle { msg, kind: k }),
        Err(e) => Action::Usage(e),
    };
    match *kind {
        "bad-checksum" => mangle(args, MangleKind::BadChecksum),
        "bad-magic" => mangle(args, MangleKind::BadMagic),
        "wrong-length" => {
            let Some((delta_tok, msg_toks)) = args.split_last() else {
                return Action::Usage("usage: misbehave wrong-length <msg> +N|-N".to_string());
            };
            match parse_i64(delta_tok) {
                Ok(d) => mangle(msg_toks, MangleKind::WrongLength(d)),
                Err(_) => Action::Usage("wrong-length: expected +N or -N".to_string()),
            }
        }
        "corrupt" => {
            let Some((off_tok, msg_toks)) = args.split_last() else {
                return Action::Usage("usage: misbehave corrupt <msg> <offset>".to_string());
            };
            match off_tok.parse::<usize>() {
                Ok(o) => mangle(msg_toks, MangleKind::CorruptAt(o)),
                Err(_) => Action::Usage("corrupt: expected a byte offset".to_string()),
            }
        }
        "oversize" => {
            let Some((bytes_tok, msg_toks)) = args.split_last() else {
                return Action::Usage("usage: misbehave oversize <msg> <bytes>".to_string());
            };
            match bytes_tok.parse::<usize>() {
                Ok(bytes) => match build_spec(msg_toks, samples) {
                    Ok(msg) => Action::ToSession(Command::Oversize { msg, bytes }),
                    Err(e) => Action::Usage(e),
                },
                Err(_) => Action::Usage("oversize: expected a byte count".to_string()),
            }
        }
        "unknown-cmd" => match args.split_first() {
            Some((cmd, rest)) => {
                let payload = match rest.first() {
                    Some(h) => match Vec::<u8>::from_hex(h) {
                        Ok(p) => p,
                        Err(_) => return Action::Usage(format!("invalid hex {h:?}")),
                    },
                    None => Vec::new(),
                };
                Action::ToSession(Command::Craft {
                    command: cmd.to_string(),
                    payload,
                    tag: format!("misbehave: unknown-cmd {cmd}"),
                })
            }
            None => Action::Usage("usage: misbehave unknown-cmd <cmd> [<hex>]".to_string()),
        },
        other => Action::Usage(format!("misbehave: unknown kind {other:?}")),
    }
}

fn parse_craft(rest: &[&str]) -> Action {
    let Some((kind, args)) = rest.split_first() else {
        return Action::Usage(
            "usage: craft <inv|compactsize|count-overflow|big-field|long-locator|bad-invtype> …"
                .to_string(),
        );
    };
    let craft = |command: String, payload: Vec<u8>, tag: String| {
        Action::ToSession(Command::Craft { command, payload, tag })
    };
    match *kind {
        "inv" => {
            let claim = match flag_u64(args, "--claim") {
                Some(Ok(v)) => v,
                _ => return Action::Usage("craft inv --claim <N> --actual <M> [--type tx|block|wtx]".to_string()),
            };
            let actual = match flag_u64(args, "--actual") {
                Some(Ok(v)) => v,
                _ => return Action::Usage("craft inv --claim <N> --actual <M> [--type tx|block|wtx]".to_string()),
            };
            let ty = flag(args, "--type").unwrap_or("tx");
            let Some(type_num) = misbehave::inv_type_num(ty) else {
                return Action::Usage(format!("craft inv: unknown --type {ty:?}"));
            };
            let (c, p) = misbehave::craft_inv(claim, actual, type_num);
            craft(c.to_string(), p, format!("craft: inv claim {claim}/{actual}"))
        }
        "compactsize" => {
            let value = match flag_u64(args, "--value") {
                Some(Ok(v)) => v,
                _ => return Action::Usage("craft compactsize --value <V> --width <1|3|5|9>".to_string()),
            };
            let width = match flag_u64(args, "--width") {
                Some(Ok(v)) => v as u8,
                _ => return Action::Usage("craft compactsize --value <V> --width <1|3|5|9>".to_string()),
            };
            match misbehave::craft_compactsize(value, width) {
                Ok((c, p)) => craft(c.to_string(), p, format!("craft: compactsize {value} w{width}")),
                Err(e) => Action::Usage(e),
            }
        }
        "count-overflow" => match args.first() {
            Some(msg) => {
                let (c, p) = misbehave::craft_count_overflow(msg.to_string());
                craft(c, p, format!("craft: count-overflow {msg}"))
            }
            None => Action::Usage("usage: craft count-overflow <msg>".to_string()),
        },
        "big-field" => match args {
            [msg, field, n] if *msg == "version" && *field == "user_agent" => match n.parse::<usize>() {
                Ok(len) => {
                    let (c, p) = misbehave::craft_big_version_user_agent(len);
                    craft(c.to_string(), p, format!("craft: big-field version user_agent {len}"))
                }
                Err(_) => Action::Usage("big-field: expected a byte count".to_string()),
            },
            _ => Action::Usage("usage: craft big-field version user_agent <bytes>".to_string()),
        },
        "long-locator" => {
            let hashes = match flag_u64(args, "--hashes") {
                Some(Ok(v)) => v,
                Some(Err(_)) => return Action::Usage("long-locator: bad --hashes".to_string()),
                None => 2000,
            };
            let (c, p) = misbehave::craft_long_locator(hashes);
            craft(c.to_string(), p, format!("craft: long-locator {hashes}"))
        }
        "bad-invtype" => {
            let ty = match flag_u64(args, "--type") {
                Some(Ok(v)) => v as u32,
                _ => return Action::Usage("craft bad-invtype --type <u32>".to_string()),
            };
            let (c, p) = misbehave::craft_bad_invtype(ty);
            craft(c.to_string(), p, format!("craft: bad-invtype {ty}"))
        }
        other => Action::Usage(format!("craft: unknown kind {other:?}")),
    }
}

fn parse_spam(rest: &[&str], samples: &SampleData) -> Action {
    // Split the send spec (before the first --flag) from the flags.
    let split = rest.iter().position(|t| t.starts_with("--")).unwrap_or(rest.len());
    let (spec, flags) = rest.split_at(split);
    if spec.is_empty() {
        return Action::Usage("usage: spam <message> [fields…] [--rate <n>/s] [--count <n>]".to_string());
    }
    let rate = match flag(flags, "--rate") {
        Some(r) => match r.trim_end_matches("/s").parse::<u32>() {
            Ok(v) => Some(v),
            Err(_) => return Action::Usage("spam: --rate expects <n>/s".to_string()),
        },
        None => None,
    };
    let count = match flag_u64(flags, "--count") {
        Some(Ok(v)) => Some(v),
        Some(Err(_)) => return Action::Usage("spam: bad --count".to_string()),
        None => None,
    };
    match build_spec(spec, samples) {
        Ok(msg) => Action::ToSession(Command::Spam { msg, rate, count }),
        Err(e) => Action::Usage(e),
    }
}

fn parse_hold(rest: &[&str], samples: &SampleData) -> Action {
    // hold <msg> [fields…] <n> [--drip <bytes> <interval>]
    let drip_at = rest.iter().position(|t| *t == "--drip");
    let (head, drip) = match drip_at {
        Some(i) => {
            let d = &rest[i + 1..];
            if d.len() != 2 {
                return Action::Usage("usage: hold <msg> <n> --drip <bytes> <interval>".to_string());
            }
            let bytes = match d[0].parse::<usize>() {
                Ok(b) => b,
                Err(_) => return Action::Usage("hold: --drip <bytes> must be a number".to_string()),
            };
            let interval = match humantime::parse_duration(d[1]) {
                Ok(x) => x,
                Err(_) => return Action::Usage("hold: --drip <interval> like 500ms".to_string()),
            };
            (&rest[..i], Some((bytes, interval)))
        }
        None => (rest, None),
    };
    let Some((n_tok, spec)) = head.split_last() else {
        return Action::Usage("usage: hold <msg> [fields…] <n> [--drip <bytes> <interval>]".to_string());
    };
    let keep = match n_tok.parse::<usize>() {
        Ok(k) => k,
        Err(_) => return Action::Usage("hold: <n> must be a byte count".to_string()),
    };
    match build_spec(spec, samples) {
        Ok(msg) => Action::ToSession(Command::Hold { msg, keep, drip }),
        Err(e) => Action::Usage(e),
    }
}

// flag helpers: args are separate tokens, e.g. ["--claim", "10000"].
fn flag<'a>(args: &[&'a str], name: &str) -> Option<&'a str> {
    args.iter().position(|a| *a == name).and_then(|i| args.get(i + 1).copied())
}
fn flag_u64(args: &[&str], name: &str) -> Option<Result<u64, ()>> {
    flag(args, name).map(|v| {
        if let Some(hex) = v.strip_prefix("0x") {
            u64::from_str_radix(hex, 16).map_err(|_| ())
        } else {
            v.parse().map_err(|_| ())
        }
    })
}
fn parse_i64(s: &str) -> Result<i64, ()> {
    s.trim_start_matches('+').parse::<i64>().map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::Network;

    fn samples() -> SampleData {
        SampleData::new(Network::Regtest)
    }

    fn p(line: &str) -> Action {
        parse(line, &samples())
    }

    #[test]
    fn parses_ping_with_nonce() {
        match p("send ping nonce=0x2a") {
            Action::ToSession(Command::Send(NetworkMessage::Ping(n))) => assert_eq!(n, 42),
            _ => panic!("expected ping"),
        }
    }

    #[test]
    fn parses_connect_with_transport() {
        match p("connect 127.0.0.1:18444 --v1") {
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
        match p(&format!("send inv tx={h},{h} block={h}")) {
            Action::ToSession(Command::Send(NetworkMessage::Inv(items))) => {
                assert_eq!(items.len(), 3)
            }
            _ => panic!("expected inv"),
        }
    }

    #[test]
    fn blank_and_comment_are_nothing() {
        assert!(matches!(p("   "), Action::Nothing));
        assert!(matches!(p("# hi"), Action::Nothing));
    }

    #[test]
    fn unknown_message_is_usage() {
        assert!(matches!(p("send frobnicate"), Action::Usage(_)));
    }

    #[test]
    fn hex_path_decodes_ping() {
        match p("send ping hex:2a00000000000000") {
            Action::ToSession(Command::Send(NetworkMessage::Ping(n))) => assert_eq!(n, 42),
            _ => panic!("expected ping from hex"),
        }
    }

    #[test]
    fn send_raw_builds_unknown() {
        match p("send raw foobar deadbeef") {
            Action::ToSession(Command::Send(NetworkMessage::Unknown { command, payload })) => {
                assert_eq!(command.to_string(), "foobar");
                assert_eq!(payload, vec![0xde, 0xad, 0xbe, 0xef]);
            }
            _ => panic!("expected unknown"),
        }
    }
}
