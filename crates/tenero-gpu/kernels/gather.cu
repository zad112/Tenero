// The GATHERED attempt's multiply (required on beta and dev from the gather fork height, 500; docs/CONSENSUS.md 8.3): W
// is gathered, column n of an attempt's W being any 8 KiB column of the whole dataset (idx[n]), not column n of one slice.
// Rust-only (the Python reference computes it on the CPU); checked bit for bit against the consensus code by
// crates/tenero-gpu/tests/gather_selftest.rs.
//
//   C[a][i][n] = sum over t < k of X[a][i][t] * (int8) data[(idx[a][n] % columns) * k + t]      (exact, int32)
//
// One block computes the 64 rows of one attempt against 128 of its columns, going through k 128 bytes at a time (the
// memory test below: 64-byte pieces of scattered columns reach about 590 GB/s on the owner's card, 128 bytes or more
// about 860), with three stages of cp.async copies into DYNAMIC shared memory (G_SMEM bytes: the launch must ask for
// them, and the function must be allowed them), ldmatrix to load the fragments and int8 tensor-core instructions
// (mma.sync m16n8k32). Needs m == 64, k a multiple of 128, nb a multiple of 128 (the caller checks).

#define G_BN 128
#define G_BK 128
#define G_STRIDE 144  // G_BK + 16: the 8 rows one ldmatrix reads are 36 words apart, so they fall in different banks
#define G_STAGES 3
#define G_SX (64 * G_STRIDE)
#define G_SW (G_BN * G_STRIDE)
#define G_SMEM (G_STAGES * (G_SX + G_SW))

__device__ __forceinline__ void g_cp_async16(void* smem, const void* gmem) {
    const unsigned s = (unsigned)__cvta_generic_to_shared(smem);
    asm volatile("cp.async.cg.shared.global [%0], [%1], 16;\n" :: "r"(s), "l"(gmem));
}

__device__ __forceinline__ void g_cp_async_commit() {
    asm volatile("cp.async.commit_group;\n" ::);
}

__device__ __forceinline__ void g_cp_async_wait_one() {
    asm volatile("cp.async.wait_group 1;\n" ::);
}

__device__ __forceinline__ void g_ldmatrix_x4(unsigned r[4], const void* smem) {
    const unsigned s = (unsigned)__cvta_generic_to_shared(smem);
    asm volatile("ldmatrix.sync.aligned.m8n8.x4.shared.b16 {%0,%1,%2,%3}, [%4];\n"
                 : "=r"(r[0]), "=r"(r[1]), "=r"(r[2]), "=r"(r[3]) : "r"(s));
}

__device__ __forceinline__ void g_mma(int c[4], const unsigned a[4], const unsigned b0, const unsigned b1) {
    asm volatile(
        "mma.sync.aligned.m16n8k32.row.col.s32.s8.s8.s32 {%0,%1,%2,%3}, {%4,%5,%6,%7}, {%8,%9}, {%0,%1,%2,%3};\n"
        : "+r"(c[0]), "+r"(c[1]), "+r"(c[2]), "+r"(c[3])
        : "r"(a[0]), "r"(a[1]), "r"(a[2]), "r"(a[3]), "r"(b0), "r"(b1));
}

