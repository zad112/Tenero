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

use crate::GpuError;
use cudarc::cublaslt::{result, sys};
use cudarc::driver::{CudaSlice, CudaStream, DevicePtr, DevicePtrMut};
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::Arc;

/// A prepared int8 GEMM for one shape `(m, k, nb)`; run it for many `(W slice, X, C)` triples.
pub struct Int8Gemm {
    stream: Arc<CudaStream>,
    handle: sys::cublasLtHandle_t,
    desc: sys::cublasLtMatmulDesc_t,
    a_layout: sys::cublasLtMatrixLayout_t,
    b_layout: sys::cublasLtMatrixLayout_t,
    c_layout: sys::cublasLtMatrixLayout_t,
    pref: sys::cublasLtMatmulPreference_t,
    algo: sys::cublasLtMatmulAlgo_t,
    workspace: CudaSlice<u8>,
    m: usize,
    k: usize,
    nb: usize,
}

// SAFETY: the cuBLASLt handle and descriptors are only used through `&self` calls made from one
// thread at a time (a `Gpu` engine is used by one thread), and are not tied to a thread.
#[allow(unsafe_code)]
unsafe impl Send for Int8Gemm {}

impl Int8Gemm {
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
        let b_layout = result::create_matrix_layout(
            sys::cudaDataType::CUDA_R_8I,
            k as u64,
            m as u64,
            k as i64,
        )?;
        let c_layout = result::create_matrix_layout(
            sys::cudaDataType::CUDA_R_32I,
            nb as u64,
            m as u64,
            nb as i64,
        )?;
        let pref = result::create_matmul_pref()?;

        let transa: i32 = 1; // A is used transposed; B is not
        let transb: i32 = 0;
        // SAFETY: the handles were just created and are valid; the attribute buffers are live for
        // the calls and their sizes are those of the values passed.
        #[allow(unsafe_code)]
        let algo = unsafe {
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
            result::get_matmul_algo_heuristic(
                handle, desc, a_layout, b_layout, c_layout, c_layout, pref,
            )?
            .algo
        };
        Ok(Int8Gemm {
            stream,
            handle,
            desc,
            a_layout,
            b_layout,
            c_layout,
            pref,
            algo,
            workspace,
            m,
            k,
            nb,
        })
    }

    /// `c = x @ w` for one attempt. `w` is one slice (`k * nb` bytes, the transposed matrix as stored),
    /// `x` is `m * k` int8 values, `c` receives `m * nb` int32 values (row-major).
    /// The buffers are checked to be large enough; the multiply runs on the engine's stream.
    pub fn run<W, X, C>(&self, w: &W, x: &X, c: &mut C) -> Result<(), GpuError>
    where
        W: DevicePtr<u8>,
        X: DevicePtr<u8>,
        C: DevicePtrMut<i32>,
    {
        if w.len() < self.k * self.nb || x.len() < self.m * self.k || c.len() < self.m * self.nb {
            return Err(GpuError::new("a buffer is too small for this GEMM"));
        }
        let (w_ptr, _w_guard) = w.device_ptr(&self.stream);
        let (x_ptr, _x_guard) = x.device_ptr(&self.stream);
        let (c_ptr, _c_guard) = c.device_ptr_mut(&self.stream);
        let (ws_ptr, _ws_guard) = self.workspace.device_ptr(&self.stream);
        let alpha: i32 = 1;
        let beta: i32 = 0;
        // SAFETY: every descriptor is valid until `drop`; the three data pointers are device buffers
        // whose lengths were checked above against the layouts; alpha and beta are live i32 values
        // (the scale type is int32); the workspace is a live 32 MiB device allocation.
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
                self.b_layout,
                c_ptr as *const c_void,
                self.c_layout,
                c_ptr as *mut c_void,
                self.c_layout,
                &self.algo,
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
        // SAFETY: each handle was created in `new`, is destroyed exactly once here, and nothing
        // uses it afterwards. Errors while tearing down are ignored (nothing useful to do).
        #[allow(unsafe_code)]
        unsafe {
            let _ = result::destroy_matmul_pref(self.pref);
            let _ = result::destroy_matrix_layout(self.a_layout);
            let _ = result::destroy_matrix_layout(self.b_layout);
            let _ = result::destroy_matrix_layout(self.c_layout);
            let _ = result::destroy_matmul_desc(self.desc);
            let _ = result::destroy_handle(self.handle);
        }
    }
}
