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

use tenero_core::v3::rules::MAX_REFERENCE_AGE;
use tenero_core::v3::Transaction;
use tenero_store::{Store, TreeState};

use crate::proofs::{ProofCheck, TxContext};

/// How many of the newest trees are read again at every refresh (a reorganisation of up to this many blocks replaces them).
const REREAD: u64 = 16;

pub struct Precheck {
    chain_id: [u8; 32],
    proofs: &'static dyn ProofCheck,
    trees: RwLock<BTreeMap<u64, TreeState>>,
}

impl Precheck {
    pub fn new(chain_id: [u8; 32], proofs: &'static dyn ProofCheck) -> Precheck {
        Precheck {
            chain_id,
            proofs,
            trees: RwLock::new(BTreeMap::new()),
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
            height: r + 1,
            tx,
            tree,
        });
    }
}