extern "C" {

// grid = (nb / 128, attempts), 256 threads, G_SMEM bytes of dynamic shared memory. The column tile is blockIdx.x, so the
// blocks that run side by side are mostly the tiles of the SAME attempt and share its X through the L2 cache (with the
// attempt as blockIdx.x instead, each X was read from memory once per tile: the gathered attempt fell from about 45,000
// to about 32,000 attempts/s on the owner's card). Attempts that read the same slice (the first design, grouped by the
// miner) follow one another, and the 16 MiB slice stays in the L2 cache between them.
//
// Which columns attempt a reads:
//   slice_mode == 0 (the gathered attempt): dataset column idx[a * idx_stride + n] % columns. idx_stride is nb (each
//     attempt its own columns) or 0 (every attempt uses attempt 0's columns: an impossible best case for a miner, used
//     only to measure how much reading the columns costs).
//   slice_mode == 1 (the first design, matmulhash v2 before the gather fork): column n of slice slices[a], which is
//     dataset column slices[a] * nb + n. idx is not read.
__global__ void __launch_bounds__(256) gather_gemm(const unsigned long long x_ptr,
                                                   const unsigned long long data_ptr,
                                                   const unsigned long long idx_ptr,
                                                   const unsigned long long c_ptr,
                                                   const unsigned k,
                                                   const unsigned nb,
                                                   const unsigned columns,
                                                   const unsigned idx_stride,
                                                   const unsigned long long slices_ptr,
                                                   const unsigned slice_mode)
{
    extern __shared__ __align__(16) unsigned char smem[];
    unsigned char* sx = smem;                       // G_STAGES x 64 rows of X
    unsigned char* sw = smem + G_STAGES * G_SX;     // G_STAGES x 128 columns of W

    const unsigned a = blockIdx.y;
    const unsigned n0 = blockIdx.x * G_BN;
    const unsigned char* X = (const unsigned char*)x_ptr + (unsigned long long)a * 64ull * k;
    const unsigned char* D = (const unsigned char*)data_ptr;
    const unsigned* idx = (const unsigned*)idx_ptr + (unsigned long long)a * idx_stride;
    const unsigned long long slice_first =
        slice_mode ? (unsigned long long)((const unsigned*)slices_ptr)[a] * nb : 0ull;
    int* C = (int*)c_ptr + (unsigned long long)a * 64ull * nb;

    const unsigned tid = threadIdx.x;
    // copies per stage: X is 64 rows x 8 pieces of 16 bytes (2 per thread), W is 128 columns x 8 (4 per thread); the 8
    // threads of one row or column read its 128 bytes together
    const unsigned pc = (tid & 7u) * 16u;
    const unsigned char* xsrc[2];
    const unsigned char* wsrc[4];
    unsigned xdst[2], wdst[4];
    #pragma unroll
    for (int i = 0; i < 2; ++i) {
        const unsigned row = (tid >> 3) + i * 32;
        xsrc[i] = X + (unsigned long long)row * k + pc;
        xdst[i] = row * G_STRIDE + pc;
    }
    #pragma unroll
    for (int i = 0; i < 4; ++i) {
        const unsigned col = (tid >> 3) + i * 32;
        const unsigned long long column =
            slice_mode ? slice_first + n0 + col : (unsigned long long)(idx[n0 + col] % columns);
        wsrc[i] = D + column * k + pc;
        wdst[i] = col * G_STRIDE + pc;
    }
    const unsigned ktiles = k / G_BK;

    const unsigned warp = tid >> 5, lane = tid & 31u;
    const unsigned wm = (warp & 1u) * 32u;   // the warp's 32 rows
    const unsigned wn = (warp >> 1) * 32u;   // and its 32 columns of the block's 128
    const unsigned g = lane >> 2, t = lane & 3u;
    const unsigned q = lane >> 3, r8 = lane & 7u;
    // ldmatrix: each lane gives the address of one 16-byte row of one of the four 8x8 matrices
    const unsigned a_off = ((q & 1u) * 8u + r8) * G_STRIDE + (q >> 1) * 16u;   // A: rows +0/+8, k bytes +0/+16
    const unsigned b_off = ((q >> 1) * 8u + r8) * G_STRIDE + (q & 1u) * 16u;   // B: columns +0/+8, k bytes +0/+16

    int acc[2][4][4];
    #pragma unroll
    for (int i = 0; i < 2; ++i)
        #pragma unroll
        for (int j = 0; j < 4; ++j)
            #pragma unroll
            for (int r = 0; r < 4; ++r)
                acc[i][j][r] = 0;

    #pragma unroll
    for (unsigned s = 0; s < G_STAGES - 1; ++s) {
        if (s < ktiles) {
            const unsigned off = s * G_BK;
            #pragma unroll
            for (int i = 0; i < 2; ++i) g_cp_async16(sx + s * G_SX + xdst[i], xsrc[i] + off);
            #pragma unroll
            for (int i = 0; i < 4; ++i) g_cp_async16(sw + s * G_SW + wdst[i], wsrc[i] + off);
        }
        g_cp_async_commit();
    }

    for (unsigned kt = 0; kt < ktiles; ++kt) {
        g_cp_async_wait_one();
        __syncthreads();
        // fetch the stage after next; its buffer was last read in iteration kt - 1, which every thread has finished
        const unsigned nk = kt + G_STAGES - 1;
        if (nk < ktiles) {
            const unsigned s = nk % G_STAGES;
            const unsigned off = nk * G_BK;
            #pragma unroll
            for (int i = 0; i < 2; ++i) g_cp_async16(sx + s * G_SX + xdst[i], xsrc[i] + off);
            #pragma unroll
            for (int i = 0; i < 4; ++i) g_cp_async16(sw + s * G_SW + wdst[i], wsrc[i] + off);
        }
        g_cp_async_commit();

        const unsigned char* xs = sx + (kt % G_STAGES) * G_SX;
        const unsigned char* ws = sw + (kt % G_STAGES) * G_SW;
        #pragma unroll
        for (unsigned ks = 0; ks < G_BK; ks += 32) {
            unsigned af[2][4];
            unsigned bf[2][4];   // bf[p]: columns wn + 16p .. +7 (regs 0, 1) and wn + 16p + 8 .. +15 (regs 2, 3)
            #pragma unroll
            for (int i = 0; i < 2; ++i)
                g_ldmatrix_x4(af[i], xs + (wm + i * 16) * G_STRIDE + ks + a_off);
            #pragma unroll
            for (int p = 0; p < 2; ++p)
                g_ldmatrix_x4(bf[p], ws + (wn + p * 16) * G_STRIDE + ks + b_off);
            #pragma unroll
            for (int i = 0; i < 2; ++i)
                #pragma unroll
                for (int j = 0; j < 4; ++j)
                    g_mma(acc[i][j], af[i], bf[j >> 1][(j & 1) * 2], bf[j >> 1][(j & 1) * 2 + 1]);
        }
    }

    #pragma unroll
    for (int i = 0; i < 2; ++i) {
        #pragma unroll
        for (int j = 0; j < 4; ++j) {
            const unsigned r = wm + i * 16 + g;
            const unsigned col = n0 + wn + j * 8 + t * 2;
            *(int2*)&C[(unsigned long long)r * nb + col] = make_int2(acc[i][j][0], acc[i][j][1]);
            *(int2*)&C[(unsigned long long)(r + 8) * nb + col] = make_int2(acc[i][j][2], acc[i][j][3]);
        }
    }
}

// A memory test, no multiply: reads gathered columns in the same order as gather_gemm, but `piece` bytes of each column
// per step (gather_gemm uses 64), so the bandwidth of that access pattern can be measured on its own. grid =
// (nb / 128, attempts), 256 threads; piece is a multiple of 16 dividing k, 16 to 1024. Writes one word per thread.
__global__ void __launch_bounds__(256) gather_read(const unsigned long long data_ptr,
                                                   const unsigned long long idx_ptr,
                                                   const unsigned long long out_ptr,
                                                   const unsigned k,
                                                   const unsigned nb,
                                                   const unsigned columns,
                                                   const unsigned piece)
{
    const unsigned a = blockIdx.y;
    const unsigned n0 = blockIdx.x * G_BN;
    const unsigned char* D = (const unsigned char*)data_ptr;
    const unsigned* idx = (const unsigned*)idx_ptr + (unsigned long long)a * nb;
    const unsigned tid = threadIdx.x;
    const unsigned per_col = piece / 16;            // 16-byte loads per column per step
    const unsigned loads = G_BN * per_col;          // per step, for the block
    uint4 acc = make_uint4(0, 0, 0, 0);
    for (unsigned off = 0; off < k; off += piece) {
        for (unsigned l = tid; l < loads; l += 256) {
            const unsigned col = l / per_col, part = l - col * per_col;
            const uint4 v = *(const uint4*)(D + (unsigned long long)(idx[n0 + col] % columns) * k + off + part * 16);
            acc.x ^= v.x; acc.y ^= v.y; acc.z ^= v.z; acc.w ^= v.w;
        }
    }
    ((unsigned*)out_ptr)[((unsigned long long)a * gridDim.x + blockIdx.x) * 256 + tid] = acc.x ^ acc.y ^ acc.z ^ acc.w;
}

}  // extern "C"
