//! What the wallet needs from a node, as two small traits, so that the wallet does not care whether the node is in
//! its own process ([`tenero_node::Node`]) or behind a socket.
//!
//! The wallet reads only the PRUNED form of blocks (outputs, key images and the coinbase), so it works against
//! a pruned node too. To spend, it asks for its coins' paths in the curve tree ([`ChainView::spend_paths`]): a node that
//! is asked learns which outputs are being spent, which is harmless for a node on the same machine (a remote wallet will
//! need another way, `docs/FCMP_CARROT_PLAN.md` 7).

use tenero_core::v3::{rules, Coinbase, Transaction, TxPrefix};
use tenero_node::Node;
use tenero_store::TreeState;
use tenero_tree::PathBytes;

/// What the rules require right now, as far as a wallet builds a transaction by them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rules {
    pub chain_id: [u8; 32],
    /// The height of the next block: the one a transaction sent now can be mined in.
    pub next_height: u64,
    /// The next block's reward before any penalty, and the weight median it is judged against: the inputs of the
    /// minimum fee.
    pub reward: u64,
    pub median: u64,
    /// The layers of the curve tree at the tip: the size of a membership proof made now.
    pub tree_layers: usize,
}

/// A block as the wallet scans it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanBlock {
    pub height: u64,
    pub id: [u8; 32],
    /// The global index of the block's first output (its coinbase's first output); the outputs follow in order:
    /// the coinbase's, then each transaction's.
    pub first_output_index: u64,
    /// The header's timestamp (Unix seconds): when the history says something happened.
    pub timestamp: u64,
    pub coinbase: Coinbase,
    pub txs: Vec<TxPrefix>,
}

/// Where a spend is proven: the reference block (the tip when asked), its tree, and each coin's path in that tree
/// (`None` for a coin not in it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpendPaths {
    pub reference_height: u64,
    pub tree: TreeState,
    pub paths: Vec<Option<PathBytes>>,
}

pub trait ChainView {
    /// The tip's height and block id.
    fn tip(&self) -> Result<(u64, [u8; 32]), String>;
    /// The block at `height` on the node's chain, `None` above the tip.
    fn block(&self, height: u64) -> Result<Option<ScanBlock>, String>;
    /// Up to `max` blocks from height `from` on, in order, as far as the node has them (fewer than `max` only at
    /// the tip, and possibly fewer for size). A node behind a socket answers this in one round trip; the default asks
    /// for them one by one.
    fn blocks(&self, from: u64, max: u64) -> Result<Vec<ScanBlock>, String> {
        let mut out = Vec::new();
        for h in from..from.saturating_add(max) {
            match self.block(h)? {
                Some(b) => out.push(b),
                None => break,
            }
        }
        Ok(out)
    }
    /// Whether this key image is in the chain (the output it belongs to has been spent).
    fn key_image_spent(&self, key_image: &[u8; 32]) -> Result<bool, String>;
    /// Whether each of these key images is in the chain, in order. A node behind a socket answers this in one round trip
    /// (a wallet with thousands of coins asked one by one took twenty seconds); the default asks one by one.
    fn key_images_spent(&self, key_images: &[[u8; 32]]) -> Result<Vec<bool>, String> {
        key_images.iter().map(|k| self.key_image_spent(k)).collect()
    }
    fn rules(&self) -> Result<Rules, String>;
    /// The paths of the outputs with these global indexes, all in the tree of one reference block.
    fn spend_paths(&self, global_indexes: &[u64]) -> Result<SpendPaths, String>;
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
                timestamp: i.header.timestamp,
                coinbase: Coinbase {
                    version: tenero_core::v3::VERSION,
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
            timestamp: b.index.header.timestamp,
            coinbase: b.coinbase,
            txs: b.transactions.into_iter().map(|t| t.tx.prefix).collect(),
        }))
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
        let state = self
            .store()
            .tree_state(next.height - 1)
            .map_err(|e| e.to_string())?
            .ok_or("no tree at the tip")?;
        Ok(Rules {
            chain_id: self.store().chain_id(),
            next_height: next.height,
            reward: next.reward,
            median: next.median,
            tree_layers: usize::from(state.n_layers),
        })
    }

    fn spend_paths(&self, global_indexes: &[u64]) -> Result<SpendPaths, String> {
        let (reference_height, tree, paths) = self
            .store()
            .spend_paths(global_indexes)
            .map_err(|e| e.to_string())?;
        Ok(SpendPaths {
            reference_height,
            tree,
            paths,
        })
    }
}

impl Submitter for Node<'_> {
    fn submit(&mut self, tx: Transaction) -> Result<(), String> {
        self.submit_tx(tx).map(|_| ()).map_err(|e| format!("{e:?}"))
    }
}

/// Whether an output made at `height` can be spent in the next block: in the curve tree by then (a coinbase output 60
/// blocks after its block, any other 10).
pub fn is_mature(rules: &Rules, height: u64, coinbase: bool) -> bool {
    let wait = if coinbase {
        rules::COINBASE_MATURITY
    } else {
        rules::SPEND_MATURITY
    };
    rules.next_height >= height.saturating_add(wait)
}
