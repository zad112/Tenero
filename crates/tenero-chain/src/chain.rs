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
//! * its parent is unknown: [`Submitted::Orphan`]. It cannot be checked yet (its target needs its ancestors),
//!   so it is held, **unvalidated**, in a small bounded pool (oldest dropped first) and handed back by
//!   [`Chain::take_orphans_of`] when its parent arrives; the network layer also asks for the missing ancestors;
//! * when a side branch's work exceeds the chain's, the chain is rolled back to the fork point and the
//!   branch is validated and appended block by block. If any block fails, the chain is restored exactly as
//!   it was, and that block and its descendants are remembered as invalid.
//!
//! **Limits, stated plainly:**
//! * A reorganisation is a series of store commits, not one transaction. A crash in the middle leaves the
//!   chain at a valid earlier state (the fork point plus a prefix of one branch or the other), never a
//!   broken one; it might then have less work than before and would be corrected by the next blocks.
//! * The side-branch and orphan pools are in memory, but can be saved and loaded ([`Chain::export_pool`],
//!   `Node::save_pool`, `Node::load_pool`): a loaded block goes through `submit_block` again, so nothing in the
//!   file is trusted. A node that crashes loses whatever it had not saved. An orphan costs its sender nothing to
//!   make (it cannot be checked), so a flood of them can push out the honest ones; that costs only a re-download.
//! * A reorganisation deeper than the pruned part is refused ([`BlockError::ReorgTooDeep`]): the blocks to
//!   undo must be restorable, and a pruned block has lost its proofs. There is no other depth limit.
//! * The pool is bounded (oldest block dropped first), so a very long side branch can lose its early
//!   blocks and become impossible to adopt until those are sent again.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use crate::params::ChainParams;
use crate::pow::PowCheck;
use crate::proofs::ProofCheck;
use crate::validate::{BlockError, Outcome, ValidatedBlock, Validator};
use tenero_core::hash::sha256;
use tenero_core::u256::U256;
use tenero_core::v2::ids;
use tenero_core::v2::{Block, Wire};
use tenero_store::{BlockIndex, BlockMeta, Store};

