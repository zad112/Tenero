//! The exact int8 x int8 -> int32 matrix multiply of one attempt, `C = X @ W`, on the tensor cores
//! through cuBLASLt (the same library call PyTorch's `torch._int_mm` makes).
//!
//! Layout. cuBLAS is column-major. A slice's raw bytes are `raw[n*k + t] = W[t][n]`, which is a
//! column-major `k x nb` matrix with leading dimension `k`; `X` is row-major `m x k`, which is a
//! column-major `k x m` matrix with leading dimension `k`. So
//!
//! ```text
//! D (nb x m, col-major, ld = nb) = op_T(A = raw slice)  (nb x k)  *  B = X  (k x m)
//! ```
//!
//! and element `(n, i)` of D sits at `n + i*nb`, which is `C[i][n]` of the row-major `m x nb` product.
//! That is the "TN" form int8 tensor cores require. The arithmetic is exact: int32 accumulation.
//!
//! **Several attempts that read the same slice are one multiply.** Their X matrices, one after the other in memory,
//! are one row-major `(g*m) x k` matrix, and row `i` of a product depends only on row `i` of X, so
//! `[X_1; ...; X_g] @ W` is `[C_1; ...; C_g]`, bit for bit (each output element is the same exact integer dot
//! product either way). The slice is then read once for `g` attempts instead of `g` times; `run_rows` does this.

use crate::GpuError;
use cudarc::cublaslt::{result, sys};
use cudarc::driver::{CudaSlice, CudaStream, DevicePtr, DevicePtrMut};
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::Arc;

/// The layouts of X and C and the algorithm cuBLASLt chose, for one number of rows.
struct Shape {
    rows: usize,
    b_layout: sys::cublasLtMatrixLayout_t,
    c_layout: sys::cublasLtMatrixLayout_t,
    algo: sys::cublasLtMatmulAlgo_t,
}

/// A prepared int8 GEMM for one `(k, nb)` and any number of rows that is a multiple of 4; run it for many
/// `(W slice, X, C)` triples. The layouts and algorithm of a number of rows are made the first time it is used.
pub struct Int8Gemm {
    stream: Arc<CudaStream>,
    handle: sys::cublasLtHandle_t,
    desc: sys::cublasLtMatmulDesc_t,
    a_layout: sys::cublasLtMatrixLayout_t,
    pref: sys::cublasLtMatmulPreference_t,
    shapes: Vec<Shape>,
    workspace: CudaSlice<u8>,
    m: usize,
    k: usize,
    nb: usize,
}

// SAFETY: the cuBLASLt handle and descriptors are only used through calls made from one
// thread at a time (a `Gpu` engine is used by one thread), and are not tied to a thread.
#[allow(unsafe_code)]
unsafe impl Send for Int8Gemm {}

impl Int8Gemm {
    /// `m` is the rows of one attempt (what `run` multiplies); `run_rows` takes any multiple of 4.
    pub fn new(
        stream: Arc<CudaStream>,
        m: usize,
        k: usize,
        nb: usize,
    ) -> Result<Int8Gemm, GpuError> {
        // int8 tensor cores need these multiples (Params::validate guarantees them for real chains)
        if !m.is_multiple_of(4) || !k.is_multiple_of(4) || !nb.is_multiple_of(4) {
            return Err(GpuError::new(
                "int8 GEMM needs m, k and nb to be multiples of 4",
            ));
        }
        let workspace_size: usize = 32 << 20;
        let workspace = stream.alloc_zeros::<u8>(workspace_size)?;

        let handle = result::create_handle()?;
        let desc = result::create_matmul_desc(
            sys::cublasComputeType_t::CUBLAS_COMPUTE_32I,
            sys::cudaDataType::CUDA_R_32I,
        )?;
        let a_layout = result::create_matrix_layout(
            sys::cudaDataType::CUDA_R_8I,
            k as u64,
            nb as u64,
            k as i64,
        )?;
        let pref = result::create_matmul_pref()?;

        let transa: i32 = 1; // A is used transposed; B is not
        let transb: i32 = 0;
        // SAFETY: the handles were just created and are valid; the attribute buffers are live for
        // the calls and their sizes are those of the values passed.
        #[allow(unsafe_code)]
        unsafe {
            result::set_matmul_desc_attribute(
                desc,
                sys::cublasLtMatmulDescAttributes_t::CUBLASLT_MATMUL_DESC_TRANSA,
                (&transa as *const i32).cast(),
                size_of::<i32>(),
            )?;
            result::set_matmul_desc_attribute(
                desc,
                sys::cublasLtMatmulDescAttributes_t::CUBLASLT_MATMUL_DESC_TRANSB,
                (&transb as *const i32).cast(),
                size_of::<i32>(),
            )?;
            result::set_matmul_pref_attribute(
                pref,
                sys::cublasLtMatmulPreferenceAttributes_t::CUBLASLT_MATMUL_PREF_MAX_WORKSPACE_BYTES,
                (&workspace_size as *const usize).cast(),
                size_of::<usize>(),
            )?;
        }
        let mut gemm = Int8Gemm {
            stream,
            handle,
            desc,
            a_layout,
            pref,
            shapes: Vec::new(),
            workspace,
            m,
            k,
            nb,
        };
        gemm.shape(m)?;
        Ok(gemm)
    }

