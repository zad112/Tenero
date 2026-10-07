//! The GPU backend: matmulhash on an NVIDIA GPU through `tenero-gpu`.
//!
//! **This code is checked only by compiling it anywhere and by running it on the owner's machine** (the tests that
//! need a GPU are `#[ignore]`d). Nothing about its speed is claimed here: the owner measures it (CLAUDE.md rule 5).
//!
//! Speed: the nonces of a job are tried in groups of `GROUP` that read the same dataset slice (`tenero_gpu::group`), so a
//! 16 MiB slice is read once for the whole group, and two batches are kept on the GPU at once (`AttemptEngine::submit`).
//! Each attempt is exactly the attempt it always was; only the order of trying nonces changes.
//!
//! Video memory: a dataset is 4 GiB, and ONE is kept: the epoch being mined. The next epoch's dataset is built when the
//! first job of that epoch arrives (a build takes about 0.12 s on the GPU, so nothing is built ahead of time), and the old
//! one is freed BEFORE the new one is allocated, so one is the most that ever exists at once (plus the attempt buffers,
//! two sets of them for the pipeline: X and C are 512 KiB each per attempt, so 2 MiB per attempt of the batch, 512 MiB
//! at batch 256). This was two (the previous epoch kept, the next one built ahead): on Windows that showed as about 9.2 GiB of committed memory for the miner process, with 0.36 GiB of it in use as RAM.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tenero_core::matmulhash::{self as mh, Params};
use tenero_core::v2::ids;
use tenero_gpu::gather::GatherEngine;
use tenero_gpu::group::SliceGrouper;
use tenero_gpu::{DeviceDataset, Gpu};

use crate::{Backend, Counters, Job, Solution};

/// Attempts per slice group: the GPU multiplies this many attempts against one read of a slice. **Measured on the owner's
/// RTX 5070 Ti (2026-10-07, `gpu_bench`): about 110,000 attempts/s at 8, 127,000 at 16 and 130,000 at 32** (against about
/// 35,000 with no grouping); 32 is within the run-to-run noise of 16 and keeps twice as many nonces waiting, so 16.
pub const GROUP: usize = 16;

pub struct GpuBackend {
    gpu: Gpu,
    params: Params,
    epoch_blocks: u64,
    batch: usize,
    cache: Vec<(u64, Arc<DeviceDataset>)>,
    /// From this height on, jobs need the GATHERED attempt (the network's gather fork; `u64::MAX`: never).
    gather_from: u64,
}

impl GpuBackend {
    /// Opens device `ordinal`. `batch` is how many attempts run together (the GPU benchmark,
    /// `cargo run --release -p tenero-gpu --example gpu_bench`, shows how the speed depends on it).
    pub fn new(
        ordinal: usize,
        params: Params,
        epoch_blocks: u64,
        batch: usize,
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
            cache: Vec::new(),
            gather_from: u64::MAX,
        })
    }

    /// The same backend, mining the GATHERED attempt for jobs at `height` and above (the network's gather fork height,
    /// `Network::gather_from`). Without it every job gets the first design, which a gathered chain refuses.
    pub fn gathered_from(mut self, height: u64) -> GpuBackend {
        self.gather_from = height;
        self
    }

    pub fn device_name(&self) -> &str {
        &self.gpu.name
    }
}

