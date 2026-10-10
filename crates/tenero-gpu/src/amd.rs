//! AMD RDNA3 backend for matmulhash v2, through HIP and native signed INT8 rocWMMA.
//!
//! The C++ side owns all device allocations and implements dataset generation, the original
//! slice attempt, the gathered attempt (what `gamma` and `dev` use from block 0), and the consensus
//! fold. This wrapper keeps hashing and target comparison in the official Rust implementation.
//!
//! **Contributed, and not tested by the project's author** (no AMD card): RDNA3 (gfx11, the RX 7000
//! series) on Linux with ROCm 6 only. Its bit-for-bit check against the CPU code is
//! `tests/amd_selftest.rs`, which only an AMD card can run.

use std::ffi::{c_char, c_void, CStr};
use std::ptr::NonNull;

use tenero_core::matmulhash::{self as mh, Attempt, Params};

use crate::GpuError;

const ERROR_CAP: usize = 1024;

#[allow(unsafe_code)]
unsafe extern "C" {
    fn tenero_hip_create(
        ordinal: u32,
        batch: u32,
        k: u32,
        nb: u32,
        num_blocks: u32,
        out: *mut *mut c_void,
        error: *mut c_char,
        error_cap: usize,
    ) -> i32;
    fn tenero_hip_destroy(ctx: *mut c_void);
    fn tenero_hip_name(
        ctx: *mut c_void,
        name: *mut c_char,
        name_cap: usize,
        arch: *mut c_char,
        arch_cap: usize,
    ) -> i32;
    fn tenero_hip_build_dataset(
        ctx: *mut c_void,
        dataset_key: *const u8,
        milliseconds: *mut f32,
        error: *mut c_char,
        error_cap: usize,
    ) -> i32;
    fn tenero_hip_attempts(
        ctx: *mut c_void,
        keys: *const u8,
        pick_keys: *const u8,
        slices: *const u32,
        count: u32,
        gathered: u32,
        sums: *mut u64,
        milliseconds: *mut f32,
        error: *mut c_char,
        error_cap: usize,
    ) -> i32;
    fn tenero_hip_read_slice(
        ctx: *mut c_void,
        slice: u32,
        out: *mut u8,
        error: *mut c_char,
        error_cap: usize,
    ) -> i32;
    fn tenero_hip_memory(
        ctx: *mut c_void,
        free_bytes: *mut u64,
        total_bytes: *mut u64,
        error: *mut c_char,
        error_cap: usize,
    ) -> i32;
}

fn error_text(error: &[c_char; ERROR_CAP], fallback: &str) -> GpuError {
    // SAFETY: every C entry point writes a nul-terminated message with snprintf; the buffer starts zeroed too.
    #[allow(unsafe_code)]
    let text = unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy();
    if text.is_empty() {
        GpuError::new(fallback)
    } else {
        GpuError::new(&text)
    }
}

fn u32_param(value: usize, name: &str) -> Result<u32, GpuError> {
    u32::try_from(value).map_err(|_| GpuError::new(&format!("{name} is too large for HIP")))
}

/// Persistent HIP context: one 4 GiB dataset and reusable attempt buffers.
pub struct AmdGpu {
    raw: NonNull<c_void>,
    params: Params,
    batch: usize,
    pub name: String,
    pub arch: String,
    pub last_dataset_ms: f32,
    pub last_batch_ms: f32,
}

