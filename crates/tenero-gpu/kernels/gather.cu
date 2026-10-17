// The GATHERED attempt's multiply AND fold (required on gamma and dev from block 0, on beta from the gather fork height, 500;
// docs/CONSENSUS.md 8.3): W is gathered, column n of an attempt's W being any 8 KiB column of the whole dataset (idx[n]),
// not column n of one slice. Rust-only (the Python reference computes it on the CPU); checked bit for bit against the
// consensus code by crates/tenero-gpu/tests/gather_selftest.rs.
//
//   C[a][i][n] = sum over t < k of X[a][i][t] * (int8) data[(idx[a][n] % columns) * k + t]      (exact, int32)
//   sums[a][i] += word i + word i + 8 of ChaCha20core(chunk c of C[a], with c XORed into word 0)   for every 16-word chunk c
//
// Compiled AFTER matmulhash.cu, in one module (build_kernels.py), for its chacha_core and set_constants: X (CONSENSUS.md 8.2
// step 3, the seed's keystream) is computed here, 64 bytes at a time, straight into shared memory, and the fold (step 6) is
// done on the block's tile of C before it leaves the registers. So neither X nor C ever touches the card's memory: an
// attempt's traffic is its 16 MiB of columns and nothing else. Before (2026-10-16), X was written by keystream_fast and read
// back, and C was written and read back by fold_fast: about 2 MiB more per attempt, 11 % of the traffic of a memory-bound
// kernel (measured on the owner's RTX 5070 Ti, old and new run alternately: 44,100 to 44,700 attempts/s before, 49,000 to
// 50,200 after, at the card's power limit; BENCHMARKS.md, 2026-10-16).
//
// One block computes the 64 rows of one attempt against 128 of its columns, going through k 128 bytes at a time (the
// memory test below: 64-byte pieces of scattered columns reach about 560 GB/s on the owner's card, 128 bytes or more
// about 820), with four stages of cp.async copies of W into DYNAMIC shared memory (G_SMEM bytes: the launch must ask for
// them, and the function must be allowed them), ldmatrix to load the fragments and int8 tensor-core instructions
// (mma.sync m16n8k32). Needs m == 64, k a multiple of 128, nb a multiple of 128 (the caller checks).

#define G_BN 128
#define G_BK 128
#define G_STAGES 4
#define G_SX (64 * G_BK)          // a stage's X: 64 rows of 128 bytes
#define G_SW (G_BN * G_BK)        // a stage's W: 128 columns of 128 bytes
#define G_STAGE (G_SX + G_SW)
#define G_SMEM (G_STAGES * G_STAGE)   // 96 KiB (the 99 KiB a block may have on every target from sm_80 on)
#define G_CHUNK_PAD 17            // words per 16-word chunk of C in the fold's shared-memory tile

// Where the 16-byte piece `chunk` (0..7) of row `row` of a stage's X or W is: the pieces of a row are XOR-swizzled by the
// row, so the 8 rows one ldmatrix reads, at the same piece, fall in 8 different groups of banks (no padding, so a stage is
// 24 KiB instead of 27 and four of them fit).
__device__ __forceinline__ unsigned g_swz(unsigned row, unsigned chunk) {
    return row * G_BK + ((chunk ^ (row & 7u)) << 4);
}

__device__ __forceinline__ void g_cp_async16(void* smem, const void* gmem) {
    const unsigned s = (unsigned)__cvta_generic_to_shared(smem);
    asm volatile("cp.async.cg.shared.global [%0], [%1], 16;\n" :: "r"(s), "l"(gmem));
}

__device__ __forceinline__ void g_cp_async_commit() {
    asm volatile("cp.async.commit_group;\n" ::);
}

// Waits until at most G_STAGES - 2 (two) of the committed groups are still in flight.
__device__ __forceinline__ void g_cp_async_wait_two() {
    asm volatile("cp.async.wait_group 2;\n" ::);
}