/// The dataset of `epoch`, built if it is not kept. Returns it and whether it had to be built. Every other epoch's
/// dataset is dropped first, so one is the most that exists at once (the old one's video memory is freed once nothing
/// uses it: an attempt engine of the last job holds it until that job returns, and it has by the time a job of another
/// epoch starts).
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
    cache.retain(|(e, _)| *e == epoch);
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
            batch,
            cache,
            gather_from,
            ..
        } = self;
        let (data, _) = ensure_dataset(gpu, cache, params, epoch, counters)?;
        let hh = ids::header_hash(&job.header);
        if job.height >= *gather_from {
            return mine_gathered(gpu, &data, *batch, &hh, job, counters);
        }
        let mut engine = gpu
            .attempt_engine(&data, *batch)
            .map_err(|e| e.to_string())?;
        // nonces are tried in groups that read the same slice (see `tenero_gpu::group`): the same attempts, in another
        // order, with the slice read once for each group
        let group = GROUP.min(*batch);
        let groups = (*batch / group).max(1);
        let mut grouper = SliceGrouper::new(hh, params.num_blocks, group, job.first_nonce());
        // the nonces of the batches on the GPU, oldest first: two are kept in flight, so the GPU starts the next one
        // while this thread hashes the last one's results and picks the nonces after it
        let mut in_flight: VecDeque<Vec<u64>> = VecDeque::new();
        loop {
            if job.stale.load(Ordering::SeqCst) {
                return Ok(None);
            }
            while engine.in_flight() < 2 {
                let b = grouper.next_batch(groups);
                in_flight.push_back(b.nonces);
                engine
                    .submit(b.seeds, b.slices)
                    .map_err(|e| e.to_string())?;
            }
            let attempts = engine
                .collect()
                .map_err(|e| e.to_string())?
                .ok_or("the GPU engine lost a batch")?;
            let nonces = in_flight.pop_front().ok_or("the GPU engine lost a batch")?;
            let n = nonces.len();
            counters.attempts.fetch_add(n as u64, Ordering::Relaxed);
            for (i, (nonce, a)) in nonces.iter().zip(&attempts).enumerate() {
                if mh::meets_target(&a.digest, &job.target) {
                    // the rest of the batch was made, but cannot make another block of this job (the batch still on
                    // the GPU is not counted at all)
                    counters
                        .discarded
                        .fetch_add((n - 1 - i) as u64, Ordering::Relaxed);
                    counters.found.fetch_add(1, Ordering::Relaxed);
                    return Ok(Some(Solution {
                        job_id: job.id,
                        nonce: *nonce,
                        mix: a.mix,
                    }));
                }
            }
        }
    }
}

/// A job at or above the gather fork: nonces in plain order (grouping cannot help a gathered attempt), one batch at a time.
fn mine_gathered(
    gpu: &Gpu,
    data: &DeviceDataset,
    batch: usize,
    hh: &[u8; 32],
    job: &Job,
    counters: &Counters,
) -> Result<Option<Solution>, String> {
    let mut engine = GatherEngine::new(gpu, data, batch).map_err(|e| e.to_string())?;
    let mut nonce = job.first_nonce();
    loop {
        if job.stale.load(Ordering::SeqCst) {
            return Ok(None);
        }
        let nonces: Vec<u64> = (0..batch as u64).map(|i| nonce.wrapping_add(i)).collect();
        let seeds: Vec<[u8; 32]> = nonces.iter().map(|&n| mh::attempt_seed(hh, n)).collect();
        let attempts = engine.attempts(&seeds, false).map_err(|e| e.to_string())?;
        counters.attempts.fetch_add(batch as u64, Ordering::Relaxed);
        for (i, (n, a)) in nonces.iter().zip(&attempts).enumerate() {
            if mh::meets_target(&a.digest, &job.target) {
                // the rest of the batch was made, but cannot make another block of this job
                counters
                    .discarded
                    .fetch_add((batch - 1 - i) as u64, Ordering::Relaxed);
                counters.found.fetch_add(1, Ordering::Relaxed);
                return Ok(Some(Solution {
                    job_id: job.id,
                    nonce: *n,
                    mix: a.mix,
                }));
            }
        }
        nonce = nonce.wrapping_add(batch as u64);
    }
}

// ---- choosing a batch size ----------------------------------------------------------------------------------------------------------

/// The batch sizes `auto` tries. **128 and 256 were measured on the owner's card (within noise of each other, about 33,000 to 36,000
/// attempts/s, and above 32 and 64); 512 was not measured before this.**
pub const AUTO_BATCHES: [usize; 3] = [128, 256, 512];

