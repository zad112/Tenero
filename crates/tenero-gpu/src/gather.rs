//! The GATHERED proof-of-work attempt on the GPU: what beta and dev blocks need from the gather fork height (500) on
//! (`docs/CONSENSUS.md` section 8.3; the definition is `tenero_core::matmulhash::compute_gathered_attempt`, and this engine
//! is checked against it bit for bit by `tests/gather_selftest.rs`).
//!
//! Why it exists: in the first design an attempt multiplies its X by ONE whole 16 MiB slice, chosen by two cheap hashes of
//! the nonce, so a miner can choose nonces 16 to a slice and read each slice once for 16 attempts (`group.rs`): the speed
//! is then set by int8 multiply throughput, not memory (`THREAT_MODEL.md` E11). A gathered attempt picks its `nb` columns
//! (8 KiB each) one by one from the WHOLE dataset, so two attempts share almost none, and no two columns are shared by
//! the same set of attempts: sharing a column read would mean bringing their X matrices (512 KiB each) together for one
//! 8 KiB column, which costs more memory traffic than it saves. Measured on the owner's card while it was a prototype
//! (`BENCHMARKS.md`): about 45,000 attempts/s, at the card's memory bandwidth.

use crate::{key_words_i64, DeviceDataset, Gpu, GpuError, THREADS};
use cudarc::driver::{CudaSlice, LaunchConfig, PushKernelArg};
use tenero_core::matmulhash::{self as mh, Attempt};

/// Batches of attempts on the GPU with our own multiply (`gather.cu`), one batch at a time: the gathered attempt
/// (`attempts`) or the first design (`slice_attempts`).
pub struct GatherEngine<'a> {
    gpu: &'a Gpu,
    data: &'a DeviceDataset,
    read_out: CudaSlice<u32>,
    batch: usize,
    keys: CudaSlice<i64>,
    pick_keys: CudaSlice<i64>,
    x: CudaSlice<u8>,
    idx: CudaSlice<u8>,
    /// The slice of each attempt, for the first design (`slice_attempts`).
    slices: CudaSlice<u32>,
    c: CudaSlice<i32>,
    sums: CudaSlice<u64>,
}

/// Which columns the attempts of a batch read.
enum Columns<'s> {
    /// The gathered attempt; `same`: every attempt reads attempt 0's columns (the benchmark's impossible best case).
    Gathered { same: bool },
    /// The first design: each attempt reads the whole of its slice.
    Slices(&'s [usize]),
}

