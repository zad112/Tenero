//! Validating a version 3 block that extends the chain's tip (`docs/CONSENSUS_V2.md` section 8 as changed by 15), in the
//! order the document lists the checks. Every rule has its own error, so a test can break exactly one rule and see
//! exactly that error.
//!
//! **What this does not check unless told to:** the cryptographic proofs (see [`crate::proofs`]).
//! Blocks that do not extend the tip are handled by [`crate::chain`], which uses this validator.

use crate::params::ChainParams;
use crate::pow::PowCheck;
use crate::proofs::{ProofCheck, TxContext};
use std::collections::HashSet;
use std::sync::Arc;
use tenero_core::difficulty;
use tenero_core::fees;
use tenero_core::u256::U256;
use tenero_core::v3::rules::{self, ShapeError};
use tenero_core::v3::{ids, Block, Transaction, Wire, VERSION};
use tenero_store::{BlockIndex, BlockMeta, Store, StoreError};
use tenero_tree::strict_point;

/// Why a block is invalid. A block that is merely too far ahead of the clock is not an error: see
/// [`Outcome::NotYet`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockError {
    /// The store failed (I/O, corruption), which says nothing about the block.
    Store(String),
    /// An object outside the limits of the wire format (it could not be encoded), or arithmetic that does not fit.
    Malformed(String),
    BadVersion(u16),
    BadParent,
    /// The timestamp is not later than the parent's: `earliest` is the least it may be (the parent's plus one second).
    TimestampTooEarly {
        timestamp: u64,
        earliest: i64,
    },
    /// The required target is 0 or 1, whose work cannot be represented (such a chain is refused).
    TargetUnrepresentable,
    /// The block id is not below the required target (the cheap proof-of-work check).
    PowTargetNotMet,
    /// The cheap check passed but the full proof of work does not reproduce the header's mix.
    PowInvalid(String),
    BadTxRoot,
    /// Its transactions' weight over twice the median (at most 4 MiB), or their real bytes over 12 MiB.
    BlockTooLarge {
        weight: u64,
        size: u64,
        weight_limit: u64,
    },
    /// A coinbase whose outputs are out of order or whose ephemeral keys are zero or repeat.
    CoinbaseShape(ShapeError),
    /// A coinbase output's one-time address is not a canonical, prime-order point.
    CoinbaseBadPoint {
        output: usize,
    },
    CoinbaseVersion(u16),
    CoinbaseHeight {
        expected: u64,
        got: u64,
    },
    CoinbaseAmount {
        expected: u64,
        got: u64,
    },
    TxVersion {
        tx: usize,
        version: u16,
    },
    FeeTooLow {
        tx: usize,
        fee: u64,
        min: u64,
    },
    /// Key images or outputs out of order, or a zero or repeated ephemeral key.
    TxShape {
        tx: usize,
        error: ShapeError,
    },
    /// An output's one-time address or commitment is not a canonical, prime-order point.
    BadOutputPoint {
        tx: usize,
        output: usize,
    },
    /// A key image is not a canonical, prime-order point.
    BadKeyImage {
        tx: usize,
        input: usize,
    },
    /// The reference block is not one the transaction may use: at most 1,440 blocks below the block, below it, with a
    /// non-empty tree.
    BadReference {
        tx: usize,
        reference_height: u64,
    },
    KeyImageSpent {
        tx: usize,
        key_image: [u8; 32],
    },
    KeyImageRepeated {
        tx: usize,
        key_image: [u8; 32],
    },
    ProofRejected {
        tx: usize,
        reason: String,
    },
    /// This block, or an ancestor of it, was found invalid before (see [`crate::chain`]).
    KnownInvalid,
    /// Switching to this branch would have to undo blocks whose proofs a pruned node no longer has, so
    /// they could not be put back if the branch turned out to be invalid.
    ReorgTooDeep {
        fork_height: u64,
        pruned_below: u64,
    },
    /// A block on a side branch turned out invalid when the branch was about to become the chain. The
    /// chain is unchanged; the block and its descendants are remembered as invalid.
    BranchInvalid {
        block_id: [u8; 32],
        reason: String,
    },
}

impl std::fmt::Display for BlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for BlockError {}

