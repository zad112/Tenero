//! The proof-of-work checks a block goes through (`docs/CONSENSUS.md` section 8, `CONSENSUS_V2.md` 5.2).
//!
//! Two steps, cheap first: the block id must be below the required target (needs no dataset, microseconds),
//! then the full recomputation (needs the epoch's dataset for matmulhash). A block that passes the cheap
//! step is not known to be valid: a forger can grind a made-up mix to the target for the cost of about
//! `difficulty` SHA-256 hashes, and only the full check rejects it.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use tenero_core::matmulhash::{self, Dataset, Params};
use tenero_core::u256::U256;
use tenero_core::v2::ids::{self, PowKind};
use tenero_core::v2::BlockHeader;

/// A proof of work, as the validator uses it.
pub trait PowCheck: Send + Sync {
    fn kind(&self) -> PowKind;

    /// The cheap check: the block id (the proof-of-work digest) is strictly below the target.
    fn check_cheap(&self, header: &BlockHeader, target: &U256) -> bool {
        U256::from_be_bytes(&ids::block_id(header, self.kind())) < *target
    }

    /// The full check: the digest really is what the proof of work produces for this header. `Ok(false)` is
    /// "valid proof of work claimed, but wrong"; `Err` is "could not check" (for example no memory).
    fn check_full(&self, header: &BlockHeader, height: u64) -> Result<bool, String>;

    /// A hint that blocks at about `height` will be checked soon: a proof of work that needs a dataset builds it in
    /// the background now, so the first block of a new epoch does not wait for it. Does nothing by default, and
    /// never blocks.
    fn prefetch(&self, _height: u64) {}
}

/// The SHA-256 test chain: `digest = sha256(header_hash || nonce)`, and the mix is 64 zero bytes.
pub struct Sha256Pow;

impl PowCheck for Sha256Pow {
    fn kind(&self) -> PowKind {
        PowKind::Sha256
    }

    fn check_full(&self, header: &BlockHeader, _height: u64) -> Result<bool, String> {
        // the id is recomputed from the header by `check_cheap`; the only thing left is the zero mix
        Ok(header.mix == [0u8; 64])
    }
}

/// What is known about the datasets: the ones built and the epochs being built right now.
struct Cache {
    kept: Vec<(u64, Arc<Dataset>)>,
    building: HashSet<u64>,
}

/// How a dataset is made (the real one, or a stand-in that tests use to make a build fail or take long).
type Builder = Box<dyn Fn(&Params, u64, usize) -> Result<Dataset, String> + Send + Sync>;

struct Inner {
    params: Params,
    epoch_blocks: u64,
    threads: usize,
    builder: Builder,
    cache: Mutex<Cache>,
    /// Signalled whenever a build ends (or fails), for callers waiting for the epoch they need.
    built: Condvar,
    builds: AtomicU64,
}

/// matmulhash v2: the dataset of an epoch is built once and kept (the last two epochs).
///
/// A dataset is built **outside the lock**, so checking a block of the current epoch is never held up by a build of
/// another; callers that need an epoch that is being built wait for that one build rather than starting a second;
/// and before a build starts, a dataset further than one epoch away is freed, so at most two exist at once (about
/// 8.6 GiB at the real parameters, briefly). `prefetch` builds the next epoch's dataset on a background thread.
pub struct MatmulPow {
    inner: Arc<Inner>,
}

fn poisoned<T>(_: T) -> String {
    "the dataset cache is poisoned".to_string()
}

impl MatmulPow {
    /// `params` are the chain's matmulhash parameters (`Params::DEFAULT` for the real chain, a small set for
    /// tests), `epoch_blocks` how many blocks share a dataset (100 on the real chain), `threads` how many
    /// threads build it.
    pub fn new(params: Params, epoch_blocks: u64, threads: usize) -> Result<MatmulPow, String> {
        MatmulPow::with_builder(
            params,
            epoch_blocks,
            threads,
            Box::new(|params, epoch, threads| {
                Dataset::build(
                    params,
                    &matmulhash::epoch_seed(epoch),
                    params.num_blocks,
                    threads,
                )
            }),
        )
    }

