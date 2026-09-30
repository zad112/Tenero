//! The hook for the cryptographic proofs (`docs/CONSENSUS_V2.md` section 7): the CLSAG signatures that
//! prove a spend, and the range proof that proves the amounts.
//!
//! **They are not checked yet.** They need the audited libraries of milestone M7 (Carrot, `monero-oxide`).
//! Until then a chain uses [`ProofsNotChecked`], which accepts every transaction's proofs, and says so:
//! `ValidatedBlock::proofs_checked` is `false`. That is the honest state of the code, and a block accepted
//! this way must never be taken to mean that no coin was forged. The hook is here so the rest of the
//! validator is written against the right shape, and cannot be forgotten.

use tenero_core::v2::Transaction;
use tenero_store::StoredOutput;

/// Everything a proof check needs about a transaction: which chain and height it is for (the proofs are
/// bound to the chain id), and the outputs of each input's ring, as the ring members exist in the chain.
pub struct TxContext<'a> {
    pub chain_id: [u8; 32],
    pub height: u64,
    pub tx: &'a Transaction,
    /// For each input, its ring members' stored outputs, in the ring's order.
    pub ring_members: Vec<Vec<StoredOutput>>,
}

pub trait ProofCheck: Send + Sync {
    /// `Err(reason)` rejects the transaction (and so the block).
    fn check_tx(&self, ctx: &TxContext<'_>) -> Result<(), String>;

    /// Whether this checker really verifies the proofs. `false` for [`ProofsNotChecked`].
    fn checks_proofs(&self) -> bool;
}

/// Accepts every transaction's proofs without looking at them. **Not safe on a chain that matters.**
pub struct ProofsNotChecked;

impl ProofCheck for ProofsNotChecked {
    fn check_tx(&self, _ctx: &TxContext<'_>) -> Result<(), String> {
        Ok(())
    }

    fn checks_proofs(&self) -> bool {
        false
    }
}
