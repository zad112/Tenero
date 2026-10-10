//! Tenero mining backend for AMD RDNA3 through HIP (Linux, ROCm 6; contributed, not tested by the
//! project's author: see `tenero_gpu::amd`). The card's health readings (NVML) are NVIDIA's only.

use std::sync::atomic::Ordering;

use tenero_core::matmulhash::{self as mh, Params};
use tenero_core::v3::ids;
use tenero_gpu::amd::AmdGpu;
use tenero_gpu::group::SliceGrouper;

use crate::{Backend, Counters, Job, Solution};

/// Slice grouping helps the first design of the attempt only (not used on `gamma`, which gathers from block 0).
const GROUP: usize = 16;

pub struct AmdBackend {
    gpu: AmdGpu,
    params: Params,
    epoch_blocks: u64,
    batch: usize,
    epoch: Option<u64>,
    gather_from: u64,
}

impl AmdBackend {
    pub fn new(
        ordinal: usize,
        params: Params,
        epoch_blocks: u64,
        batch: usize,
    ) -> Result<AmdBackend, String> {
        params.validate()?;
        if epoch_blocks == 0 || batch == 0 {
            return Err("the batch and epoch length must be at least 1".into());
        }
        Ok(AmdBackend {
            gpu: AmdGpu::new(ordinal, params, batch).map_err(|e| e.to_string())?,
            params,
            epoch_blocks,
            batch,
            epoch: None,
            gather_from: u64::MAX,
        })
    }

    pub fn gathered_from(mut self, height: u64) -> AmdBackend {
        self.gather_from = height;
        self
    }

    pub fn device_name(&self) -> &str {
        &self.gpu.name
    }

    fn ensure_dataset(&mut self, epoch: u64, counters: &Counters) -> Result<(), String> {
        if self.epoch == Some(epoch) {
            return Ok(());
        }
        let _building = counters.building();
        self.gpu
            .build_dataset(&mh::epoch_seed(epoch))
            .map_err(|e| e.to_string())?;
        self.epoch = Some(epoch);
        counters.dataset_builds.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

impl Backend for AmdBackend {
    fn name(&self) -> String {
        format!(
            "matmulhash on AMD HIP ({} {}, batch {})",
            self.gpu.name, self.gpu.arch, self.batch
        )
    }

    fn mine(&mut self, job: &Job, counters: &Counters) -> Result<Option<Solution>, String> {
        let epoch = mh::epoch_of(job.height, self.epoch_blocks)
            .ok_or("the genesis block has no proof of work")?;
        self.ensure_dataset(epoch, counters)?;
        let header_hash = ids::header_hash(&job.header);

        if job.height >= self.gather_from {
            let mut nonce = job.first_nonce();
            loop {
                if job.stale.load(Ordering::SeqCst) {
                    return Ok(None);
                }
                let nonces: Vec<u64> = (0..self.batch as u64)
                    .map(|offset| nonce.wrapping_add(offset))
                    .collect();
                let seeds: Vec<[u8; 32]> = nonces
                    .iter()
                    .map(|&value| mh::attempt_seed(&header_hash, value))
                    .collect();
                let attempts = self.gpu.attempts(&seeds, None).map_err(|e| e.to_string())?;
                counters
                    .attempts
                    .fetch_add(self.batch as u64, Ordering::Relaxed);
                for (index, (value, attempt)) in nonces.iter().zip(&attempts).enumerate() {
                    if mh::meets_target(&attempt.digest, &job.target) {
                        counters
                            .discarded
                            .fetch_add((self.batch - 1 - index) as u64, Ordering::Relaxed);
                        counters.found.fetch_add(1, Ordering::Relaxed);
                        return Ok(Some(Solution {
                            job_id: job.id,
                            nonce: *value,
                            mix: attempt.mix,
                        }));
                    }
                }
                nonce = nonce.wrapping_add(self.batch as u64);
            }
        }

        // Pre-fork/alpha design: same nonce grouping as the NVIDIA backend, so a slice is
        // normally still in cache for the rest of its group.
        let group = GROUP.min(self.batch);
        let groups = (self.batch / group).max(1);
        let mut grouper = SliceGrouper::new(
            header_hash,
            self.params.num_blocks,
            group,
            job.first_nonce(),
        );
        loop {
            if job.stale.load(Ordering::SeqCst) {
                return Ok(None);
            }
            let batch = grouper.next_batch(groups);
            let attempts = self
                .gpu
                .attempts(&batch.seeds, Some(&batch.slices))
                .map_err(|e| e.to_string())?;
            counters
                .attempts
                .fetch_add(batch.len() as u64, Ordering::Relaxed);
            for (index, (value, attempt)) in batch.nonces.iter().zip(&attempts).enumerate() {
                if mh::meets_target(&attempt.digest, &job.target) {
                    counters
                        .discarded
                        .fetch_add((batch.len() - 1 - index) as u64, Ordering::Relaxed);
                    counters.found.fetch_add(1, Ordering::Relaxed);
                    return Ok(Some(Solution {
                        job_id: job.id,
                        nonce: *value,
                        mix: attempt.mix,
                    }));
                }
            }
        }
    }
}