    /// The index in `shapes` of `rows`, made if it is new.
    fn shape(&mut self, rows: usize) -> Result<usize, GpuError> {
        if let Some(i) = self.shapes.iter().position(|s| s.rows == rows) {
            return Ok(i);
        }
        if rows == 0 || !rows.is_multiple_of(4) {
            return Err(GpuError::new(
                "int8 GEMM needs the rows to be a positive multiple of 4",
            ));
        }
        let b_layout = result::create_matrix_layout(
            sys::cudaDataType::CUDA_R_8I,
            self.k as u64,
            rows as u64,
            self.k as i64,
        )?;
        let c_layout = result::create_matrix_layout(
            sys::cudaDataType::CUDA_R_32I,
            self.nb as u64,
            rows as u64,
            self.nb as i64,
        )?;
        // SAFETY: every handle and layout passed is valid (made in `new` or just above).
        #[allow(unsafe_code)]
        let algo = unsafe {
            result::get_matmul_algo_heuristic(
                self.handle,
                self.desc,
                self.a_layout,
                b_layout,
                c_layout,
                c_layout,
                self.pref,
            )
        };
        let algo = match algo {
            Ok(h) => h.algo,
            Err(e) => {
                // SAFETY: both were made just above, are not stored anywhere, and are destroyed once.
                #[allow(unsafe_code)]
                unsafe {
                    let _ = result::destroy_matrix_layout(b_layout);
                    let _ = result::destroy_matrix_layout(c_layout);
                }
                return Err(e.into());
            }
        };
        self.shapes.push(Shape {
            rows,
            b_layout,
            c_layout,
            algo,
        });
        Ok(self.shapes.len() - 1)
    }

    /// Times up to 16 of cuBLASLt's candidate algorithms for `rows` rows on these buffers (`reps` multiplies each, after
    /// one to warm up, going round the slices in `ws`) and keeps the fastest for that shape. **Give it many slices**: a
    /// slice used again and again stays in the GPU's cache, which favours different algorithms than reading each slice
    /// from memory, as mining does. Returns each candidate's time per multiply in seconds, in
    /// cuBLASLt's order (its first is the one `run_rows` would use untuned).
    ///
    /// **Any algorithm gives the same product, bit for bit**: the arithmetic is int8 x int8 into int32, and a dot product
    /// of `k` such terms is at most `k * 2^14` in size (2^27 for k = 8192), so no order of adding them can overflow.
    /// The GPU tests check the results whatever was chosen.
    pub fn tune<W, X, C>(
        &mut self,
        rows: usize,
        ws: &[W],
        x: &X,
        c: &mut C,
        reps: usize,
    ) -> Result<Vec<f64>, GpuError>
    where
        W: DevicePtr<u8>,
        X: DevicePtr<u8>,
        C: DevicePtrMut<i32>,
    {
        if ws.is_empty() {
            return Err(GpuError::new("no slices to tune on"));
        }
        let s = self.shape(rows)?;
        const MAX: usize = 16;
        // SAFETY: an all-zero heuristic result is a valid value of this plain C struct; it is only read where
        // cuBLASLt has written it (the first `count` entries).
        #[allow(unsafe_code)]
        let mut found: [sys::cublasLtMatmulHeuristicResult_t; MAX] = unsafe { std::mem::zeroed() };
        let mut count: i32 = 0;
        let shape = &self.shapes[s];
        // SAFETY: every handle and layout is valid; the result array holds MAX entries and `count` is a live i32.
        #[allow(unsafe_code)]
        unsafe {
            sys::cublasLtMatmulAlgoGetHeuristic(
                self.handle,
                self.desc,
                self.a_layout,
                shape.b_layout,
                shape.c_layout,
                shape.c_layout,
                self.pref,
                MAX as i32,
                found.as_mut_ptr(),
                &mut count,
            )
            .result()?;
        }
        let candidates: Vec<sys::cublasLtMatmulAlgo_t> = found
            [..count.clamp(0, MAX as i32) as usize]
            .iter()
            .filter(|h| h.state == sys::cublasStatus_t::CUBLAS_STATUS_SUCCESS)
            .map(|h| h.algo)
            .collect();
        let original = self.shapes[s].algo;
        let mut times = Vec::new();
        let mut best: Option<(f64, sys::cublasLtMatmulAlgo_t)> = None;
        for algo in candidates {
            self.shapes[s].algo = algo;
            // an algorithm that cannot run this shape is skipped, not fatal
            if self.run_rows(rows, &ws[0], x, c).is_err() || self.stream.synchronize().is_err() {
                times.push(f64::INFINITY);
                continue;
            }
            let t = std::time::Instant::now();
            for i in 0..reps.max(1) {
                self.run_rows(rows, &ws[(i + 1) % ws.len()], x, c)?;
            }
            self.stream.synchronize()?;
            let per = t.elapsed().as_secs_f64() / reps.max(1) as f64;
            times.push(per);
            if best.is_none_or(|(b, _)| per < b) {
                best = Some((per, algo));
            }
        }
        match best {
            Some((_, algo)) => self.shapes[s].algo = algo,
            None => {
                self.shapes[s].algo = original;
                return Err(GpuError::new(
                    "no cuBLASLt algorithm could run this multiply",
                ));
            }
        }
        Ok(times)
    }

