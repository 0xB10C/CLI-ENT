//! v2 (BIP324) transport (PLAN.md §5).
//!
//! Drives bip324's sans-IO `Handshake` over the raw `TcpStream`, keeps the cipher
//! pair, and hand-encodes the v2 message form (the short-ID / long-command scheme),
//! since released `bitcoin` 0.32 has no `V2NetworkMessage`. Plaintext is captured
//! for the ring buffer; received decoys are surfaced and ignored.

use std::time::Duration;

use bip324::{
    CipherSession, GarbageResult, Handshake, InboundCipher, Initialized, OutboundCipher, PacketType,
    ReceivedKey, Role, VersionResult, NUM_LENGTH_BYTES,
};
use bitcoin::p2p::message::NetworkMessage;
use bitcoin::p2p::Magic;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::time::timeout;

use super::transport::{FrameError, FrameKind, Wire};

/// The 64-byte ElligatorSwift public key length.
const KEY_LEN: usize = 64;

/// BIP324 short message-ID table. Index + 1 is the ID on the wire; messages not
/// listed here use the long form (a 0x00 byte followed by the 12-byte command).
const SHORT_IDS: &[&str] = &[
    "addr",         // 1
    "block",        // 2
    "blocktxn",     // 3
    "cmpctblock",   // 4
    "feefilter",    // 5
    "filteradd",    // 6
    "filterclear",  // 7
    "filterload",   // 8
    "getblocks",    // 9
    "getblocktxn",  // 10
    "getdata",      // 11
    "getheaders",   // 12
    "headers",      // 13
    "inv",          // 14
    "mempool",      // 15
    "merkleblock",  // 16
    "notfound",     // 17
    "ping",         // 18
    "pong",         // 19
    "sendcmpct",    // 20
    "tx",           // 21
    "getcfilters",  // 22
    "cfilter",      // 23
    "getcfheaders", // 24
    "cfheaders",    // 25
    "getcfcheckpt", // 26
    "cfcheckpt",    // 27
    "addrv2",       // 28
];

fn short_id(command: &str) -> Option<u8> {
    SHORT_IDS
        .iter()
        .position(|c| *c == command)
        .map(|i| (i + 1) as u8)
}

fn command_from_short_id(id: u8) -> Option<&'static str> {
    SHORT_IDS.get(id as usize - 1).copied()
}

/// Encode a message's `(command, payload)` into the v2 content (message-id form).
fn encode_content(command: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 13);
    match short_id(command) {
        Some(id) => out.push(id),
        None => {
            out.push(0);
            let mut cmd = [0u8; 12];
            let b = command.as_bytes();
            let n = b.len().min(12);
            cmd[..n].copy_from_slice(&b[..n]);
            out.extend_from_slice(&cmd);
        }
    }
    out.extend_from_slice(payload);
    out
}

/// Decode v2 content (after the bip324 header byte) into `(command, payload)`.
fn decode_content(content: &[u8]) -> Option<(String, Vec<u8>)> {
    let first = *content.first()?;
    if first == 0 {
        if content.len() < 13 {
            return None;
        }
        let end = content[1..13].iter().position(|&b| b == 0).unwrap_or(12);
        let command = String::from_utf8_lossy(&content[1..1 + end]).into_owned();
        Some((command, content[13..].to_vec()))
    } else {
        let command = command_from_short_id(first)?;
        Some((command.to_string(), content[1..].to_vec()))
    }
}

/// The read half of a v2 connection.
pub struct V2Reader {
    inner: OwnedReadHalf,
    cipher: InboundCipher,
    magic: Magic,
    /// Buffered ciphertext bytes not yet consumed (leftover from the handshake,
    /// plus whatever we read ahead).
    buf: Vec<u8>,
}

/// The write half of a v2 connection.
pub struct V2Writer {
    inner: OwnedWriteHalf,
    cipher: OutboundCipher,
    magic: Magic,
}

impl V2Reader {
    pub fn new(inner: OwnedReadHalf, cipher: InboundCipher, magic: Magic, leftover: Vec<u8>) -> Self {
        Self {
            inner,
            cipher,
            magic,
            buf: leftover,
        }
    }

    /// Read one v2 packet, decrypt it, and parse it. Decoys are returned with
    /// `msg = None` and `FrameKind::V2 { decoy: true, .. }`.
    pub async fn read_message(&mut self) -> Result<Wire, FrameError> {
        self.fill_to(NUM_LENGTH_BYTES).await?;
        let len_bytes = [self.buf[0], self.buf[1], self.buf[2]];
        let packet_len = self.cipher.decrypt_packet_len(len_bytes);

        let total = NUM_LENGTH_BYTES + packet_len;
        self.fill_to(total).await?;

        let ciphertext = self.buf[NUM_LENGTH_BYTES..total].to_vec();
        self.buf.drain(..total);

        let (packet_type, plaintext) = self
            .cipher
            .decrypt_to_vec(&ciphertext, None)
            .map_err(|e| FrameError::Io(std::io::Error::other(format!("v2 decrypt: {e:?}"))))?;

        // plaintext[0] is the bip324 header byte; the message content follows.
        let content = &plaintext[1..];

        if packet_type == PacketType::Decoy {
            return Ok(Wire {
                msg: None,
                raw: content.to_vec(),
                frame: FrameKind::V2 {
                    cipher_len: total,
                    decoy: true,
                    short_id: None,
                },
                decode_error: None,
            });
        }

        let short = content.first().copied().filter(|&b| b != 0);
        let (msg, decode_error) = match decode_content(content) {
            Some((command, payload)) => super::v1::decode_payload(self.magic, &command, &payload),
            None => (None, Some("undecodable v2 message id".to_string())),
        };

        Ok(Wire {
            msg,
            raw: content.to_vec(),
            frame: FrameKind::V2 {
                cipher_len: total,
                decoy: false,
                short_id: short,
            },
            decode_error,
        })
    }

