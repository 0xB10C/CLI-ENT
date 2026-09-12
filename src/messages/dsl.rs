//! The `field=value` message DSL (PLAN.md §9).
//!
//! [`build_message`] turns a message name and its field tokens into a
//! `NetworkMessage`. Sample-backed messages (`tx sample`, `block sample:block1`,
//! …) use [`SampleData`]. The generic `hex:` and `send raw` paths live in the
//! REPL command parser; this module is the typed field path plus the value
//! parsers shared with the CLI.

use std::net::SocketAddr;
use std::str::FromStr;

use bitcoin::hex::FromHex;
use bitcoin::p2p::message::NetworkMessage as M;
use bitcoin::p2p::message_blockdata::{GetBlocksMessage, GetHeadersMessage, Inventory};
use bitcoin::p2p::message_compact_blocks::{BlockTxn, GetBlockTxn, SendCmpct};
use bitcoin::p2p::message_filter::{GetCFHeaders, GetCFilters};
use bitcoin::p2p::message_network::VersionMessage;
use bitcoin::p2p::{Address, ServiceFlags};
use bitcoin::bip152::{BlockTransactions, BlockTransactionsRequest};
use bitcoin::consensus::deserialize;
use bitcoin::hashes::Hash;
use bitcoin::{Block, BlockHash, Transaction, Txid, Wtxid};

use super::samples::{SampleData, SampleName};

/// Build a `NetworkMessage` from a message name and `field=value` tokens.
pub fn build_message(name: &str, fields: &[&str], samples: &SampleData) -> Result<M, String> {
    match name {
        // --- no payload ---
        "verack" => Ok(M::Verack),
        "getaddr" => Ok(M::GetAddr),
        "mempool" => Ok(M::MemPool),
        "sendheaders" => Ok(M::SendHeaders),
        "wtxidrelay" => Ok(M::WtxidRelay),
        "sendaddrv2" => Ok(M::SendAddrV2),
        "filterclear" => Ok(M::FilterClear),

        // --- scalar payloads ---
        "ping" => Ok(M::Ping(opt_u64(fields, "nonce")?.unwrap_or_else(rand::random))),
        "pong" => Ok(M::Pong(
            opt_u64(fields, "nonce")?.ok_or("usage: send pong nonce=<u64>")?,
        )),
        "feefilter" => {
            let v = fields.first().ok_or("usage: send feefilter <sat/kvB>")?;
            Ok(M::FeeFilter(v.parse().map_err(|_| "feefilter: expected an integer")?))
        }
        "sendcmpct" => Ok(M::SendCmpct(SendCmpct {
            send_compact: req_bool(fields, "hb")?,
            version: req_u64(fields, "version")?,
        })),

        // --- inventory vectors ---
        "inv" => Ok(M::Inv(parse_inv(fields)?)),
        "getdata" => Ok(M::GetData(parse_inv(fields)?)),
        "notfound" => Ok(M::NotFound(parse_inv(fields)?)),

        // --- locators ---
        "getheaders" => {
            let (locator_hashes, stop_hash) = parse_locator(fields)?;
            Ok(M::GetHeaders(GetHeadersMessage {
                version: 70016,
                locator_hashes,
                stop_hash,
            }))
        }
        "getblocks" => {
            let (locator_hashes, stop_hash) = parse_locator(fields)?;
            Ok(M::GetBlocks(GetBlocksMessage {
                version: 70016,
                locator_hashes,
                stop_hash,
            }))
        }

        // --- compact filters ---
        "getcfilters" => Ok(M::GetCFilters(GetCFilters {
            filter_type: req_u8(fields, "type")?,
            start_height: req_u32(fields, "start")?,
            stop_hash: req_block_hash(fields, "stop")?,
        })),
        "getcfheaders" => Ok(M::GetCFHeaders(GetCFHeaders {
            filter_type: req_u8(fields, "type")?,
            start_height: req_u32(fields, "start")?,
            stop_hash: req_block_hash(fields, "stop")?,
        })),

        // --- version ---
        "version" => Ok(M::Version(build_version(fields)?)),

        // --- bip152 ---
        "getblocktxn" => Ok(M::GetBlockTxn(GetBlockTxn {
            txs_request: BlockTransactionsRequest {
                block_hash: req_block_hash(fields, "block")?,
                indexes: req_index_list(fields, "idx")?,
            },
        })),

        // --- block/tx/headers, from hex or samples ---
        "tx" => build_tx(fields, samples),
        "block" => build_block(fields, samples),
        "headers" => build_headers(fields, samples),
        "cmpctblock" => build_cmpctblock(fields, samples),
        "blocktxn" => build_blocktxn(fields, samples),

        other => Err(format!(
            "send: message {other:?} has no field form; try `send {other} hex:<payload>`"
        )),
    }
}

