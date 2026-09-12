//! The static preset table (PLAN.md §10).
//!
//! A preset is a name, a one-line description, and a small sequence of messages
//! to send. Presets that reference chain data use the embedded samples.

use bitcoin::bip152::{BlockTransactions, BlockTransactionsRequest, HeaderAndShortIds};
use bitcoin::hashes::Hash;
use bitcoin::p2p::message::NetworkMessage as M;
use bitcoin::p2p::message_blockdata::{GetBlocksMessage, GetHeadersMessage, Inventory};
use bitcoin::p2p::message_compact_blocks::SendCmpct;
use bitcoin::p2p::message_filter::{GetCFHeaders, GetCFilters};
use bitcoin::BlockHash;

use super::samples::SampleData;

/// All preset names with their descriptions, for `preset list`.
pub fn list() -> Vec<(&'static str, &'static str)> {
    vec![
        ("inv-getdata-tx", "announce the sample tx (serve answers the getdata)"),
        ("inv-getdata-block", "announce block1 (node fetches and connects it on regtest)"),
        ("cmpctblock-block1", "send block1 as a compact block (no prefill; node must getblocktxn)"),
        ("getblocktxn-block1", "request block1 tx index 0"),
        ("blocktxn-block1", "send block1 tx index 0"),
        ("ping", "ping with a random nonce"),
        ("getaddr", "request addresses (Core answers inbound peers)"),
        ("mempool", "request mempool (Core disconnects unless -peerbloomfilters=1)"),
        ("sendheaders", "prefer header announcements"),
        ("feefilter", "feefilter 1000 sat/kvB"),
        ("sendcmpct", "sendcmpct hb=true version=2"),
        ("getheaders-genesis", "getheaders with a genesis locator"),
        ("getblocks-genesis", "getblocks with a genesis locator"),
        ("getcfheaders-genesis", "getcfheaders type 0 over genesis (needs -blockfilterindex=1)"),
        ("getcfilters-genesis", "getcfilters type 0 over genesis (needs -blockfilterindex=1)"),
    ]
}

/// Build the messages for a preset, or `None` if the name is unknown.
pub fn build(name: &str, s: &SampleData) -> Option<Vec<M>> {
    let msgs = match name {
        "inv-getdata-tx" => vec![M::Inv(vec![Inventory::Transaction(s.txid)])],
        "inv-getdata-block" => vec![M::Inv(vec![Inventory::Block(s.block1_hash)])],
        "cmpctblock-block1" => {
            let compact_block = HeaderAndShortIds::from_block(&s.block1, rand::random(), 2, &[])
                .expect("block1 compacts");
            vec![M::CmpctBlock(
                bitcoin::p2p::message_compact_blocks::CmpctBlock { compact_block },
            )]
        }
        "getblocktxn-block1" => vec![M::GetBlockTxn(bitcoin::p2p::message_compact_blocks::GetBlockTxn {
            txs_request: BlockTransactionsRequest {
                block_hash: s.block1_hash,
                indexes: vec![0],
            },
        })],
        "blocktxn-block1" => vec![M::BlockTxn(bitcoin::p2p::message_compact_blocks::BlockTxn {
            transactions: BlockTransactions {
                block_hash: s.block1_hash,
                transactions: vec![s.block1.txdata[0].clone()],
            },
        })],
        "ping" => vec![M::Ping(rand::random())],
        "getaddr" => vec![M::GetAddr],
        "mempool" => vec![M::MemPool],
        "sendheaders" => vec![M::SendHeaders],
        "feefilter" => vec![M::FeeFilter(1000)],
        "sendcmpct" => vec![M::SendCmpct(SendCmpct {
            send_compact: true,
            version: 2,
        })],
        "getheaders-genesis" => vec![M::GetHeaders(GetHeadersMessage {
            version: 70016,
            locator_hashes: vec![s.genesis_hash],
            stop_hash: zero_hash(),
        })],
        "getblocks-genesis" => vec![M::GetBlocks(GetBlocksMessage {
            version: 70016,
            locator_hashes: vec![s.genesis_hash],
            stop_hash: zero_hash(),
        })],
        "getcfheaders-genesis" => vec![M::GetCFHeaders(GetCFHeaders {
            filter_type: 0,
            start_height: 0,
            stop_hash: s.genesis_hash,
        })],
        "getcfilters-genesis" => vec![M::GetCFilters(GetCFilters {
            filter_type: 0,
            start_height: 0,
            stop_hash: s.genesis_hash,
        })],
        _ => return None,
    };
    Some(msgs)
}

fn zero_hash() -> BlockHash {
    BlockHash::all_zeros()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::Network;

    #[test]
    fn every_preset_builds() {
        let s = SampleData::new(Network::Regtest);
        for (name, _) in list() {
            assert!(build(name, &s).is_some(), "preset {name} failed to build");
        }
    }

    #[test]
    fn unknown_preset_is_none() {
        let s = SampleData::new(Network::Regtest);
        assert!(build("nope", &s).is_none());
    }
}
