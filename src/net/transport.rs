//! The transport abstraction (PLAN.md §5).
//!
//! The socket is split into a read half and a write half so the session's
//! `select!` loop can await an incoming message and service a command (which
//! writes) at the same time — two disjoint mutable borrows. Each half is an enum
//! so the v2 (BIP324) variant slots in during milestone 3 without touching the
//! session loop. Milestone 1 implements the v1 variant only.

use std::fmt;

use bitcoin::p2p::message::NetworkMessage;

use super::v1::{V1Reader, V1Writer};

/// Which transport a connection is using.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    V1,
    V2,
}

impl fmt::Display for TransportKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportKind::V1 => f.write_str("v1"),
            TransportKind::V2 => f.write_str("v2"),
        }
    }
}

/// Transport-specific framing metadata, kept alongside the raw bytes so `show`
/// and `status` can report exactly what was on the wire.
#[derive(Debug, Clone)]
pub enum FrameKind {
    V1 {
        magic: [u8; 4],
        command: String,
        len: u32,
        /// Whether the checksum the peer sent matched the payload.
        checksum_ok: bool,
    },
    // V2 { cipher_len, decoy, short_id } — added in milestone 3.
}

/// One message as it appeared on the wire, in either direction.
///
/// `raw` is the exact bytes (v1: the full frame, including a bad checksum if the
/// peer sent one). `msg` is the parsed message, or `None` if the payload could
/// not be decoded (in which case `decode_error` carries the reason and the
/// session emits a `DecodeWarning`).
#[derive(Debug, Clone)]
pub struct Wire {
    pub msg: Option<NetworkMessage>,
    pub raw: Vec<u8>,
    pub frame: FrameKind,
    pub decode_error: Option<String>,
}

impl Wire {
    /// The command name for this message: the parsed message's command, or the
    /// raw frame's command string when the payload did not decode.
    pub fn command(&self) -> String {
        match &self.msg {
            Some(m) => m.cmd().to_string(),
            None => match &self.frame {
                FrameKind::V1 { command, .. } => command.clone(),
            },
        }
    }
}

/// A frame-level read failure: the byte stream is no longer interpretable, so the
/// session disconnects. (Payload-decode failures are *not* frame errors — those
/// keep the session alive with `Wire.msg == None`.)
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("peer closed the connection")]
    Eof,
    #[error("connection reset by peer")]
    Reset,
    #[error("io error: {0}")]
    Io(std::io::Error),
    #[error("bad magic: got {got}, expected {expected}")]
    BadMagic { got: String, expected: String },
    #[error("frame length {len} exceeds cap {cap}")]
    TooLarge { len: u32, cap: u32 },
}

impl From<std::io::Error> for FrameError {
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind::*;
        match e.kind() {
            UnexpectedEof => FrameError::Eof,
            ConnectionReset | BrokenPipe => FrameError::Reset,
            _ => FrameError::Io(e),
        }
    }
}

/// The read half of a transport.
pub enum Reader {
    V1(V1Reader),
}

impl Reader {
    /// Read and parse the next message. Frame-level failures return `FrameError`
    /// and end the session; a payload that fails to decode returns `Ok` with
    /// `Wire.msg == None`.
    pub async fn read_message(&mut self) -> Result<Wire, FrameError> {
        match self {
            Reader::V1(r) => r.read_message().await,
        }
    }
}

/// The write half of a transport, plus the encoder (encoding for v2 advances the
/// cipher, which lives on the write side).
pub enum Writer {
    V1(V1Writer),
}

impl Writer {
    pub fn kind(&self) -> TransportKind {
        match self {
            Writer::V1(_) => TransportKind::V1,
        }
    }

    /// Encode a message to the bytes to write, plus a `Wire` describing them for
    /// the ring buffer.
    pub fn encode(&mut self, msg: &NetworkMessage) -> (Vec<u8>, Wire) {
        match self {
            Writer::V1(w) => w.encode(msg),
        }
    }

    pub async fn write_bytes(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            Writer::V1(w) => w.write_bytes(bytes).await,
        }
    }

    pub async fn close(&mut self) -> std::io::Result<()> {
        match self {
            Writer::V1(w) => w.close().await,
        }
    }
}