__device__ __forceinline__ void g_cp_async_wait_all() {
    asm volatile("cp.async.wait_all;\n" ::);
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

// Bytes half * 64 .. + 63 of row `row` of stage `stage`'s X, into the stage buffer `sx`: the attempt's X is the ChaCha20
// keystream of key `key` (the seed), counter from 0, nonce 0, read row-major (CONSENSUS.md 8.2 step 3), so these 64 bytes are
// keystream block (row * k + stage * 128 + half * 64) / 64, exactly as keystream_kernel (matmulhash.cu) writes it.
__device__ __forceinline__ void g_x_piece(unsigned char* sx, const unsigned key[8], unsigned blocks_per_row,
                                          unsigned stage, unsigned row, unsigned half) {
    unsigned x[16];
    set_constants(x);
    #pragma unroll
    for (int i = 0; i < 8; ++i) x[4 + i] = key[i];
    x[12] = row * blocks_per_row + stage * 2u + half;
    x[13] = 0u; x[14] = 0u; x[15] = 0u;
    chacha_core(x);
    #pragma unroll
    for (unsigned c = 0; c < 4; ++c)
        *(uint4*)(sx + g_swz(row, half * 4u + c)) = make_uint4(x[4 * c], x[4 * c + 1], x[4 * c + 2], x[4 * c + 3]);
}

extern "C" {

// grid = (nb / 128, attempts), 256 threads, G_SMEM bytes of dynamic shared memory. The column tile is blockIdx.x, so the
// blocks that run side by side are mostly the tiles of the SAME attempt (attempts that read the same slice, in the first
// design grouped by the miner, follow one another, and the 16 MiB slice stays in the L2 cache between them).
//
// keys: 8 words (as long long) of each attempt's seed. sums: 8 uint64 of each attempt, ZERO before the launch; each block
// adds its tile's share (integer addition is exact and order-independent, so the atomics stay deterministic).
//
// Which columns attempt a reads:
//   slice_mode == 0 (the gathered attempt): dataset column idx[a * idx_stride + n] % columns. idx_stride is nb (each
//     attempt its own columns) or 0 (every attempt uses attempt 0's columns: an impossible best case for a miner, used
//     only to measure how much reading the columns costs).
//   slice_mode == 1 (the first design, matmulhash v2 before the gather fork): column n of slice slices[a], which is
//     dataset column slices[a] * nb + n. idx is not read.
//
// The pipeline: stage s is the 128 bytes s * 128 .. of every row of X and every column of W. W comes by cp.async, three
// stages ahead of the multiply; X is computed by one half of the threads (128 ChaCha20 blocks a stage, one each), the
// halves taking turns, also three stages ahead, after the warp has issued its tensor-core work for the current stage.
__global__ void __launch_bounds__(256) gather_gemm(const unsigned long long keys_ptr,
                                                   const unsigned long long data_ptr,
                                                   const unsigned long long idx_ptr,
                                                   const unsigned long long sums_ptr,
                                                   const unsigned k,
                                                   const unsigned nb,
                                                   const unsigned columns,
                                                   const unsigned idx_stride,
                                                   const unsigned long long slices_ptr,
                                                   const unsigned slice_mode)
{
    extern __shared__ __align__(16) unsigned char smem[];

    const unsigned a = blockIdx.y;
    const unsigned n0 = blockIdx.x * G_BN;
    const unsigned char* D = (const unsigned char*)data_ptr;
    const unsigned* idx = (const unsigned*)idx_ptr + (unsigned long long)a * idx_stride;
    const long long* keyw = (const long long*)keys_ptr + (unsigned long long)a * 8ull;
    unsigned long long* sums = (unsigned long long*)sums_ptr + (unsigned long long)a * 8ull;
    const unsigned long long slice_first =
        slice_mode ? (unsigned long long)((const unsigned*)slices_ptr)[a] * nb : 0ull;

    const unsigned tid = threadIdx.x;
    unsigned key[8];
    #pragma unroll
    for (int i = 0; i < 8; ++i) key[i] = (unsigned)keyw[i];
    const unsigned blocks_per_row = k / 64u;
    const unsigned ktiles = k / G_BK;

    // W copies per stage: 128 columns x 8 pieces of 16 bytes, 4 per thread; the 8 threads of one column read its 128 bytes
    // together
    const unsigned pc = tid & 7u;
    const unsigned char* wsrc[4];
    unsigned wdst[4];
    #pragma unroll
    for (int i = 0; i < 4; ++i) {
        const unsigned col = (tid >> 3) + i * 32;
        const unsigned long long column =
            slice_mode ? slice_first + n0 + col : (unsigned long long)(idx[n0 + col] % columns);
        wsrc[i] = D + column * k + pc * 16u;
        wdst[i] = G_SX + g_swz(col, pc);
    }
    // X per stage: 128 threads (half xh), one 64-byte piece each
    const unsigned xh = tid >> 7, xrow = (tid & 127u) >> 1, xhalf = tid & 1u;

    const unsigned warp = tid >> 5, lane = tid & 31u;
    const unsigned wm = (warp & 1u) * 32u;   // the warp's 32 rows
    const unsigned wn = (warp >> 1) * 32u;   // and its 32 columns of the block's 128
    const unsigned g = lane >> 2, t = lane & 3u;
    const unsigned q = lane >> 3, r8 = lane & 7u;
    // ldmatrix: each lane gives the address of one 16-byte piece of one row of one of the four 8x8 matrices
    const unsigned a_row = (q & 1u) * 8u + r8, a_kc = q >> 1;   // A: rows +0/+8, k pieces +0/+1 (of the 32-k step)
    const unsigned b_row = (q >> 1) * 8u + r8, b_kc = q & 1u;   // B: columns +0/+8, k pieces +0/+1

    int acc[2][4][4];
    #pragma unroll
    for (int i = 0; i < 2; ++i)
        #pragma unroll
        for (int j = 0; j < 4; ++j)
            #pragma unroll
            for (int r = 0; r < 4; ++r)
                acc[i][j][r] = 0;

    // the first G_STAGES - 1 stages: W on its way, X computed (stage 0 by half 0, stages 1 and 2 by half 1; from then on
    // the halves alternate, so each computes half of the stages)
    #pragma unroll
    for (unsigned s = 0; s < G_STAGES - 1; ++s) {
        if (s < ktiles) {
            #pragma unroll
            for (int i = 0; i < 4; ++i) g_cp_async16(smem + s * G_STAGE + wdst[i], wsrc[i] + s * G_BK);
        }
        g_cp_async_commit();
    }
    if (xh == 0u) {
        g_x_piece(smem, key, blocks_per_row, 0, xrow, xhalf);
    } else {
        if (1 < ktiles) g_x_piece(smem + 1 * G_STAGE, key, blocks_per_row, 1, xrow, xhalf);
        if (2 < ktiles) g_x_piece(smem + 2 * G_STAGE, key, blocks_per_row, 2, xrow, xhalf);
    }

    for (unsigned kt = 0; kt < ktiles; ++kt) {
        g_cp_async_wait_two();   // stage kt's W has arrived (the two stages after it may still be in flight)
        __syncthreads();         // its X is visible, and every thread has finished reading the buffer that stage kt + 3
                                 // takes over (read in iteration kt - 1)
        const unsigned nk = kt + G_STAGES - 1;
        unsigned char* nbuf = smem + (nk % G_STAGES) * G_STAGE;
        if (nk < ktiles) {
            #pragma unroll
            for (int i = 0; i < 4; ++i) g_cp_async16(nbuf + wdst[i], wsrc[i] + nk * G_BK);
        }
        g_cp_async_commit();

        const unsigned char* xs = smem + (kt % G_STAGES) * G_STAGE;
        const unsigned char* ws = xs + G_SX;
        #pragma unroll
        for (unsigned ks = 0; ks < G_BK; ks += 32) {
            const unsigned kc = ks >> 4;
            unsigned af[2][4];
            unsigned bf[2][4];   // bf[p]: columns wn + 16p .. +7 (regs 0, 1) and wn + 16p + 8 .. +15 (regs 2, 3)
            #pragma unroll
            for (int i = 0; i < 2; ++i)
                g_ldmatrix_x4(af[i], xs + g_swz(wm + i * 16 + a_row, kc + a_kc));
            #pragma unroll
            for (int p = 0; p < 2; ++p)
                g_ldmatrix_x4(bf[p], ws + g_swz(wn + p * 16 + b_row, kc + b_kc));
            #pragma unroll
            for (int i = 0; i < 2; ++i)
                #pragma unroll
                for (int j = 0; j < 4; ++j)
                    g_mma(acc[i][j], af[i], bf[j >> 1][(j & 1) * 2], bf[j >> 1][(j & 1) * 2 + 1]);
        }

        // stage kt + 3's X, by the half whose turn it is, now that the warp's tensor-core work is on its way
        if (nk < ktiles && xh == (kt & 1u)) g_x_piece(nbuf, key, blocks_per_row, nk, xrow, xhalf);
    }

    // The fold of the block's 64 x 128 tile of C: 512 chunks of 16 words, 8 a row (128 is a multiple of 16, so a chunk
    // never straddles two tiles); chunk cc of row r is chunk number r * (nb / 16) + (n0 + 16 cc) / 16 of C. Through shared
    // memory, G_CHUNK_PAD words a chunk so that 32 lanes reading word i of 32 consecutive chunks hit 32 different banks;
    // then every thread folds two chunks, the warp adds up its sums, and lane 0 adds them to the attempt's.
    g_cp_async_wait_all();
    __syncthreads();   // every warp has read its last fragments
    unsigned* ct = (unsigned*)smem;
    #pragma unroll
    for (int i = 0; i < 2; ++i) {
        #pragma unroll
        for (int j = 0; j < 4; ++j) {
            const unsigned r = wm + i * 16 + g;
            const unsigned col = wn + j * 8 + t * 2;
            const unsigned ch = r * 8u + (col >> 4), w = col & 15u;
            ct[ch * G_CHUNK_PAD + w] = (unsigned)acc[i][j][0];
            ct[ch * G_CHUNK_PAD + w + 1] = (unsigned)acc[i][j][1];
            ct[(ch + 64u) * G_CHUNK_PAD + w] = (unsigned)acc[i][j][2];       // row r + 8: 8 chunks on
            ct[(ch + 64u) * G_CHUNK_PAD + w + 1] = (unsigned)acc[i][j][3];
        }
    }
    __syncthreads();
    unsigned long long s8[8];
    #pragma unroll
    for (int i = 0; i < 8; ++i) s8[i] = 0ull;
    #pragma unroll
    for (unsigned ch = tid; ch < 64u * 8u; ch += 256u) {
        unsigned x[16];
        #pragma unroll
        for (int i = 0; i < 16; ++i) x[i] = ct[ch * G_CHUNK_PAD + i];
        x[0] ^= (ch >> 3) * (nb >> 4) + (n0 >> 4) + (ch & 7u);
        chacha_core(x);
        #pragma unroll
        for (int i = 0; i < 8; ++i) s8[i] += (unsigned long long)x[i] + (unsigned long long)x[i + 8];
    }
    #pragma unroll
    for (int i = 0; i < 8; ++i) {
        unsigned long long v = s8[i];
        #pragma unroll
        for (int off = 16; off > 0; off >>= 1) v += __shfl_down_sync(0xffffffffu, v, off);
        if (lane == 0u && v != 0ull) atomicAdd(&sums[i], v);
    }
}

// A memory test, no multiply: reads gathered columns in the same order as gather_gemm, but `piece` bytes of each column
// per step (gather_gemm uses 128), so the bandwidth of that access pattern can be measured on its own. grid =
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
