//! The node core: a [`Chain`] and a [`Mempool`] that are kept consistent with each other.
//!
//! **A node refuses to run without real proof checking** unless it is told, explicitly, that it is a test:
//! `Validator` with `ProofsNotChecked` accepts a forged transaction, and a node that did so silently would be
//! a node that could be paid with money that does not exist.

use tenero_chain::{BlockError, Chain, ChainParams, PowCheck, ProofCheck, Submitted, Validator};
use tenero_core::v2::{Block, Transaction};
use tenero_crypto::ringct::RINGCT;
use tenero_store::Store;

use crate::mempool::{AddOutcome, Mempool, MempoolConfig, PoolError};

#[derive(Debug, PartialEq, Eq)]
pub enum NodeError {
    /// The proof check does not check proofs, and the configuration did not say this is a test.
    ProofsNotChecked,
}

impl std::fmt::Display for NodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeError::ProofsNotChecked => write!(
                f,
                "refusing to run: signatures and range proofs would not be checked"
            ),
        }
    }
}

impl std::error::Error for NodeError {}

#[derive(Clone, Debug, Default)]
pub struct NodeConfig {
    pub pool: MempoolConfig,
    /// Lets the node run with a proof check that verifies nothing. **For tests only**: such a node accepts
    /// forged transactions.
    pub allow_unchecked_proofs_for_tests: bool,
}

pub struct Node<'a> {
    store: &'a Store,
    params: &'a ChainParams,
    pow: &'a dyn PowCheck,
    proofs: &'a dyn ProofCheck,
    chain: Chain<'a>,
    pool: Mempool,
}

impl<'a> Node<'a> {
    /// A node with the real proof check ([`RINGCT`]).
    pub fn new(
        store: &'a Store,
        params: &'a ChainParams,
        pow: &'a dyn PowCheck,
        cfg: NodeConfig,
    ) -> Result<Node<'a>, NodeError> {
        Node::with_proof_check(store, params, pow, &RINGCT, cfg)
    }

    /// A node with a chosen proof check. Refused if it does not check proofs, unless the configuration
    /// allows that for tests.
    pub fn with_proof_check(
        store: &'a Store,
        params: &'a ChainParams,
        pow: &'a dyn PowCheck,
        proofs: &'a dyn ProofCheck,
        cfg: NodeConfig,
    ) -> Result<Node<'a>, NodeError> {
        if !proofs.checks_proofs() && !cfg.allow_unchecked_proofs_for_tests {
            return Err(NodeError::ProofsNotChecked);
        }
        Ok(Node {
            store,
            params,
            pow,
            proofs,
            chain: Chain::new(store, params, pow, proofs),
            pool: Mempool::new(cfg.pool),
        })
    }

    fn validator(&self) -> Validator<'a> {
        Validator::new(self.store, self.params, self.pow, self.proofs)
    }

    pub fn chain(&self) -> &Chain<'a> {
        &self.chain
    }

    pub fn pool(&self) -> &Mempool {
        &self.pool
    }

    pub fn store(&self) -> &'a Store {
        self.store
    }

    /// Takes a block from anywhere and keeps the mempool consistent with what became of it.
    pub fn submit_block(&mut self, block: &Block, now: u64) -> Result<Submitted, BlockError> {
        let result = self.chain.submit_block(block, now)?;
        let v = self.validator();
        match &result {
            Submitted::Extended(_) => {
                self.pool.on_block_connected(&v, block);
            }
            Submitted::Reorganised { .. } => {
                if let Some(report) = self.chain.take_reorg_report() {
                    self.pool.on_reorganised(&v, &report);
                }
            }
            _ => {}
        }
        Ok(result)
    }

    /// Takes a loose transaction into the mempool if it is valid at the tip.
    pub fn submit_tx(&mut self, tx: Transaction) -> Result<AddOutcome, PoolError> {
        let v = self.validator();
        self.pool.add(&v, tx)
    }

    /// The transactions a miner should put in the next block: best fee rate first, within `max_body_bytes`.
    pub fn block_template_txs(&self, max_body_bytes: u64) -> Vec<Transaction> {
        self.pool.select(max_body_bytes)
    }

    /// What the rules require of the next block, for a miner building on the tip.
    pub fn next_block(&self) -> Result<tenero_chain::NextBlock, BlockError> {
        self.validator().next_block()
    }

    /// The tip's height and id.
    pub fn tip(&self) -> Result<(u64, [u8; 32]), BlockError> {
        let (h, i) = self.store.tip()?;
        Ok((h, i.block_id))
    }
}
