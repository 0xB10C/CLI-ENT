//! Tab completion (PLAN.md §8).
//!
//! Completes the first word against command names, and the word after `send`
//! against message names. Preset and automation names join in later milestones.

use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper};

/// Top-level REPL commands (PLAN.md §8).
pub const COMMANDS: &[&str] = &[
    "connect",
    "disconnect",
    "status",
    "send",
    "show",
    "auto",
    "preset",
    "misbehave",
    "craft",
    "spam",
    "stop",
    "hold",
    "release",
    "drop",
    "pause-reads",
    "resume-reads",
    "help",
    "quit",
    "exit",
];

/// Preset names (PLAN.md §10), for completion after `preset`.
pub const PRESETS: &[&str] = &[
    "inv-getdata-tx",
    "inv-getdata-block",
    "cmpctblock-block1",
    "getblocktxn-block1",
    "blocktxn-block1",
    "ping",
    "getaddr",
    "mempool",
    "sendheaders",
    "feefilter",
    "sendcmpct",
    "getheaders-genesis",
    "getblocks-genesis",
    "getcfheaders-genesis",
    "getcfilters-genesis",
];

/// Automation names, for completion after `auto on`/`auto off`.
pub const AUTOMATIONS: &[&str] = &["pong", "headers-empty", "serve", "getdata"];

/// Message names understood by `send` in milestone 2.
pub const MESSAGES: &[&str] = &[
    "verack",
    "getaddr",
    "mempool",
    "sendheaders",
    "wtxidrelay",
    "sendaddrv2",
    "filterclear",
    "ping",
    "pong",
    "feefilter",
    "sendcmpct",
    "inv",
    "getdata",
    "notfound",
    "getheaders",
    "getblocks",
    "getcfilters",
    "getcfheaders",
    "getblocktxn",
    "version",
    "tx",
    "block",
    "headers",
    "cmpctblock",
    "blocktxn",
    "raw",
];

/// The rustyline helper carrying our completer.
pub struct ReplHelper;

impl Completer for ReplHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        // Start of the word under the cursor.
        let start = line[..pos]
            .rfind(char::is_whitespace)
            .map(|i| i + 1)
            .unwrap_or(0);
        let word = &line[start..pos];
        let before = line[..start].trim();

        let candidates: &[&str] = if before.is_empty() {
            COMMANDS
        } else if before == "send" {
            MESSAGES
        } else if before == "preset" {
            PRESETS
        } else if before == "auto on" || before == "auto off" {
            AUTOMATIONS
        } else {
            &[]
        };

        let pairs = candidates
            .iter()
            .filter(|c| c.starts_with(word))
            .map(|c| Pair {
                display: c.to_string(),
                replacement: c.to_string(),
            })
            .collect();
        Ok((start, pairs))
    }
}

impl Hinter for ReplHelper {
    type Hint = String;
}
impl Highlighter for ReplHelper {}
impl Validator for ReplHelper {}
impl Helper for ReplHelper {}
