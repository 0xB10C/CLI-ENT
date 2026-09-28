//! Shared, readable session state (PLAN.md §3, §6).
//!
//! Held behind an `Arc<Mutex<_>>`; the session task writes it and the REPL reads
//! it directly for `status` and `show`. Milestone 1 populates peer state, running
//! stats, and a bounded ring buffer of recent messages.

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::time::Instant;

use bitcoin::p2p::message_network::VersionMessage;

use crate::net::transport::{TransportKind, Wire};

use super::automations::AutoState;
use super::events::{Direction, HandshakeState};

/// How many recent messages the ring buffer retains (PLAN.md §12).
const RING_CAPACITY: usize = 10_000;

/// What we know about the peer and the negotiation so far.
#[derive(Debug, Default)]
pub struct PeerState {
    pub addr: Option<SocketAddr>,
    /// The original peer spec, when it differed from the resolved address.
    pub resolved_from: Option<String>,
    pub transport: Option<TransportKind>,
    pub v2_session_id: Option<[u8; 32]>,
    pub fell_back: Option<String>,
    pub handshake: HandshakeState,
    pub peer_version: Option<VersionMessage>,
    pub negotiated: Negotiated,
}

/// Feature negotiation observed from the peer (for `status`).
#[derive(Debug, Default, Clone)]
pub struct Negotiated {
    pub wtxidrelay: bool,
    pub addrv2: bool,
    pub sendheaders: bool,
    pub sendcmpct: Option<(bool, u64)>,
    pub feefilter: Option<i64>,
}

/// Byte and message counters, per direction and per command (PLAN.md §12).
#[derive(Debug, Clone, Default)]
pub struct SessionStats {
    pub connected_at: Option<Instant>,
    pub msgs_in: u64,
    pub bytes_in: u64,
    pub msgs_out: u64,
    pub bytes_out: u64,
    pub per_msg_in: BTreeMap<String, u64>,
    pub per_msg_out: BTreeMap<String, u64>,
}

impl SessionStats {
    fn record(&mut self, dir: Direction, command: &str, bytes: usize) {
        match dir {
            Direction::Sent => {
                self.msgs_out += 1;
                self.bytes_out += bytes as u64;
                *self.per_msg_out.entry(command.to_string()).or_default() += 1;
            }
            Direction::Recv => {
                self.msgs_in += 1;
                self.bytes_in += bytes as u64;
                *self.per_msg_in.entry(command.to_string()).or_default() += 1;
            }
        }
    }
}

/// One entry in the ring buffer.
#[derive(Debug, Clone)]
pub struct RingEntry {
    pub seq: u64,
    pub dir: Direction,
    pub wire: Wire,
    pub at: Instant,
}

/// The full session view: peer state, stats, and recent-message ring.
#[derive(Debug, Default)]
pub struct SessionView {
    pub peer: PeerState,
    pub stats: SessionStats,
    pub ring: VecDeque<RingEntry>,
    /// Current automation on/off state, mirrored from the session.
    pub automations: AutoState,
}

impl SessionView {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a message in both the stats and the ring buffer.
    pub fn record(&mut self, seq: u64, dir: Direction, wire: &Wire, at: Instant) {
        self.stats.record(dir, &wire.command(), wire.raw.len());
        if self.ring.len() == RING_CAPACITY {
            self.ring.pop_front();
        }
        self.ring.push_back(RingEntry {
            seq,
            dir,
            wire: wire.clone(),
            at,
        });
    }

    /// Count a sent message in the stats without adding it to the ring.
    ///
    /// High-rate `spam` prints (and rings) only a sample of its sends, but every
    /// send still has to reach the traffic counters, and the ring has to keep the
    /// history a burst would otherwise evict.
    pub fn count_sent(&mut self, wire: &Wire) {
        self.stats.record(Direction::Sent, &wire.command(), wire.raw.len());
    }

    /// Look up a ring entry by sequence number.
    pub fn get(&self, seq: u64) -> Option<&RingEntry> {
        self.ring.iter().find(|e| e.seq == seq)
    }

    /// The most recent ring entry.
    pub fn last(&self) -> Option<&RingEntry> {
        self.ring.back()
    }

    /// Clear peer state and stats for a fresh connection. The ring buffer is kept
    /// (its sequence numbers continue across reconnects, per PLAN §3).
    pub fn reset_connection(&mut self) {
        self.peer = PeerState::default();
        self.stats = SessionStats::default();
    }

    /// Whether a peer is currently connected.
    pub fn is_connected(&self) -> bool {
        self.peer.addr.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::transport::FrameKind;

    fn wire(command: &str, len: usize) -> Wire {
        Wire {
            msg: None,
            frame: FrameKind::V1 {
                magic: [0; 4],
                command: command.to_string(),
                len: len as u32,
                checksum_ok: true,
            },
            raw: vec![0; len],
            decode_error: None,
        }
    }

    #[test]
    fn count_sent_updates_the_stats_but_not_the_ring() {
        let mut v = SessionView::new();
        let at = Instant::now();
        v.record(0, Direction::Sent, &wire("ping", 32), at);
        for _ in 0..10 {
            v.count_sent(&wire("ping", 32));
        }
        assert_eq!(v.stats.msgs_out, 11);
        assert_eq!(v.stats.bytes_out, 32 * 11);
        assert_eq!(v.stats.per_msg_out["ping"], 11);
        assert_eq!(v.ring.len(), 1, "only the recorded send enters the ring");
    }
}
