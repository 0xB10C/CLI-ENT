//! v1 (plaintext) transport (PLAN.md §5).
//!
//! Encoding uses `RawNetworkMessage` + `consensus::serialize`. Decoding is done by
//! hand: read the 24-byte header, validate magic and length ourselves, read the
//! payload, and record whether the peer's checksum matched. To obtain the parsed
//! message even when the checksum is wrong, we patch the correct checksum into a
//! copy of the frame and hand that to `RawNetworkMessage::consensus_decode`. The
//! `raw` bytes we keep for display are the *original* frame, bad checksum and all.

use bitcoin::consensus;
use bitcoin::hashes::{sha256d, Hash};
use bitcoin::p2p::message::{NetworkMessage, RawNetworkMessage};
use bitcoin::p2p::Magic;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use super::transport::{FrameError, FrameKind, Wire};

/// The v1 message header is a fixed 24 bytes: magic(4) command(12) length(4) checksum(4).
const HEADER_LEN: usize = 24;

/// Hard cap on a single frame's payload length (PLAN.md §17: fixed at 32 MiB).
pub const MAX_FRAME_LEN: u32 = 32 * 1024 * 1024;

/// The P2P checksum: the first four bytes of SHA256d(payload).
pub fn checksum(payload: &[u8]) -> [u8; 4] {
    let d = sha256d::Hash::hash(payload).to_byte_array();
    [d[0], d[1], d[2], d[3]]
}

/// Trim the trailing NUL padding from a 12-byte command field.
fn parse_command(field: &[u8]) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// The read half of a v1 connection.
pub struct V1Reader {
    inner: OwnedReadHalf,
    magic: Magic,
}

/// The write half of a v1 connection.
pub struct V1Writer {
    inner: OwnedWriteHalf,
    magic: Magic,
}

impl V1Reader {
    pub fn new(inner: OwnedReadHalf, magic: Magic) -> Self {
        Self { inner, magic }
    }

    pub async fn read_message(&mut self) -> Result<Wire, FrameError> {
        let mut header = [0u8; HEADER_LEN];
        self.inner.read_exact(&mut header).await?;

        let magic_bytes = [header[0], header[1], header[2], header[3]];
        let got_magic = Magic::from_bytes(magic_bytes);
        if got_magic != self.magic {
            return Err(FrameError::BadMagic {
                got: hex4(&magic_bytes),
                expected: hex4(&self.magic.to_bytes()),
            });
        }

        let command = parse_command(&header[4..16]);
        let len = u32::from_le_bytes([header[16], header[17], header[18], header[19]]);
        let recv_checksum = [header[20], header[21], header[22], header[23]];

        if len > MAX_FRAME_LEN {
            return Err(FrameError::TooLarge {
                len,
                cap: MAX_FRAME_LEN,
            });
        }

        let mut payload = vec![0u8; len as usize];
        self.inner.read_exact(&mut payload).await?;

        let computed = checksum(&payload);
        let checksum_ok = computed == recv_checksum;

        // Keep the original bytes (with whatever checksum the peer sent) for display.
        let mut raw = Vec::with_capacity(HEADER_LEN + payload.len());
        raw.extend_from_slice(&header);
        raw.extend_from_slice(&payload);

        // Decode from a copy with the checksum corrected, so a bad-checksum frame
        // still yields the parsed message. Unknown commands decode into
        // NetworkMessage::Unknown; a genuinely malformed payload yields an error
        // that becomes a DecodeWarning (msg = None), and the session continues.
        let mut canonical = raw.clone();
        canonical[20..24].copy_from_slice(&computed);
        let (msg, decode_error) = match consensus::deserialize::<RawNetworkMessage>(&canonical) {
            Ok(raw_msg) => (Some(raw_msg.into_payload()), None),
            Err(e) => (None, Some(e.to_string())),
        };

        Ok(Wire {
            msg,
            raw,
            frame: FrameKind::V1 {
                magic: magic_bytes,
                command,
                len,
                checksum_ok,
            },
            decode_error,
        })
    }
}

