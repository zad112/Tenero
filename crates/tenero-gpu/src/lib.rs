//! The GPU miner's engine, in Rust, without Python. Experimental and unaudited.
//!
//! It runs the same three CUDA kernels as the Python miner (`kernels/matmulhash.cu`, kept
//! identical to `reference/tenero/gpubackend.py` by `reference/tests/test_kernel_source_copy.py`), compiled at run time
//! with NVRTC, and the exact int8 matrix multiply through cuBLASLt (`gemm`). Everything is checked
//! against the CPU code in `tenero-core`, which is checked against the golden vectors.
//!
//! Unsafe code is confined to the calls into CUDA, each with a `SAFETY` comment.

pub mod gemm;
pub mod group;

use cudarc::cublaslt::result::CublasError;
use cudarc::driver::{
    CudaContext, CudaFunction, CudaSlice, CudaStream, DriverError, LaunchConfig, PinnedHostSlice,
    PushKernelArg,
};
use cudarc::nvrtc::{compile_ptx_with_opts, CompileError, CompileOptions};
use gemm::Int8Gemm;
use std::collections::VecDeque;
use std::sync::Arc;
use tenero_core::matmulhash::{self as mh, Attempt, Params};
use tenero_core::u256::U256;

/// The CUDA source: three kernels (keystream, dataset fill, fold), the same text as the Python reference's.
pub const KERNEL_SOURCE: &str = include_str!("../kernels/matmulhash.cu");

/// Faster versions of the keystream and the fold (the same values, better memory access), for this engine only.
/// Compiled after `KERNEL_SOURCE`, in one program, because it uses its ChaCha20 functions.
pub const FAST_KERNEL_SOURCE: &str = include_str!("../kernels/fast.cu");

const THREADS: u32 = 256;
const FOLD_BLOCKS: u32 = 4;
const MAX_GRID: u64 = 1 << 20;

#[derive(Debug)]
pub struct GpuError(String);

impl GpuError {
    pub fn new(msg: &str) -> GpuError {
        GpuError(msg.to_string())
    }
}

impl std::fmt::Display for GpuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GpuError {}

impl From<DriverError> for GpuError {
    fn from(e: DriverError) -> GpuError {
        GpuError(format!("CUDA driver: {e:?}"))
    }
}

impl From<CompileError> for GpuError {
    fn from(e: CompileError) -> GpuError {
        GpuError(format!("NVRTC: {e:?}"))
    }
}

impl From<CublasError> for GpuError {
    fn from(e: CublasError) -> GpuError {
        GpuError(format!("cuBLASLt: {e:?}"))
    }
}

impl From<String> for GpuError {
    fn from(e: String) -> GpuError {
        GpuError(e)
    }
}

fn grid(total: u64) -> u32 {
    u32::try_from(total.div_ceil(u64::from(THREADS)).min(MAX_GRID)).expect("capped at 2^20")
}

fn key_words_i64(key: &[u8; 32]) -> [i64; 8] {
    let w = tenero_core::chacha20::key_words(key);
    w.map(i64::from)
}

/// A CUDA device with the kernels compiled and loaded.
pub struct Gpu {
    stream: Arc<CudaStream>,
    keystream_fn: CudaFunction,
    fill_fn: CudaFunction,
    fold_fn: CudaFunction,
    pub name: String,
    pub compute_capability: (i32, i32),
}

