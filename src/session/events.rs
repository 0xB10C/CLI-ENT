//! The channel vocabulary between the REPL/script and the session task (PLAN.md §3).
//!
//! Commands flow in; events flow out. Milestone 1 defines the subset the v1
//! handshake demo needs; later milestones extend both enums (hold/spam/craft,
//! automations, pause-reads, …).

use std::net::SocketAddr;
use std::time::Instant;

use bitcoin::p2p::message::NetworkMessage;

use crate::net::transport::{TransportKind, Wire};

/// A request from the REPL/script to the session task.
#[derive(Debug)]
pub enum Command {
    /// Connect to a peer (raw spec; the session resolves it). `transport` of
    /// `None` uses the session's default preference.
    Connect {
        addr: String,
        transport: Option<crate::cli::Transport>,
    },
    /// Send a fully-formed message (framed/encrypted by the transport).
    Send(NetworkMessage),
    /// Send pre-framed (v1) or pre-encrypted (v2) bytes as-is.
    SendRaw(Vec<u8>),
    /// Turn an automation on or off.
    SetAuto {
        kind: crate::session::automations::AutoKind,
        on: bool,
    },
    /// Close the connection but keep the session task alive.
    Disconnect,
    /// Close the connection and end the session task.
    Quit,
}

/// The handshake progress, tracked independently in each direction. `Ready` is
/// reached once all four flags are set (PLAN.md §6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HandshakeState {
    pub version_sent: bool,
    pub version_received: bool,
    pub verack_sent: bool,
    pub verack_received: bool,
}

impl HandshakeState {
    pub fn is_ready(&self) -> bool {
        self.version_sent && self.version_received && self.verack_sent && self.verack_received
    }
}

/// Direction of a message on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Sent,
    Recv,
}

/// Why a session ended (PLAN.md §12).
#[derive(Debug, Clone)]
pub enum DisconnectReason {
    PeerClosed,
    PeerReset,
    ConnectTimeout,
    HandshakeTimeout,
    FrameError(String),
    UserRequested,
}

impl std::fmt::Display for DisconnectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DisconnectReason::PeerClosed => f.write_str("peer closed connection (EOF)"),
            DisconnectReason::PeerReset => f.write_str("connection reset by peer"),
            DisconnectReason::ConnectTimeout => f.write_str("connect timed out"),
            DisconnectReason::HandshakeTimeout => f.write_str("handshake timed out"),
            DisconnectReason::FrameError(s) => write!(f, "frame error: {s}"),
            DisconnectReason::UserRequested => f.write_str("disconnected by user"),
        }
    }
}

/// An observation emitted by the session task for the REPL/printer.
#[derive(Debug)]
pub enum Event {
    Connecting {
        addr: SocketAddr,
        transport: TransportKind,
    },
    Connected {
        addr: SocketAddr,
        transport: TransportKind,
        v2_session_id: Option<[u8; 32]>,
    },
    /// `auto` probed v2 and fell back to v1.
    FellBackToV1 {
        reason: String,
    },
    Sent {
        seq: u64,
        wire: Wire,
        at: Instant,
        tag: Option<String>,
    },
    Recv {
        seq: u64,
        wire: Wire,
        at: Instant,
    },
    DecodeWarning {
        seq: u64,
        command: String,
        error: String,
    },
    /// The handshake reached `Ready`.
    Ready,
    Disconnected {
        reason: DisconnectReason,
        stats: crate::session::view::SessionStats,
    },
    Error(String),
}
