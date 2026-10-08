//! The node core: a [`Chain`] and a [`Mempool`] that are kept consistent with each other.
//!
//! **A node refuses to run without real proof checking** unless it is told, explicitly, that it is a test:
//! `Validator` with `ProofsNotChecked` accepts a forged transaction, and a node that did so silently would be
//! a node that could be paid with money that does not exist.

use tenero_chain::{BlockError, Chain, ChainParams, PowCheck, ProofCheck, Submitted, Validator};
use tenero_core::v3::ids::block_tx_root;
use tenero_core::v3::rules;
use tenero_core::v3::{Block, BlockHeader, Coinbase, CoinbaseOutput, Transaction, VERSION};
use tenero_crypto::fcmp::FCMP;
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
                "refusing to run: the FCMP++ and range proofs would not be checked"
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

/// Where a block's reward goes: the fields of the coinbase output other than its amount. A Carrot coinbase output's
/// one-time address depends on its amount, so a block template asks for the payout once it knows the amount.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Payout {
    pub onetime_address: [u8; 32],
    pub view_tag: [u8; 3],
    pub ephemeral_pubkey: [u8; 32],
    pub anchor_enc: [u8; 16],
}

/// What became of the blocks of a pool file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolLoad {
    /// Kept on a side branch.
    pub side: usize,
    /// Held as orphans.
    pub orphans: usize,
    /// Extended the chain or made it reorganise (the chain had moved since the file was written).
    pub adopted: usize,
    /// Already in the chain or the pool.
    pub known: usize,
    /// Refused (invalid now) or not yet acceptable.
    pub dropped: usize,
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
    /// A node with the real proof check ([`FCMP`], which remembers the verdicts of the transactions it took into the
    /// mempool, so a block of them is checked almost at once).
    pub fn new(
        store: &'a Store,
        params: &'a ChainParams,
        pow: &'a dyn PowCheck,
        cfg: NodeConfig,
    ) -> Result<Node<'a>, NodeError> {
        Node::with_proof_check(store, params, pow, &*FCMP, cfg)
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

    /// The rules this node runs.
    pub fn params(&self) -> &'a ChainParams {
        self.params
    }

    /// The full proof-of-work check of a header at a height (`PowCheck::check_full`): is the mix the one the proof of work gives for the header's nonce? Needs the
    /// epoch's dataset, which the node already holds for checking blocks. `Err` is "could not check" (no memory for the dataset).
    pub fn check_proof_of_work(&self, header: &BlockHeader, height: u64) -> Result<bool, String> {
        self.pow.check_full(header, height)
    }

    /// Tells the proof of work that blocks about `lookahead` blocks past the tip will be checked soon, so that one
    /// that needs a dataset for a new epoch builds it in the background now (it never blocks, and does nothing when
    /// the dataset is already there or the proof of work needs none). Call it when the tip moves.
    pub fn prefetch_proof_of_work(&self, lookahead: u64) {
        if let Ok((height, _)) = self.tip() {
            self.pow.prefetch(height + 1 + lookahead);
        }
    }

    /// Assume-valid (see `Chain::set_assumed`): these block ids skip the full proof of work and the proofs.
    pub fn set_assumed(&mut self, ids: std::collections::HashSet<[u8; 32]>) {
        self.chain.set_assumed(ids);
    }

    /// Back to checking every block in full.
    pub fn clear_assumed(&mut self) {
        self.chain.clear_assumed();
    }

    /// The orphan blocks that were waiting for `parent` (see `Chain::take_orphans_of`). Submit them with
    /// [`Node::submit_block`] once `parent` is known, so the mempool follows.
    pub fn take_orphans_of(&mut self, parent: &[u8; 32]) -> Vec<Block> {
        self.chain.take_orphans_of(parent)
    }

    /// Writes the side-branch and orphan pools to `path`: to a temporary file next to it first, then renamed over
    /// it, so a crash never leaves a half-written file where a good one was.
    pub fn save_pool(&self, path: &std::path::Path) -> std::io::Result<()> {
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp);
        std::fs::write(&tmp, self.chain.export_pool())?;
        std::fs::rename(&tmp, path)
    }

    /// Reads a pool file and submits each block through [`Node::submit_block`], so nothing in the file is
    /// trusted and the mempool follows. A missing file is an empty pool; a damaged one is an error and changes
    /// nothing.
    pub fn load_pool(&mut self, path: &std::path::Path, now: u64) -> Result<PoolLoad, String> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(PoolLoad::default()),
            Err(e) => return Err(e.to_string()),
        };
        let blocks = Chain::decode_pool(&bytes)?;
        let mut report = PoolLoad::default();
        for b in &blocks {
            match self.submit_block(b, now) {
                Ok(Submitted::SideChain { .. }) => report.side += 1,
                Ok(Submitted::Orphan) => report.orphans += 1,
                Ok(Submitted::Extended(_)) | Ok(Submitted::Reorganised { .. }) => {
                    report.adopted += 1
                }
                Ok(Submitted::AlreadyKnown) => report.known += 1,
                Ok(Submitted::NotYet) | Err(_) => report.dropped += 1,
            }
        }
        Ok(report)
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
        self.submit_tx_at(tx, 0)
    }

    /// [`Node::submit_tx`], recording `now` (Unix seconds) as when the transaction arrived (a block explorer shows it).
    pub fn submit_tx_at(&mut self, tx: Transaction, now: u64) -> Result<AddOutcome, PoolError> {
        let v = self.validator();
        self.pool.add_at(&v, tx, now)
    }

    /// Whether a loose transaction would be taken now, and if not why not (nothing changes). A wallet's node link
    /// uses it to say no with a reason before the transaction is handed over.
    pub fn check_tx(&self, tx: &Transaction) -> Result<(), String> {
        let id = tenero_core::v3::ids::tx_id(tx).map_err(|e| e.to_string())?;
        if self.pool.contains(&id) {
            return Err("the node already has this transaction".into());
        }
        self.validator()
            .check_pool_tx(tx)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// The transactions a miner should put in the next block: best fee rate first, weighing at most `max_weight`.
    pub fn block_template_txs(&self, max_weight: u64) -> Vec<Transaction> {
        // never more than a block may carry, whatever the caller asks for (none if the rules cannot be read)
        let limit = self
            .validator()
            .next_block()
            .map(|n| rules::block_limit(n.median))
            .unwrap_or(0);
        self.pool.select(max_weight.min(limit))
    }

    /// An unmined block on the tip: transactions from the pool (best fee rate first, weighing at most `max_weight`), a
    /// coinbase paying exactly `reward - penalty + fees` to `payout(amount)`, the Merkle root, and a timestamp no
    /// earlier than the rules allow. The miner searches the nonce (and mix) and submits it.
    pub fn block_template(
        &self,
        timestamp: u64,
        max_weight: u64,
        payout: &dyn Fn(u64) -> Payout,
    ) -> Result<Block, BlockError> {
        let v = self.validator();
        let next = v.next_block()?;
        // never more than a block may carry, whatever the caller asks for (a block over the limit is invalid)
        let txs = self
            .pool
            .select(max_weight.min(rules::block_limit(next.median)));
        let mut weight = 0u64;
        let mut fees_total = 0u64;
        for t in &txs {
            weight += rules::tx_weight(t).map_err(|e| BlockError::Malformed(e.to_string()))?;
            fees_total += t.prefix.fee;
        }
        let amount = v.coinbase_amount(&next, weight, fees_total)?;
        let to = payout(amount);
        let coinbase = Coinbase {
            version: VERSION,
            height: next.height,
            outputs: vec![CoinbaseOutput {
                onetime_address: to.onetime_address,
                amount,
                view_tag: to.view_tag,
                ephemeral_pubkey: to.ephemeral_pubkey,
                anchor_enc: to.anchor_enc,
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