impl V1Writer {
    pub fn new(inner: OwnedWriteHalf, magic: Magic) -> Self {
        Self { inner, magic }
    }

    /// Encode a message into a full v1 frame. Returns the bytes to write plus a
    /// `Wire` describing them for the ring buffer (for v1 the wire bytes and the
    /// display bytes are identical, since the frame is already plaintext).
    pub fn encode(&self, msg: &NetworkMessage) -> (Vec<u8>, Wire) {
        let raw = RawNetworkMessage::new(self.magic, msg.clone());
        let bytes = consensus::serialize(&raw);
        let wire = Wire {
            msg: Some(msg.clone()),
            frame: frame_kind(&bytes),
            raw: bytes.clone(),
            decode_error: None,
        };
        (bytes, wire)
    }

    pub async fn write_bytes(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.inner.write_all(bytes).await?;
        self.inner.flush().await
    }

    pub async fn close(&mut self) -> std::io::Result<()> {
        self.inner.shutdown().await
    }
}

/// The consensus-serialized payload of a message (the bytes after the 24-byte v1
/// header). Used by the v2 path, which frames payloads differently.
pub fn payload_bytes(magic: Magic, msg: &NetworkMessage) -> Vec<u8> {
    let raw = RawNetworkMessage::new(magic, msg.clone());
    let bytes = consensus::serialize(&raw);
    bytes[HEADER_LEN..].to_vec()
}

/// Build a canonical v1 frame (with the correct checksum) from a command and
/// payload, then decode it into a `NetworkMessage`. Returns the parsed message or
/// the decode error. Shared by the v1 reader and the v2 short-ID decoder.
pub fn decode_payload(
    magic: Magic,
    command: &str,
    payload: &[u8],
) -> (Option<NetworkMessage>, Option<String>) {
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.extend_from_slice(&magic.to_bytes());
    let mut cmd = [0u8; 12];
    let bytes = command.as_bytes();
    let n = bytes.len().min(12);
    cmd[..n].copy_from_slice(&bytes[..n]);
    frame.extend_from_slice(&cmd);
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&checksum(payload));
    frame.extend_from_slice(payload);

    match consensus::deserialize::<RawNetworkMessage>(&frame) {
        Ok(raw_msg) => (Some(raw_msg.into_payload()), None),
        Err(e) => (None, Some(e.to_string())),
    }
}

/// Derive the frame metadata from a well-formed serialized v1 frame (used for
/// messages we send, whose checksum is always correct).
pub fn frame_kind(raw: &[u8]) -> FrameKind {
    if raw.len() < HEADER_LEN {
        return FrameKind::V1 {
            magic: [0; 4],
            command: String::new(),
            len: 0,
            checksum_ok: true,
        };
    }
    FrameKind::V1 {
        magic: [raw[0], raw[1], raw[2], raw[3]],
        command: parse_command(&raw[4..16]),
        len: u32::from_le_bytes([raw[16], raw[17], raw[18], raw[19]]),
        checksum_ok: true,
    }
}

fn hex4(b: &[u8; 4]) -> String {
    format!("{:02x}{:02x}{:02x}{:02x}", b[0], b[1], b[2], b[3])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verack_checksum_is_known_value() {
        // SHA256d("") starts 5df6e0e2 — the canonical empty-payload checksum,
        // which is what a `verack` carries on the wire. This also pins the byte
        // order of our checksum helper.
        assert_eq!(checksum(&[]), [0x5d, 0xf6, 0xe0, 0xe2]);
    }

    #[test]
    fn parse_command_trims_nul() {
        assert_eq!(parse_command(b"verack\0\0\0\0\0\0"), "verack");
        assert_eq!(parse_command(b"version\0\0\0\0\0"), "version");
    }
}