// --- version builder ---

fn build_version(fields: &[&str]) -> Result<VersionMessage, String> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let unspecified: SocketAddr = "0.0.0.0:0".parse().unwrap();

    let services = match field(fields, "services") {
        Some(s) => parse_services(s)?,
        None => ServiceFlags::NETWORK | ServiceFlags::WITNESS,
    };
    let receiver = match field(fields, "addr_recv") {
        Some(s) => Address::new(&parse_socket_addr(s)?, ServiceFlags::NONE),
        None => Address::new(&unspecified, ServiceFlags::NONE),
    };
    let sender = match field(fields, "addr_from") {
        Some(s) => Address::new(&parse_socket_addr(s)?, services),
        None => Address::new(&unspecified, services),
    };
    let timestamp = match opt_i64(fields, "timestamp")? {
        Some(t) => t,
        None => SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    };
    Ok(VersionMessage {
        version: opt_u64(fields, "protocol")?.map(|v| v as u32).unwrap_or(70016),
        services,
        timestamp,
        receiver,
        sender,
        nonce: opt_u64(fields, "nonce")?.unwrap_or_else(rand::random),
        user_agent: field(fields, "user_agent")
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("/cli-ent:{}/", env!("CARGO_PKG_VERSION"))),
        start_height: opt_i64(fields, "height")?.map(|h| h as i32).unwrap_or(0),
        relay: match field(fields, "relay") {
            Some(v) => parse_bool(v)?,
            None => true,
        },
    })
}

// --- block/tx/headers builders ---

fn build_tx(fields: &[&str], samples: &SampleData) -> Result<M, String> {
    match fields.first() {
        Some(&"sample") | Some(&"sample:tx") => Ok(M::Tx(samples.tx.clone())),
        Some(hex) => {
            let bytes = Vec::<u8>::from_hex(hex).map_err(|_| "tx: expected raw tx hex or `sample`")?;
            let tx: Transaction = deserialize(&bytes).map_err(|e| format!("tx decode: {e}"))?;
            Ok(M::Tx(tx))
        }
        None => Err("usage: send tx <hex> | send tx sample".to_string()),
    }
}

fn build_block(fields: &[&str], samples: &SampleData) -> Result<M, String> {
    match fields.first() {
        Some(&"sample:genesis") => Ok(M::Block(samples.genesis.clone())),
        Some(&"sample:block1") => Ok(M::Block(samples.block1.clone())),
        Some(hex) => {
            let bytes =
                Vec::<u8>::from_hex(hex).map_err(|_| "block: expected raw block hex or `sample:genesis|block1`")?;
            let block: Block = deserialize(&bytes).map_err(|e| format!("block decode: {e}"))?;
            Ok(M::Block(block))
        }
        None => Err("usage: send block sample:genesis|block1 | send block <hex>".to_string()),
    }
}

fn build_headers(fields: &[&str], samples: &SampleData) -> Result<M, String> {
    match fields.first() {
        Some(&"sample:block1") => Ok(M::Headers(vec![samples.block1.header])),
        Some(&"sample:genesis") => Ok(M::Headers(vec![samples.genesis.header])),
        _ => Err("usage: send headers sample:block1|genesis".to_string()),
    }
}

fn build_cmpctblock(fields: &[&str], samples: &SampleData) -> Result<M, String> {
    let name = sample_name(fields, "cmpctblock")?;
    let block = samples.block(name);
    let compact_block = bitcoin::bip152::HeaderAndShortIds::from_block(block, rand::random(), 2, &[])
        .map_err(|e| format!("cmpctblock: {e:?}"))?;
    Ok(M::CmpctBlock(
        bitcoin::p2p::message_compact_blocks::CmpctBlock { compact_block },
    ))
}

