//! Hashes and ids of version 3 (`docs/CONSENSUS_V2.md` 15.3): version 2's construction with new domain tags, so no
//! version 3 object can be taken for a version 2 one; and the message the FCMP++ spend proofs sign (15.7).

use super::types::{Prunable, PrunedTransaction, Transaction, VERSION};
use crate::hash::sha256;
use crate::matmulhash;
use crate::v2::codec::{EncodeError, Wire};
use crate::v2::ids::PowKind;
use crate::v2::{BlockHeader, Coinbase};

pub const HEADER_TAG: &[u8] = b"tenero block header v3";
pub const TX_TAG: &[u8] = b"tenero tx v3";
pub const PRUNABLE_TAG: &[u8] = b"tenero tx prunable v3";
pub const COINBASE_TAG: &[u8] = b"tenero coinbase v3";
pub const GENESIS_TAG: &[u8] = b"tenero genesis v3";
pub const GENESIS_ID_TAG: &[u8] = b"tenero genesis id v3";
pub const MESSAGE_TAG: &[u8] = b"tenero fcmp++ message v3";
pub const GAMMA_LABEL: &str = "tenero gamma network 1";
pub const DEV_LABEL: &str = "tenero development network v3";
pub const TEST_LABEL: &str = "tenero test network v3";

pub fn header_hash(h: &BlockHeader) -> [u8; 32] {
    sha256(&[
        HEADER_TAG,
        &h.version.to_le_bytes(),
        &h.prev_id,
        &h.timestamp.to_le_bytes(),
        &h.tx_root,
    ])
}

/// The block id: the proof-of-work digest (as in version 2, over the version 3 header hash).
pub fn block_id(h: &BlockHeader, pow: PowKind) -> [u8; 32] {
    let hh = header_hash(h);
    match pow {
        PowKind::Matmul => matmulhash::digest_of(&matmulhash::attempt_seed(&hh, h.nonce), &h.mix),
        PowKind::Sha256 => sha256(&[&hh, &h.nonce.to_le_bytes()]),
    }
}

pub fn prunable_hash(p: &Prunable) -> Result<[u8; 32], EncodeError> {
    Ok(sha256(&[PRUNABLE_TAG, &p.to_bytes()?]))
}

pub fn tx_id(t: &Transaction) -> Result<[u8; 32], EncodeError> {
    Ok(sha256(&[
        TX_TAG,
        &t.prefix.to_bytes()?,
        &prunable_hash(&t.prunable)?,
    ]))
}

/// The same id as the full transaction's.
pub fn pruned_tx_id(p: &PrunedTransaction) -> Result<[u8; 32], EncodeError> {
    Ok(sha256(&[TX_TAG, &p.prefix.to_bytes()?, &p.prunable_hash]))
}

pub fn coinbase_id(c: &Coinbase) -> Result<[u8; 32], EncodeError> {
    Ok(sha256(&[COINBASE_TAG, &c.to_bytes()?]))
}

impl Transaction {
    pub fn prune(&self) -> Result<PrunedTransaction, EncodeError> {
        Ok(PrunedTransaction {
            prefix: self.prefix.clone(),
            prunable_hash: prunable_hash(&self.prunable)?,
        })
    }
}

/// The Merkle root of a block's transactions (version 2's RFC 6962 tree over the version 3 ids).
pub fn block_tx_root(coinbase: &Coinbase, txs: &[Transaction]) -> Result<[u8; 32], EncodeError> {
    let mut ids = Vec::with_capacity(1 + txs.len());
    ids.push(coinbase_id(coinbase)?);
    for t in txs {
        ids.push(tx_id(t)?);
    }
    Ok(crate::v2::ids::merkle_root(&ids))
}

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

/// The chain id: every proof on the chain is bound to it.
pub fn genesis_id(label: &str) -> [u8; 32] {
    let bytes = genesis_header(label)
        .to_bytes()
        .expect("a header always encodes");
    sha256(&[GENESIS_ID_TAG, &bytes])
}

/// The 32 bytes the FCMP++ spend-authorisation proofs sign (`signable_tx_hash`): the chain, the whole prefix, the
/// reference height, the pseudo-outputs and the range proof.
pub fn proof_message(
    chain_id: &[u8; 32],
    t: &Transaction,
    pseudo_outs: &[[u8; 32]],
    range_proof: &[u8],
) -> Result<[u8; 32], EncodeError> {
    let prefix = t.prefix.to_bytes()?;
    let pseudo: Vec<u8> = pseudo_outs.iter().flatten().copied().collect();
    Ok(sha256(&[
        MESSAGE_TAG,
        chain_id,
        &prefix,
        &t.prunable.reference_height.to_le_bytes(),
        &pseudo,
        range_proof,
    ]))
}