/// The batch size with the highest rate; on a tie the smaller (less video memory). Rates that are not finite or not above 0 are ignored;
/// `None` if none is left. **One short measurement varies by several per cent from run to run on the same card** (see `BENCHMARKS.md`),
/// so this picks the best of what it saw and does not claim the winner is better by that margin.
pub fn choose_batch(results: &[(usize, f64)]) -> Option<usize> {
    results
        .iter()
        .filter(|(b, r)| *b > 0 && r.is_finite() && *r > 0.0)
        .fold(None, |best: Option<(usize, f64)>, &(b, r)| match best {
            Some((bb, br)) if br > r || (br == r && bb <= b) => Some((bb, br)),
            _ => Some((b, r)),
        })
        .map(|(b, _)| b)
}

/// Measures each of `candidates` for `secs` seconds (after a second and a half of warming up) on a job that can never succeed, and
/// returns the rates. A candidate that cannot start (no video memory for it, say) is left out, with the reason in `log`.
pub fn measure_batches(
    ordinal: usize,
    params: Params,
    epoch_blocks: u64,
    candidates: &[usize],
    secs: f64,
    log: &dyn Fn(&str),
) -> Vec<(usize, f64)> {
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};
    use tenero_core::u256::U256;
    use tenero_core::v2::BlockHeader;

    let mut out = vec![];
    for &batch in candidates {
        let mut backend = match GpuBackend::new(ordinal, params, epoch_blocks, batch) {
            Ok(b) => b,
            Err(e) => {
                log(&format!("batch {batch}: cannot start ({e}); left out"));
                continue;
            }
        };
        let stale = Arc::new(AtomicBool::new(false));
        let job = Job {
            id: batch as u64,
            header: BlockHeader {
                version: tenero_core::v2::VERSION,
                prev_id: [0; 32],
                timestamp: 1,
                tx_root: [0; 32],
                nonce: 0,
                mix: [0; 64],
            },
            height: 1,
            target: U256::ZERO, // never met
            stale: Arc::clone(&stale),
            nonce_start: None,
        };
        let counters = Arc::new(Counters::default());
        let c = Arc::clone(&counters);
        let thread = std::thread::spawn(move || backend.mine(&job, &c));
        std::thread::sleep(Duration::from_millis(1500));
        let (a0, t0) = (counters.attempts.load(Ordering::Relaxed), Instant::now());
        std::thread::sleep(Duration::from_secs_f64(secs));
        let (a1, t1) = (counters.attempts.load(Ordering::Relaxed), Instant::now());
        stale.store(true, Ordering::SeqCst);
        match thread.join() {
            Ok(Ok(_)) => {
                let rate = (a1 - a0) as f64 / t1.duration_since(t0).as_secs_f64();
                log(&format!("batch {batch}: {rate:.0} attempts/s"));
                out.push((batch, rate));
            }
            Ok(Err(e)) => log(&format!(
                "batch {batch}: failed while measuring ({e}); left out"
            )),
            Err(_) => log(&format!(
                "batch {batch}: the measuring thread panicked; left out"
            )),
        }
    }
    out
}

/// Picks the batch size for this card by measuring (`AUTO_BATCHES`, 4 s each): about 20 seconds at start-up. Falls back to `fallback`,
/// saying so in `log`, if nothing could be measured.
pub fn auto_batch(
    ordinal: usize,
    params: Params,
    epoch_blocks: u64,
    fallback: usize,
    log: &dyn Fn(&str),
) -> usize {
    log("choosing the batch size by measuring (about 20 seconds)...");
    let results = measure_batches(ordinal, params, epoch_blocks, &AUTO_BATCHES, 4.0, log);
    match choose_batch(&results) {
        Some(b) => {
            log(&format!("batch {b} chosen"));
            b
        }
        None => {
            log(&format!(
                "nothing could be measured; using batch {fallback}"
            ));
            fallback
        }
    }
}
