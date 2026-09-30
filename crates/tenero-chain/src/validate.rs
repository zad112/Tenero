//! Validating a block that extends the chain's tip (`docs/CONSENSUS_V2.md` section 8), in the order the
//! document lists the checks. Every rule has its own error, so a test can break exactly one rule and see
//! exactly that error.
//!
//! **What this does not check, yet:** the cryptographic proofs (see [`crate::proofs`]) and blocks that
//! do not extend the tip (side chains and reorganisations: fork choice comes next).

use crate::params::ChainParams;
use crate::pow::PowCheck;
use crate::proofs::{ProofCheck, TxContext};
use std::collections::HashSet;
use tenero_core::difficulty;
use tenero_core::fees;
use tenero_core::u256::U256;
use tenero_core::v2::ids;
use tenero_core::v2::{Block, Transaction, Wire, VERSION};
use tenero_store::{BlockIndex, BlockMeta, Store, StoreError};

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
    TimestampTooEarly {
        timestamp: u64,
        median: i64,
    },
    /// The required target is 0 or 1, whose work cannot be represented (such a chain is refused).
    TargetUnrepresentable,
    /// The block id is not below the required target (the cheap proof-of-work check).
    PowTargetNotMet,
    /// The cheap check passed but the full proof of work does not reproduce the header's mix.
    PowInvalid(String),
    BadTxRoot,
    BlockTooLarge {
        size: u64,
        limit: u64,
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
    InputsNotAscending {
        tx: usize,
    },
    KeyImageSpent {
        tx: usize,
        key_image: [u8; 32],
    },
    KeyImageRepeated {
        tx: usize,
        key_image: [u8; 32],
    },
    RingWrongSize {
        tx: usize,
        input: usize,
        size: usize,
    },
    RingNotAscending {
        tx: usize,
        input: usize,
    },
    RingMemberMissing {
        tx: usize,
        input: usize,
        member: u64,
    },
    RingMemberImmature {
        tx: usize,
        input: usize,
        member: u64,
    },
    ProofRejected {
        tx: usize,
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
    /// The block-size median the block is judged against.
    pub median: u64,
    /// The chain's total work up to the tip.
    pub cumulative_work: U256,
}

/// A block that passed every check the validator makes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedBlock {
    pub height: u64,
    pub block_id: [u8; 32],
    /// What the store must record with it.
    pub meta: BlockMeta,
    /// `false` while the cryptographic proofs are not checked (`ProofsNotChecked`): the block is then valid
    /// **except** that no signature or range proof was verified.
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
        }
    }

    /// What the rules require of the next block: its height, parent, target, earliest timestamp, reward and
    /// the block-size median. A miner builds a block from this; the validator checks a block against it.
    pub fn next_block(&self) -> Result<NextBlock, BlockError> {
        let (tip_height, tip) = self.store.tip()?;
        let height = tip_height + 1;
        let pos = usize::try_from(height).map_err(|_| BlockError::Malformed("height".into()))?;
        // the difficulty looks back `window + 1` positions, the median time 11, the median size 10
        let window = usize::try_from(self.params.difficulty.window).unwrap_or(usize::MAX - 1);
        let need = window
            .saturating_add(1)
            .max(difficulty::MEDIAN_TIME_WINDOW)
            .max(fees::MEDIAN_WINDOW);
        let take = pos.min(need);
        let mut recent: Vec<BlockIndex> = Vec::with_capacity(take);
        for h in (height - take as u64)..height {
            recent.push(
                self.store
                    .block_index(h)?
                    .ok_or_else(|| BlockError::Store(format!("block {h} is missing")))?,
            );
        }
        let first = height - take as u64;
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

        // positions from 1 on: the genesis block counts for neither the median time nor the median size
        let non_genesis = usize::from(first == 0);
        let min_timestamp = if self.params.difficulty.window > 0 {
            difficulty::median_time_recent(&ts[non_genesis..])
        } else {
            0
        };
        let sizes: Vec<u64> = recent[non_genesis..].iter().map(|r| r.body_size).collect();
        let last_sizes = &sizes[sizes.len().saturating_sub(fees::MEDIAN_WINDOW)..];
        let median = fees::median(last_sizes, self.params.min_block_median);

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

    /// What the coinbase must pay in total: `reward - penalty(body_size) + fees`, exactly.
    pub fn coinbase_amount(
        &self,
        next: &NextBlock,
        body_size: u64,
        fees_total: u64,
    ) -> Result<u64, BlockError> {
        let penalty =
            fees::penalty(next.reward, body_size, next.median).map_err(BlockError::Malformed)?;
        next.reward
            .checked_sub(penalty)
            .and_then(|r| r.checked_add(fees_total))
            .ok_or_else(|| BlockError::Malformed("the coinbase amount does not fit".into()))
    }

    /// Checks `block` against the rules, as a block on top of the current tip. `now` is the node's clock in
    /// seconds.
    pub fn validate_block(&self, block: &Block, now: u64) -> Result<Outcome, BlockError> {
        let next = self.next_block()?;
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
                median: next.min_timestamp,
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
        // 3. the full proof of work
        match self.pow.check_full(header, next.height) {
            Ok(true) => {}
            Ok(false) => {
                return Err(BlockError::PowInvalid(
                    "the mix is not what the proof of work gives".into(),
                ))
            }
            Err(e) => return Err(BlockError::PowInvalid(e)),
        }

        // 4. the body: it encodes (so every count and length is within its limit), the Merkle root, the size
        let malformed = |e: tenero_core::v2::EncodeError| BlockError::Malformed(e.to_string());
        if ids::block_tx_root(&block.coinbase, &block.transactions).map_err(malformed)?
            != header.tx_root
        {
            return Err(BlockError::BadTxRoot);
        }
        let mut sizes = Vec::with_capacity(block.transactions.len());
        let mut body_size: u64 = 0;
        for t in &block.transactions {
            let size = t.to_bytes().map_err(malformed)?.len() as u64;
            body_size = body_size
                .checked_add(size)
                .ok_or_else(|| BlockError::Malformed("the block size overflows".into()))?;
            sizes.push(size);
        }
        let limit = next.median.saturating_mul(2);
        if fees::over_hard_limit(body_size, next.median) {
            return Err(BlockError::BlockTooLarge {
                size: body_size,
                limit,
            });
        }

        // 5. the coinbase: version, height and exactly reward - penalty + fees
        if block.coinbase.version != VERSION {
            return Err(BlockError::CoinbaseVersion(block.coinbase.version));
        }
        if block.coinbase.height != next.height {
            return Err(BlockError::CoinbaseHeight {
                expected: next.height,
                got: block.coinbase.height,
            });
        }
        let fees_total = block
            .transactions
            .iter()
            .try_fold(0u64, |a, t| a.checked_add(t.prefix.fee))
            .ok_or_else(|| BlockError::Malformed("the fees overflow".into()))?;
        let expected = self.coinbase_amount(&next, body_size, fees_total)?;
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

        // 6. every other transaction, in order
        let mut spent_in_block: HashSet<[u8; 32]> = HashSet::new();
        for (i, t) in block.transactions.iter().enumerate() {
            self.check_transaction(i, t, sizes[i], &next, &mut spent_in_block)?;
        }

        Ok(Outcome::Valid(ValidatedBlock {
            height: next.height,
            block_id: ids::block_id(header, self.pow.kind()),
            meta: BlockMeta {
                cumulative_work: cumulative_work.to_be_bytes(),
                target: next.target.to_be_bytes(),
                body_size,
            },
            proofs_checked: self.proofs.checks_proofs(),
        }))
    }

    fn check_transaction(
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
        let min =
            fees::dynamic_min_fee(size, next.reward, next.median).map_err(BlockError::Malformed)?;
        if t.prefix.fee < min {
            return Err(BlockError::FeeTooLow {
                tx: i,
                fee: t.prefix.fee,
                min,
            });
        }
        // one canonical order: key images strictly ascending (which also means no two the same)
        if t.prefix
            .inputs
            .windows(2)
            .any(|w| w[0].key_image >= w[1].key_image)
        {
            return Err(BlockError::InputsNotAscending { tx: i });
        }
        if t.prunable.rings.len() != t.prefix.inputs.len() {
            return Err(BlockError::Malformed("one ring per input".into()));
        }
        let mut ring_members = Vec::with_capacity(t.prunable.rings.len());
        for (j, ring) in t.prunable.rings.iter().enumerate() {
            if ring.len() != self.params.ring_size {
                return Err(BlockError::RingWrongSize {
                    tx: i,
                    input: j,
                    size: ring.len(),
                });
            }
            if ring.windows(2).any(|w| w[0] >= w[1]) {
                return Err(BlockError::RingNotAscending { tx: i, input: j });
            }
            let mut members = Vec::with_capacity(ring.len());
            for &member in ring {
                let out = self
                    .store
                    .output(member)?
                    .ok_or(BlockError::RingMemberMissing {
                        tx: i,
                        input: j,
                        member,
                    })?;
                let wait = if out.coinbase {
                    self.params.coinbase_maturity
                } else {
                    self.params.spend_maturity
                };
                if next.height < out.height.saturating_add(wait) {
                    return Err(BlockError::RingMemberImmature {
                        tx: i,
                        input: j,
                        member,
                    });
                }
                members.push(out);
            }
            ring_members.push(members);
        }
        for input in &t.prefix.inputs {
            if self.store.key_image_height(&input.key_image)?.is_some() {
                return Err(BlockError::KeyImageSpent {
                    tx: i,
                    key_image: input.key_image,
                });
            }
            if !spent_in_block.insert(input.key_image) {
                return Err(BlockError::KeyImageRepeated {
                    tx: i,
                    key_image: input.key_image,
                });
            }
        }
        let ctx = TxContext {
            chain_id: self.store.chain_id(),
            height: next.height,
            tx: t,
            ring_members,
        };
        self.proofs
            .check_tx(&ctx)
            .map_err(|reason| BlockError::ProofRejected { tx: i, reason })
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