    /// `c = x @ w` for one attempt. `w` is one slice (`k * nb` bytes, the transposed matrix as stored),
    /// `x` is `m * k` int8 values, `c` receives `m * nb` int32 values (row-major).
    /// The buffers are checked to be large enough; the multiply runs on the engine's stream.
    pub fn run<W, X, C>(&mut self, w: &W, x: &X, c: &mut C) -> Result<(), GpuError>
    where
        W: DevicePtr<u8>,
        X: DevicePtr<u8>,
        C: DevicePtrMut<i32>,
    {
        self.run_rows(self.m, w, x, c)
    }

    /// `c = x @ w` for `rows` rows of X (several attempts against the same slice, one after the other): `x` is
    /// `rows * k` int8 values and `c` receives `rows * nb` int32 values (row-major). `rows` must be a multiple of 4.
    pub fn run_rows<W, X, C>(
        &mut self,
        rows: usize,
        w: &W,
        x: &X,
        c: &mut C,
    ) -> Result<(), GpuError>
    where
        W: DevicePtr<u8>,
        X: DevicePtr<u8>,
        C: DevicePtrMut<i32>,
    {
        if w.len() < self.k * self.nb || x.len() < rows * self.k || c.len() < rows * self.nb {
            return Err(GpuError::new("a buffer is too small for this GEMM"));
        }
        let s = self.shape(rows)?;
        let shape = &self.shapes[s];
        let (w_ptr, _w_guard) = w.device_ptr(&self.stream);
        let (x_ptr, _x_guard) = x.device_ptr(&self.stream);
        let (c_ptr, _c_guard) = c.device_ptr_mut(&self.stream);
        let (ws_ptr, _ws_guard) = self.workspace.device_ptr(&self.stream);
        let alpha: i32 = 1;
        let beta: i32 = 0;
        // SAFETY: every descriptor is valid until `drop`; the three data pointers are device buffers
        // whose lengths were checked above against the layouts of `rows`; alpha and beta are live i32
        // values (the scale type is int32); the workspace is a live 32 MiB device allocation.
        #[allow(unsafe_code)]
        unsafe {
            result::matmul(
                self.handle,
                self.desc,
                (&alpha as *const i32).cast::<c_void>(),
                (&beta as *const i32).cast::<c_void>(),
                w_ptr as *const c_void,
                self.a_layout,
                x_ptr as *const c_void,
                shape.b_layout,
                c_ptr as *const c_void,
                shape.c_layout,
                c_ptr as *mut c_void,
                shape.c_layout,
                &shape.algo,
                ws_ptr as *mut c_void,
                self.workspace.len(),
                self.stream.cu_stream() as sys::cudaStream_t,
            )?;
        }
        Ok(())
    }
}

impl Drop for Int8Gemm {
    fn drop(&mut self) {
        // SAFETY: each handle was created in `new` or `shape`, is destroyed exactly once here, and
        // nothing uses it afterwards. Errors while tearing down are ignored (nothing useful to do).
        #[allow(unsafe_code)]
        unsafe {
            let _ = result::destroy_matmul_pref(self.pref);
            for s in &self.shapes {
                let _ = result::destroy_matrix_layout(s.b_layout);
                let _ = result::destroy_matrix_layout(s.c_layout);
            }
            let _ = result::destroy_matrix_layout(self.a_layout);
            let _ = result::destroy_matmul_desc(self.desc);
            let _ = result::destroy_handle(self.handle);
        }
    }
}
