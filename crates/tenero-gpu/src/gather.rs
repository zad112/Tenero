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
use cudarc::driver::sys::CUfunction_attribute;
use cudarc::driver::{CudaFunction, CudaSlice, LaunchConfig, PushKernelArg};
use cudarc::nvrtc::{compile_ptx_with_opts, CompileOptions};
use tenero_core::matmulhash::{self as mh, Attempt};

pub const GATHER_KERNEL_SOURCE: &str = include_str!("../kernels/gather.cu");

/// `G_SMEM` in `gather.cu`: 3 stages of (64 + 128) rows of 144 bytes.
const GATHER_SMEM: u32 = 3 * (64 + 128) * 144;

/// Batches of gathered attempts on the GPU (one at a time; no pipeline: this is for measuring).
pub struct GatherEngine<'a> {
    gpu: &'a Gpu,
    data: &'a DeviceDataset,
    gemm_fn: CudaFunction,
    read_fn: CudaFunction,
    read_out: CudaSlice<u32>,
    batch: usize,
    keys: CudaSlice<i64>,
    pick_keys: CudaSlice<i64>,
    x: CudaSlice<u8>,
    idx: CudaSlice<u8>,
    c: CudaSlice<i32>,
    sums: CudaSlice<u64>,
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
                "the gather prototype needs m = 64, k a multiple of 128 and nb a multiple of 128",
            ));
        }
        if data.slices != p.num_blocks {
            return Err(GpuError::new(
                "the gather prototype needs the whole dataset",
            ));
        }
        if batch == 0 || batch > 65_535 {
            return Err(GpuError::new("the batch must be 1..=65535"));
        }
        let (major, minor) = gpu.compute_capability;
        let arch: &'static str = Box::leak(format!("compute_{major}{minor}").into_boxed_str());
        let ptx = compile_ptx_with_opts(
            GATHER_KERNEL_SOURCE,
            CompileOptions {
                arch: Some(arch),
                ..Default::default()
            },
        )?;
        let module = gpu.stream.context().load_module(ptx)?;
        let gemm_fn = module.load_function("gather_gemm")?;
        // more than the default 48 KiB of shared memory (G_SMEM in gather.cu), and as much of the cache as shared memory
        gemm_fn.set_attribute(
            CUfunction_attribute::CU_FUNC_ATTRIBUTE_MAX_DYNAMIC_SHARED_SIZE_BYTES,
            GATHER_SMEM as i32,
        )?;
        gemm_fn.set_attribute(
            CUfunction_attribute::CU_FUNC_ATTRIBUTE_PREFERRED_SHARED_MEMORY_CARVEOUT,
            100,
        )?;
        let s = &gpu.stream;
        Ok(GatherEngine {
            gpu,
            data,
            gemm_fn,
            read_fn: module.load_function("gather_read")?,
            read_out: s.alloc_zeros::<u32>(batch * (p.nb / 128) * 256)?,
            batch,
            keys: s.alloc_zeros::<i64>(batch * 8)?,
            pick_keys: s.alloc_zeros::<i64>(batch * 8)?,
            x: s.alloc_zeros::<u8>(batch * p.m * p.k)?,
            idx: s.alloc_zeros::<u8>(batch * p.nb * 4)?,
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
        let mut b = stream.launch_builder(&self.read_fn);
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
        let p = self.data.params;
        let n = seeds.len();
        if n == 0 || n > self.batch {
            return Err(GpuError::new("between 1 and `batch` seeds"));
        }
        let gpu = self.gpu;
        let stream = &gpu.stream;
        let keys: Vec<i64> = seeds.iter().flat_map(key_words_i64).collect();
        let pick: Vec<i64> = seeds
            .iter()
            .flat_map(|s| key_words_i64(&mh::pick_key(s)))
            .collect();
        stream.memcpy_htod(&keys, &mut self.keys.slice_mut(0..n * 8))?;
        stream.memcpy_htod(&pick, &mut self.pick_keys.slice_mut(0..n * 8))?;
        // X, and the column numbers (nb words per attempt, from the pick key's keystream)
        let bpk = (p.m * p.k / 64) as u64;
        gpu.launch_keystream(&self.keys, &self.x, bpk, 0, n as u64 * bpk)?;
        let ipk = (p.nb / 16) as u64;
        gpu.launch_keystream(&self.pick_keys, &self.idx, ipk, 0, n as u64 * ipk)?;

        let cfg = LaunchConfig {
            grid_dim: ((p.nb / 128) as u32, n as u32, 1),
            block_dim: (THREADS, 1, 1),
            shared_mem_bytes: GATHER_SMEM,
        };
        let (k, nb) = (p.k as u32, p.nb as u32);
        let columns = (p.num_blocks * p.nb) as u32;
        let idx_stride: u32 = if same_columns { 0 } else { nb };
        let mut b = stream.launch_builder(&self.gemm_fn);
        b.arg(&self.x)
            .arg(&self.data.buf)
            .arg(&self.idx)
            .arg(&self.c)
            .arg(&k)
            .arg(&nb)
            .arg(&columns)
            .arg(&idx_stride);
        // SAFETY: the kernel is `gather_gemm(u64 x, u64 data, u64 idx, u64 c, u32 k, u32 nb, u32 columns, u32 stride)`
        // and the arguments have those types. Block (bx, a) reads X rows of attempt a < n (`x` holds batch * 64 * k
        // bytes), idx words a * stride + [0, nb) (`idx` holds batch * nb words), and dataset columns `idx % columns`, each
        // k bytes inside the whole dataset (`data.slices == num_blocks`, checked in `new`); it writes 64 rows x 128
        // columns of attempt a's C (`c` holds batch * 64 * nb). m == 64, k % 128 == 0 and nb % 128 == 0 were checked,
        // and the function was allowed the GATHER_SMEM bytes of shared memory the launch asks for.
        #[allow(unsafe_code)]
        unsafe {
            b.launch(cfg)?;
        }

        stream.memset_zeros(&mut self.sums)?;
        gpu.launch_fold(&self.c, &self.sums, (p.m * p.nb / 16) as u64, n as u32)?;
        let sums = stream.clone_dtoh(&self.sums.slice(0..n * 8))?;
        Ok(seeds
            .iter()
            .zip(sums.chunks(8))
            .map(|(seed, s)| {
                let sums: [u64; 8] = s.try_into().expect("8 sums");
                let mix = mh::mix_bytes(&sums);
                Attempt {
                    seed: *seed,
                    slice_index: 0,
                    sums,
                    mix,
                    digest: mh::digest_of(seed, &mix),
                }
            })
            .collect())
    }
}