fn build_blocktxn(fields: &[&str], samples: &SampleData) -> Result<M, String> {
    let name = sample_name(fields, "blocktxn")?;
    let block = samples.block(name);
    let idx = req_index_list(fields, "idx")?;
    let mut transactions = Vec::new();
    for i in idx {
        let tx = block
            .txdata
            .get(i as usize)
            .ok_or_else(|| format!("blocktxn: index {i} out of range"))?;
        transactions.push(tx.clone());
    }
    Ok(M::BlockTxn(BlockTxn {
        transactions: BlockTransactions {
            block_hash: block.block_hash(),
            transactions,
        },
    }))
}

fn sample_name(fields: &[&str], msg: &str) -> Result<SampleName, String> {
    match fields.first() {
        Some(&"sample:block1") => Ok(SampleName::Block1),
        Some(&"sample:genesis") => Ok(SampleName::Genesis),
        _ => Err(format!("usage: send {msg} sample:block1|genesis [idx=<n>,…]")),
    }
}

// --- inventory / locator parsing ---

fn parse_inv(fields: &[&str]) -> Result<Vec<Inventory>, String> {
    let mut out = Vec::new();
    for f in fields {
        let (key, list) = f.split_once('=').ok_or_else(usage_inv)?;
        for h in list.split(',').filter(|s| !s.is_empty()) {
            out.push(match key {
                "tx" => Inventory::Transaction(Txid::from_str(h).map_err(|_| bad_hash(h))?),
                "block" => Inventory::Block(BlockHash::from_str(h).map_err(|_| bad_hash(h))?),
                "wtx" => Inventory::WTx(Wtxid::from_str(h).map_err(|_| bad_hash(h))?),
                "cmpct" => Inventory::CompactBlock(BlockHash::from_str(h).map_err(|_| bad_hash(h))?),
                _ => return Err(usage_inv()),
            });
        }
    }
    if out.is_empty() {
        return Err(usage_inv());
    }
    Ok(out)
}

fn parse_locator(fields: &[&str]) -> Result<(Vec<BlockHash>, BlockHash), String> {
    let usage = || "usage: send getheaders|getblocks locator=<hash>[,…] [stop=<hash>]".to_string();
    let loc = field(fields, "locator").ok_or_else(usage)?;
    let mut locator_hashes = Vec::new();
    for h in loc.split(',').filter(|s| !s.is_empty()) {
        locator_hashes.push(BlockHash::from_str(h).map_err(|_| bad_hash(h))?);
    }
    if locator_hashes.is_empty() {
        return Err(usage());
    }
    let stop_hash = match field(fields, "stop") {
        Some(h) => BlockHash::from_str(h).map_err(|_| bad_hash(h))?,
        None => BlockHash::all_zeros(),
    };
    Ok((locator_hashes, stop_hash))
}

// --- value parsers (PLAN.md §9) ---

/// Service flags: an integer (`0x…`/decimal) or `|`-joined names.
pub fn parse_services(s: &str) -> Result<ServiceFlags, String> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x") {
        let n = u64::from_str_radix(hex, 16).map_err(|_| format!("invalid hex mask {s:?}"))?;
        return Ok(ServiceFlags::from(n));
    }
    if let Ok(n) = s.parse::<u64>() {
        return Ok(ServiceFlags::from(n));
    }
    let mut flags = ServiceFlags::NONE;
    for name in s.split('|') {
        flags |= match name.trim().to_ascii_uppercase().as_str() {
            "NONE" => ServiceFlags::NONE,
            "NETWORK" => ServiceFlags::NETWORK,
            "GETUTXO" => ServiceFlags::GETUTXO,
            "BLOOM" => ServiceFlags::BLOOM,
            "WITNESS" => ServiceFlags::WITNESS,
            "COMPACT_FILTERS" => ServiceFlags::COMPACT_FILTERS,
            "NETWORK_LIMITED" => ServiceFlags::NETWORK_LIMITED,
            "P2P_V2" => ServiceFlags::P2P_V2,
            other => return Err(format!("unknown service flag {other:?}")),
        };
    }
    Ok(flags)
}

