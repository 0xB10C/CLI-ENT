//! Handshake profiles and the `version` builder (PLAN.md §6).
//!
//! A [`Handshaker`] turns connection events into the messages a profile sends:
//! `core` mimics Bitcoin Core's negotiation, `minimal` sends only version/verack,
//! and `manual` sends nothing (you drive it by hand). The session sets the
//! handshake state flags from the actual messages sent and received, so these
//! responders only decide *what* to send, not *when* `Ready` is reached.
//!
//! Milestone 1 wires up the responders and the version builder; `--verack-delay`
//! and per-field `send version …` control arrive in milestone 4.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{SystemTime, UNIX_EPOCH};

use bitcoin::p2p::message::NetworkMessage;
use bitcoin::p2p::message_compact_blocks::SendCmpct;
use bitcoin::p2p::message_network::VersionMessage;
use bitcoin::p2p::{Address, ServiceFlags};

/// Which handshake profile is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Core,
    Minimal,
    Manual,
}

/// The fields of the `version` message we send. Defaults match PLAN.md §6.
#[derive(Debug, Clone)]
pub struct VersionConfig {
    pub protocol_version: u32,
    pub services: ServiceFlags,
    pub user_agent: String,
    pub start_height: i32,
    pub relay: bool,
}

impl Default for VersionConfig {
    fn default() -> Self {
        Self {
            protocol_version: 70016,
            services: ServiceFlags::NETWORK | ServiceFlags::WITNESS,
            user_agent: format!("/cli-ent:{}/", env!("CARGO_PKG_VERSION")),
            start_height: 0,
            relay: true,
        }
    }
}

/// Turns handshake milestones into the messages a profile sends.
#[derive(Debug, Clone)]
pub struct Handshaker {
    pub profile: Profile,
    /// When true, the profile does not auto-send our `verack`; it awaits `send verack`.
    pub verack_manual: bool,
    pub version: VersionConfig,
}

impl Handshaker {
    /// Build our `version` message for this peer.
    pub fn build_version(&self, peer: SocketAddr) -> VersionMessage {
        let receiver = Address::new(&peer, ServiceFlags::NONE);
        let sender = Address::new(
            &SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)),
            self.version.services,
        );
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        VersionMessage {
            version: self.version.protocol_version,
            services: self.version.services,
            timestamp,
            receiver,
            sender,
            nonce: rand::random(),
            user_agent: self.version.user_agent.clone(),
            start_height: self.version.start_height,
            relay: self.version.relay,
        }
    }

    /// Messages to send immediately on connect.
    pub fn on_connect(&self, peer: SocketAddr) -> Vec<NetworkMessage> {
        match self.profile {
            Profile::Core | Profile::Minimal => {
                vec![NetworkMessage::Version(self.build_version(peer))]
            }
            Profile::Manual => vec![],
        }
    }

    /// Messages to send in response to the peer's `version`.
    pub fn on_peer_version(&self, peer: &VersionMessage) -> Vec<NetworkMessage> {
        match self.profile {
            Profile::Manual => vec![],
            Profile::Minimal => self.maybe_verack(vec![]),
            Profile::Core => {
                let mut out = Vec::new();
                // BIP339: wtxidrelay must precede verack, and is only understood
                // by peers advertising protocol >= 70016.
                if peer.version >= 70016 {
                    out.push(NetworkMessage::WtxidRelay);
                }
                out.push(NetworkMessage::SendAddrV2);
                self.maybe_verack(out)
            }
        }
    }

    /// Messages to send in response to the peer's `verack`.
    pub fn on_peer_verack(&self) -> Vec<NetworkMessage> {
        match self.profile {
            Profile::Core => vec![
                NetworkMessage::SendCmpct(SendCmpct {
                    send_compact: false,
                    version: 2,
                }),
                NetworkMessage::Ping(rand::random()),
                NetworkMessage::SendHeaders,
                NetworkMessage::FeeFilter(1000),
                NetworkMessage::GetAddr,
            ],
            Profile::Minimal | Profile::Manual => vec![],
        }
    }

    /// Append our `verack` unless verack is under manual control.
    fn maybe_verack(&self, mut out: Vec<NetworkMessage>) -> Vec<NetworkMessage> {
        if !self.verack_manual {
            out.push(NetworkMessage::Verack);
        }
        out
    }
}
