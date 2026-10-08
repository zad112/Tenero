//! The version 3 rules that need no cryptography (`docs/CONSENSUS_V2.md` 15.4-15.6): weight, shape, which reference
//! heights a transaction may use, and which outputs enter the curve tree when. Point validity and the proofs are in
//! `tenero-crypto`.

use super::types::{Transaction, TxPrefix};
use crate::v2::codec::{EncodeError, Wire};
use crate::v2::Coinbase;

/// A prunable byte weighs a quarter.
pub const PROOF_WEIGHT_DIVISOR: u64 = 4;
/// A transaction's reference block is at most this many blocks below the block it is in (PROPOSED).
pub const MAX_REFERENCE_AGE: u64 = 1440;
pub const COINBASE_MATURITY: u64 = 60;
pub const SPEND_MATURITY: u64 = 10;

/// `prefix bytes + ceil(prunable bytes / 4)`.
pub fn tx_weight(t: &Transaction) -> Result<u64, EncodeError> {
    let prefix = t.prefix.to_bytes()?.len() as u64;
    let prunable = t.prunable.to_bytes()?.len() as u64;
    Ok(prefix + prunable.div_ceil(PROOF_WEIGHT_DIVISOR))
}

/// Why a transaction's shape is not the one consensus allows (15.5; point validity is checked with the proofs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShapeError {
    KeyImagesNotAscending,
    OutputsNotAscending,
    ZeroEphemeralKey,
    EphemeralKeysRepeat,
}

impl ShapeError {
    /// The wording of `tests/vectors/v3_shape.json`.
    pub fn as_str(self) -> &'static str {
        match self {
            ShapeError::KeyImagesNotAscending => "key images not strictly ascending",
            ShapeError::OutputsNotAscending => "outputs not strictly ascending by one-time address",
            ShapeError::ZeroEphemeralKey => "an ephemeral key is zero",
            ShapeError::EphemeralKeysRepeat => "ephemeral keys repeat",
        }
    }
}

fn keys_ok(keys: &[[u8; 32]]) -> Result<(), ShapeError> {
    if keys.contains(&[0; 32]) {
        return Err(ShapeError::ZeroEphemeralKey);
    }
    let mut sorted = keys.to_vec();
    sorted.sort_unstable();
    if sorted.windows(2).any(|w| w[0] == w[1]) {
        return Err(ShapeError::EphemeralKeysRepeat);
    }
    Ok(())
}

pub fn shape(p: &TxPrefix) -> Result<(), ShapeError> {
    if p.inputs
        .windows(2)
        .any(|w| w[0].key_image >= w[1].key_image)
    {
        return Err(ShapeError::KeyImagesNotAscending);
    }
    if p.outputs
        .windows(2)
        .any(|w| w[0].onetime_address >= w[1].onetime_address)
    {
        return Err(ShapeError::OutputsNotAscending);
    }
    keys_ok(&p.ephemeral_pubkeys)
}

pub fn coinbase_shape(c: &Coinbase) -> Result<(), ShapeError> {
    if c.outputs
        .windows(2)
        .any(|w| w[0].onetime_address >= w[1].onetime_address)
    {
        return Err(ShapeError::OutputsNotAscending);
    }
    let keys: Vec<[u8; 32]> = c.outputs.iter().map(|o| o.ephemeral_pubkey).collect();
    keys_ok(&keys)
}

/// Whether a transaction in the block at `block_height` may reference the tree after block `reference_height`, which
/// has `tree_leaves` leaves.
pub fn reference_ok(reference_height: u64, block_height: u64, tree_leaves: u64) -> bool {
    block_height >= 1
        && reference_height < block_height
        && reference_height + MAX_REFERENCE_AGE >= block_height
        && tree_leaves > 0
}

/// The blocks whose outputs enter the tree when the block at `height` is applied: the coinbase outputs of
/// `height + 1 - 60` and the other outputs of `height + 1 - 10` (15.6). `None` where the chain is not that long yet.
pub fn entering_from(height: u64) -> (Option<u64>, Option<u64>) {
    (
        (height + 1).checked_sub(COINBASE_MATURITY),
        (height + 1).checked_sub(SPEND_MATURITY),
    )
}
