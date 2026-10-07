// Faster versions of two kernels of matmulhash.cu, for the Rust engine only. They compute EXACTLY the same values
// (the same chacha_core, the same inputs per block and chunk); what changes is how memory is touched:
//
// * keystream_fast: each thread still makes one ChaCha20 block, but the block of threads first puts its 256 results in
//   shared memory and then writes them out together, so neighbouring threads write neighbouring words (the original
//   has each thread write 64 bytes of its own, 64 bytes from its neighbour's: about a quarter of the write bandwidth).
// * fold_fast: the chunks of C are read through shared memory the same way, and the sums are added within a warp
//   before one atomic per warp (the original makes 8 atomics per thread on the same 8 addresses). Integer addition is
//   exact and order-independent, so the sums are the same.
//
// This file is compiled after matmulhash.cu, in one program: it uses chacha_core and set_constants from there. It is
// NOT the Python reference's copy (that is matmulhash.cu, kept identical by reference/tests/test_kernel_source_copy.py);
// it is checked by the Rust GPU tests against the CPU and the golden vectors.

#define FAST_THREADS 256
#define FAST_PAD 17  // 16 words of a block + 1, so the 32 threads of a warp hit 32 different shared-memory banks

extern "C" {

// The same as keystream_kernel (same arguments, same output), launched with FAST_THREADS threads per block.
__global__ void __launch_bounds__(FAST_THREADS) keystream_fast(const unsigned long long keys_ptr,
                                                              const unsigned long long out_ptr,
                                                              const unsigned long long blocks_per_key,
                                                              const unsigned long long start_counter,
                                                              const unsigned long long total_blocks)
{
    __shared__ unsigned tile[FAST_THREADS * FAST_PAD];
    const long long* keys = (const long long*)keys_ptr;
    unsigned* out = (unsigned*)out_ptr;
    const unsigned tid = threadIdx.x;
    for (unsigned long long base = (unsigned long long)blockIdx.x * FAST_THREADS; base < total_blocks;
         base += (unsigned long long)gridDim.x * FAST_THREADS) {
        const unsigned long long idx = base + tid;
        if (idx < total_blocks) {
            const unsigned long long b = idx / blocks_per_key;
            const unsigned long long local = idx - b * blocks_per_key;
            unsigned x[16];
            set_constants(x);
            #pragma unroll
            for (int i = 0; i < 8; ++i) {
                x[4 + i] = (unsigned)keys[b * 8 + i];
            }
            x[12] = (unsigned)(start_counter + local);
            x[13] = 0u; x[14] = 0u; x[15] = 0u;
            chacha_core(x);
            #pragma unroll
            for (int i = 0; i < 16; ++i) {
                tile[tid * FAST_PAD + i] = x[i];
            }
        }
        __syncthreads();
        const unsigned long long left = total_blocks - base;
        const unsigned n = left < FAST_THREADS ? (unsigned)left : FAST_THREADS;
        unsigned* dst = out + base * 16ull;
        for (unsigned w = tid; w < n * 16u; w += FAST_THREADS) {
            dst[w] = tile[(w >> 4) * FAST_PAD + (w & 15u)];
        }
        __syncthreads();
    }
}

// The same as fold_kernel (same arguments, same sums), launched with FAST_THREADS threads per block and
// grid = (any number of blocks per attempt, number of attempts).
__global__ void __launch_bounds__(FAST_THREADS) fold_fast(const unsigned long long c_ptr,
                                                         const unsigned long long sums_ptr,
                                                         const unsigned long long chunks)
{
    __shared__ unsigned tile[FAST_THREADS * FAST_PAD];
    const unsigned long long b = blockIdx.y;
    const unsigned* C = (const unsigned*)c_ptr + b * chunks * 16ull;
    unsigned long long* sums = (unsigned long long*)sums_ptr + b * 8ull;
    const unsigned tid = threadIdx.x;
    unsigned long long acc[8];
    #pragma unroll
    for (int i = 0; i < 8; ++i) {
        acc[i] = 0ull;
    }
    for (unsigned long long base = (unsigned long long)blockIdx.x * FAST_THREADS; base < chunks;
         base += (unsigned long long)gridDim.x * FAST_THREADS) {
        const unsigned long long left = chunks - base;
        const unsigned n = left < FAST_THREADS ? (unsigned)left : FAST_THREADS;
        const unsigned* src = C + base * 16ull;
        for (unsigned w = tid; w < n * 16u; w += FAST_THREADS) {
            tile[(w >> 4) * FAST_PAD + (w & 15u)] = src[w];
        }
        __syncthreads();
        if (tid < n) {
            unsigned x[16];
            #pragma unroll
            for (int i = 0; i < 16; ++i) {
                x[i] = tile[tid * FAST_PAD + i];
            }
            x[0] ^= (unsigned)(base + tid);
            chacha_core(x);
            #pragma unroll
            for (int i = 0; i < 8; ++i) {
                acc[i] += (unsigned long long)x[i] + (unsigned long long)x[i + 8];
            }
        }
        __syncthreads();
    }
    #pragma unroll
    for (int i = 0; i < 8; ++i) {
        unsigned long long v = acc[i];
        #pragma unroll
        for (int off = 16; off > 0; off >>= 1) {
            v += __shfl_down_sync(0xffffffffu, v, off);
        }
        if ((tid & 31u) == 0u && v != 0ull) {
            atomicAdd(&sums[i], v);
        }
    }
}

}  // extern "C"