    /// Like `new`, but the datasets are made by `builder(params, epoch, threads)`. **For tests** (to make a build
    /// fail or take long); a chain uses `new`.
    #[doc(hidden)]
    pub fn with_builder(
        params: Params,
        epoch_blocks: u64,
        threads: usize,
        builder: Builder,
    ) -> Result<MatmulPow, String> {
        params.validate()?;
        if epoch_blocks == 0 {
            return Err("epoch_blocks must be at least 1".into());
        }
        Ok(MatmulPow {
            inner: Arc::new(Inner {
                params,
                epoch_blocks,
                threads,
                builder,
                cache: Mutex::new(Cache {
                    kept: Vec::new(),
                    building: HashSet::new(),
                }),
                built: Condvar::new(),
                builds: AtomicU64::new(0),
            }),
        })
    }

    pub fn params(&self) -> &Params {
        &self.inner.params
    }

    /// The dataset of the epoch that `height` belongs to (building it if it is not kept, or waiting for the build
    /// that is already under way).
    pub fn dataset_for(&self, height: u64) -> Result<Arc<Dataset>, String> {
        self.inner.dataset_for(height)
    }

    /// Whether the dataset of `height`'s epoch is built and kept.
    pub fn has_dataset(&self, height: u64) -> bool {
        let Some(epoch) = matmulhash::epoch_of(height, self.inner.epoch_blocks) else {
            return false;
        };
        self.inner
            .cache
            .lock()
            .map(|c| c.kept.iter().any(|(e, _)| *e == epoch))
            .unwrap_or(false)
    }

    /// How many datasets have been built in all.
    pub fn builds(&self) -> u64 {
        self.inner.builds.load(Ordering::Relaxed)
    }
}

impl Inner {
    fn dataset_for(&self, height: u64) -> Result<Arc<Dataset>, String> {
        let epoch = matmulhash::epoch_of(height, self.epoch_blocks)
            .ok_or("the genesis block has no proof of work")?;
        let mut c = self.cache.lock().map_err(poisoned)?;
        loop {
            if let Some((_, d)) = c.kept.iter().find(|(e, _)| *e == epoch) {
                return Ok(Arc::clone(d));
            }
            if c.building.contains(&epoch) {
                c = self.built.wait(c).map_err(poisoned)?;
                continue;
            }
            break;
        }
        c.building.insert(epoch);
        // make room BEFORE building, so two datasets are the most that ever exist: the one furthest from this
        // epoch goes
        while c.kept.len() >= 2 {
            let far = (0..c.kept.len())
                .max_by_key(|&i| c.kept[i].0.abs_diff(epoch))
                .expect("not empty");
            c.kept.remove(far);
        }
        drop(c);
        let result = (self.builder)(&self.params, epoch, self.threads).map(Arc::new);
        let mut c = self.cache.lock().map_err(poisoned)?;
        c.building.remove(&epoch);
        if let Ok(d) = &result {
            self.builds.fetch_add(1, Ordering::Relaxed);
            c.kept.push((epoch, Arc::clone(d)));
        }
        self.built.notify_all();
        result
    }

    fn prefetch(self: &Arc<Inner>, height: u64) {
        let Some(epoch) = matmulhash::epoch_of(height, self.epoch_blocks) else {
            return;
        };
        {
            let Ok(c) = self.cache.lock() else {
                return;
            };
            if c.kept.iter().any(|(e, _)| *e == epoch) || c.building.contains(&epoch) {
                return; // already there, or already on its way
            }
        }
        let me = Arc::clone(self);
        thread::spawn(move || {
            let _ = me.dataset_for(height);
        });
    }
}

impl PowCheck for MatmulPow {
    fn kind(&self) -> PowKind {
        PowKind::Matmul
    }

    fn check_full(&self, header: &BlockHeader, height: u64) -> Result<bool, String> {
        let data = self.dataset_for(height)?;
        let attempt = matmulhash::compute_attempt(&data, &ids::header_hash(header), header.nonce)?;
        Ok(attempt.mix == header.mix)
    }

    fn prefetch(&self, height: u64) {
        self.inner.prefetch(height);
    }
}