fn parse_socket_addr(s: &str) -> Result<SocketAddr, String> {
    SocketAddr::from_str(s).map_err(|_| format!("invalid ip:port {s:?} (use [v6]:port for IPv6)"))
}

fn parse_u64_str(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).map_err(|_| format!("invalid integer {s:?}"))
    } else {
        s.parse().map_err(|_| format!("invalid integer {s:?}"))
    }
}

fn parse_i64_str(s: &str) -> Result<i64, String> {
    s.trim().parse().map_err(|_| format!("invalid integer {s:?}"))
}

fn parse_bool(s: &str) -> Result<bool, String> {
    match s.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => Err(format!("invalid bool {s:?} (true/false)")),
    }
}

// --- field accessors ---

fn field<'a>(fields: &[&'a str], key: &str) -> Option<&'a str> {
    fields
        .iter()
        .find_map(|f| f.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v))
}

fn opt_u64(fields: &[&str], key: &str) -> Result<Option<u64>, String> {
    field(fields, key).map(parse_u64_str).transpose()
}
fn opt_i64(fields: &[&str], key: &str) -> Result<Option<i64>, String> {
    field(fields, key).map(parse_i64_str).transpose()
}
fn req_u64(fields: &[&str], key: &str) -> Result<u64, String> {
    opt_u64(fields, key)?.ok_or_else(|| format!("missing {key}=<u64>"))
}
fn req_u32(fields: &[&str], key: &str) -> Result<u32, String> {
    Ok(req_u64(fields, key)? as u32)
}
fn req_u8(fields: &[&str], key: &str) -> Result<u8, String> {
    Ok(req_u64(fields, key)? as u8)
}
fn req_bool(fields: &[&str], key: &str) -> Result<bool, String> {
    field(fields, key)
        .ok_or_else(|| format!("missing {key}=<bool>"))
        .and_then(parse_bool)
}
fn req_block_hash(fields: &[&str], key: &str) -> Result<BlockHash, String> {
    let h = field(fields, key).ok_or_else(|| format!("missing {key}=<hash>"))?;
    BlockHash::from_str(h).map_err(|_| bad_hash(h))
}
fn req_index_list(fields: &[&str], key: &str) -> Result<Vec<u64>, String> {
    let raw = field(fields, key).ok_or_else(|| format!("missing {key}=<n>[,…]"))?;
    raw.split(',')
        .filter(|s| !s.is_empty())
        .map(parse_u64_str)
        .collect()
}

fn bad_hash(h: &str) -> String {
    format!("invalid hash {h:?} (expect 64 hex chars)")
}
fn usage_inv() -> String {
    "usage: send inv tx=<txid>,… block=<hash>,… wtx=<wtxid>,… cmpct=<hash>,…".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::Network;

    fn samples() -> SampleData {
        SampleData::new(Network::Regtest)
    }

    #[test]
    fn version_defaults_and_overrides() {
        let m = build_message("version", &["protocol=70015", "relay=false"], &samples()).unwrap();
        match m {
            M::Version(v) => {
                assert_eq!(v.version, 70015);
                assert!(!v.relay);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn tx_and_block_samples() {
        let s = samples();
        assert!(matches!(build_message("tx", &["sample"], &s), Ok(M::Tx(_))));
        assert!(matches!(
            build_message("block", &["sample:block1"], &s),
            Ok(M::Block(_))
        ));
        assert!(matches!(
            build_message("headers", &["sample:block1"], &s),
            Ok(M::Headers(_))
        ));
    }

    #[test]
    fn getcfilters_fields() {
        let s = samples();
        let stop = s.genesis_hash.to_string();
        let m = build_message(
            "getcfilters",
            &["type=0", "start=0", &format!("stop={stop}")],
            &s,
        )
        .unwrap();
        assert!(matches!(m, M::GetCFilters(_)));
    }

    #[test]
    fn blocktxn_sample_index() {
        let s = samples();
        let m = build_message("blocktxn", &["sample:block1", "idx=0"], &s).unwrap();
        match m {
            M::BlockTxn(bt) => assert_eq!(bt.transactions.transactions.len(), 1),
            _ => panic!(),
        }
    }
}