impl<'a> GatherEngine<'a> {
    pub fn new(
        gpu: &'a Gpu,
        data: &'a DeviceDataset,
        batch: usize,
    ) -> Result<GatherEngine<'a>, GpuError> {
        let p = data.params;
        if p.m != 64 || !p.k.is_multiple_of(128) || !p.nb.is_multiple_of(128) {
            return Err(GpuError::new(
                "the GPU engine needs m = 64, k a multiple of 128 and nb a multiple of 128",
            ));
        }
        if data.slices != p.num_blocks {
            return Err(GpuError::new("the GPU engine needs the whole dataset"));
        }
        if batch == 0 || batch > 65_535 {
            return Err(GpuError::new("the batch must be 1..=65535"));
        }
        let s = &gpu.stream;
        Ok(GatherEngine {
            gpu,
            data,
            read_out: s.alloc_zeros::<u32>(batch * (p.nb / 128) * 256)?,
            batch,
            keys: s.alloc_zeros::<i64>(batch * 8)?,
            pick_keys: s.alloc_zeros::<i64>(batch * 8)?,
            x: s.alloc_zeros::<u8>(batch * p.m * p.k)?,
            idx: s.alloc_zeros::<u8>(batch * p.nb * 4)?,
            slices: s.alloc_zeros::<u32>(batch)?,
            c: s.alloc_zeros::<i32>(batch * p.m * p.nb)?,
            sums: s.alloc_zeros::<u64>(batch * 8)?,
        })
    }

    /// A memory test: reads these seeds' columns as `attempts` does, `piece` bytes of each column at a time, with no
    /// multiply (`gather_read`), and waits for it. `piece` must be a multiple of 16 that divides `k`, at most 1024.
    pub fn read_only(&mut self, seeds: &[[u8; 32]], piece: usize) -> Result<(), GpuError> {
        let p = self.data.params;
        let n = seeds.len();
        if n == 0 || n > self.batch {
            return Err(GpuError::new("between 1 and `batch` seeds"));
        }
        if !piece.is_multiple_of(16) || piece == 0 || piece > 1024 || !p.k.is_multiple_of(piece) {
            return Err(GpuError::new(
                "piece must be a multiple of 16 dividing k, at most 1024",
            ));
        }
        let gpu = self.gpu;
        let stream = &gpu.stream;
        let pick: Vec<i64> = seeds
            .iter()
            .flat_map(|s| key_words_i64(&mh::pick_key(s)))
            .collect();
        stream.memcpy_htod(&pick, &mut self.pick_keys.slice_mut(0..n * 8))?;
        let ipk = (p.nb / 16) as u64;
        gpu.launch_keystream(&self.pick_keys, &self.idx, ipk, 0, n as u64 * ipk)?;
        let cfg = LaunchConfig {
            grid_dim: ((p.nb / 128) as u32, n as u32, 1),
            block_dim: (THREADS, 1, 1),
            shared_mem_bytes: 0,
        };
        let (k, nb, piece) = (p.k as u32, p.nb as u32, piece as u32);
        let columns = (p.num_blocks * p.nb) as u32;
        let mut b = stream.launch_builder(&gpu.read_fn);
        b.arg(&self.data.buf)
            .arg(&self.idx)
            .arg(&self.read_out)
            .arg(&k)
            .arg(&nb)
            .arg(&columns)
            .arg(&piece);
        // SAFETY: the kernel is `gather_read(u64 data, u64 idx, u64 out, u32 k, u32 nb, u32 columns, u32 piece)`. It
        // reads idx words of attempts < n, dataset columns `idx % columns` (k bytes each, inside the whole dataset), in
        // pieces that divide k (checked), and writes one word per thread of an n x (nb / 128) x 256 grid (`read_out`
        // holds batch * (nb / 128) * 256).
        #[allow(unsafe_code)]
        unsafe {
            b.launch(cfg)?;
        }
        stream.synchronize()?;
        Ok(())
    }

    /// The gathered attempts of these seeds (`attempt_seed` of each nonce), in order (`Attempt::slice_index` is 0, as
    /// in `compute_gathered_attempt`). With `same_columns`, every attempt reads attempt 0's columns instead of its own:
    /// **wrong results**, and a best case no miner can reach, used only by the benchmark to see what the reads cost.
    pub fn attempts(
        &mut self,
        seeds: &[[u8; 32]],
        same_columns: bool,
    ) -> Result<Vec<Attempt>, GpuError> {
        self.run(seeds, Columns::Gathered { same: same_columns })
    }

    /// Attempts of the FIRST design (matmulhash v2 before the gather fork: each attempt reads the whole of its slice),
    /// with our own multiply instead of cuBLASLt. `slices[i]` must be `attempt_slice` of `seeds[i]` (trusted, as in
    /// `AttemptEngine::attempts_of_seeds`). Attempts that share a slice should be next to each other in the batch (as
    /// `group::SliceGrouper` makes them): they then share the slice's reads through the L2 cache.
    pub fn slice_attempts(
        &mut self,
        seeds: &[[u8; 32]],
        slices: &[usize],
    ) -> Result<Vec<Attempt>, GpuError> {
        if slices.len() != seeds.len() {
            return Err(GpuError::new("one slice for each seed"));
        }
        if slices.iter().any(|&b| b >= self.data.params.num_blocks) {
            return Err(GpuError::new("a slice is outside the dataset"));
        }
        self.run(seeds, Columns::Slices(slices))
    }

    fn run(&mut self, seeds: &[[u8; 32]], cols: Columns<'_>) -> Result<Vec<Attempt>, GpuError> {
        let p = self.data.params;
        let n = seeds.len();
        if n == 0 || n > self.batch {
            return Err(GpuError::new("between 1 and `batch` seeds"));
        }
        let gpu = self.gpu;
        let stream = &gpu.stream;
        let keys: Vec<i64> = seeds.iter().flat_map(key_words_i64).collect();
        stream.memcpy_htod(&keys, &mut self.keys.slice_mut(0..n * 8))?;
        // X
        let bpk = (p.m * p.k / 64) as u64;
        gpu.launch_keystream(&self.keys, &self.x, bpk, 0, n as u64 * bpk)?;
        let nb = p.nb as u32;
        let columns = (p.num_blocks * p.nb) as u32;
        let idx_stride: u32 = match cols {
            Columns::Gathered { same } => {
                // the column numbers: nb words per attempt, from the pick key's keystream
                let pick: Vec<i64> = seeds
                    .iter()
                    .flat_map(|s| key_words_i64(&mh::pick_key(s)))
                    .collect();
                stream.memcpy_htod(&pick, &mut self.pick_keys.slice_mut(0..n * 8))?;
                let ipk = (p.nb / 16) as u64;
                gpu.launch_keystream(&self.pick_keys, &self.idx, ipk, 0, n as u64 * ipk)?;
                if same {
                    0
                } else {
                    nb
                }
            }
            Columns::Slices(slices) => {
                let s32: Vec<u32> = slices.iter().map(|&b| b as u32).collect();
                stream.memcpy_htod(&s32, &mut self.slices.slice_mut(0..n))?;
                0
            }
        };
        let slice_index_of = |i: usize| match cols {
            Columns::Slices(slices) => slices[i],
            Columns::Gathered { .. } => 0,
        };

        // SAFETY of the launch (`Gpu::launch_multiply`): m == 64, k % 128 == 0 and nb % 128 == 0 and the whole dataset
        // were checked in `new`; x, c, idx and slices hold `batch >= n` attempts' worth; slices were checked to be inside
        // the dataset in `slice_attempts`, and gathered columns are taken modulo `columns`, all inside the dataset.
        gpu.launch_multiply(
            &self.x,
            &self.data.buf,
            &self.idx,
            match cols {
                Columns::Slices(_) => Some(&self.slices),
                Columns::Gathered { .. } => None,
            },
            &self.c,
            (p.k, p.nb, columns as usize),
            idx_stride,
            n,
        )?;

        stream.memset_zeros(&mut self.sums)?;
        gpu.launch_fold(&self.c, &self.sums, (p.m * p.nb / 16) as u64, n as u32)?;
        let sums = stream.clone_dtoh(&self.sums.slice(0..n * 8))?;
        Ok(seeds
            .iter()
            .zip(sums.chunks(8))
            .enumerate()
            .map(|(i, (seed, s))| {
                let sums: [u64; 8] = s.try_into().expect("8 sums");
                let mix = mh::mix_bytes(&sums);
                Attempt {
                    seed: *seed,
                    slice_index: slice_index_of(i),
                    sums,
                    mix,
                    digest: mh::digest_of(seed, &mix),
                }
            })
            .collect())
    }
}
