//! The GPU backend: matmulhash on an NVIDIA GPU through `tenero-gpu`.
//!
//! **This code is checked only by compiling it anywhere and by running it on the owner's machine** (the tests that
//! need a GPU are `#[ignore]`d). Nothing about its speed is claimed here: the owner measures it (CLAUDE.md rule 5).
//!
//! Video memory: a dataset is 4 GiB, and up to two are kept (the epoch being mined and the next one, built ahead of
//! time, see [`GpuBackend::new`]). Before a dataset is built, any that is further than one epoch away from it is
//! freed, so two are the most that ever exist at once (plus the attempt buffers, which are small).

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tenero_core::matmulhash::{self as mh, Params};
use tenero_core::v2::ids;
use tenero_gpu::{DeviceDataset, Gpu};

use crate::{start_nonce, Backend, Counters, Job, Solution};

pub struct GpuBackend {
    gpu: Gpu,
    params: Params,
    epoch_blocks: u64,
    batch: usize,
    prefetch_blocks: u64,
    cache: Vec<(u64, Arc<DeviceDataset>)>,
}

impl GpuBackend {
    /// Opens device `ordinal`. `batch` is how many attempts run together (the GPU benchmark,
    /// `cargo run --release -p tenero-gpu --example gpu_bench`, shows how the speed depends on it); when a job is
    /// within `prefetch_blocks` of the end of its epoch, the next epoch's dataset is built between two batches.
    pub fn new(
        ordinal: usize,
        params: Params,
        epoch_blocks: u64,
        batch: usize,
        prefetch_blocks: u64,
    ) -> Result<GpuBackend, String> {
        params.validate()?;
        if batch == 0 || epoch_blocks == 0 {
            return Err("the batch and the epoch length must be at least 1".into());
        }
        Ok(GpuBackend {
            gpu: Gpu::new(ordinal).map_err(|e| e.to_string())?,
            params,
            epoch_blocks,
            batch,
            prefetch_blocks,
            cache: Vec::new(),
        })
    }

    pub fn device_name(&self) -> &str {
        &self.gpu.name
    }
}

/// The dataset of `epoch`, built if it is not kept. Returns it and whether it had to be built. Datasets more than
/// one epoch away are freed first, so two is the most that exist at once.
fn ensure_dataset(
    gpu: &Gpu,
    cache: &mut Vec<(u64, Arc<DeviceDataset>)>,
    params: &Params,
    epoch: u64,
    counters: &Counters,
) -> Result<(Arc<DeviceDataset>, bool), String> {
    if let Some((_, d)) = cache.iter().find(|(e, _)| *e == epoch) {
        return Ok((Arc::clone(d), false));
    }
    cache.retain(|(e, _)| e + 1 >= epoch && *e <= epoch + 1);
    let _building = counters.building();
    let built = gpu
        .build_dataset(params, &mh::epoch_seed(epoch), params.num_blocks)
        .map_err(|e| e.to_string())?;
    gpu.synchronize().map_err(|e| e.to_string())?;
    counters.dataset_builds.fetch_add(1, Ordering::Relaxed);
    let built = Arc::new(built);
    cache.push((epoch, Arc::clone(&built)));
    Ok((built, true))
}

impl Backend for GpuBackend {
    fn name(&self) -> String {
        format!(
            "matmulhash on the GPU ({}, batch {})",
            self.gpu.name, self.batch
        )
    }

    fn mine(&mut self, job: &Job, counters: &Counters) -> Result<Option<Solution>, String> {
        let epoch = mh::epoch_of(job.height, self.epoch_blocks)
            .ok_or("the genesis block has no proof of work")?;
        let GpuBackend {
            gpu,
            params,
            epoch_blocks,
            batch,
            prefetch_blocks,
            cache,
        } = self;
        let (data, _) = ensure_dataset(gpu, cache, params, epoch, counters)?;
        let hh = ids::header_hash(&job.header);
        let mut engine = gpu
            .attempt_engine(&data, *batch)
            .map_err(|e| e.to_string())?;
        let mut nonce = start_nonce(job.id);
        let mut prefetched: Option<u64> = None;
        loop {
            if job.stale.load(Ordering::SeqCst) {
                return Ok(None);
            }
            // the next epoch's dataset, a few blocks before it is needed
            let ahead = mh::epoch_of(job.height + *prefetch_blocks, *epoch_blocks);
            if *prefetch_blocks > 0 && ahead != Some(epoch) && ahead != prefetched {
                if let Some(next) = ahead {
                    let (_, built) = ensure_dataset(gpu, cache, params, next, counters)?;
                    if built {
                        counters.prefetches.fetch_add(1, Ordering::Relaxed);
                    }
                }
                prefetched = ahead;
            }
            let nonces: Vec<u64> = (0..*batch as u64).map(|i| nonce.wrapping_add(i)).collect();
            let attempts = engine.attempts(&hh, &nonces).map_err(|e| e.to_string())?;
            counters
                .attempts
                .fetch_add(*batch as u64, Ordering::Relaxed);
            for (n, a) in nonces.iter().zip(&attempts) {
                if mh::meets_target(&a.digest, &job.target) {
                    counters.found.fetch_add(1, Ordering::Relaxed);
                    return Ok(Some(Solution {
                        job_id: job.id,
                        nonce: *n,
                        mix: a.mix,
                    }));
                }
            }
            nonce = nonce.wrapping_add(*batch as u64);
        }
    }
}