impl Gpu {
    /// Opens device `ordinal`, compiles the kernels for its architecture and loads them.
    pub fn new(ordinal: usize) -> Result<Gpu, GpuError> {
        let ctx = CudaContext::new(ordinal)?;
        let compute_capability = ctx.compute_capability()?;
        let name = ctx.name()?;
        let arch: &'static str = Box::leak(
            format!("compute_{}{}", compute_capability.0, compute_capability.1).into_boxed_str(),
        );
        let ptx = compile_ptx_with_opts(
            format!("{KERNEL_SOURCE}\n{FAST_KERNEL_SOURCE}"),
            CompileOptions {
                arch: Some(arch),
                ..Default::default()
            },
        )?;
        let module = ctx.load_module(ptx)?;
        Ok(Gpu {
            stream: ctx.new_stream()?,
            // the fast versions: the same arguments and results as keystream_kernel and fold_kernel
            keystream_fn: module.load_function("keystream_fast")?,
            fill_fn: module.load_function("fill_kernel")?,
            fold_fn: module.load_function("fold_fast")?,
            name,
            compute_capability,
        })
    }

    pub fn synchronize(&self) -> Result<(), GpuError> {
        Ok(self.stream.synchronize()?)
    }

    /// `blocks_per_key` ChaCha20 blocks for each key, from block counter `start_counter`, nonce 0:
    /// `keys.len() * blocks_per_key * 64` bytes (the counter wraps modulo 2^32).
    pub fn keystream(
        &self,
        keys: &[[u8; 32]],
        blocks_per_key: usize,
        start_counter: u64,
    ) -> Result<Vec<u8>, GpuError> {
        let host: Vec<i64> = keys.iter().flat_map(key_words_i64).collect();
        let keys_dev = self.stream.clone_htod(&host)?;
        let total = (keys.len() * blocks_per_key) as u64;
        let out = self.stream.alloc_zeros::<u8>(total as usize * 64)?;
        self.launch_keystream(&keys_dev, &out, blocks_per_key as u64, start_counter, total)?;
        Ok(self.stream.clone_dtoh(&out)?)
    }

    fn launch_keystream(
        &self,
        keys: &CudaSlice<i64>,
        out: &CudaSlice<u8>,
        blocks_per_key: u64,
        start_counter: u64,
        total: u64,
    ) -> Result<(), GpuError> {
        let cfg = LaunchConfig {
            grid_dim: (grid(total), 1, 1),
            block_dim: (THREADS, 1, 1),
            shared_mem_bytes: 0,
        };
        let mut b = self.stream.launch_builder(&self.keystream_fn);
        b.arg(keys)
            .arg(out)
            .arg(&blocks_per_key)
            .arg(&start_counter)
            .arg(&total);
        // SAFETY: the kernel is `keystream_kernel(u64 keys_ptr, u64 out_ptr, u64 blocks_per_key,
        // u64 start_counter, u64 total_blocks)`; the arguments have those types. `keys` holds 8 words
        // per key and `out` holds `total * 64` bytes, so every thread's reads and writes are in range.
        #[allow(unsafe_code)]
        unsafe {
            b.launch(cfg)?;
        }
        Ok(())
    }

    /// The fold of `batch` products of `chunks * 16` int32 values each, from a flat host array.
    pub fn fold_sums(&self, c: &[i32], chunks: usize) -> Result<Vec<[u64; 8]>, GpuError> {
        let per = chunks * 16;
        if per == 0 || !c.len().is_multiple_of(per) {
            return Err(GpuError::new("c must hold a whole number of products"));
        }
        let batch = c.len() / per;
        let c_dev = self.stream.clone_htod(c)?;
        let sums = self.stream.alloc_zeros::<u64>(batch * 8)?;
        self.launch_fold(&c_dev, &sums, chunks as u64, batch as u32)?;
        Ok(self
            .stream
            .clone_dtoh(&sums)?
            .chunks(8)
            .map(|s| s.try_into().expect("8 sums"))
            .collect())
    }

    fn launch_fold(
        &self,
        c: &CudaSlice<i32>,
        sums: &CudaSlice<u64>,
        chunks: u64,
        batch: u32,
    ) -> Result<(), GpuError> {
        let cfg = LaunchConfig {
            grid_dim: (FOLD_BLOCKS.min(grid(chunks)), batch, 1),
            block_dim: (THREADS, 1, 1),
            shared_mem_bytes: 0,
        };
        let mut b = self.stream.launch_builder(&self.fold_fn);
        b.arg(c).arg(sums).arg(&chunks);
        // SAFETY: the kernel is `fold_kernel(u64 c_ptr, u64 sums_ptr, u64 chunks)`; `c` holds
        // `batch * chunks * 16` int32 values and `sums` `batch * 8` u64 (zeroed), which is all it touches.
        #[allow(unsafe_code)]
        unsafe {
            b.launch(cfg)?;
        }
        Ok(())
    }

    /// `x @ w` on the tensor cores, from host arrays: `x` is `m*k` int8 (row-major), `w` is one slice
    /// as stored (`nb*k` bytes, the transposed matrix); the result is `m*nb` int32 (row-major).
    pub fn int8_matmul(
        &self,
        x: &[i8],
        w: &[u8],
        m: usize,
        k: usize,
        nb: usize,
    ) -> Result<Vec<i32>, GpuError> {
        if x.len() != m * k || w.len() != nb * k {
            return Err(GpuError::new("x must be m*k and w nb*k"));
        }
        let mut gemm = Int8Gemm::new(self.stream.clone(), m, k, nb)?;
        let x_dev = self
            .stream
            .clone_htod(&x.iter().map(|&v| v as u8).collect::<Vec<u8>>())?;
        let w_dev = self.stream.clone_htod(w)?;
        let mut c_dev = self.stream.alloc_zeros::<i32>(m * nb)?;
        gemm.run(&w_dev, &x_dev, &mut c_dev)?;
        Ok(self.stream.clone_dtoh(&c_dev)?)
    }

    /// Builds the first `slices` slices of the epoch's dataset in device memory (all of them when
    /// `slices == params.num_blocks`): slice 0 from the keystream, then each from the ones before.
    pub fn build_dataset(
        &self,
        params: &Params,
        epoch_seed: &[u8; 32],
        slices: usize,
    ) -> Result<DeviceDataset, GpuError> {
        params.validate()?;
        if slices == 0 || slices > params.num_blocks {
            return Err(GpuError(format!(
                "slices must be 1..={}",
                params.num_blocks
            )));
        }
        let blocks = params.blocks_per_slice() as u64;
        let buf = self
            .stream
            .alloc_zeros::<u8>(slices * params.slice_bytes())?;
        let key = self
            .stream
            .clone_htod(&key_words_i64(&mh::dataset_key(epoch_seed)))?;
        self.launch_keystream(&key, &buf, blocks, 0, blocks)?;
        let cfg = LaunchConfig {
            grid_dim: (grid(blocks), 1, 1),
            block_dim: (THREADS, 1, 1),
            shared_mem_bytes: 0,
        };
        for j in 1..slices {
            let j = u32::try_from(j).map_err(|_| GpuError::new("too many slices"))?;
            let mut b = self.stream.launch_builder(&self.fill_fn);
            b.arg(&buf).arg(&blocks).arg(&j);
            // SAFETY: the kernel is `fill_kernel(u64 data_ptr, u64 blocks_per_slice, u32 slice_j)`;
            // `buf` holds `slices` whole slices and `1 <= j < slices`, so it reads slices 0..j and
            // writes slice j, all inside the allocation. Slices are filled in order on one stream.
            #[allow(unsafe_code)]
            unsafe {
                b.launch(cfg)?;
            }
        }
        Ok(DeviceDataset {
            params: *params,
            slices,
            buf,
        })
    }

    /// One slice of the dataset, copied to the host.
    pub fn read_slice(&self, data: &DeviceDataset, j: usize) -> Result<Vec<u8>, GpuError> {
        if j >= data.slices {
            return Err(GpuError::new("slice not built"));
        }
        let sb = data.params.slice_bytes();
        Ok(self
            .stream
            .clone_dtoh(&data.buf.slice(j * sb..(j + 1) * sb))?)
    }

    /// Buffers and a prepared GEMM for batches of up to `batch` attempts (two sets of buffers: one batch can be
    /// computed while the next is prepared, `AttemptEngine::submit`).
    pub fn attempt_engine<'a>(
        &'a self,
        data: &'a DeviceDataset,
        batch: usize,
    ) -> Result<AttemptEngine<'a>, GpuError> {
        let p = data.params;
        if batch == 0 {
            return Err(GpuError::new("the batch must be at least 1"));
        }
        let gemm = Int8Gemm::new(self.stream.clone(), p.m, p.k, p.nb)?;
        let ctx = self.stream.context();
        let mut slots = Vec::new();
        for _ in 0..2 {
            // SAFETY: page-locked host memory that is not read before it is written: `keys_host` is filled before each
            // copy to the GPU, and `sums_host` is read only after a copy from the GPU into it has finished.
            #[allow(unsafe_code)]
            let (keys_host, sums_host) = unsafe {
                (
                    ctx.alloc_pinned_with_flags::<i64>(batch * 8, 0)?,
                    ctx.alloc_pinned_with_flags::<u64>(batch * 8, 0)?,
                )
            };
            slots.push(Slot {
                keys: self.stream.alloc_zeros::<i64>(batch * 8)?,
                keys_host,
                x: self.stream.alloc_zeros::<u8>(batch * p.m * p.k)?,
                c: self.stream.alloc_zeros::<i32>(batch * p.m * p.nb)?,
                sums: self.stream.alloc_zeros::<u64>(batch * 8)?,
                sums_host,
                pending: None,
            });
        }
        Ok(AttemptEngine {
            gpu: self,
            data,
            gemm,
            batch,
            slots,
            next: 0,
            queue: VecDeque::new(),
        })
    }
}