/// How many side-branch blocks are kept unless told otherwise.
pub const DEFAULT_MAX_SIDE_BLOCKS: usize = 512;
/// How many orphan blocks are held, and how many bytes of them, unless told otherwise.
pub const DEFAULT_MAX_ORPHANS: usize = 128;
pub const DEFAULT_MAX_ORPHAN_BYTES: usize = 32 * 1024 * 1024;
/// The most blocks a pool file may hold, and its largest size: a file is untrusted input.
const MAX_POOL_FILE_BLOCKS: usize = 4096;
const MAX_POOL_FILE_BYTES: usize = 512 * 1024 * 1024;
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
    assumed: Option<Arc<HashSet<[u8; 32]>>>,
    /// Blocks whose parent is unknown, in the order they came, each with its size in bytes.
    orphans: HashMap<[u8; 32], (Block, usize)>,
    orphan_order: VecDeque<[u8; 32]>,
    orphan_bytes: usize,
    max_orphans: usize,
    max_orphan_bytes: usize,
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
            assumed: None,
            orphans: HashMap::new(),
            orphan_order: VecDeque::new(),
            orphan_bytes: 0,
            max_orphans: DEFAULT_MAX_ORPHANS,
            max_orphan_bytes: DEFAULT_MAX_ORPHAN_BYTES,
        }
    }

    /// How many orphan blocks, and how many bytes of them, to hold.
    pub fn with_max_orphans(mut self, count: usize, bytes: usize) -> Chain<'a> {
        self.max_orphans = count;
        self.max_orphan_bytes = bytes;
        self
    }

    /// How many orphan blocks are waiting for their parent.
    pub fn orphan_count(&self) -> usize {
        self.orphans.len()
    }

    pub fn is_orphan(&self, block_id: &[u8; 32]) -> bool {
        self.orphans.contains_key(block_id)
    }

    /// Held in the side pool or as an orphan: there is no need to fetch it again.
    pub fn holds_block(&self, block_id: &[u8; 32]) -> bool {
        self.side.contains_key(block_id) || self.orphans.contains_key(block_id)
    }

    /// Removes and returns the orphans whose parent is `parent`, oldest first. The caller submits them (through
    /// the node, so the mempool follows) once `parent` is known.
    pub fn take_orphans_of(&mut self, parent: &[u8; 32]) -> Vec<Block> {
        let ids: Vec<[u8; 32]> = self
            .orphan_order
            .iter()
            .filter(|id| {
                self.orphans
                    .get(*id)
                    .is_some_and(|(b, _)| b.header.prev_id == *parent)
            })
            .copied()
            .collect();
        ids.into_iter()
            .filter_map(|id| self.remove_orphan(&id))
            .collect()
    }

    fn remove_orphan(&mut self, id: &[u8; 32]) -> Option<Block> {
        let (block, size) = self.orphans.remove(id)?;
        self.orphan_order.retain(|x| x != id);
        self.orphan_bytes -= size;
        Some(block)
    }

    fn insert_orphan(&mut self, id: [u8; 32], block: &Block) {
        let Ok(bytes) = block.to_bytes() else {
            return;
        };
        let size = bytes.len();
        // (a block bigger than the whole allowance is not held, rather than held and then flushing the others out)
        if size > self.max_orphan_bytes {
            return;
        }
        self.orphans.insert(id, (block.clone(), size));
        self.orphan_order.push_back(id);
        self.orphan_bytes += size;
        while self.orphans.len() > self.max_orphans || self.orphan_bytes > self.max_orphan_bytes {
            match self.orphan_order.front().copied() {
                Some(oldest) => {
                    self.remove_orphan(&oldest);
                }
                None => break,
            }
        }
    }

    /// The blocks held in the pools, for saving: the side pool, then the orphans, each oldest first. The file is
    /// `"TPL1" | count u32 | (length u32 | block) ... | checksum 4`, the checksum being the first 4 bytes of the
    /// SHA-256 of everything before it: it detects a damaged file, it is not a defence against a forged one (a
    /// loaded block is validated like any other).
    pub fn export_pool(&self) -> Vec<u8> {
        let mut blocks: Vec<&Block> = Vec::new();
        for id in &self.order {
            if let Some(e) = self.side.get(id) {
                blocks.push(&e.block);
            }
        }
        for id in &self.orphan_order {
            if let Some((b, _)) = self.orphans.get(id) {
                blocks.push(b);
            }
        }
        let mut out = b"TPL1".to_vec();
        out.extend_from_slice(&(blocks.len() as u32).to_le_bytes());
        for b in blocks {
            let bytes = b.to_bytes().expect("a block held in a pool encodes");
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(&bytes);
        }
        let sum = sha256(&[&out]);
        out.extend_from_slice(&sum[..4]);
        out
    }

    /// The blocks of a pool file, or why it is refused (damaged, or not a pool file). Nothing is trusted: the
    /// blocks are only decoded; submitting them is the caller's job.
    pub fn decode_pool(bytes: &[u8]) -> Result<Vec<Block>, String> {
        if bytes.len() > MAX_POOL_FILE_BYTES {
            return Err("the pool file is too large".into());
        }
        if bytes.len() < 12 {
            return Err("the pool file is too short".into());
        }
        let (body, sum) = bytes.split_at(bytes.len() - 4);
        if sha256(&[body])[..4] != *sum {
            return Err("the pool file is damaged (checksum)".into());
        }
        if &body[..4] != b"TPL1" {
            return Err("not a pool file".into());
        }
        let count = u32::from_le_bytes([body[4], body[5], body[6], body[7]]) as usize;
        if count > MAX_POOL_FILE_BLOCKS {
            return Err("the pool file claims too many blocks".into());
        }
        let mut at = 8;
        let mut blocks = Vec::new();
        for _ in 0..count {
            let Some(len_bytes) = body.get(at..at + 4) else {
                return Err("the pool file is cut short".into());
            };
            let len = u32::from_le_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]])
                as usize;
            at += 4;
            let Some(data) = body.get(at..at.saturating_add(len)) else {
                return Err("the pool file is cut short".into());
            };
            at += len;
            blocks.push(
                Block::from_bytes(data).map_err(|e| format!("a block in the pool file: {e}"))?,
            );
        }
        if at != body.len() {
            return Err("bytes after the last block of the pool file".into());
        }
        Ok(blocks)
    }

    /// Assume-valid: blocks with these ids skip the full proof of work and the transaction proofs (see
    /// [`Validator::with_assumed`]). The caller vouches that they are ancestors of a trusted checkpoint.
    pub fn set_assumed(&mut self, ids: HashSet<[u8; 32]>) {
        self.assumed = Some(Arc::new(ids));
    }

    /// Back to checking everything.
    pub fn clear_assumed(&mut self) {
        self.assumed = None;
    }

    /// How many block ids are currently assumed valid.
    pub fn assumed_count(&self) -> usize {
        self.assumed.as_ref().map_or(0, |s| s.len())
    }

    /// Whether `id` is assumed valid.
    pub fn is_assumed(&self, id: &[u8; 32]) -> bool {
        self.assumed.as_ref().is_some_and(|s| s.contains(id))
    }

    pub fn with_max_side_blocks(mut self, n: usize) -> Chain<'a> {
        self.max_side_blocks = n;
        self
    }

    fn validator(&self) -> Validator<'a> {
        Validator::new(self.store, self.params, self.pow, self.proofs)
            .with_assumed(self.assumed.clone())
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
            let orphan_children: Vec<[u8; 32]> = self
                .orphans
                .iter()
                .filter(|(_, (b, _))| self.invalid.contains(&b.header.prev_id))
                .map(|(k, _)| *k)
                .collect();
            if children.is_empty() && orphan_children.is_empty() {
                break;
            }
            for c in children {
                self.invalid.insert(c);
                self.remove_side(&c);
            }
            for c in orphan_children {
                self.invalid.insert(c);
                self.remove_orphan(&c);
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
        if self.store.height_of(&id)?.is_some() || self.holds_block(&id) {
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
            self.insert_orphan(id, block);
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