impl From<StoreError> for BlockError {
    fn from(e: StoreError) -> BlockError {
        BlockError::Store(e.to_string())
    }
}

/// What the rules say about the next block, worked out from the chain so far.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NextBlock {
    pub height: u64,
    pub prev_id: [u8; 32],
    /// The block id must be strictly below this.
    pub target: U256,
    /// The earliest timestamp the block may carry (0 when the timestamp rules are off).
    pub min_timestamp: i64,
    /// The block's reward before any penalty.
    pub reward: u64,
    /// The block-weight median the block is judged against.
    pub median: u64,
    /// The chain's total work up to the tip.
    pub cumulative_work: U256,
}

/// What [`Validator::check_pool_tx`] learned about a transaction that passed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolTx {
    /// Its size on the wire, the number the fee rules use.
    pub size: u64,
    /// Its weight (prefix bytes plus a quarter of the proof bytes), the number the block limit uses.
    pub weight: u64,
    pub fee: u64,
    /// The height of the block it was checked for (the tip's height plus one).
    pub next_height: u64,
    /// The block-weight median at that height: a block may weigh at most twice this.
    pub median: u64,
}

/// A block that passed every check the validator makes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedBlock {
    pub height: u64,
    pub block_id: [u8; 32],
    /// What the store must record with it.
    pub meta: BlockMeta,
    /// `false` when the cryptographic proofs were not checked (`ProofsNotChecked`, or a block assumed valid): the block is
    /// then valid **except** that no membership, spend or range proof was verified.
    pub proofs_checked: bool,
}

/// The result of validating a block that is not invalid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Valid(ValidatedBlock),
    /// More than `future_limit_seconds` ahead of the clock: hold it and look again later. Never a permanent
    /// rejection, so two honest nodes with slightly different clocks end up agreeing.
    NotYet,
}

/// The result of `accept_block`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Accepted {
    Added(ValidatedBlock),
    NotYet,
}

pub struct Validator<'a> {
    store: &'a Store,
    params: &'a ChainParams,
    pow: &'a dyn PowCheck,
    proofs: &'a dyn ProofCheck,
    /// Ids of blocks known, by other means, to lie on the chain up to a trusted checkpoint (**assume-valid**).
    /// For such a block the full proof of work and the transaction proofs are not checked; everything else is.
    assumed: Option<Arc<HashSet<[u8; 32]>>>,
}

