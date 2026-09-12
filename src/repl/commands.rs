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
