//! The GPU miner's engine, in Rust, without Python. Experimental and unaudited.
//!
//! It runs the CUDA kernels of `kernels/` (`matmulhash.cu`, kept identical to `reference/tenero/gpubackend.py` by
//! `reference/tests/test_kernel_source_copy.py`; `fast.cu`; `gather.cu`, the exact int8 multiply of both proof-of-work
//! designs on the tensor cores). Everything is checked against the CPU code in `tenero-core`, which is checked against
//! the golden vectors.
//!
//! **Only the NVIDIA driver is needed at run time** (no CUDA Toolkit, no NVRTC, no cuBLAS): the kernels are compiled ahead
//! of time into fat binaries (`kernels/build_kernels.py`; machine code for sm_80 to sm_120 and PTX the driver compiles
//! for newer GPUs), embedded in the program with `include_bytes!` and loaded by the driver. A GPU needs compute
//! capability 8.0 or newer (RTX 30 series, A100 and later) and a driver for CUDA 13 (R580 or newer).
//!
//! Unsafe code is confined to the calls into CUDA, each with a `SAFETY` comment.

pub mod gather;
pub mod group;

use cudarc::driver::sys::CUfunction_attribute;
use cudarc::driver::{
    CudaContext, CudaFunction, CudaSlice, CudaStream, DriverError, LaunchConfig, PinnedHostSlice,
    PushKernelArg,
};
use cudarc::nvrtc::Ptx;
use std::collections::VecDeque;
use std::sync::Arc;
use tenero_core::matmulhash::{self as mh, Attempt, Params};
use tenero_core::u256::U256;

/// The CUDA source: three kernels (keystream, dataset fill, fold), the same text as the Python reference's.
pub const KERNEL_SOURCE: &str = include_str!("../kernels/matmulhash.cu");

/// Faster versions of the keystream and the fold (the same values, better memory access), for this engine only.
/// Compiled after `KERNEL_SOURCE`, in one module, because it uses its ChaCha20 functions.
pub const FAST_KERNEL_SOURCE: &str = include_str!("../kernels/fast.cu");

/// The multiply of both proof-of-work designs (`gather.cu`).
pub const GATHER_KERNEL_SOURCE: &str = include_str!("../kernels/gather.cu");

/// `KERNEL_SOURCE` and `FAST_KERNEL_SOURCE`, compiled ahead of time (`kernels/build_kernels.py`).
pub const MATMULHASH_FATBIN: &[u8] = include_bytes!("../kernels/matmulhash.fatbin");

/// `GATHER_KERNEL_SOURCE`, compiled ahead of time.
pub const GATHER_FATBIN: &[u8] = include_bytes!("../kernels/gather.fatbin");

/// The source hashes the fat binaries were built from (`kernels.sha256`; a test checks they are current).
pub const KERNEL_HASHES: &str = include_str!("../kernels/kernels.sha256");

/// The oldest GPU the kernels run on: compute capability 8.0 (the gathered multiply needs `cp.async` and int8
/// `mma.sync m16n8k32`).
pub const MIN_COMPUTE_CAPABILITY: (i32, i32) = (8, 0);

/// `G_SMEM` in `gather.cu`: 3 stages of (64 + 128) rows of 144 bytes of shared memory.
const GATHER_SMEM: u32 = 3 * (64 + 128) * 144;

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

/// A CUDA device with the kernels loaded.
pub struct Gpu {
    stream: Arc<CudaStream>,
    keystream_fn: CudaFunction,
    fill_fn: CudaFunction,
    fold_fn: CudaFunction,
    gather_fn: CudaFunction,
    read_fn: CudaFunction,
    pub name: String,
    pub compute_capability: (i32, i32),
}

/// What a kernel load failure means, in words a miner can act on.
fn load_error(name: &str, cc: (i32, i32), e: DriverError) -> GpuError {
    GpuError(format!(
        "the GPU kernels could not be loaded on {name} (compute capability {}.{}): {e:?}. They need an NVIDIA driver \
         for CUDA 13 (R580 or newer): update the driver. (The CUDA Toolkit is NOT needed.)",
        cc.0, cc.1
    ))
}

