//! cli-ent — CLI Extended Node Talker.
//!
//! An interactive Bitcoin P2P client that connects to a single peer over the v1
//! (plaintext) or v2 (BIP324) transport, performs the version handshake, and lets
//! you send any P2P message, inspect every byte on the wire, and deliberately
//! misbehave to observe how the remote node reacts.
//!
//! The crate is split into a library and a thin binary (`src/main.rs`) so that
//! integration tests can drive a [`session`] without a terminal.
//!
//! Module map (see `PLAN.md` §4):
//! - [`cli`]       — clap argument definitions.
//! - [`net`]       — transport layer: resolve, v1/v2 framing, connect + fallback.
//! - [`session`]   — the tokio session task, handshake state machine, automations.
//! - [`messages`]  — message DSL, builders, presets, samples, summaries, annotation.
//! - [`misbehave`] — malformed frame builders, craft builders, the spam repeater.
//! - [`repl`]      — rustyline REPL, command grammar, completion, output printing.
//! - [`script`]    — script-file driver.

pub mod cli;
pub mod messages;
pub mod misbehave;
pub mod net;
pub mod repl;
pub mod script;
pub mod session;