/// The epoch's dataset (or its first slices) in device memory.
pub struct DeviceDataset {
    params: Params,
    slices: usize,
    buf: CudaSlice<u8>,
}

impl DeviceDataset {
    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn slices(&self) -> usize {
        self.slices
    }
}

/// A nonce that met the target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub nonce: u64,
    pub attempt: Attempt,
}

/// The buffers of one batch: device memory, and page-locked host memory for the copies (a copy from ordinary host
/// memory waits for the whole stream, which would stop the next batch from being queued while one runs).
struct Slot {
    keys: CudaSlice<i64>,
    keys_host: PinnedHostSlice<i64>,
    x: CudaSlice<u8>,
    c: CudaSlice<i32>,
    sums: CudaSlice<u64>,
    sums_host: PinnedHostSlice<u64>,
    /// The batch in these buffers, submitted and not yet collected.
    pending: Option<Pending>,
}

struct Pending {
    seeds: Vec<[u8; 32]>,
    slices: Vec<usize>,
    /// Position `j` on the GPU is attempt `order[j]` of the batch (the batch sorted by slice).
    order: Vec<usize>,
}

/// Runs batches of attempts against a device dataset.
///
/// Either one batch at a time (`attempts`, `attempts_of_seeds`), or as a pipeline: `submit` queues a batch on the GPU
/// and returns at once, `collect` waits for the oldest one. With two submitted, the GPU starts the second as soon as
/// the first is done, while the CPU works out the first one's hashes and the next batch's nonces.
pub struct AttemptEngine<'a> {
    gpu: &'a Gpu,
    data: &'a DeviceDataset,
    gemm: Int8Gemm,
    batch: usize,
    slots: Vec<Slot>,
    /// The slot the next `submit` uses.
    next: usize,
    /// Submitted batches, oldest first (slot numbers).
    queue: VecDeque<usize>,
}