    /// Ensure `buf` holds at least `n` bytes, reading from the socket as needed.
    async fn fill_to(&mut self, n: usize) -> Result<(), FrameError> {
        let mut tmp = [0u8; 8192];
        while self.buf.len() < n {
            let read = self.inner.read(&mut tmp).await?;
            if read == 0 {
                return Err(FrameError::Eof);
            }
            self.buf.extend_from_slice(&tmp[..read]);
        }
        Ok(())
    }
}

impl V2Writer {
    pub fn new(inner: OwnedWriteHalf, cipher: OutboundCipher, magic: Magic) -> Self {
        Self {
            inner,
            cipher,
            magic,
        }
    }

    /// Encode a message into an encrypted v2 packet. Returns the wire bytes plus a
    /// `Wire` whose `raw` is the plaintext content (captured before encryption).
    /// Advances the outbound cipher, so only call when the bytes will be sent.
    pub fn encode(&mut self, msg: &NetworkMessage) -> (Vec<u8>, Wire) {
        // Read the true command from the serialized frame (correct for Unknown).
        let (command, payload) = super::v1::command_and_payload(self.magic, msg);
        let content = encode_content(&command, &payload);
        let wire_bytes = self
            .cipher
            .encrypt_to_vec(&content, PacketType::Genuine, None);
        let wire = Wire {
            msg: Some(msg.clone()),
            frame: FrameKind::V2 {
                cipher_len: wire_bytes.len(),
                decoy: false,
                short_id: short_id(&command),
            },
            raw: content,
            decode_error: None,
        };
        (wire_bytes, wire)
    }

    pub async fn write_bytes(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.inner.write_all(bytes).await?;
        self.inner.flush().await
    }

    pub async fn close(&mut self) -> std::io::Result<()> {
        self.inner.shutdown().await
    }
}

/// Result of a v2 handshake attempt.
pub struct V2Connection {
    pub reader: V2Reader,
    pub writer: V2Writer,
    pub session_id: [u8; 32],
}

/// A v2 handshake failure, tagged by whether `--transport auto` should fall back
/// to v1 (PLAN.md §5: only before the peer's key arrives, or on a v1 peer).
#[derive(Debug)]
pub enum HandshakeErr {
    Fallback(String),
    Fatal(String),
}

impl HandshakeErr {
    pub fn is_fallback(&self) -> bool {
        matches!(self, HandshakeErr::Fallback(_))
    }
    pub fn message(&self) -> String {
        match self {
            HandshakeErr::Fallback(s) | HandshakeErr::Fatal(s) => s.clone(),
        }
    }
}

/// Perform the v2 client handshake over `stream`. `key_timeout` bounds the wait
/// for the peer's key (the probe window); `overall` bounds the rest.
pub async fn client_handshake(
    mut stream: TcpStream,
    magic: Magic,
    key_timeout: Duration,
    overall: Duration,
) -> Result<V2Connection, HandshakeErr> {
    let magic_bytes = magic.to_bytes();

    // 1) Send our key (no garbage).
    let hs = Handshake::<Initialized>::new(&magic_bytes, Role::Initiator)
        .map_err(|e| HandshakeErr::Fatal(format!("v2 init: {e:?}")))?;
    let mut key_buf = vec![0u8; Handshake::<Initialized>::send_key_len(None)];
    let hs = hs
        .send_key(None, &mut key_buf)
        .map_err(|e| HandshakeErr::Fatal(format!("v2 send_key: {e:?}")))?;
    stream
        .write_all(&key_buf)
        .await
        .map_err(|e| HandshakeErr::Fallback(format!("v2 send_key write: {e}")))?;
    stream
        .flush()
        .await
        .map_err(|e| HandshakeErr::Fallback(format!("v2 flush: {e}")))?;

    // 2) Await the peer's key within the probe window (fallback-eligible).
    let mut their_key = [0u8; KEY_LEN];
    match timeout(key_timeout, stream.read_exact(&mut their_key)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            return Err(HandshakeErr::Fallback(format!(
                "peer closed during v2 probe: {e}"
            )))
        }
        Err(_) => {
            return Err(HandshakeErr::Fallback(
                "no v2 key from peer within probe window".to_string(),
            ))
        }
    }

    // 3) Derive session keys. A v1 peer is detected here (magic match).
    let hs = hs.receive_key(their_key).map_err(|e| match e {
        bip324::Error::V1Protocol => {
            HandshakeErr::Fallback("peer is speaking v1".to_string())
        }
        other => HandshakeErr::Fatal(format!("v2 receive_key: {other:?}")),
    })?;

    // From here failures are fatal, under the overall timeout.
    match timeout(overall, finish_handshake(stream, hs, magic)).await {
        Ok(res) => res,
        Err(_) => Err(HandshakeErr::Fatal("v2 handshake timed out".to_string())),
    }
}

