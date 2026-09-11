//! Command-line argument definitions (PLAN.md §14).
//!
//! Only flags are exposed here — the interactive surface lives in the REPL
//! (PLAN.md §8). Values that need parsing beyond clap's built-ins (service masks,
//! magic bytes, durations) are taken as strings and validated when the session
//! starts, so a bad value produces a clear startup error rather than a clap panic.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

/// Interactive Bitcoin P2P client for a single peer over v1/v2 transport.
#[derive(Debug, Parser)]
#[command(name = "cli-ent", version, about, long_about = None)]
pub struct Args {
    /// Peer to connect to: `host:port`, `[v6]:port`, or bare `host`
    /// (default port depends on `--network`). Omit to start disconnected.
    pub peer: Option<String>,

    // --- Network ---
    /// Bitcoin network to speak.
    #[arg(long, value_enum, default_value_t = Network::Mainnet)]
    pub network: Network,

    /// Custom 4-byte network magic as 8 hex chars; v1 only.
    #[arg(long, value_name = "HEX8")]
    pub magic: Option<String>,

    // --- Transport ---
    /// Transport selection: probe v2 then fall back to v1 (`auto`), or force one.
    #[arg(long, value_enum, default_value_t = Transport::Auto)]
    pub transport: Transport,

    /// Connect + handshake timeout.
    #[arg(long, value_name = "DUR", default_value = "10s")]
    pub timeout: String,

    // --- Handshake ---
    /// Handshake profile controlling what is sent automatically.
    #[arg(long, value_enum, default_value_t = Handshake::Core)]
    pub handshake: Handshake,

    /// Whether verack is sent automatically or awaits `send verack`.
    #[arg(long, value_enum, default_value_t = Verack::Auto)]
    pub verack: Verack,

    /// Delay applied only to the verack we send.
    #[arg(long, value_name = "DUR")]
    pub verack_delay: Option<String>,

    // --- Version fields (the five commonly varied; the rest via `send version`) ---
    /// User agent string advertised in `version`.
    #[arg(long, value_name = "STR")]
    pub user_agent: Option<String>,

    /// Advertised protocol version.
    #[arg(long, value_name = "U32")]
    pub protocol_version: Option<u32>,

    /// Advertised service flags: integer (`0x…`/decimal) or `|`-joined names.
    #[arg(long, value_name = "MASK|NAMES")]
    pub services: Option<String>,

    /// Advertised best block height.
    #[arg(long, value_name = "I32")]
    pub start_height: Option<i32>,

    /// Advertised relay flag.
    #[arg(long, value_name = "BOOL")]
    pub relay: Option<bool>,

    // --- Output ---
    /// Disable ANSI colour even on a TTY.
    #[arg(long)]
    pub no_color: bool,

    /// Mirror every printed line to this file (colours stripped, absolute timestamps).
    #[arg(long, value_name = "PATH")]
    pub log: Option<PathBuf>,

    // --- Script ---
    /// Run a script file of REPL commands after the handshake is ready.
    #[arg(long, value_name = "PATH")]
    pub script: Option<PathBuf>,

    /// Pause between every script line.
    #[arg(long, value_name = "DUR")]
    pub script_delay: Option<String>,

    /// Exit when the script finishes instead of dropping into the REPL.
    #[arg(long)]
    pub exit_after_script: bool,
}

/// Supported networks (PLAN.md §14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Network {
    Mainnet,
    Testnet3,
    Testnet4,
    Signet,
    Regtest,
}

impl Network {
    /// The corresponding rust-bitcoin network.
    pub fn to_bitcoin(self) -> bitcoin::Network {
        match self {
            Network::Mainnet => bitcoin::Network::Bitcoin,
            Network::Testnet3 => bitcoin::Network::Testnet,
            Network::Testnet4 => bitcoin::Network::Testnet4,
            Network::Signet => bitcoin::Network::Signet,
            Network::Regtest => bitcoin::Network::Regtest,
        }
    }

    /// The 4-byte P2P network magic.
    pub fn magic(self) -> bitcoin::p2p::Magic {
        bitcoin::p2p::Magic::from(self.to_bitcoin())
    }

    /// The conventional default P2P port for the network.
    pub fn default_port(self) -> u16 {
        match self {
            Network::Mainnet => 8333,
            Network::Testnet3 => 18333,
            Network::Testnet4 => 48333,
            Network::Signet => 38333,
            Network::Regtest => 18444,
        }
    }
}

/// Transport preference (PLAN.md §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Transport {
    /// Probe v2, fall back to v1.
    Auto,
    /// Plaintext v1 only, never probe v2.
    V1,
    /// BIP324 v2 only, never fall back.
    V2,
}

/// Handshake profile (PLAN.md §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Handshake {
    /// Mimic Bitcoin Core's negotiation.
    Core,
    /// `version` + `verack` and nothing else.
    Minimal,
    /// Send nothing automatically; you drive it.
    Manual,
}

/// Whether verack is automatic (PLAN.md §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Verack {
    Auto,
    Manual,
}
