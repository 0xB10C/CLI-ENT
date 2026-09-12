//! The toggleable responders (PLAN.md §7).
//!
//! Each reacts to one received message and returns messages to send back, tagged
//! for the printer. `auto on|off <name>` flips them at runtime.
//!
//! | name          | trigger              | action                                  | default |
//! |---------------|----------------------|-----------------------------------------|---------|
//! | pong          | ping                 | pong with the same nonce                | on      |
//! | headers-empty | getheaders/getblocks | empty headers / nothing                 | on      |
//! | serve         | getdata for a sample | tx / block / notfound                   | on      |
//! | getdata       | inv                  | getdata for every item                  | off     |

use std::str::FromStr;
use std::sync::Arc;

use bitcoin::p2p::message::NetworkMessage;
use bitcoin::p2p::message_blockdata::Inventory;

use crate::messages::samples::SampleData;

/// The four automations, addressable by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoKind {
    Pong,
    HeadersEmpty,
    Serve,
    Getdata,
}

impl AutoKind {
    pub fn name(self) -> &'static str {
        match self {
            AutoKind::Pong => "pong",
            AutoKind::HeadersEmpty => "headers-empty",
            AutoKind::Serve => "serve",
            AutoKind::Getdata => "getdata",
        }
    }

    pub const ALL: [AutoKind; 4] = [
        AutoKind::Pong,
        AutoKind::HeadersEmpty,
        AutoKind::Serve,
        AutoKind::Getdata,
    ];
}

impl FromStr for AutoKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pong" => Ok(AutoKind::Pong),
            "headers-empty" => Ok(AutoKind::HeadersEmpty),
            "serve" => Ok(AutoKind::Serve),
            "getdata" => Ok(AutoKind::Getdata),
            other => Err(format!("unknown automation {other:?}")),
        }
    }
}

/// On/off state for the four automations (mirrored into `SessionView`).
#[derive(Debug, Clone, Copy)]
pub struct AutoState {
    pub pong: bool,
    pub headers_empty: bool,
    pub serve: bool,
    pub getdata: bool,
}

impl Default for AutoState {
    fn default() -> Self {
        // Defaults per PLAN §7: pong/headers-empty/serve on, getdata off.
        Self {
            pong: true,
            headers_empty: true,
            serve: true,
            getdata: false,
        }
    }
}

impl AutoState {
    pub fn get(&self, kind: AutoKind) -> bool {
        match kind {
            AutoKind::Pong => self.pong,
            AutoKind::HeadersEmpty => self.headers_empty,
            AutoKind::Serve => self.serve,
            AutoKind::Getdata => self.getdata,
        }
    }
    pub fn set(&mut self, kind: AutoKind, on: bool) {
        match kind {
            AutoKind::Pong => self.pong = on,
            AutoKind::HeadersEmpty => self.headers_empty = on,
            AutoKind::Serve => self.serve = on,
            AutoKind::Getdata => self.getdata = on,
        }
    }
}

/// Holds the automation state and the sample data used by `serve`.
pub struct Automations {
    pub state: AutoState,
    pub samples: Arc<SampleData>,
}

impl Automations {
    pub fn new(samples: Arc<SampleData>) -> Self {
        Self {
            state: AutoState::default(),
            samples,
        }
    }

    /// Compute the automatic responses to one received message. Each response is
    /// tagged for the printer (e.g. `auto: pong`).
    pub fn on_message(
        &self,
        msg: &NetworkMessage,
        wtxidrelay: bool,
    ) -> Vec<(NetworkMessage, String)> {
        let mut out = Vec::new();
        match msg {
            NetworkMessage::Ping(nonce) if self.state.pong => {
                out.push((NetworkMessage::Pong(*nonce), "auto: pong".to_string()));
            }
            NetworkMessage::GetHeaders(_) if self.state.headers_empty => {
                out.push((NetworkMessage::Headers(vec![]), "auto: headers-empty".to_string()));
            }
            // getblocks: an empty reply is "nothing", so we send nothing.
            NetworkMessage::GetData(items) if self.state.serve => {
                out.extend(self.serve(items));
            }
            NetworkMessage::Inv(items) if self.state.getdata => {
                let requested = self.mirror_inv(items, wtxidrelay);
                if !requested.is_empty() {
                    out.push((NetworkMessage::GetData(requested), "auto: getdata".to_string()));
                }
            }
            _ => {}
        }
        out
    }

    /// Answer a getdata from the sample map: tx/block for known hashes, one
    /// notfound for the rest.
    fn serve(&self, items: &[Inventory]) -> Vec<(NetworkMessage, String)> {
        let mut out = Vec::new();
        let mut notfound = Vec::new();
        for item in items {
            match item {
                Inventory::Transaction(txid) | Inventory::WitnessTransaction(txid) => {
                    match self.samples.tx_for_txid(txid) {
                        Some(tx) => out.push((NetworkMessage::Tx(tx.clone()), "auto: serve".to_string())),
                        None => notfound.push(*item),
                    }
                }
                Inventory::WTx(wtxid) => match self.samples.tx_for_wtxid(wtxid) {
                    Some(tx) => out.push((NetworkMessage::Tx(tx.clone()), "auto: serve".to_string())),
                    None => notfound.push(*item),
                },
                Inventory::Block(hash)
                | Inventory::WitnessBlock(hash)
                | Inventory::CompactBlock(hash) => match self.samples.block_for_hash(hash) {
                    Some(block) => {
                        out.push((NetworkMessage::Block(block.clone()), "auto: serve".to_string()))
                    }
                    None => notfound.push(*item),
                },
                other => notfound.push(*other),
            }
        }
        if !notfound.is_empty() {
            out.push((NetworkMessage::NotFound(notfound), "auto: serve".to_string()));
        }
        out
    }

    /// Build a getdata mirroring an inv, upgrading tx/block to witness types when
    /// wtxidrelay is negotiated.
    fn mirror_inv(&self, items: &[Inventory], wtxidrelay: bool) -> Vec<Inventory> {
        items
            .iter()
            .map(|item| {
                if wtxidrelay {
                    match item {
                        Inventory::Transaction(t) => Inventory::WitnessTransaction(*t),
                        Inventory::Block(b) => Inventory::WitnessBlock(*b),
                        other => *other,
                    }
                } else {
                    *item
                }
            })
            .collect()
    }
}