/// Send our version, then read the peer's garbage + version to complete the session.
async fn finish_handshake(
    mut stream: TcpStream,
    hs: Handshake<ReceivedKey<'_>>,
    magic: Magic,
) -> Result<V2Connection, HandshakeErr> {
    // 4) Send garbage terminator + version packet (no decoys).
    let mut ver_buf = vec![0u8; Handshake::<ReceivedKey>::send_version_len(None)];
    let hs = hs
        .send_version(&mut ver_buf, None)
        .map_err(|e| HandshakeErr::Fatal(format!("v2 send_version: {e:?}")))?;
    stream
        .write_all(&ver_buf)
        .await
        .map_err(|e| HandshakeErr::Fatal(format!("v2 version write: {e}")))?;
    stream
        .flush()
        .await
        .map_err(|e| HandshakeErr::Fatal(format!("v2 flush: {e}")))?;

    // 5) Read until the peer's garbage terminator is found.
    let mut buf: Vec<u8> = Vec::new();
    let mut sv = hs;
    let (mut rg, consumed) = loop {
        match sv.receive_garbage(&buf) {
            Ok(GarbageResult::FoundGarbage {
                handshake,
                consumed_bytes,
            }) => break (handshake, consumed_bytes),
            Ok(GarbageResult::NeedMoreData(h)) => {
                sv = h;
                read_more(&mut stream, &mut buf).await?;
            }
            Err(e) => return Err(HandshakeErr::Fatal(format!("v2 garbage: {e:?}"))),
        }
    };

    // 6) Process packets (decoys then the version) from the leftover bytes.
    let mut rest: Vec<u8> = buf[consumed..].to_vec();
    let cipher: CipherSession = loop {
        while rest.len() < NUM_LENGTH_BYTES {
            read_more(&mut stream, &mut rest).await?;
        }
        let plen = rg
            .decrypt_packet_len([rest[0], rest[1], rest[2]])
            .map_err(|e| HandshakeErr::Fatal(format!("v2 packet len: {e:?}")))?;
        let total = NUM_LENGTH_BYTES + plen;
        while rest.len() < total {
            read_more(&mut stream, &mut rest).await?;
        }
        let mut packet = rest[NUM_LENGTH_BYTES..total].to_vec();
        rest.drain(..total);
        match rg
            .receive_version(&mut packet)
            .map_err(|e| HandshakeErr::Fatal(format!("v2 version: {e:?}")))?
        {
            VersionResult::Complete { cipher } => break cipher,
            VersionResult::Decoy(h) => rg = h,
        }
    };

    let session_id = *cipher.id();
    let (inbound, outbound) = cipher.into_split();
    let (rh, wh) = stream.into_split();
    Ok(V2Connection {
        reader: V2Reader::new(rh, inbound, magic, rest),
        writer: V2Writer::new(wh, outbound, magic),
        session_id,
    })
}

async fn read_more(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Result<(), HandshakeErr> {
    let mut tmp = [0u8; 8192];
    let n = stream
        .read(&mut tmp)
        .await
        .map_err(|e| HandshakeErr::Fatal(format!("v2 read: {e}")))?;
    if n == 0 {
        return Err(HandshakeErr::Fatal(
            "peer closed during v2 handshake".to_string(),
        ));
    }
    buf.extend_from_slice(&tmp[..n]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_id_roundtrip() {
        assert_eq!(short_id("ping"), Some(18));
        assert_eq!(short_id("addr"), Some(1));
        assert_eq!(short_id("addrv2"), Some(28));
        assert_eq!(short_id("version"), None);
        assert_eq!(command_from_short_id(18), Some("ping"));
        assert_eq!(command_from_short_id(1), Some("addr"));
    }

    #[test]
    fn content_roundtrip_short() {
        let c = encode_content("ping", &[1, 2, 3, 4]);
        assert_eq!(c[0], 18);
        let (cmd, payload) = decode_content(&c).unwrap();
        assert_eq!(cmd, "ping");
        assert_eq!(payload, vec![1, 2, 3, 4]);
    }

    #[test]
    fn content_roundtrip_long() {
        let c = encode_content("version", &[9, 9]);
        assert_eq!(c[0], 0);
        let (cmd, payload) = decode_content(&c).unwrap();
        assert_eq!(cmd, "version");
        assert_eq!(payload, vec![9, 9]);
    }
}
