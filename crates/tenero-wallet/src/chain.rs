//! What the wallet needs from a node, as two small traits, so that the wallet does not care whether the node is in
//! its own process (now: [`tenero_node::Node`]) or behind a socket (M8.7).
//!
//! The wallet reads only the PRUNED form of blocks (outputs, key images and the coinbase), so it works against
//! a pruned node too.

use tenero_core::v2::{Coinbase, Transaction, TxPrefix};
use tenero_node::Node;
use tenero_store::StoredOutput;

/// What the rules require right now, as far as a wallet builds a transaction by them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rules {
    pub chain_id: [u8; 32],
    pub ring_size: usize,
    pub coinbase_maturity: u64,
    pub spend_maturity: u64,
    /// The height of the next block: the one a transaction sent now can be mined in.
    pub next_height: u64,
    /// The next block's reward before any penalty, and the size median it is judged against: the inputs of the
    /// dynamic minimum fee.
    pub reward: u64,
    pub median: u64,
}

/// A block as the wallet scans it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanBlock {
    pub height: u64,
    pub id: [u8; 32],
    /// The global index of the block's first output (its coinbase's first output); the outputs follow in order:
    /// the coinbase's, then each transaction's.
    pub first_output_index: u64,
    pub coinbase: Coinbase,
    pub txs: Vec<TxPrefix>,
}

pub trait ChainView {
    /// The tip's height and block id.
    fn tip(&self) -> Result<(u64, [u8; 32]), String>;
    /// The block at `height` on the node's chain, `None` above the tip.
    fn block(&self, height: u64) -> Result<Option<ScanBlock>, String>;
    /// The output with this global index.
    fn output(&self, global_index: u64) -> Result<Option<StoredOutput>, String>;
    /// How many outputs the chain has (their indexes are `0..count`).
    fn output_count(&self) -> Result<u64, String>;
    /// Whether this key image is in the chain (the output it belongs to has been spent).
    fn key_image_spent(&self, key_image: &[u8; 32]) -> Result<bool, String>;
    fn rules(&self) -> Result<Rules, String>;
}

/// Where a finished transaction goes.
pub trait Submitter {
    fn submit(&mut self, tx: Transaction) -> Result<(), String>;
}

impl ChainView for Node<'_> {
    fn tip(&self) -> Result<(u64, [u8; 32]), String> {
        Node::tip(self).map_err(|e| e.to_string())
    }

    fn block(&self, height: u64) -> Result<Option<ScanBlock>, String> {
        if height == 0 {
            // the genesis block has no coinbase and no transactions, and no outputs
            let index = self.store().block_index(0).map_err(|e| e.to_string())?;
            return Ok(index.map(|i| ScanBlock {
                height: 0,
                id: i.block_id,
                first_output_index: i.first_output_index,
                coinbase: Coinbase {
                    version: tenero_core::v2::VERSION,
                    height: 0,
                    outputs: vec![],
                    extra: vec![],
                },
                txs: vec![],
            }));
        }
        let stored = self.store().get_block(height).map_err(|e| e.to_string())?;
        Ok(stored.map(|b| ScanBlock {
            height,
            id: b.index.block_id,
            first_output_index: b.index.first_output_index,
            coinbase: b.coinbase,
            txs: b.transactions.into_iter().map(|t| t.tx.prefix).collect(),
        }))
    }

    fn output(&self, global_index: u64) -> Result<Option<StoredOutput>, String> {
        self.store().output(global_index).map_err(|e| e.to_string())
    }

    fn output_count(&self) -> Result<u64, String> {
        self.store().output_count().map_err(|e| e.to_string())
    }

    fn key_image_spent(&self, key_image: &[u8; 32]) -> Result<bool, String> {
        Ok(self
            .store()
            .key_image_height(key_image)
            .map_err(|e| e.to_string())?
            .is_some())
    }

    fn rules(&self) -> Result<Rules, String> {
        let next = self.next_block().map_err(|e| e.to_string())?;
        let p = self.params();
        Ok(Rules {
            chain_id: self.store().chain_id(),
            ring_size: p.ring_size,
            coinbase_maturity: p.coinbase_maturity,
            spend_maturity: p.spend_maturity,
            next_height: next.height,
            reward: next.reward,
            median: next.median,
        })
    }
}

impl Submitter for Node<'_> {
    fn submit(&mut self, tx: Transaction) -> Result<(), String> {
        self.submit_tx(tx).map(|_| ()).map_err(|e| format!("{e:?}"))
    }
}