impl<'a> Validator<'a> {
    pub fn new(
        store: &'a Store,
        params: &'a ChainParams,
        pow: &'a dyn PowCheck,
        proofs: &'a dyn ProofCheck,
    ) -> Validator<'a> {
        Validator {
            store,
            params,
            pow,
            proofs,
            assumed: None,
        }
    }

    /// Skips the full proof of work and the transaction proofs for the blocks whose ids are in `set`. The caller
    /// must have shown that they are ancestors of a checkpoint it trusts (`docs/M8_PLAN.md`, M8.3): this is a
    /// **trust decision**, and a block in the set is still held to every other rule (the cheap proof-of-work
    /// check, the Merkle root, the coinbase, every point, key images, fees, reference heights).
    pub fn with_assumed(mut self, set: Option<Arc<HashSet<[u8; 32]>>>) -> Validator<'a> {
        self.assumed = set;
        self
    }

    /// What the rules require of the next block: its height, parent, target, earliest timestamp, reward and
    /// the block-weight median. A miner builds a block from this; the validator checks a block against it.
    pub fn lookback(&self) -> usize {
        let window = usize::try_from(self.params.difficulty.window).unwrap_or(usize::MAX - 1);
        window.saturating_add(1).max(fees::MEDIAN_WINDOW)
    }

    /// The next block on top of the current tip.
    pub fn next_block(&self) -> Result<NextBlock, BlockError> {
        let (tip_height, _) = self.store.tip()?;
        let height = tip_height + 1;
        let take = usize::try_from(height)
            .map_err(|_| BlockError::Malformed("height".into()))?
            .min(self.lookback());
        let mut recent: Vec<BlockIndex> = Vec::with_capacity(take);
        for h in (height - take as u64)..height {
            recent.push(
                self.store
                    .block_index(h)?
                    .ok_or_else(|| BlockError::Store(format!("block {h} is missing")))?,
            );
        }
        self.next_block_from(height, &recent)
    }

    /// The next block at `height` on ANY branch: `recent` are that branch's last `min(height, lookback())`
    /// blocks, oldest first, the last one being the parent. This is what a side branch is judged by: the
    /// rules look only at these blocks, never at the store's tip.
    pub fn next_block_from(
        &self,
        height: u64,
        recent: &[BlockIndex],
    ) -> Result<NextBlock, BlockError> {
        let tip = recent
            .last()
            .ok_or_else(|| BlockError::Malformed("a block needs a parent".into()))?;
        let pos = usize::try_from(height).map_err(|_| BlockError::Malformed("height".into()))?;
        let first = height - recent.len() as u64;
        let ts_of = |r: &BlockIndex| i64::try_from(r.header.timestamp).unwrap_or(i64::MAX);
        let ts: Vec<i64> = recent.iter().map(ts_of).collect();
        let targets: Vec<U256> = recent
            .iter()
            .enumerate()
            .map(|(i, r)| {
                if first + i as u64 == 0 {
                    self.params.difficulty.start_target // the genesis block has no target of its own
                } else {
                    U256::from_be_bytes(&r.target)
                }
            })
            .collect();
        let target = difficulty::retarget_recent(&self.params.difficulty, &ts, &targets, pos)
            .map_err(BlockError::Malformed)?;

        // positions from 1 on: the genesis block does not count for the block-weight median
        let non_genesis = usize::from(first == 0);
        // a block's timestamp must be later than its parent's (the genesis block's is 0)
        let min_timestamp = if self.params.difficulty.window > 0 {
            difficulty::earliest_time_after(*ts.last().expect("a parent"))
        } else {
            0
        };
        let weights: Vec<u64> = recent[non_genesis..]
            .iter()
            .map(|r| r.body_weight)
            .collect();
        let last = &weights[weights.len().saturating_sub(fees::MEDIAN_WINDOW)..];
        let median = fees::median(last, self.params.min_block_median);

        Ok(NextBlock {
            height,
            prev_id: tip.block_id,
            target,
            min_timestamp,
            reward: self.params.emission.reward_at(height),
            median,
            cumulative_work: U256::from_be_bytes(&tip.cumulative_work),
        })
    }

    /// What the coinbase must pay in total: `reward - penalty(body_weight) + fees`, exactly.
    pub fn coinbase_amount(
        &self,
        next: &NextBlock,
        body_weight: u64,
        fees_total: u64,
    ) -> Result<u64, BlockError> {
        let penalty =
            fees::penalty(next.reward, body_weight, next.median).map_err(BlockError::Malformed)?;
        next.reward
            .checked_sub(penalty)
            .and_then(|r| r.checked_add(fees_total))
            .ok_or_else(|| BlockError::Malformed("the coinbase amount does not fit".into()))
    }

    /// Checks `block` against the rules, as a block on top of the current tip. `now` is the node's clock in
    /// seconds.
    pub fn validate_block(&self, block: &Block, now: u64) -> Result<Outcome, BlockError> {
        let next = self.next_block()?;
        self.validate_block_on(block, &next, now, true)
    }

    /// The same checks against an explicit [`NextBlock`] (any branch). With `check_state` false, the parts of step 6
    /// that need the state at the parent are skipped (key images not yet spent, the reference block's tree, the proofs):
    /// they can be checked only when the branch becomes the chain. Everything else is checked.
    pub fn validate_block_on(
        &self,
        block: &Block,
        next: &NextBlock,
        now: u64,
        check_state: bool,
    ) -> Result<Outcome, BlockError> {
        let header = &block.header;

        // 1. the header: version, parent, time
        if header.version != VERSION {
            return Err(BlockError::BadVersion(header.version));
        }
        if header.prev_id != next.prev_id {
            return Err(BlockError::BadParent);
        }
        if self.params.difficulty.window > 0
            && i128::from(header.timestamp) < i128::from(next.min_timestamp)
        {
            return Err(BlockError::TimestampTooEarly {
                timestamp: header.timestamp,
                earliest: next.min_timestamp,
            });
        }
        if header.timestamp > now.saturating_add(self.params.future_limit_seconds) {
            return Ok(Outcome::NotYet);
        }

        // 2. the target, and the cheap proof-of-work check
        let work = U256::work_of_target(&next.target).ok_or(BlockError::TargetUnrepresentable)?;
        let cumulative_work = next
            .cumulative_work
            .checked_add(&work)
            .ok_or_else(|| BlockError::Malformed("cumulative work overflows".into()))?;
        if !self.pow.check_cheap(header, &next.target) {
            return Err(BlockError::PowTargetNotMet);
        }
        // 3. the full proof of work (not for a block assumed valid)
        let block_id = ids::block_id(header, self.pow.kind());
        let assumed = self.assumed.as_ref().is_some_and(|s| s.contains(&block_id));
        if !assumed {
            match self.pow.check_full(header, next.height) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(BlockError::PowInvalid(
                        "the mix is not what the proof of work gives".into(),
                    ))
                }
                Err(e) => return Err(BlockError::PowInvalid(e)),
            }
        }

        // 4. the body: it encodes (so every count and length is within its limit), the Merkle root, the weight and size
        let malformed = |e: tenero_core::v3::EncodeError| BlockError::Malformed(e.to_string());
        if ids::block_tx_root(&block.coinbase, &block.transactions).map_err(malformed)?
            != header.tx_root
        {
            return Err(BlockError::BadTxRoot);
        }
        let mut sizes = Vec::with_capacity(block.transactions.len());
        let (mut body_weight, mut body_size) = (0u64, 0u64);
        for t in &block.transactions {
            let size = t.to_bytes().map_err(malformed)?.len() as u64;
            let weight = rules::tx_weight(t).map_err(malformed)?;
            // (each is at most MAX_TX_SIZE and there are at most MAX_BLOCK_TXS: no overflow)
            body_size += size;
            body_weight += weight;
            sizes.push(size);
        }
        if rules::block_too_large(body_weight, body_size, next.median) {
            return Err(BlockError::BlockTooLarge {
                weight: body_weight,
                size: body_size,
                weight_limit: fees::v2_block_limit(next.median),
            });
        }

        // 5. the coinbase: version, height, shape, points, and exactly reward - penalty + fees
        if block.coinbase.version != VERSION {
            return Err(BlockError::CoinbaseVersion(block.coinbase.version));
        }
        if block.coinbase.height != next.height {
            return Err(BlockError::CoinbaseHeight {
                expected: next.height,
                got: block.coinbase.height,
            });
        }
        rules::coinbase_shape(&block.coinbase).map_err(BlockError::CoinbaseShape)?;
        for (j, o) in block.coinbase.outputs.iter().enumerate() {
            if strict_point(&o.onetime_address).is_none() {
                return Err(BlockError::CoinbaseBadPoint { output: j });
            }
        }
        let fees_total = block
            .transactions
            .iter()
            .try_fold(0u64, |a, t| a.checked_add(t.prefix.fee))
            .ok_or_else(|| BlockError::Malformed("the fees overflow".into()))?;
        let expected = self.coinbase_amount(next, body_weight, fees_total)?;
        let paid = block
            .coinbase
            .outputs
            .iter()
            .try_fold(0u64, |a, o| a.checked_add(o.amount))
            .ok_or_else(|| BlockError::Malformed("the coinbase amounts overflow".into()))?;
        if paid != expected {
            return Err(BlockError::CoinbaseAmount {
                expected,
                got: paid,
            });
        }

        // 6. every other transaction, in order: what needs no state always, the rest (and the proofs, as one batch) when
        // the block is on the chain's state
        let mut spent_in_block: HashSet<[u8; 32]> = HashSet::new();
        let mut contexts = Vec::with_capacity(block.transactions.len());
        for (i, t) in block.transactions.iter().enumerate() {
            self.check_stateless(i, t, sizes[i], next, &mut spent_in_block)?;
            if check_state {
                contexts.push(self.check_state(i, t, next)?);
            }
        }
        if check_state && !assumed {
            self.proofs
                .check_block(&contexts)
                .map_err(|(tx, reason)| BlockError::ProofRejected { tx, reason })?;
        }

        Ok(Outcome::Valid(ValidatedBlock {
            height: next.height,
            block_id,
            meta: BlockMeta {
                cumulative_work: cumulative_work.to_be_bytes(),
                target: next.target.to_be_bytes(),
                body_weight,
            },
            proofs_checked: self.proofs.checks_proofs() && !assumed && check_state,
        }))
    }

    /// Checks one loose transaction against the tip's state, as if it were in the next block: everything the block
    /// validator checks of a transaction, and its proofs on their own. This is what a mempool needs.
    pub fn check_pool_tx(&self, t: &Transaction) -> Result<PoolTx, BlockError> {
        let next = self.next_block()?;
        let malformed = |e: tenero_core::v3::EncodeError| BlockError::Malformed(e.to_string());
        let size = t.to_bytes().map_err(malformed)?.len() as u64;
        let weight = rules::tx_weight(t).map_err(malformed)?;
        self.check_stateless(0, t, size, &next, &mut HashSet::new())?;
        let ctx = self.check_state(0, t, &next)?;
        self.proofs
            .check_tx(&ctx)
            .map_err(|reason| BlockError::ProofRejected { tx: 0, reason })?;
        Ok(PoolTx {
            size,
            weight,
            fee: t.prefix.fee,
            next_height: next.height,
            median: next.median,
        })
    }

    /// What a transaction must satisfy whatever the chain's state: its version, its shape, its points, the minimum fee
    /// of its real size, and no key image twice in the block.
    fn check_stateless(
        &self,
        i: usize,
        t: &Transaction,
        size: u64,
        next: &NextBlock,
        spent_in_block: &mut HashSet<[u8; 32]>,
    ) -> Result<(), BlockError> {
        if t.prefix.version != VERSION {
            return Err(BlockError::TxVersion {
                tx: i,
                version: t.prefix.version,
            });
        }
        rules::shape(&t.prefix).map_err(|error| BlockError::TxShape { tx: i, error })?;
        for (j, o) in t.prefix.outputs.iter().enumerate() {
            if strict_point(&o.onetime_address).is_none()
                || strict_point(&o.amount_commitment).is_none()
            {
                return Err(BlockError::BadOutputPoint { tx: i, output: j });
            }
        }
        for (j, input) in t.prefix.inputs.iter().enumerate() {
            if strict_point(&input.key_image).is_none() {
                return Err(BlockError::BadKeyImage { tx: i, input: j });
            }
        }
        let min = rules::min_fee(size, next.reward, next.median).map_err(BlockError::Malformed)?;
        if t.prefix.fee < min {
            return Err(BlockError::FeeTooLow {
                tx: i,
                fee: t.prefix.fee,
                min,
            });
        }
        for input in &t.prefix.inputs {
            if !spent_in_block.insert(input.key_image) {
                return Err(BlockError::KeyImageRepeated {
                    tx: i,
                    key_image: input.key_image,
                });
            }
        }
        Ok(())
    }

    /// What needs the state at the parent: no key image already spent, and a reference block the transaction may use
    /// (whose tree the proofs are then checked against).
    fn check_state<'t>(
        &self,
        i: usize,
        t: &'t Transaction,
        next: &NextBlock,
    ) -> Result<TxContext<'t>, BlockError> {
        for input in &t.prefix.inputs {
            if self.store.key_image_height(&input.key_image)?.is_some() {
                return Err(BlockError::KeyImageSpent {
                    tx: i,
                    key_image: input.key_image,
                });
            }
        }
        let r = t.prunable.reference_height;
        let bad = BlockError::BadReference {
            tx: i,
            reference_height: r,
        };
        if r >= next.height {
            return Err(bad);
        }
        let tree = self
            .store
            .tree_state(r)?
            .ok_or_else(|| BlockError::Store(format!("the tree after block {r} is missing")))?;
        if !rules::reference_ok(r, next.height, tree.n_leaves) {
            return Err(bad);
        }
        Ok(TxContext {
            chain_id: self.store.chain_id(),
            height: next.height,
            tx: t,
            tree,
        })
    }

    /// Validates `block` and, if it is valid, adds it to the store. A block that is not yet acceptable is
    /// neither added nor rejected.
    pub fn accept_block(&self, block: &Block, now: u64) -> Result<Accepted, BlockError> {
        match self.validate_block(block, now)? {
            Outcome::NotYet => Ok(Accepted::NotYet),
            Outcome::Valid(v) => {
                self.store.append_block(block, v.meta)?;
                Ok(Accepted::Added(v))
            }
        }
    }
}
