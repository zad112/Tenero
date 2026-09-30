//! The node core: a [`Chain`] and a [`Mempool`] that are kept consistent with each other.
//!
//! **A node refuses to run without real proof checking** unless it is told, explicitly, that it is a test:
//! `Validator` with `ProofsNotChecked` accepts a forged transaction, and a node that did so silently would be
//! a node that could be paid with money that does not exist.

use tenero_chain::{BlockError, Chain, ChainParams, PowCheck, ProofCheck, Submitted, Validator};
use tenero_core::v2::ids::block_tx_root;
use tenero_core::v2::{Block, BlockHeader, Coinbase, CoinbaseOutput, Transaction, VERSION};
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

/// Where a block's reward goes: the fields of the coinbase output other than its amount.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Payout {
    pub onetime_address: [u8; 32],
    pub view_tag: [u8; 3],
    pub ephemeral_pubkey: [u8; 32],
    pub anchor_enc: [u8; 16],
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

    /// Assume-valid (see `Chain::set_assumed`): these block ids skip the full proof of work and the proofs.
    pub fn set_assumed(&mut self, ids: std::collections::HashSet<[u8; 32]>) {
        self.chain.set_assumed(ids);
    }

    /// Back to checking every block in full.
    pub fn clear_assumed(&mut self) {
        self.chain.clear_assumed();
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

    /// An unmined block on the tip: transactions from the pool (best fee rate first, within `max_body_bytes`),
    /// a coinbase paying exactly `reward - penalty + fees` to `payout`, the Merkle root, and a timestamp no
    /// earlier than the rules allow. The miner searches the nonce (and mix) and submits it.
    pub fn block_template(
        &self,
        timestamp: u64,
        max_body_bytes: u64,
        payout: Payout,
    ) -> Result<Block, BlockError> {
        let v = self.validator();
        let next = v.next_block()?;
        let txs = self.pool.select(max_body_bytes);
        let mut body = 0u64;
        let mut fees_total = 0u64;
        for t in &txs {
            body += tenero_core::v2::Wire::to_bytes(t)
                .map_err(|e| BlockError::Malformed(e.to_string()))?
                .len() as u64;
            fees_total += t.prefix.fee;
        }
        let amount = v.coinbase_amount(&next, body, fees_total)?;
        let coinbase = Coinbase {
            version: VERSION,
            height: next.height,
            outputs: vec![CoinbaseOutput {
                onetime_address: payout.onetime_address,
                amount,
                view_tag: payout.view_tag,
                ephemeral_pubkey: payout.ephemeral_pubkey,
                anchor_enc: payout.anchor_enc,
            }],
            extra: vec![],
        };
        let tx_root =
            block_tx_root(&coinbase, &txs).map_err(|e| BlockError::Malformed(e.to_string()))?;
        Ok(Block {
            header: BlockHeader {
                version: VERSION,
                prev_id: next.prev_id,
                timestamp: timestamp.max(u64::try_from(next.min_timestamp).unwrap_or(0)),
                tx_root,
                nonce: 0,
                mix: [0; 64],
            },
            coinbase,
            transactions: txs,
        })
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
