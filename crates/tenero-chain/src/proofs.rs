//! The hook for the cryptographic proofs of a version 3 transaction (`docs/CONSENSUS_V2.md` 15.7): the pseudo-outputs
//! and their balance, the Bulletproofs+ range proof, and the FCMP++ proof that each input is an output in the curve tree
//! of the transaction's reference block, with its spend authorised and its key image right.
//!
//! The chain does not know how to check them: `tenero_crypto::fcmp::FcmpProofs` does (`Node::new` uses it). A bare
//! [`crate::Validator`] can run with [`ProofsNotChecked`], which accepts every transaction's proofs and says so:
//! `ValidatedBlock::proofs_checked` is then `false`, and a block accepted this way must never be taken to mean that no
//! coin was forged.

use tenero_core::v3::Transaction;
use tenero_store::TreeState;

/// Everything a proof check needs about a transaction: which chain it is for (the proofs are bound to the chain id), the
/// height of the block it is (to be) in, and the curve tree after its reference block (the root and the layer count the
/// membership proof was made against).
pub struct TxContext<'a> {
    pub chain_id: [u8; 32],
    pub height: u64,
    pub tx: &'a Transaction,
    pub tree: TreeState,
}

pub trait ProofCheck: Send + Sync {
    /// `Err(reason)` rejects the transaction (and so the block).
    fn check_tx(&self, ctx: &TxContext<'_>) -> Result<(), String>;

    /// A block's transactions at once, which may be much cheaper than one by one (FCMP++ proofs verify in a batch).
    /// `Err((i, reason))` names a transaction that fails. The default checks them one by one.
    fn check_block(&self, txs: &[TxContext<'_>]) -> Result<(), (usize, String)> {
        for (i, ctx) in txs.iter().enumerate() {
            self.check_tx(ctx).map_err(|e| (i, e))?;
        }
        Ok(())
    }

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