impl Gpu {
    /// Opens device `ordinal` and loads the embedded kernels (the driver picks the machine code for this GPU, or
    /// compiles the PTX for a GPU newer than all of them). Needs compute capability 8.0 or newer.
    pub fn new(ordinal: usize) -> Result<Gpu, GpuError> {
        let ctx = CudaContext::new(ordinal)?;
        let compute_capability = ctx.compute_capability()?;
        let name = ctx.name()?;
        if compute_capability < MIN_COMPUTE_CAPABILITY {
            return Err(GpuError(format!(
                "{name} has compute capability {}.{}; the miner needs {}.{} or newer (an RTX 30 series, A100 or later \
                 NVIDIA GPU)",
                compute_capability.0,
                compute_capability.1,
                MIN_COMPUTE_CAPABILITY.0,
                MIN_COMPUTE_CAPABILITY.1
            )));
        }
        let load = |image: &[u8]| {
            ctx.load_module(Ptx::from_binary(image.to_vec()))
                .map_err(|e| load_error(&name, compute_capability, e))
        };
        let module = load(MATMULHASH_FATBIN)?;
        let gather = load(GATHER_FATBIN)?;
        let gather_fn = gather.load_function("gather_gemm")?;
        // more than the default 48 KiB of shared memory (G_SMEM in gather.cu), and as much of the cache as shared memory
        gather_fn.set_attribute(
            CUfunction_attribute::CU_FUNC_ATTRIBUTE_MAX_DYNAMIC_SHARED_SIZE_BYTES,
            GATHER_SMEM as i32,
        )?;
        gather_fn.set_attribute(
            CUfunction_attribute::CU_FUNC_ATTRIBUTE_PREFERRED_SHARED_MEMORY_CARVEOUT,
            100,
        )?;
        Ok(Gpu {
            stream: ctx.new_stream()?,
            // the fast versions: the same arguments and results as keystream_kernel and fold_kernel
            keystream_fn: module.load_function("keystream_fast")?,
            fill_fn: module.load_function("fill_kernel")?,
            fold_fn: module.load_function("fold_fast")?,
            gather_fn,
            read_fn: gather.load_function("gather_read")?,
            name,
            compute_capability,
        })
    }

