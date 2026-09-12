//! Misbehaviour toolkit byte builders (PLAN.md §11).
//!
//! Pure functions: they mangle already-encoded frames, or hand-build a
//! `(command, payload)` pair whose *contents* violate an invariant. The session
//! frames/encrypts and sends them (see `Writer::encode_raw`), so craft bytes ride
//! the same path on v1 and v2. Keeping these pure makes the exact byte diffs
//! unit-testable.

use bitcoin::consensus::serialize;
use bitcoin::p2p::message_network::VersionMessage;
use bitcoin::p2p::{Address, ServiceFlags};

/// A byte-level mangle applied to an *encoded* message (PLAN.md §11 tables).
#[derive(Debug, Clone, Copy)]
pub enum MangleKind {
    /// Flip one checksum byte (v1).
    BadChecksum,
    /// Zero the magic (v1).
    BadMagic,
    /// Offset the header length field by N, payload unchanged (v1).
    WrongLength(i64),
    /// Flip the byte at `offset` in the encrypted packet (v2).
    CorruptAt(usize),
}

impl MangleKind {
    /// Whether this mangle applies to the given transport (the others print
    /// "not applicable").
    pub fn applies_to(&self, is_v2: bool) -> bool {
        match self {
            MangleKind::CorruptAt(_) => is_v2,
            _ => !is_v2,
        }
    }

    pub fn label(&self) -> String {
        match self {
            MangleKind::BadChecksum => "misbehave: bad-checksum".to_string(),
            MangleKind::BadMagic => "misbehave: bad-magic".to_string(),
            MangleKind::WrongLength(d) => format!("misbehave: wrong-length {d:+}"),
            MangleKind::CorruptAt(o) => format!("misbehave: corrupt @{o}"),
        }
    }
}

/// Apply a mangle to encoded bytes (a v1 frame or a v2 packet).
pub fn mangle(bytes: &[u8], kind: MangleKind) -> Vec<u8> {
    let mut out = bytes.to_vec();
    match kind {
        MangleKind::BadChecksum => {
            if out.len() >= 21 {
                out[20] ^= 0x01; // one byte of the v1 checksum field
            }
        }
        MangleKind::BadMagic => {
            if out.len() >= 4 {
                out[0..4].copy_from_slice(&[0, 0, 0, 0]);
            }
        }
        MangleKind::WrongLength(delta) => {
            if out.len() >= 20 {
                let orig = u32::from_le_bytes([out[16], out[17], out[18], out[19]]) as i64;
                let patched = (orig + delta).clamp(0, u32::MAX as i64) as u32;
                out[16..20].copy_from_slice(&patched.to_le_bytes());
            }
        }
        MangleKind::CorruptAt(offset) => {
            if offset < out.len() {
                out[offset] ^= 0xff;
            }
        }
    }
    out
}

/// Build an oversized payload: `base` padded with zeros to `target` bytes (used
/// by `misbehave oversize`; the command frames it, so v1 and v2 both work).
pub fn oversize_payload(base: &[u8], target: usize) -> Vec<u8> {
    let mut out = base.to_vec();
    if out.len() < target {
        out.resize(target, 0);
    }
    out
}

// --- craft builders: structural lies (PLAN.md §11) ---

/// Inventory item type numbers.
pub fn inv_type_num(name: &str) -> Option<u32> {
    match name {
        "tx" => Some(1),
        "block" => Some(2),
        "wtx" => Some(5),
        "cmpct" => Some(4),
        _ => None,
    }
}

/// `craft inv --claim N --actual M --type T`: a CompactSize claiming N items with
/// only M following. Returns `(command, payload)`.
pub fn craft_inv(claim: u64, actual: u64, type_num: u32) -> (&'static str, Vec<u8>) {
    let mut payload = compact_size(claim);
    for i in 0..actual {
        payload.extend_from_slice(&type_num.to_le_bytes());
        let mut hash = [0u8; 32];
        hash[..8].copy_from_slice(&i.to_le_bytes());
        payload.extend_from_slice(&hash);
    }
    ("inv", payload)
}

/// `craft compactsize --value V --width W`: a non-minimal CompactSize as an inv
/// count (strict parsers reject; sloppy ones accept).
pub fn craft_compactsize(value: u64, width: u8) -> Result<(&'static str, Vec<u8>), String> {
    let payload = compact_size_width(value, width)?;
    Ok(("inv", payload))
}

/// `craft count-overflow <msg>`: a vector count of 2^64-1.
pub fn craft_count_overflow(command: String) -> (String, Vec<u8>) {
    let mut payload = vec![0xff];
    payload.extend_from_slice(&u64::MAX.to_le_bytes());
    (command, payload)
}