impl AttemptEngine<'_> {
    pub fn batch(&self) -> usize {
        self.batch
    }

    /// Picks the fastest of cuBLASLt's algorithms for a multiply of `group` attempts against one slice
    /// (`Int8Gemm::tune`, `reps` multiplies each, on this engine's own buffers, going round the built slices); returns
    /// the times.
    /// Nothing may be in flight. The results do not depend on the algorithm, only the speed does.
    pub fn tune(&mut self, group: usize, reps: usize) -> Result<Vec<f64>, GpuError> {
        let p = self.data.params;
        if group == 0 || group > self.batch {
            return Err(GpuError::new("the group must be 1..=batch"));
        }
        if !self.queue.is_empty() {
            return Err(GpuError::new("a submitted batch was not collected"));
        }
        let rows = group * p.m;
        let slot = &mut self.slots[0];
        let sb = p.slice_bytes();
        let ws: Vec<_> = (0..self.data.slices)
            .map(|j| self.data.buf.slice(j * sb..(j + 1) * sb))
            .collect();
        let x = slot.x.slice(0..rows * p.k);
        let mut c = slot.c.slice_mut(0..rows * p.nb);
        self.gemm.tune(rows, &ws, &x, &mut c, reps)
    }

    /// How many batches are submitted and not yet collected (0, 1 or 2).
    pub fn in_flight(&self) -> usize {
        self.queue.len()
    }

    /// The full recomputation of these nonces on the GPU, in order.
    pub fn attempts(
        &mut self,
        header_hash: &[u8; 32],
        nonces: &[u64],
    ) -> Result<Vec<Attempt>, GpuError> {
        let num_blocks = self.data.params.num_blocks;
        let seeds: Vec<[u8; 32]> = nonces
            .iter()
            .map(|&nonce| mh::attempt_seed(header_hash, nonce))
            .collect();
        let slices: Vec<usize> = seeds
            .iter()
            .map(|s| mh::attempt_slice(s, num_blocks))
            .collect();
        self.attempts_of_seeds(&seeds, &slices)
    }

    /// The same, from each attempt's seed (`attempt_seed`) and slice (`attempt_slice` of that seed), for a caller that
    /// has already worked them out. **They are trusted**: a slice that is not the seed's gives a wrong attempt.
    /// Nothing may be in flight (`submit` without `collect`).
    pub fn attempts_of_seeds(
        &mut self,
        seeds: &[[u8; 32]],
        slices: &[usize],
    ) -> Result<Vec<Attempt>, GpuError> {
        if !self.queue.is_empty() {
            return Err(GpuError::new("a submitted batch was not collected"));
        }
        self.submit(seeds.to_vec(), slices.to_vec())?;
        Ok(self.collect()?.expect("one batch was submitted"))
    }

    /// Queues a batch on the GPU and returns without waiting (seeds and slices as in `attempts_of_seeds`). At most two
    /// can be in flight: `collect` one before submitting a third.
    ///
    /// The attempts are multiplied grouped by slice (one multiply per slice in the batch, `Int8Gemm::run_rows`), so a
    /// batch whose attempts share few slices reads less of the dataset (`group::SliceGrouper` makes such batches).
    pub fn submit(&mut self, seeds: Vec<[u8; 32]>, slices: Vec<usize>) -> Result<(), GpuError> {
        let p = self.data.params;
        let n = seeds.len();
        if n == 0 || n > self.batch {
            return Err(GpuError::new("between 1 and `batch` nonces"));
        }
        if slices.len() != n {
            return Err(GpuError::new("one slice for each seed"));
        }
        if slices.iter().any(|&b| b >= self.data.slices) {
            return Err(GpuError::new("an attempt reads a slice that was not built"));
        }
        if self.queue.len() >= self.slots.len() {
            return Err(GpuError::new(
                "two batches are in flight: collect one first",
            ));
        }
        let stream = &self.gpu.stream;
        let slot = &mut self.slots[self.next];
        // the attempts in slice order: position j on the GPU is attempt order[j]
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by_key(|&i| slices[i]);

        // 1. X for every attempt: the keystream keyed by its seed
        {
            // waits until the last copy out of these buffers has finished (it has: the batch was collected)
            let keys_host = slot.keys_host.as_mut_slice()?;
            for (j, &i) in order.iter().enumerate() {
                keys_host[j * 8..j * 8 + 8].copy_from_slice(&key_words_i64(&seeds[i]));
            }
        }
        stream.memcpy_htod(&slot.keys_host, &mut slot.keys)?;
        let bpk = (p.m * p.k / 64) as u64;
        let total = n as u64 * bpk;
        let cfg = LaunchConfig {
            grid_dim: (grid(total), 1, 1),
            block_dim: (THREADS, 1, 1),
            shared_mem_bytes: 0,
        };
        let mut b = stream.launch_builder(&self.gpu.keystream_fn);
        b.arg(&slot.keys)
            .arg(&slot.x)
            .arg(&bpk)
            .arg(&0u64)
            .arg(&total);
        // SAFETY: as in `Gpu::launch_keystream`: `keys` holds at least `n * 8` words and `x` holds
        // `batch * m * k >= n * bpk * 64` bytes.
        #[allow(unsafe_code)]
        unsafe {
            b.launch(cfg)?;
        }

        // 2. C = X @ W_b, one multiply for each run of attempts against the same slice
        let (sb, mk, mnb) = (p.slice_bytes(), p.m * p.k, p.m * p.nb);
        let mut start = 0;
        while start < n {
            let slice = slices[order[start]];
            let mut end = start + 1;
            while end < n && slices[order[end]] == slice {
                end += 1;
            }
            let w = self.data.buf.slice(slice * sb..(slice + 1) * sb);
            let x = slot.x.slice(start * mk..end * mk);
            let mut c = slot.c.slice_mut(start * mnb..end * mnb);
            self.gemm.run_rows((end - start) * p.m, &w, &x, &mut c)?;
            start = end;
        }

        // 3. the fold of every product, into the sums, and the sums to the host (without waiting)
        stream.memset_zeros(&mut slot.sums)?;
        self.gpu
            .launch_fold(&slot.c, &slot.sums, (mnb / 16) as u64, n as u32)?;
        stream.memcpy_dtoh(&slot.sums, &mut slot.sums_host)?;

        slot.pending = Some(Pending {
            seeds,
            slices,
            order,
        });
        self.queue.push_back(self.next);
        self.next = (self.next + 1) % self.slots.len();
        Ok(())
    }

    /// Waits for the oldest submitted batch and returns its attempts, in the order they were submitted; `None` if
    /// nothing is in flight.
    pub fn collect(&mut self) -> Result<Option<Vec<Attempt>>, GpuError> {
        let Some(s) = self.queue.pop_front() else {
            return Ok(None);
        };
        let slot = &mut self.slots[s];
        let pending = slot.pending.take().expect("a queued slot has a batch");
        let sums = slot.sums_host.as_slice()?; // waits for the copy, so for the whole batch
        let n = pending.seeds.len();
        let mut out: Vec<Option<Attempt>> = vec![None; n];
        for (&i, s) in pending.order.iter().zip(sums.chunks(8)) {
            let sums: [u64; 8] = s.try_into().expect("8 sums");
            let mix = mh::mix_bytes(&sums);
            out[i] = Some(Attempt {
                seed: pending.seeds[i],
                slice_index: pending.slices[i],
                sums,
                mix,
                digest: mh::digest_of(&pending.seeds[i], &mix),
            });
        }
        Ok(Some(
            out.into_iter().map(|a| a.expect("every attempt")).collect(),
        ))
    }

    /// Searches nonces `start_nonce, start_nonce + 1, ...` in batches until one meets the target or
    /// `max_nonces` have been tried. Returns the lowest nonce of the first batch containing a
    /// solution (like the Python searcher), and how many nonces were tried.
    pub fn search(
        &mut self,
        header_hash: &[u8; 32],
        target: &U256,
        start_nonce: u64,
        max_nonces: u64,
    ) -> Result<(Option<Found>, u64), GpuError> {
        let mut tried = 0u64;
        let mut nonce = start_nonce;
        while tried < max_nonces {
            let n = self
                .batch
                .min(usize::try_from(max_nonces - tried).unwrap_or(usize::MAX));
            let nonces: Vec<u64> = (0..n as u64).map(|i| nonce.wrapping_add(i)).collect();
            for (nonce, attempt) in nonces.iter().zip(self.attempts(header_hash, &nonces)?) {
                if mh::meets_target(&attempt.digest, target) {
                    return Ok((
                        Some(Found {
                            nonce: *nonce,
                            attempt,
                        }),
                        tried + n as u64,
                    ));
                }
            }
            tried += n as u64;
            nonce = nonce.wrapping_add(n as u64);
        }
        Ok((None, tried))
    }
}
