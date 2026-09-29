//! Hashes and ids of version 2 (`docs/CONSENSUS_V2.md` sections 5 and 6.4): the proof-of-work header
//! hash, the block id, transaction and coinbase ids, the Merkle root, the genesis block and the chain
//! id. Every hash carries a domain tag, so no two kinds of object share an input.

use super::codec::{EncodeError, Wire};
use super::types::{BlockHeader, Coinbase, Prunable, PrunedTransaction, Transaction, VERSION};
use crate::hash::sha256;
use crate::matmulhash;

pub const HEADER_TAG: &[u8] = b"tenero block header v2";
pub const TX_TAG: &[u8] = b"tenero tx v2";
pub const PRUNABLE_TAG: &[u8] = b"tenero tx prunable v2";
pub const COINBASE_TAG: &[u8] = b"tenero coinbase v2";
pub const GENESIS_TAG: &[u8] = b"tenero genesis";
pub const GENESIS_ID_TAG: &[u8] = b"tenero genesis id v2";
/// The default network label: what makes this network's genesis, and so its chain id, its own.
pub const NETWORK_LABEL: &str = "tenero experimental network 1";

/// Which proof of work a chain uses; it decides how the block id is computed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowKind {
    Matmul,
    Sha256,
}

/// The 32-byte input of the proof of work: the header WITHOUT its nonce and mix, with a domain tag.
pub fn header_hash(h: &BlockHeader) -> [u8; 32] {
    sha256(&[
        HEADER_TAG,
        &h.version.to_le_bytes(),
        &h.prev_id,
        &h.timestamp.to_le_bytes(),
        &h.tx_root,
    ])
}

/// `sha256(header_hash || nonce)`: the seed of a matmulhash attempt.
pub fn pow_seed(header_hash: &[u8; 32], nonce: u64) -> [u8; 32] {
    matmulhash::attempt_seed(header_hash, nonce)
}

/// The block id: the proof-of-work digest. It commits to the header, the nonce and the mix.
pub fn block_id(h: &BlockHeader, pow: PowKind) -> [u8; 32] {
    let hh = header_hash(h);
    match pow {
        PowKind::Matmul => matmulhash::digest_of(&pow_seed(&hh, h.nonce), &h.mix),
        PowKind::Sha256 => sha256(&[&hh, &h.nonce.to_le_bytes()]),
    }
}

/// The hash of a transaction's prunable part: its serialized bytes (one ring per input, then the proofs),
/// so it covers every ring index as well as every proof byte. `n_inputs` is the prefix's input count.
pub fn prunable_hash(p: &Prunable, n_inputs: usize) -> Result<[u8; 32], EncodeError> {
    Ok(sha256(&[PRUNABLE_TAG, &p.to_bytes(n_inputs)?]))
}

/// The id of a full transaction: over the prefix and the hash of the prunable part.
pub fn tx_id(t: &Transaction) -> Result<[u8; 32], EncodeError> {
    Ok(sha256(&[
        TX_TAG,
        &t.prefix.to_bytes()?,
        &prunable_hash(&t.prunable, t.prefix.inputs.len())?,
    ]))
}

/// The id of a pruned transaction: **the same** as the full one it came from.
pub fn pruned_tx_id(p: &PrunedTransaction) -> Result<[u8; 32], EncodeError> {
    Ok(sha256(&[TX_TAG, &p.prefix.to_bytes()?, &p.prunable_hash]))
}

pub fn coinbase_id(c: &Coinbase) -> Result<[u8; 32], EncodeError> {
    Ok(sha256(&[COINBASE_TAG, &c.to_bytes()?]))
}

impl Transaction {
    /// The pruned form: the same prefix, the rings and proofs replaced by their hash.
    pub fn prune(&self) -> Result<PrunedTransaction, EncodeError> {
        Ok(PrunedTransaction {
            prefix: self.prefix.clone(),
            prunable_hash: prunable_hash(&self.prunable, self.prefix.inputs.len())?,
        })
    }
}

fn merkle_leaf(id: &[u8; 32]) -> [u8; 32] {
    sha256(&[&[0x00], id])
}

fn merkle_node(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    sha256(&[&[0x01], left, right])
}

/// The transaction Merkle root, as in RFC 6962: split at the largest power of two below `n`, and an odd
/// leaf is never duplicated. The empty tree is `sha256("")`.
pub fn merkle_root(ids: &[[u8; 32]]) -> [u8; 32] {
    match ids.len() {
        0 => sha256(&[]),
        1 => merkle_leaf(&ids[0]),
        n => {
            let k = 1usize << (usize::BITS - 1 - (n - 1).leading_zeros());
            merkle_node(&merkle_root(&ids[..k]), &merkle_root(&ids[k..]))
        }
    }
}

/// The Merkle root of a block's transactions: the coinbase id first, then the others in order.
pub fn block_tx_root(coinbase: &Coinbase, txs: &[Transaction]) -> Result<[u8; 32], EncodeError> {
    let mut ids = Vec::with_capacity(1 + txs.len());
    ids.push(coinbase_id(coinbase)?);
    for t in txs {
        ids.push(tx_id(t)?);
    }
    Ok(merkle_root(&ids))
}

/// The genesis header of the network named `label`: no parent, timestamp 0, nonce 0, a zero mix, and a
/// `tx_root` that is a hash of the label (the genesis block has no transactions).
pub fn genesis_header(label: &str) -> BlockHeader {
    BlockHeader {
        version: VERSION,
        prev_id: [0; 32],
        timestamp: 0,
        tx_root: sha256(&[GENESIS_TAG, label.as_bytes()]),
        nonce: 0,
        mix: [0; 64],
    }
}

/// The genesis block id, which is also the **chain id**. The genesis block is fixed in the software and
/// exempt from the proof of work.
pub fn genesis_id(label: &str) -> [u8; 32] {
    // A header is a fixed-size object: writing it cannot fail.
    let bytes = genesis_header(label)
        .to_bytes()
        .expect("a header always encodes");
    sha256(&[GENESIS_ID_TAG, &bytes])
}
