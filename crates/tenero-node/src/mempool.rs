//! The mempool: transactions that are valid against the chain's tip, not yet confirmed, and do not conflict.
//!
//! **What it guarantees** (and the tests check after every operation):
//! * every transaction in it passes `Validator::check_pool_tx` against the current tip, proofs included;
//! * no two share a key image (a spend can be in the pool once; there is no replace-by-fee);
//! * none is confirmed, and none spends a key image the chain has already spent;
//! * its total size is at most `max_bytes`, and when full it drops the lowest fee rate first, and only for a
//!   transaction that pays a strictly higher rate.
//!
//! **What it does not do:** it lives in memory (a restart empties it), it does not relay anything (M8), and
//! there are no chains of unconfirmed spends: an output cannot be spent before it is mature, so no pooled
//! transaction can depend on another.

use std::collections::HashMap;

use tenero_chain::{BlockError, ReorgReport, Validator};
use tenero_core::fees;
use tenero_core::v2::ids::tx_id;
use tenero_core::v2::{Block, Transaction};

pub type TxId = [u8; 32];

#[derive(Clone, Debug)]
pub struct MempoolConfig {
    /// The total size of the transactions held, in wire bytes.
    pub max_bytes: u64,
}

impl Default for MempoolConfig {
    fn default() -> MempoolConfig {
        MempoolConfig {
            max_bytes: 32 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PoolError {
    AlreadyKnown,
    /// It fails a rule against the tip's state (the error says which).
    Invalid(BlockError),
    /// Larger than any block could hold.
    TooLarge {
        size: u64,
        limit: u64,
    },
    /// Shares a key image with a transaction already in the pool.
    Conflict {
        key_image: [u8; 32],
        with: TxId,
    },
    /// The pool is full and this pays no better than what it would have to push out.
    PoolFull,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddOutcome {
    /// Added; these lower-paying transactions were pushed out to make room.
    Added { id: TxId, evicted: Vec<TxId> },
}

struct Entry {
    tx: Transaction,
    size: u64,
    fee: u64,
    /// Arrival order, to break ties between equal fee rates (older first).
    seq: u64,
}

/// `a` pays a strictly higher fee per byte than `b`.
fn better_rate(a: (u64, u64), b: (u64, u64)) -> bool {
    u128::from(a.0) * u128::from(b.1) > u128::from(b.0) * u128::from(a.1)
}

pub struct Mempool {
    cfg: MempoolConfig,
    entries: HashMap<TxId, Entry>,
    by_image: HashMap<[u8; 32], TxId>,
    bytes: u64,
    next_seq: u64,
}

impl Mempool {
    pub fn new(cfg: MempoolConfig) -> Mempool {
        Mempool {
            cfg,
            entries: HashMap::new(),
            by_image: HashMap::new(),
            bytes: 0,
            next_seq: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn total_bytes(&self) -> u64 {
        self.bytes
    }

    pub fn contains(&self, id: &TxId) -> bool {
        self.entries.contains_key(id)
    }

    pub fn get(&self, id: &TxId) -> Option<&Transaction> {
        self.entries.get(id).map(|e| &e.tx)
    }

    pub fn ids(&self) -> Vec<TxId> {
        self.entries.keys().copied().collect()
    }

    /// How many key images the pool tracks: always the number of inputs of the transactions it holds.
    pub fn tracked_key_images(&self) -> usize {
        self.by_image.len()
    }

    /// The pooled transaction that spends this key image, if any.
    pub fn spender_of(&self, key_image: &[u8; 32]) -> Option<TxId> {
        self.by_image.get(key_image).copied()
    }

    fn remove_entry(&mut self, id: &TxId) -> bool {
        match self.entries.remove(id) {
            Some(e) => {
                self.bytes -= e.size;
                for i in &e.tx.prefix.inputs {
                    self.by_image.remove(&i.key_image);
                }
                true
            }
            None => false,
        }
    }

    /// Checks `tx` against the tip's state and adds it if it is valid, does not conflict and fits.
    pub fn add(
        &mut self,
        validator: &Validator<'_>,
        tx: Transaction,
    ) -> Result<AddOutcome, PoolError> {
        let id =
            tx_id(&tx).map_err(|e| PoolError::Invalid(BlockError::Malformed(e.to_string())))?;
        if self.entries.contains_key(&id) {
            return Err(PoolError::AlreadyKnown);
        }
        let info = validator.check_pool_tx(&tx).map_err(PoolError::Invalid)?;
        // a transaction no block could ever hold is not worth keeping
        if fees::over_hard_limit(info.size, info.median) {
            return Err(PoolError::TooLarge {
                size: info.size,
                limit: info.median.saturating_mul(2),
            });
        }
        for input in &tx.prefix.inputs {
            if let Some(&with) = self.by_image.get(&input.key_image) {
                return Err(PoolError::Conflict {
                    key_image: input.key_image,
                    with,
                });
            }
        }
        // make room: push out the lowest fee rates, but only for a strictly better one
        let mut evicted = Vec::new();
        if self.bytes + info.size > self.cfg.max_bytes {
            let mut order: Vec<(&TxId, &Entry)> = self.entries.iter().collect();
            // lowest rate first; among equals, the newest first
            order.sort_by(|a, b| {
                let (x, y) = ((a.1.fee, a.1.size), (b.1.fee, b.1.size));
                if better_rate(x, y) {
                    std::cmp::Ordering::Greater
                } else if better_rate(y, x) {
                    std::cmp::Ordering::Less
                } else {
                    b.1.seq.cmp(&a.1.seq)
                }
            });
            let mut freed = 0u64;
            let mut victims = Vec::new();
            for (vid, e) in order {
                if self.bytes - freed + info.size <= self.cfg.max_bytes {
                    break;
                }
                if !better_rate((info.fee, info.size), (e.fee, e.size)) {
                    return Err(PoolError::PoolFull);
                }
                freed += e.size;
                victims.push(*vid);
            }
            if self.bytes - freed + info.size > self.cfg.max_bytes {
                return Err(PoolError::PoolFull);
            }
            for v in victims {
                self.remove_entry(&v);
                evicted.push(v);
            }
        }
        for input in &tx.prefix.inputs {
            self.by_image.insert(input.key_image, id);
        }
        self.bytes += info.size;
        self.entries.insert(
            id,
            Entry {
                tx,
                size: info.size,
                fee: info.fee,
                seq: self.next_seq,
            },
        );
        self.next_seq += 1;
        Ok(AddOutcome::Added { id, evicted })
    }

    /// A block was added to the tip: drop what it confirmed, and whatever it makes impossible (any pooled
    /// transaction sharing a key image with one of its transactions), then drop what no longer pays the
    /// minimum fee at the next height. Returns the ids removed.
    pub fn on_block_connected(&mut self, validator: &Validator<'_>, block: &Block) -> Vec<TxId> {
        let mut removed = self.remove_spent_by(block);
        removed.extend(self.drop_below_min_fee(validator));
        removed
    }

    /// Removes the pooled transactions that share a key image with a transaction of `block`.
    fn remove_spent_by(&mut self, block: &Block) -> Vec<TxId> {
        let mut removed = Vec::new();
        for t in &block.transactions {
            for i in &t.prefix.inputs {
                if let Some(id) = self.by_image.get(&i.key_image).copied() {
                    self.remove_entry(&id);
                    removed.push(id);
                }
            }
        }
        removed
    }

    /// The minimum fee moves with the block-size median and the reward, so a new tip can raise it.
    fn drop_below_min_fee(&mut self, validator: &Validator<'_>) -> Vec<TxId> {
        let Ok(next) = validator.next_block() else {
            return Vec::new();
        };
        let low: Vec<TxId> = self
            .entries
            .iter()
            .filter(|(_, e)| {
                fees::dynamic_min_fee(e.size, next.reward, next.median)
                    .map(|min| e.fee < min)
                    .unwrap_or(true)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in &low {
            self.remove_entry(id);
        }
        low
    }

    /// Checks every pooled transaction again against the tip, in full (proofs included), and drops those
    /// that fail. Needed after a reorganisation, when the state under them changed.
    pub fn revalidate(&mut self, validator: &Validator<'_>) -> Vec<TxId> {
        let bad: Vec<TxId> = self
            .entries
            .iter()
            .filter(|(_, e)| match validator.check_pool_tx(&e.tx) {
                Ok(info) => fees::over_hard_limit(info.size, info.median),
                Err(_) => true,
            })
            .map(|(id, _)| *id)
            .collect();
        for id in &bad {
            self.remove_entry(id);
        }
        bad
    }

    /// The chain reorganised (`report`). Drops whatever the new state makes invalid (which includes what the
    /// new blocks confirmed or conflict with), then puts back the transactions of the undone blocks that are
    /// still valid (coinbases are not transactions and are not put back).
    pub fn on_reorganised(&mut self, validator: &Validator<'_>, report: &ReorgReport) -> Vec<TxId> {
        // the full check also drops whatever the new blocks confirmed or spent (their key images are spent)
        let mut removed = self.revalidate(validator);
        for b in &report.disconnected {
            for t in &b.transactions {
                // valid on the new chain, or dropped; not already confirmed there (its key image would be spent)
                let _ = self.add(validator, t.clone());
            }
        }
        removed.extend(self.drop_below_min_fee(validator));
        removed
    }

    /// Transactions for a block, best fee rate first, at most `max_body_bytes` of them in total. A
    /// transaction that does not fit is skipped and smaller ones after it may still be taken.
    pub fn select(&self, max_body_bytes: u64) -> Vec<Transaction> {
        let mut all: Vec<&Entry> = self.entries.values().collect();
        all.sort_by(|a, b| {
            let (x, y) = ((a.fee, a.size), (b.fee, b.size));
            if better_rate(x, y) {
                std::cmp::Ordering::Less
            } else if better_rate(y, x) {
                std::cmp::Ordering::Greater
            } else {
                a.seq.cmp(&b.seq)
            }
        });
        let mut out = Vec::new();
        let mut used = 0u64;
        for e in all {
            if used + e.size <= max_body_bytes {
                used += e.size;
                out.push(e.tx.clone());
            }
        }
        out
    }
}
