//! Checking a loose transaction's proofs AHEAD, off the node's loop.
//!
//! A transaction's proofs are the dearest part of checking it (about 2 s for a combine of 95 coins), and the node's loop,
//! which also serves every peer and the control interface, checked them itself: a big combine stalled it for tens of
//! seconds (seen on `gamma`, 2026-10-13). The proof check remembers its verdicts, keyed by the transaction, the chain and
//! the tree it was checked against (`tenero_crypto::FcmpProofs`), so checking a transaction here first, on whatever thread
//! received it (a peer's reader, a control connection), leaves the loop's own check a look-up.
//!
//! This needs the tree of the transaction's reference block, which lives in the store the loop owns: [`Precheck`] keeps a
//! copy of the trees a transaction may reference now (the last [`MAX_REFERENCE_AGE`] blocks), refreshed by the loop. A stale
//! copy (after a reorganisation) costs nothing but the speed-up: the verdict is keyed by the tree it was made against, so
//! the loop's check against the real tree does not find it and checks the proofs itself, as before. A transaction whose
//! reference is not known here is left to the loop. **Nothing is decided here**: whatever this finds, the loop checks the
//! transaction in full.

use std::collections::BTreeMap;
use std::sync::RwLock;

use tenero_core::v3::rules::{self, MAX_REFERENCE_AGE};
use tenero_core::v3::{Transaction, Wire};
use tenero_store::{Store, TreeState};

use crate::proofs::{ProofCheck, TxContext};

/// How many of the newest trees are read again at every refresh (a reorganisation of up to this many blocks replaces them).
const REREAD: u64 = 16;

pub struct Precheck {
    chain_id: [u8; 32],
    proofs: &'static dyn ProofCheck,
    trees: RwLock<BTreeMap<u64, TreeState>>,
    /// The next block's reward and block-weight median, the inputs of the minimum fee (`None` until the loop says): a
    /// transaction that pays less is refused by the loop before its proofs, and is not checked here either.
    fee_basis: RwLock<Option<(u64, u64)>>,
}

impl Precheck {
    pub fn new(chain_id: [u8; 32], proofs: &'static dyn ProofCheck) -> Precheck {
        Precheck {
            chain_id,
            proofs,
            trees: RwLock::new(BTreeMap::new()),
            fee_basis: RwLock::new(None),
        }
    }

    /// The next block's reward and median (`Validator::next_block`), so that a transaction under the minimum fee costs no
    /// proof check here (a peer could otherwise send them without end: a fee that is merely too low is not held against it).
    pub fn set_fee_basis(&self, reward: u64, median: u64) {
        if let Ok(mut b) = self.fee_basis.write() {
            *b = Some((reward, median));
        }
    }

    /// Brings the copy of the trees up to the store's tip: those of blocks `tip - MAX_REFERENCE_AGE` to `tip` (the newest
    /// [`REREAD`] read again, for a reorganisation). The loop calls it as blocks come.
    pub fn refresh(&self, store: &Store) -> Result<(), String> {
        let (tip, _) = store.tip().map_err(|e| e.to_string())?;
        let from = tip.saturating_sub(MAX_REFERENCE_AGE);
        let known: Vec<u64> = match self.trees.read() {
            Ok(t) => t.keys().copied().collect(),
            Err(_) => Vec::new(),
        };
        let mut fresh = Vec::new();
        for h in from..=tip {
            if known.binary_search(&h).is_err() || h + REREAD > tip {
                if let Some(t) = store.tree_state(h).map_err(|e| e.to_string())? {
                    fresh.push((h, t));
                }
            }
        }
        let mut trees = self
            .trees
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        trees.retain(|h, _| *h >= from && *h <= tip);
        trees.extend(fresh);
        Ok(())
    }

    /// How many trees are known (for a test).
    pub fn trees(&self) -> usize {
        self.trees.read().map_or(0, |t| t.len())
    }

    /// Checks `tx`'s proofs against the tree of its reference block, if that tree is known here, so that the verdict is
    /// remembered before the loop asks. The result is not used: the loop decides.
    pub fn check(&self, tx: &Transaction) {
        if !self.proofs.checks_proofs() {
            return;
        }
        let basis = self.fee_basis.read().ok().and_then(|b| *b);
        if let Some((reward, median)) = basis {
            let Ok(bytes) = tx.to_bytes() else { return };
            if rules::min_fee(bytes.len() as u64, reward, median)
                .is_ok_and(|min| tx.prefix.fee < min)
            {
                return;
            }
        }
        let r = tx.prunable.reference_height;
        let tree = match self.trees.read() {
            Ok(t) => t.get(&r).cloned(),
            Err(_) => None,
        };
        let Some(tree) = tree else {
            return;
        };
        let _ = self.proofs.check_tx(&TxContext {
            chain_id: self.chain_id,
            // (not the height of a block it will be in, which is not known here; no proof check depends on it)
            height: r + 1,
            tx,
            tree,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tenero_core::v3::{Input, Output, Prunable, TxPrefix, VERSION};

    struct Counting(AtomicUsize);

    impl ProofCheck for Counting {
        fn check_tx(&self, _ctx: &TxContext<'_>) -> Result<(), String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn checks_proofs(&self) -> bool {
            true
        }
    }

    fn tx(fee: u64) -> Transaction {
        let output = |k: u8| Output {
            onetime_address: [k; 32],
            amount_commitment: [k; 32],
            amount_enc: [k; 8],
            view_tag: [k; 3],
            anchor_enc: [k; 16],
        };
        Transaction {
            prefix: TxPrefix {
                version: VERSION,
                inputs: vec![Input { key_image: [9; 32] }],
                outputs: vec![output(1), output(2)],
                ephemeral_pubkeys: vec![[3; 32]],
                fee,
                encrypted_payment_id: [0; 8],
            },
            prunable: Prunable {
                reference_height: 7,
                proof_data: vec![0; 100],
            },
        }
    }

    /// A transaction under the minimum fee is refused by the loop before its proofs, and is not held against its sender (it
    /// may only be stale), so checking its proofs ahead would let a peer make the node verify proofs for nothing.
    #[test]
    fn a_transaction_under_the_minimum_fee_is_not_checked_ahead() {
        let proofs: &'static Counting = Box::leak(Box::new(Counting(AtomicUsize::new(0))));
        let p = Precheck::new([1; 32], proofs);
        p.trees.write().unwrap().insert(
            7,
            TreeState {
                n_leaves: 1,
                n_layers: 1,
                root: [0; 32],
            },
        );
        // no basis yet: checked
        p.check(&tx(0));
        assert_eq!(proofs.0.load(Ordering::SeqCst), 1);
        let (reward, median) = (2_000_000_000, 150_000);
        p.set_fee_basis(reward, median);
        let size = tx(0).to_bytes().unwrap().len() as u64;
        let min = rules::min_fee(size, reward, median).unwrap();
        p.check(&tx(min - 1));
        assert_eq!(
            proofs.0.load(Ordering::SeqCst),
            1,
            "under the minimum: not checked"
        );
        p.check(&tx(min));
        assert_eq!(proofs.0.load(Ordering::SeqCst), 2);
    }
}
