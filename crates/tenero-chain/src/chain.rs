//! Fork choice and reorganisation: taking blocks that do not necessarily extend the tip.
//!
//! **The rule:** the chain is the branch with the most cumulative work (the sum of `floor(2^256 / target)`
//! of its blocks). A branch replaces the chain only if its work is strictly greater, so on a tie the chain
//! that was seen first stays.
//!
//! **How a block is treated** ([`Chain::submit_block`]):
//! * it extends the tip: fully validated and appended;
//! * its parent is a block we know but not the tip (a side branch): it gets every check that needs only
//!   the branch's own headers (version, parent, time, target, proof of work, Merkle root, size, coinbase),
//!   using the branch's ancestors, and is kept in a bounded in-memory pool. Its transactions' fees, key
//!   images, rings and proofs need the state at its parent and are checked only if the branch is about to
//!   become the chain;
//! * its parent is unknown: [`Submitted::Orphan`], and it is not kept (the network layer asks for the
//!   missing ancestors again);
//! * when a side branch's work exceeds the chain's, the chain is rolled back to the fork point and the
//!   branch is validated and appended block by block. If any block fails, the chain is restored exactly as
//!   it was, and that block and its descendants are remembered as invalid.
//!
//! **Limits, stated plainly:**
//! * A reorganisation is a series of store commits, not one transaction. A crash in the middle leaves the
//!   chain at a valid earlier state (the fork point plus a prefix of one branch or the other), never a
//!   broken one; it might then have less work than before and would be corrected by the next blocks.
//! * The side-branch pool is in memory and is lost on restart.
//! * A reorganisation deeper than the pruned part is refused ([`BlockError::ReorgTooDeep`]): the blocks to
//!   undo must be restorable, and a pruned block has lost its proofs. There is no other depth limit.
//! * The pool is bounded (oldest block dropped first), so a very long side branch can lose its early
//!   blocks and become impossible to adopt until those are sent again.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::params::ChainParams;
use crate::pow::PowCheck;
use crate::proofs::ProofCheck;
use crate::validate::{BlockError, Outcome, ValidatedBlock, Validator};
use tenero_core::u256::U256;
use tenero_core::v2::ids;
use tenero_core::v2::Block;
use tenero_store::{BlockIndex, BlockMeta, Store};

/// How many side-branch blocks are kept unless told otherwise.
pub const DEFAULT_MAX_SIDE_BLOCKS: usize = 512;
/// How many block ids remembered as invalid before the list is cleared.
const MAX_INVALID: usize = 8192;

/// What became of a submitted block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Submitted {
    /// It was already in the chain or the side pool.
    AlreadyKnown,
    /// It extended the tip and is now the tip.
    Extended(ValidatedBlock),
    /// It is on a side branch that does not have more work than the chain; kept.
    SideChain { height: u64, cumulative_work: U256 },
    /// Its branch had more work, so the chain switched to it.
    Reorganised {
        fork_height: u64,
        /// How many blocks of the old chain were undone.
        disconnected: usize,
        /// How many blocks of the new branch were applied.
        connected: usize,
        tip_height: u64,
    },
    /// Its parent is not known. Not kept.
    Orphan,
    /// More than the future limit ahead of the clock: hold it and look again later.
    NotYet,
}

/// The blocks a reorganisation removed from the chain and the blocks it put in their place, oldest first: what
/// a mempool needs to put back the transactions that are no longer confirmed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReorgReport {
    pub disconnected: Vec<Block>,
    pub connected: Vec<Block>,
}

struct SideBlock {
    block: Block,
    /// The record the branch's later blocks are judged by (only the fields the rules read are filled).
    index: BlockIndex,
}

/// The store, the rules, and the blocks that are not (yet) on the chain.
pub struct Chain<'a> {
    store: &'a Store,
    params: &'a ChainParams,
    pow: &'a dyn PowCheck,
    proofs: &'a dyn ProofCheck,
    side: HashMap<[u8; 32], SideBlock>,
    order: VecDeque<[u8; 32]>,
    invalid: HashSet<[u8; 32]>,
    max_side_blocks: usize,
    last_reorg: Option<ReorgReport>,
}

enum Failure {
    Invalid(BlockError),
    NotYet,
}