    /// `C = X @ W` for `n` attempts on the tensor cores (`gather_gemm`): attempt `a`'s 64 rows of X at `a * 64 * k` in
    /// `x`, its result at `a * 64 * nb` in `c`. Its columns are, in slice mode (`slices: Some`), the `nb` columns of
    /// slice `slices[a]` of `data` (the first design); otherwise dataset column `idx[a * idx_stride + n] % columns`
    /// (the gathered attempt). The caller guarantees the buffer sizes and slice numbers (see the SAFETY comment).
    #[allow(clippy::too_many_arguments)]
    fn launch_multiply(
        &self,
        x: &CudaSlice<u8>,
        data: &CudaSlice<u8>,
        idx: &CudaSlice<u8>,
        slices: Option<&CudaSlice<u32>>,
        c: &CudaSlice<i32>,
        (k, nb, columns): (usize, usize, usize),
        idx_stride: u32,
        n: usize,
    ) -> Result<(), GpuError> {
        let cfg = LaunchConfig {
            grid_dim: ((nb / 128) as u32, n as u32, 1),
            block_dim: (THREADS, 1, 1),
            shared_mem_bytes: GATHER_SMEM,
        };
        let (k, nb, columns) = (k as u32, nb as u32, columns as u32);
        let slice_mode: u32 = u32::from(slices.is_some());
        // in gathered mode the kernel never reads `slices`; any device buffer will do for the argument
        let mut b = self.stream.launch_builder(&self.gather_fn);
        b.arg(x)
            .arg(data)
            .arg(idx)
            .arg(c)
            .arg(&k)
            .arg(&nb)
            .arg(&columns)
            .arg(&idx_stride);
        match slices {
            Some(s) => b.arg(s),
            None => b.arg(idx),
        };
        b.arg(&slice_mode);
        // SAFETY: the kernel is `gather_gemm(u64 x, u64 data, u64 idx, u64 c, u32 k, u32 nb, u32 columns, u32 stride,
        // u64 slices, u32 slice_mode)` and the arguments have those types. The callers guarantee: m == 64, k % 128 == 0,
        // nb % 128 == 0; `x` holds n * 64 * k bytes and `c` n * 64 * nb int32; in slice mode `slices` holds n numbers,
        // each with (slice + 1) * nb * k <= data's length; in gathered mode `idx` holds the words a * stride + [0, nb)
        // for every a < n and `columns * k` <= data's length. The function was allowed GATHER_SMEM bytes of shared
        // memory in `new`, which this launch asks for.
        #[allow(unsafe_code)]
        unsafe {
            b.launch(cfg)?;
        }
        Ok(())
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
    /// as stored (`nb*k` bytes, the transposed matrix); the result is `m*nb` int32 (row-major). `m` must be a
    /// multiple of 64 and `k` and `nb` multiples of 128 (the kernel's tiles; the chain's 64, 8192 and 2048 are).
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
        if m == 0 || !m.is_multiple_of(64) || !k.is_multiple_of(128) || !nb.is_multiple_of(128) {
            return Err(GpuError::new(
                "the multiply needs m a multiple of 64 and k and nb multiples of 128",
            ));
        }
        // every 64 rows of X are one "attempt" against slice 0, which is `w`
        let n = m / 64;
        let x_dev = self
            .stream
            .clone_htod(&x.iter().map(|&v| v as u8).collect::<Vec<u8>>())?;
        let w_dev = self.stream.clone_htod(w)?;
        let slices = self.stream.alloc_zeros::<u32>(n)?;
        let c_dev = self.stream.alloc_zeros::<i32>(m * nb)?;
        self.launch_multiply(
            &x_dev,
            &w_dev,
            &w_dev,
            Some(&slices),
            &c_dev,
            (k, nb, nb),
            0,
            n,
        )?;
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

    /// The device's practical memory bandwidth: copies `bytes` from one device buffer to another for about `seconds`.
    /// Returns GB/s counting the reads and the writes, and the number of copies made.
    pub fn copy_bandwidth(&self, bytes: usize, seconds: f64) -> Result<(f64, u64), GpuError> {
        let src = self.stream.alloc_zeros::<u8>(bytes)?;
        let mut dst = self.stream.alloc_zeros::<u8>(bytes)?;
        self.stream.memcpy_dtod(&src, &mut dst)?; // warm up
        self.stream.synchronize()?;
        let start = std::time::Instant::now();
        let mut copies = 0u64;
        while start.elapsed().as_secs_f64() < seconds {
            for _ in 0..8 {
                self.stream.memcpy_dtod(&src, &mut dst)?;
            }
            self.stream.synchronize()?;
            copies += 8;
        }
        let secs = start.elapsed().as_secs_f64();
        Ok((2.0 * bytes as f64 * copies as f64 / secs / 1e9, copies))
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

    /// Buffers for batches of up to `batch` attempts of the FIRST design (two sets of buffers: one batch can be
    /// computed while the next is prepared, `AttemptEngine::submit`). Needs m == 64 and k and nb multiples of 128.
    pub fn attempt_engine<'a>(
        &'a self,
        data: &'a DeviceDataset,
        batch: usize,
    ) -> Result<AttemptEngine<'a>, GpuError> {
        let p = data.params;
        if batch == 0 || batch > 65_535 {
            return Err(GpuError::new("the batch must be 1..=65535"));
        }
        if p.m != 64 || !p.k.is_multiple_of(128) || !p.nb.is_multiple_of(128) {
            return Err(GpuError::new(
                "the GPU engine needs m = 64 and k and nb multiples of 128",
            ));
        }
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
                slices: self.stream.alloc_zeros::<u32>(batch)?,
                slices_host: unsafe_pinned_u32(ctx, batch)?,
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
            batch,
            slots,
            next: 0,
            queue: VecDeque::new(),
        })
    }
}