impl AmdGpu {
    pub fn new(ordinal: usize, params: Params, batch: usize) -> Result<AmdGpu, GpuError> {
        params.validate()?;
        if params.m != 64 || !params.k.is_multiple_of(32) || !params.nb.is_multiple_of(64) {
            return Err(GpuError::new(
                "the AMD engine needs m=64, k a multiple of 32 and nb a multiple of 64",
            ));
        }
        if batch == 0 || batch > 65_535 {
            return Err(GpuError::new("the AMD batch must be 1..=65535"));
        }
        let mut raw = std::ptr::null_mut();
        let mut error = [0 as c_char; ERROR_CAP];
        // SAFETY: all scalar parameters were range-checked, `raw` and `error` are writable for the advertised sizes.
        #[allow(unsafe_code)]
        let status = unsafe {
            tenero_hip_create(
                u32_param(ordinal, "GPU number")?,
                u32_param(batch, "batch")?,
                u32_param(params.k, "k")?,
                u32_param(params.nb, "nb")?,
                u32_param(params.num_blocks, "num_blocks")?,
                &mut raw,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        let raw = NonNull::new(raw);
        if status != 0 || raw.is_none() {
            return Err(error_text(&error, "cannot create the AMD HIP engine"));
        }
        let raw = raw.expect("checked");
        let mut name = [0 as c_char; 256];
        let mut arch = [0 as c_char; 64];
        // SAFETY: `raw` is a live context and both output buffers are writable.
        #[allow(unsafe_code)]
        let named = unsafe {
            tenero_hip_name(
                raw.as_ptr(),
                name.as_mut_ptr(),
                name.len(),
                arch.as_mut_ptr(),
                arch.len(),
            )
        };
        if named != 0 {
            // SAFETY: ownership has not yet moved into `AmdGpu`.
            #[allow(unsafe_code)]
            unsafe {
                tenero_hip_destroy(raw.as_ptr());
            }
            return Err(GpuError::new("cannot read the AMD GPU name"));
        }
        // SAFETY: `tenero_hip_name` always uses snprintf on both buffers.
        #[allow(unsafe_code)]
        let (name, arch) = unsafe {
            (
                CStr::from_ptr(name.as_ptr()).to_string_lossy().into_owned(),
                CStr::from_ptr(arch.as_ptr()).to_string_lossy().into_owned(),
            )
        };
        Ok(AmdGpu {
            raw,
            params,
            batch,
            name,
            arch,
            last_dataset_ms: 0.0,
            last_batch_ms: 0.0,
        })
    }

    pub fn batch(&self) -> usize {
        self.batch
    }

    /// Build the complete consensus dataset for an epoch in VRAM.
    pub fn build_dataset(&mut self, epoch_seed: &[u8; 32]) -> Result<f32, GpuError> {
        let key = mh::dataset_key(epoch_seed);
        let mut milliseconds = 0.0;
        let mut error = [0 as c_char; ERROR_CAP];
        // SAFETY: the context is live; `key`, timing and error buffers are valid for the call.
        #[allow(unsafe_code)]
        let status = unsafe {
            tenero_hip_build_dataset(
                self.raw.as_ptr(),
                key.as_ptr(),
                &mut milliseconds,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if status != 0 {
            return Err(error_text(&error, "the AMD dataset build failed"));
        }
        self.last_dataset_ms = milliseconds;
        Ok(milliseconds)
    }

    /// Run either gathered attempts (`slices=None`) or original-design attempts (`slices=Some`).
    pub fn attempts(
        &mut self,
        seeds: &[[u8; 32]],
        slices: Option<&[usize]>,
    ) -> Result<Vec<Attempt>, GpuError> {
        if seeds.is_empty() || seeds.len() > self.batch {
            return Err(GpuError::new(
                "between 1 and `batch` AMD attempts are required",
            ));
        }
        if let Some(values) = slices {
            if values.len() != seeds.len()
                || values.iter().any(|&slice| slice >= self.params.num_blocks)
            {
                return Err(GpuError::new(
                    "one in-range slice is required for every attempt",
                ));
            }
        }
        let pick_keys: Vec<[u8; 32]> = if slices.is_none() {
            seeds.iter().map(mh::pick_key).collect()
        } else {
            Vec::new()
        };
        let slice_words: Vec<u32> = slices
            .unwrap_or(&[])
            .iter()
            .map(|&value| u32::try_from(value).expect("validated against num_blocks"))
            .collect();
        let mut sums = vec![0u64; seeds.len() * 8];
        let mut milliseconds = 0.0;
        let mut error = [0 as c_char; ERROR_CAP];
        // SAFETY: all input/output slices remain alive for the synchronous C call. Null is used only for the
        // input that the selected mode does not read.
        #[allow(unsafe_code)]
        let status = unsafe {
            tenero_hip_attempts(
                self.raw.as_ptr(),
                seeds.as_ptr().cast(),
                if pick_keys.is_empty() {
                    std::ptr::null()
                } else {
                    pick_keys.as_ptr().cast()
                },
                if slice_words.is_empty() {
                    std::ptr::null()
                } else {
                    slice_words.as_ptr()
                },
                u32_param(seeds.len(), "attempt count")?,
                u32::from(slices.is_none()),
                sums.as_mut_ptr(),
                &mut milliseconds,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if status != 0 {
            return Err(error_text(&error, "the AMD attempt failed"));
        }
        self.last_batch_ms = milliseconds;
        Ok(seeds
            .iter()
            .zip(sums.chunks_exact(8))
            .enumerate()
            .map(|(index, (seed, values))| {
                let sums: [u64; 8] = values.try_into().expect("chunks of eight");
                let mix = mh::mix_bytes(&sums);
                Attempt {
                    seed: *seed,
                    slice_index: slices.map_or(0, |v| v[index]),
                    sums,
                    mix,
                    digest: mh::digest_of(seed, &mix),
                }
            })
            .collect())
    }

    /// Read one GPU dataset slice, used by the bit-for-bit dataset test.
    pub fn read_slice(&self, slice: usize) -> Result<Vec<u8>, GpuError> {
        if slice >= self.params.num_blocks {
            return Err(GpuError::new("dataset slice is out of range"));
        }
        let mut out = vec![0u8; self.params.slice_bytes()];
        let mut error = [0 as c_char; ERROR_CAP];
        // SAFETY: the context is live and `out` has exactly one slice of writable storage.
        #[allow(unsafe_code)]
        let status = unsafe {
            tenero_hip_read_slice(
                self.raw.as_ptr(),
                u32_param(slice, "slice")?,
                out.as_mut_ptr(),
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if status != 0 {
            return Err(error_text(&error, "cannot read the AMD dataset slice"));
        }
        Ok(out)
    }

    pub fn memory(&self) -> Result<(u64, u64), GpuError> {
        let (mut free, mut total) = (0, 0);
        let mut error = [0 as c_char; ERROR_CAP];
        // SAFETY: the context is live and output scalars/error storage are writable.
        #[allow(unsafe_code)]
        let status = unsafe {
            tenero_hip_memory(
                self.raw.as_ptr(),
                &mut free,
                &mut total,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if status != 0 {
            Err(error_text(&error, "cannot read AMD VRAM information"))
        } else {
            Ok((free, total))
        }
    }
}

impl Drop for AmdGpu {
    fn drop(&mut self) {
        // SAFETY: `raw` is owned by this value and destroyed exactly once here.
        #[allow(unsafe_code)]
        unsafe {
            tenero_hip_destroy(self.raw.as_ptr());
        }
    }
}