impl<'a> Chain<'a> {
    pub fn new(
        store: &'a Store,
        params: &'a ChainParams,
        pow: &'a dyn PowCheck,
        proofs: &'a dyn ProofCheck,
    ) -> Chain<'a> {
        Chain {
            store,
            params,
            pow,
            proofs,
            side: HashMap::new(),
            order: VecDeque::new(),
            invalid: HashSet::new(),
            max_side_blocks: DEFAULT_MAX_SIDE_BLOCKS,
            last_reorg: None,
        }
    }

    pub fn with_max_side_blocks(mut self, n: usize) -> Chain<'a> {
        self.max_side_blocks = n;
        self
    }

    fn validator(&self) -> Validator<'a> {
        Validator::new(self.store, self.params, self.pow, self.proofs)
    }

    /// The report of the last reorganisation, once: a caller that keeps a mempool takes it after every
    /// `submit_block` that returned [`Submitted::Reorganised`].
    pub fn take_reorg_report(&mut self) -> Option<ReorgReport> {
        self.last_reorg.take()
    }

    /// How many blocks are waiting in the side pool.
    pub fn side_block_count(&self) -> usize {
        self.side.len()
    }

    pub fn is_known_invalid(&self, block_id: &[u8; 32]) -> bool {
        self.invalid.contains(block_id)
    }

    pub fn in_side_pool(&self, block_id: &[u8; 32]) -> bool {
        self.side.contains_key(block_id)
    }

    fn mark_invalid(&mut self, id: [u8; 32]) {
        if self.invalid.len() >= MAX_INVALID {
            self.invalid.clear();
        }
        self.invalid.insert(id);
        self.remove_side(&id);
        // and everything built on it
        loop {
            let children: Vec<[u8; 32]> = self
                .side
                .iter()
                .filter(|(_, e)| self.invalid.contains(&e.block.header.prev_id))
                .map(|(k, _)| *k)
                .collect();
            if children.is_empty() {
                break;
            }
            for c in children {
                self.invalid.insert(c);
                self.remove_side(&c);
            }
        }
    }

    fn remove_side(&mut self, id: &[u8; 32]) {
        self.side.remove(id);
        self.order.retain(|x| x != id);
    }

    fn insert_side(&mut self, id: [u8; 32], entry: SideBlock) {
        if self.side.insert(id, entry).is_none() {
            self.order.push_back(id);
        }
        while self.side.len() > self.max_side_blocks {
            match self.order.pop_front() {
                Some(old) => {
                    self.side.remove(&old);
                }
                None => break,
            }
        }
    }

    /// Takes a block from anywhere (see the module docs).
    pub fn submit_block(&mut self, block: &Block, now: u64) -> Result<Submitted, BlockError> {
        let id = ids::block_id(&block.header, self.pow.kind());
        if self.invalid.contains(&id) {
            return Err(BlockError::KnownInvalid);
        }
        if self.store.height_of(&id)?.is_some() || self.side.contains_key(&id) {
            return Ok(Submitted::AlreadyKnown);
        }
        let prev = block.header.prev_id;
        if self.invalid.contains(&prev) {
            self.mark_invalid(id);
            return Err(BlockError::KnownInvalid);
        }
        let (_, tip) = self.store.tip()?;
        if prev == tip.block_id {
            return match self.validator().accept_block(block, now) {
                Ok(crate::validate::Accepted::Added(v)) => Ok(Submitted::Extended(v)),
                Ok(crate::validate::Accepted::NotYet) => Ok(Submitted::NotYet),
                Err(e) => Err(self.note_failure(id, e)),
            };
        }

        // a side branch, or an orphan
        let Some((parent_height, recent)) = self.branch_context(&prev)? else {
            return Ok(Submitted::Orphan);
        };
        let height = parent_height + 1;
        let v = self.validator();
        let next = v.next_block_from(height, &recent)?;
        let checked = match v.validate_block_on(block, &next, now, false) {
            Ok(o) => o,
            Err(e) => return Err(self.note_failure(id, e)),
        };
        let vb = match checked {
            Outcome::NotYet => return Ok(Submitted::NotYet),
            Outcome::Valid(vb) => vb,
        };
        let work = U256::from_be_bytes(&vb.meta.cumulative_work);
        let index = BlockIndex {
            block_id: vb.block_id,
            header: block.header.clone(),
            cumulative_work: vb.meta.cumulative_work,
            target: vb.meta.target,
            body_size: vb.meta.body_size,
            first_output_index: 0,
            output_count: 0,
            tx_count: 0,
        };
        self.insert_side(
            id,
            SideBlock {
                block: block.clone(),
                index,
            },
        );
        if work > U256::from_be_bytes(&tip.cumulative_work) {
            self.reorganise_to(id, now)
        } else {
            Ok(Submitted::SideChain {
                height,
                cumulative_work: work,
            })
        }
    }

    /// A store failure says nothing about the block; anything else makes it permanently invalid.
    fn note_failure(&mut self, id: [u8; 32], e: BlockError) -> BlockError {
        if !matches!(e, BlockError::Store(_)) {
            self.mark_invalid(id);
        }
        e
    }

    /// The height of `parent` and the last blocks of its branch (`Validator::next_block_from`), or `None` if
    /// the branch does not lead back to the chain.
    fn branch_context(
        &self,
        parent: &[u8; 32],
    ) -> Result<Option<(u64, Vec<BlockIndex>)>, BlockError> {
        let mut side_rev: Vec<&SideBlock> = Vec::new();
        let mut cur = *parent;
        while let Some(e) = self.side.get(&cur) {
            side_rev.push(e);
            cur = e.block.header.prev_id;
        }
        let Some(fork_height) = self.store.height_of(&cur)? else {
            return Ok(None);
        };
        side_rev.reverse();
        let parent_height = fork_height + side_rev.len() as u64;
        let take = usize::try_from(parent_height + 1)
            .unwrap_or(usize::MAX)
            .min(self.validator().lookback()) as u64;
        let mut recent = Vec::with_capacity(take as usize);
        for h in (parent_height + 1 - take)..=parent_height {
            if h <= fork_height {
                recent.push(
                    self.store
                        .block_index(h)?
                        .ok_or_else(|| BlockError::Store(format!("block {h} is missing")))?,
                );
            } else {
                recent.push(side_rev[(h - fork_height - 1) as usize].index.clone());
            }
        }
        Ok(Some((parent_height, recent)))
    }

    /// Switches the chain to the branch ending at `target_id` (which is in the side pool).
    fn reorganise_to(&mut self, target_id: [u8; 32], now: u64) -> Result<Submitted, BlockError> {
        let mut path = Vec::new();
        let mut cur = target_id;
        while let Some(e) = self.side.get(&cur) {
            path.push(cur);
            cur = e.block.header.prev_id;
        }
        path.reverse();
        let Some(fork_height) = self.store.height_of(&cur)? else {
            // an ancestor was dropped from the pool: this branch cannot be adopted now
            return Ok(Submitted::Orphan);
        };
        let (tip_height, _) = self.store.tip()?;
        let pruned_below = self.store.pruned_below()?;
        if fork_height + 1 < pruned_below {
            return Err(BlockError::ReorgTooDeep {
                fork_height,
                pruned_below,
            });
        }

        // 1. undo the old chain down to the fork point, keeping every block so it can be put back
        let mut old: Vec<(Block, BlockMeta)> = Vec::new();
        for _ in fork_height..tip_height {
            let popped = self.store.pop_block()?;
            let meta = BlockMeta {
                cumulative_work: popped.index.cumulative_work,
                target: popped.index.target,
                body_size: popped.index.body_size,
            };
            let full = popped.into_full().ok_or_else(|| {
                BlockError::Store("an undone block has no proofs to restore it with".into())
            })?;
            old.push((full, meta));
        }
        old.reverse();

        // 2. apply the branch, validating each block against the state it now sits on
        let mut connected = 0usize;
        let mut failure: Option<([u8; 32], Failure)> = None;
        for bid in &path {
            let blk = self.side[bid].block.clone();
            match self.validator().validate_block(&blk, now) {
                Ok(Outcome::Valid(vb)) => {
                    if let Err(e) = self.store.append_block(&blk, vb.meta) {
                        failure = Some((*bid, Failure::Invalid(BlockError::from(e))));
                        break;
                    }
                    connected += 1;
                }
                Ok(Outcome::NotYet) => {
                    failure = Some((*bid, Failure::NotYet));
                    break;
                }
                Err(e) => {
                    failure = Some((*bid, Failure::Invalid(e)));
                    break;
                }
            }
        }

        // 3a. a failure: put the old chain back exactly
        if let Some((bad, why)) = failure {
            for _ in 0..connected {
                self.store.pop_block()?;
            }
            for (blk, meta) in &old {
                self.store.append_block(blk, *meta).map_err(|e| {
                    BlockError::Store(format!("could not restore the previous chain: {e}"))
                })?;
            }
            return match why {
                Failure::NotYet => Ok(Submitted::NotYet),
                Failure::Invalid(BlockError::Store(s)) => Err(BlockError::Store(s)),
                Failure::Invalid(e) => {
                    self.mark_invalid(bad);
                    Err(BlockError::BranchInvalid {
                        block_id: bad,
                        reason: format!("{e:?}"),
                    })
                }
            };
        }

        // 3b. success: the branch left the pool, the old chain went into it (so it can win back)
        let connected_blocks: Vec<Block> =
            path.iter().map(|b| self.side[b].block.clone()).collect();
        for bid in &path {
            self.remove_side(bid);
        }
        self.last_reorg = Some(ReorgReport {
            disconnected: old.iter().map(|(b, _)| b.clone()).collect(),
            connected: connected_blocks,
        });
        for (blk, meta) in &old {
            let id = ids::block_id(&blk.header, self.pow.kind());
            let index = BlockIndex {
                block_id: id,
                header: blk.header.clone(),
                cumulative_work: meta.cumulative_work,
                target: meta.target,
                body_size: meta.body_size,
                first_output_index: 0,
                output_count: 0,
                tx_count: 0,
            };
            self.insert_side(
                id,
                SideBlock {
                    block: blk.clone(),
                    index,
                },
            );
        }
        Ok(Submitted::Reorganised {
            fork_height,
            disconnected: old.len(),
            connected: path.len(),
            tip_height: fork_height + path.len() as u64,
        })
    }
}
