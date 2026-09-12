//! Sample data (PLAN.md §10).
//!
//! Static, embedded artifacts for exercising inv/getdata/tx and block flows:
//! - `genesis`: `bitcoin::constants::genesis_block(network)`.
//! - `block1`: a pre-mined regtest block at height 1 (valid on any fresh regtest;
//!   its `prev_blockhash` is the regtest genesis). Generated once with a real
//!   regtest node (`generatetoaddress 1`, `getblock <hash> 0`).
//! - `tx`: one well-formed segwit transaction (a regtest 1 BTC send). Nodes reject
//!   it (unknown inputs); the point is exercising the inv/getdata/tx path.
//!
//! [`SampleData::new`] parses these once and indexes them by hash so the `serve`
//! automation (PLAN.md §7) can answer a `getdata` for any hash we announced.

use bitcoin::consensus::deserialize;
use bitcoin::hex::FromHex;
use bitcoin::{Block, BlockHash, Network, Transaction, Txid, Wtxid};

/// Pre-mined regtest block at height 1 (coinbase-only), as raw block hex.
pub const BLOCK1_HEX: &str = "0000002006226e46111a0b59caaf126043eb5bbf28c34f3a5e332a1fc7b2b73cf188910f10010d4831a654a2aead825d8a78bdeddfd526b6e7f144a4d3cb24e756cfc4a10ec8a46affff7f200000000001020000000001010000000000000000000000000000000000000000000000000000000000000000ffffffff025100feffffff0200f2052a01000000160014bec6e1dffe426d1cfe5fad144c9f1beb77991df70000000000000000266a24aa21a9ede2f61c3f71d1defd3fa999dfa36953755c690689799962b48bebd836974e8cf90120000000000000000000000000000000000000000000000000000000000000000000000000";

/// A well-formed segwit transaction (regtest 1 BTC send with a witness).
pub const SAMPLE_TX_HEX: &str = "020000000001011573fe6a0d2f31f4641dbd69457eb0df3e872bd268a0fa476ac417024f450d2a0000000000fdffffff0200e1f50500000000160014cbaf0f60ebeb5406c9c977f4b12f2c7682f3e0857e0b102401000000160014e53883a7d9b641c4d6baa4509a33d489a25882140247304402204d80693e259e21034f2f020d8e50a03035656a7bd2b824472fe6df062042208e022034194623d52ceed0e3aa9d55eebe65c4fbc2e3f8bffc7cd4e12c14f8245692e8012102a397164a5c63cd2fe0626bdd03e7af370e25f8e0db1d88308bd854207f12b31b66000000";

/// Which sample a command referred to.
#[derive(Debug, Clone, Copy)]
pub enum SampleName {
    Genesis,
    Block1,
}

/// The parsed samples plus their hashes, built once at startup.
pub struct SampleData {
    pub network: Network,
    pub genesis: Block,
    pub genesis_hash: BlockHash,
    pub block1: Block,
    pub block1_hash: BlockHash,
    pub tx: Transaction,
    pub txid: Txid,
    pub wtxid: Wtxid,
    /// Poison: block1 with its merkle root byte-flipped. The message is
    /// well-formed; the node accepts it then rejects on the merkle check.
    pub block_badmerkle: Block,
}

impl SampleData {
    /// Parse the embedded samples for the given network.
    pub fn new(network: Network) -> Self {
        let genesis = bitcoin::constants::genesis_block(network);
        let genesis_hash = genesis.block_hash();

        let block1: Block =
            deserialize(&Vec::from_hex(BLOCK1_HEX).expect("block1 hex")).expect("block1 decodes");
        let block1_hash = block1.block_hash();

        let tx: Transaction =
            deserialize(&Vec::from_hex(SAMPLE_TX_HEX).expect("tx hex")).expect("tx decodes");
        let txid = tx.compute_txid();
        let wtxid = tx.compute_wtxid();

        // Poison: flip a byte of block1's merkle root. Well-formed message,
        // fails the node's merkle check.
        let mut block_badmerkle = block1.clone();
        {
            use bitcoin::hashes::Hash;
            let mut root = block_badmerkle.header.merkle_root.to_byte_array();
            root[0] ^= 0x01;
            block_badmerkle.header.merkle_root =
                bitcoin::TxMerkleNode::from_byte_array(root);
        }

        Self {
            network,
            genesis,
            genesis_hash,
            block1,
            block1_hash,
            tx,
            txid,
            wtxid,
            block_badmerkle,
        }
    }

    /// The block for a named sample.
    pub fn block(&self, name: SampleName) -> &Block {
        match name {
            SampleName::Genesis => &self.genesis,
            SampleName::Block1 => &self.block1,
        }
    }

    /// A block we can serve for this hash (block1 or genesis).
    pub fn block_for_hash(&self, hash: &BlockHash) -> Option<&Block> {
        if *hash == self.block1_hash {
            Some(&self.block1)
        } else if *hash == self.genesis_hash {
            Some(&self.genesis)
        } else {
            None
        }
    }

    /// The sample tx if it matches this txid.
    pub fn tx_for_txid(&self, txid: &Txid) -> Option<&Transaction> {
        (*txid == self.txid).then_some(&self.tx)
    }

    /// The sample tx if it matches this wtxid.
    pub fn tx_for_wtxid(&self, wtxid: &Wtxid) -> Option<&Transaction> {
        (*wtxid == self.wtxid).then_some(&self.tx)
    }

    /// The txid of the sample tx as an all-zeros-comparable byte string (for tests).
    pub fn txid_hex(&self) -> String {
        self.txid.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hashes::Hash;

    #[test]
    fn block1_prev_is_regtest_genesis() {
        let s = SampleData::new(Network::Regtest);
        assert_eq!(s.block1.header.prev_blockhash, s.genesis_hash);
    }

    #[test]
    fn samples_parse_and_hash() {
        let s = SampleData::new(Network::Regtest);
        // The sample tx has a witness (segwit), so txid != wtxid.
        assert_ne!(s.txid.to_byte_array(), s.wtxid.to_byte_array());
        // block1 has exactly the coinbase.
        assert_eq!(s.block1.txdata.len(), 1);
    }
}