/// `craft long-locator [--hashes N]`: a getheaders with N locator hashes.
pub fn craft_long_locator(hashes: u64) -> (&'static str, Vec<u8>) {
    let mut payload = Vec::new();
    payload.extend_from_slice(&70016u32.to_le_bytes()); // version
    payload.extend_from_slice(&compact_size(hashes));
    for _ in 0..hashes {
        payload.extend_from_slice(&[0u8; 32]);
    }
    payload.extend_from_slice(&[0u8; 32]); // stop hash
    ("getheaders", payload)
}

/// `craft bad-invtype --type N`: an inv with one item of an unknown type number.
pub fn craft_bad_invtype(type_num: u32) -> (&'static str, Vec<u8>) {
    let mut payload = compact_size(1);
    payload.extend_from_slice(&type_num.to_le_bytes());
    payload.extend_from_slice(&[0u8; 32]);
    ("inv", payload)
}

/// `craft big-field version user_agent <N>`: a version whose user agent is N
/// bytes (Core caps the subversion at 256). Returns the version payload.
pub fn craft_big_version_user_agent(len: usize) -> (&'static str, Vec<u8>) {
    let unspecified = "0.0.0.0:0".parse().unwrap();
    let services = ServiceFlags::NETWORK | ServiceFlags::WITNESS;
    let vm = VersionMessage {
        version: 70016,
        services,
        timestamp: 0,
        receiver: Address::new(&unspecified, ServiceFlags::NONE),
        sender: Address::new(&unspecified, services),
        nonce: 0,
        user_agent: "A".repeat(len),
        start_height: 0,
        relay: true,
    };
    ("version", serialize(&vm))
}

// --- CompactSize helpers ---

fn compact_size(n: u64) -> Vec<u8> {
    if n < 0xfd {
        vec![n as u8]
    } else if n <= 0xffff {
        let mut v = vec![0xfd];
        v.extend_from_slice(&(n as u16).to_le_bytes());
        v
    } else if n <= 0xffff_ffff {
        let mut v = vec![0xfe];
        v.extend_from_slice(&(n as u32).to_le_bytes());
        v
    } else {
        let mut v = vec![0xff];
        v.extend_from_slice(&n.to_le_bytes());
        v
    }
}

fn compact_size_width(value: u64, width: u8) -> Result<Vec<u8>, String> {
    match width {
        1 => {
            if value > 0xfc {
                return Err("width 1 holds only values 0..=252".to_string());
            }
            Ok(vec![value as u8])
        }
        3 => {
            let mut v = vec![0xfd];
            v.extend_from_slice(&(value as u16).to_le_bytes());
            Ok(v)
        }
        5 => {
            let mut v = vec![0xfe];
            v.extend_from_slice(&(value as u32).to_le_bytes());
            Ok(v)
        }
        9 => {
            let mut v = vec![0xff];
            v.extend_from_slice(&value.to_le_bytes());
            Ok(v)
        }
        _ => Err("width must be 1, 3, 5, or 9".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_checksum_flips_one_byte() {
        let frame = vec![0u8; 30];
        let out = mangle(&frame, MangleKind::BadChecksum);
        assert_eq!(out[20], 0x01);
        assert_eq!(out[..20], frame[..20]);
        assert_eq!(out[21..], frame[21..]);
    }

    #[test]
    fn wrong_length_offsets_header() {
        let mut frame = vec![0u8; 30];
        frame[16..20].copy_from_slice(&100u32.to_le_bytes());
        let out = mangle(&frame, MangleKind::WrongLength(5));
        assert_eq!(u32::from_le_bytes([out[16], out[17], out[18], out[19]]), 105);
    }

    #[test]
    fn craft_inv_claims_more_than_present() {
        let (cmd, payload) = craft_inv(1000, 1, 1);
        assert_eq!(cmd, "inv");
        // CompactSize(1000) = fd e8 03, then one 36-byte item.
        assert_eq!(&payload[..3], &[0xfd, 0xe8, 0x03]);
        assert_eq!(payload.len(), 3 + 36);
    }

    #[test]
    fn compactsize_widths() {
        assert_eq!(compact_size_width(5, 9).unwrap()[0], 0xff);
        assert_eq!(compact_size_width(5, 9).unwrap().len(), 9);
        assert!(compact_size_width(5, 2).is_err());
    }

    #[test]
    fn count_overflow_is_max() {
        let (_, payload) = craft_count_overflow("inv".to_string());
        assert_eq!(payload, vec![0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
    }
}
