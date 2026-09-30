//! The proof-of-work checks a block goes through (`docs/CONSENSUS.md` section 8, `CONSENSUS_V2.md` 5.2).
//!
//! Two steps, cheap first: the block id must be below the required target (needs no dataset, microseconds),
//! then the full recomputation (needs the epoch's dataset for matmulhash). A block that passes the cheap
//! step is not known to be valid: a forger can grind a made-up mix to the target for the cost of about
//! `difficulty` SHA-256 hashes, and only the full check rejects it.

use std::sync::{Arc, Mutex};
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

/// matmulhash v2: the dataset of an epoch is built once and kept (the last two epochs).
pub struct MatmulPow {
    params: Params,
    epoch_blocks: u64,
    threads: usize,
    datasets: Mutex<Vec<(u64, Arc<Dataset>)>>,
}

impl MatmulPow {
    /// `params` are the chain's matmulhash parameters (`Params::DEFAULT` for the real chain, a small set for
    /// tests), `epoch_blocks` how many blocks share a dataset (100 on the real chain), `threads` how many
    /// threads build it.
    pub fn new(params: Params, epoch_blocks: u64, threads: usize) -> Result<MatmulPow, String> {
        params.validate()?;
        if epoch_blocks == 0 {
            return Err("epoch_blocks must be at least 1".into());
        }
        Ok(MatmulPow {
            params,
            epoch_blocks,
            threads,
            datasets: Mutex::new(Vec::new()),
        })
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    /// The dataset of the epoch that `height` belongs to (building it if it is not kept).
    pub fn dataset_for(&self, height: u64) -> Result<Arc<Dataset>, String> {
        let epoch = matmulhash::epoch_of(height, self.epoch_blocks)
            .ok_or("the genesis block has no proof of work")?;
        let mut kept = self
            .datasets
            .lock()
            .map_err(|_| "the dataset cache is poisoned")?;
        if let Some((_, d)) = kept.iter().find(|(e, _)| *e == epoch) {
            return Ok(Arc::clone(d));
        }
        let built = Arc::new(Dataset::build(
            &self.params,
            &matmulhash::epoch_seed(epoch),
            self.params.num_blocks,
            self.threads,
        )?);
        if kept.len() >= 2 {
            kept.remove(0);
        }
        kept.push((epoch, Arc::clone(&built)));
        Ok(built)
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
}