/// Page-locked host memory for `len` u32 values.
fn unsafe_pinned_u32(ctx: &Arc<CudaContext>, len: usize) -> Result<PinnedHostSlice<u32>, GpuError> {
    // SAFETY: the memory is not read before it is written: `AttemptEngine::submit` fills it before each copy to the GPU.
    #[allow(unsafe_code)]
    unsafe {
        Ok(ctx.alloc_pinned_with_flags::<u32>(len, 0)?)
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
    /// The slice of each attempt, in GPU order.
    slices: CudaSlice<u32>,
    slices_host: PinnedHostSlice<u32>,
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
    /// The attempts are sorted by slice and multiplied in one launch of our own kernel (`gather_gemm` in slice mode).
    /// Attempts that share a slice then run one after another and find it in the GPU's L2 cache, so a batch whose
    /// attempts share few slices reads less of the dataset (`group::SliceGrouper` makes such batches).
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

        // 1. X for every attempt: the keystream keyed by its seed (and each attempt's slice, in the same order)
        {
            // waits until the last copy out of these buffers has finished (it has: the batch was collected)
            let keys_host = slot.keys_host.as_mut_slice()?;
            for (j, &i) in order.iter().enumerate() {
                keys_host[j * 8..j * 8 + 8].copy_from_slice(&key_words_i64(&seeds[i]));
            }
            let slices_host = slot.slices_host.as_mut_slice()?;
            for (j, &i) in order.iter().enumerate() {
                slices_host[j] = slices[i] as u32;
            }
        }
        stream.memcpy_htod(&slot.keys_host, &mut slot.keys)?;
        stream.memcpy_htod(&slot.slices_host, &mut slot.slices)?;
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

        // 2. C = X @ W_b for every attempt, in one launch (slice mode; `idx` is not read, the keys buffer stands in)
        let mnb = p.m * p.nb;
        self.gpu.launch_multiply(
            &slot.x,
            &self.data.buf,
            &slot.x,
            Some(&slot.slices),
            &slot.c,
            (p.k, p.nb, self.data.slices * p.nb),
            0,
            n,
        )?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use tenero_core::hash::{hex_lower, sha256};

    /// The fat binaries are built from the sources by `kernels/build_kernels.py`, which records the sources' hashes in
    /// `kernels.sha256`. A change to a `.cu` file without rebuilding them fails here (in CI too, which has no CUDA): the
    /// program would otherwise run kernels that are not the ones in the repository.
    #[test]
    fn the_embedded_kernels_were_built_from_these_sources() {
        let lf = |s: &str| s.replace("\r\n", "\n");
        let modules = [
            (
                "matmulhash",
                format!("{}\n{}", lf(KERNEL_SOURCE), lf(FAST_KERNEL_SOURCE)),
            ),
            ("gather", lf(GATHER_KERNEL_SOURCE)),
        ];
        for (name, source) in modules {
            let want = KERNEL_HASHES
                .lines()
                .filter(|l| !l.starts_with('#'))
                .find_map(|l| l.split_once("  ").filter(|(_, m)| m.trim() == name))
                .map(|(h, _)| h.trim().to_string())
                .unwrap_or_else(|| panic!("{name} is not in kernels.sha256"));
            assert_eq!(
                hex_lower(&sha256(&[source.as_bytes()])),
                want,
                "kernels/{name}.fatbin is stale: run python crates/tenero-gpu/kernels/build_kernels.py and commit the result"
            );
        }
        assert!(MATMULHASH_FATBIN.len() > 1024 && GATHER_FATBIN.len() > 1024);
    }
}
