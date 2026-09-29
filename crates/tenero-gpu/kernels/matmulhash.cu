
#define PICKS 3

__device__ __forceinline__ unsigned rotl32(unsigned x, int n) {
    return (x << n) | (x >> (32 - n));
}

__device__ __forceinline__ void quarter_round(unsigned &a, unsigned &b, unsigned &c, unsigned &d) {
    a += b; d ^= a; d = rotl32(d, 16);
    c += d; b ^= c; b = rotl32(b, 12);
    a += b; d ^= a; d = rotl32(d, 8);
    c += d; b ^= c; b = rotl32(b, 7);
}

// The ChaCha20 permutation plus the feed-forward add, in place on 16 words
// (the same as tenero.chacha.chacha_core).
__device__ __forceinline__ void chacha_core(unsigned x[16]) {
    unsigned s[16];
    #pragma unroll
    for (int i = 0; i < 16; ++i) {
        s[i] = x[i];
    }
    #pragma unroll
    for (int r = 0; r < 10; ++r) {
        quarter_round(x[0], x[4], x[8],  x[12]);
        quarter_round(x[1], x[5], x[9],  x[13]);
        quarter_round(x[2], x[6], x[10], x[14]);
        quarter_round(x[3], x[7], x[11], x[15]);
        quarter_round(x[0], x[5], x[10], x[15]);
        quarter_round(x[1], x[6], x[11], x[12]);
        quarter_round(x[2], x[7], x[8],  x[13]);
        quarter_round(x[3], x[4], x[9],  x[14]);
    }
    #pragma unroll
    for (int i = 0; i < 16; ++i) {
        x[i] += s[i];
    }
}

__device__ __forceinline__ void set_constants(unsigned x[16]) {
    x[0] = 0x61707865u; x[1] = 0x3320646eu; x[2] = 0x79622d32u; x[3] = 0x6b206574u;
}

extern "C" {

// ChaCha20 keystream. For each of the keys (8 words each, stored as long long) it writes
// `blocks_per_key` blocks of 16 words, starting at block counter `start_counter`, nonce 0.
// Output layout: (num_keys, blocks_per_key, 16) words. Used for the attempt matrices X and for
// slice 0 of the dataset.
__global__ void keystream_kernel(const unsigned long long keys_ptr,
                                 const unsigned long long out_ptr,
                                 const unsigned long long blocks_per_key,
                                 const unsigned long long start_counter,
                                 const unsigned long long total_blocks)
{
    const long long* keys = (const long long*)keys_ptr;
    unsigned* out = (unsigned*)out_ptr;
    const unsigned long long stride = (unsigned long long)gridDim.x * blockDim.x;
    for (unsigned long long idx = (unsigned long long)blockIdx.x * blockDim.x + threadIdx.x;
         idx < total_blocks; idx += stride) {
        const unsigned long long b = idx / blocks_per_key;
        const unsigned long long local = idx - b * blocks_per_key;
        unsigned x[16];
        set_constants(x);
        for (int i = 0; i < 8; ++i) {
            x[4 + i] = (unsigned)keys[b * 8 + i];
        }
        x[12] = (unsigned)(start_counter + local);
        x[13] = 0u; x[14] = 0u; x[15] = 0u;
        chacha_core(x);
        for (int i = 0; i < 16; ++i) {
            out[idx * 16 + i] = x[i];
        }
    }
}

// Builds slice `slice_j` (>= 1) of the dataset from the slices before it. The dataset is
// (num_slices, blocks_per_slice, 16) words. Block u of slice j takes block u of slice j-1, makes
// PICKS data-dependent picks from anywhere in slices 0..j-1 (chosen by the words of that previous
// block), XORs the picks together, and mixes it all with one ChaCha20 block.
// (Identical to tenero.matmulhash.fill_slice.) Launch it for j = 1, 2, 3... in order.
__global__ void fill_kernel(const unsigned long long data_ptr,
                            const unsigned long long blocks_per_slice,
                            const unsigned slice_j)
{
    unsigned* data = (unsigned*)data_ptr;
    const unsigned long long slice_words = blocks_per_slice * 16ull;
    const unsigned* prev_slice = data + (unsigned long long)(slice_j - 1u) * slice_words;
    unsigned* out_slice = data + (unsigned long long)slice_j * slice_words;
    const unsigned long long stride = (unsigned long long)gridDim.x * blockDim.x;
    for (unsigned long long u = (unsigned long long)blockIdx.x * blockDim.x + threadIdx.x;
         u < blocks_per_slice; u += stride) {
        unsigned prev[16];
        unsigned ref[16];
        for (int w = 0; w < 16; ++w) {
            prev[w] = prev_slice[u * 16 + w];
            ref[w] = 0u;
        }
        for (int i = 0; i < PICKS; ++i) {
            const unsigned pick_slice = prev[2 * i] % slice_j;
            const unsigned pick_block = prev[2 * i + 1] % (unsigned)blocks_per_slice;
            const unsigned* picked = data + (unsigned long long)pick_slice * slice_words
                                          + (unsigned long long)pick_block * 16ull;
            for (int w = 0; w < 16; ++w) {
                ref[w] ^= picked[w];
            }
        }
        unsigned x[16];
        set_constants(x);
        for (int w = 0; w < 8; ++w) {
            x[4 + w] = prev[w] ^ ref[w];
        }
        x[12] = (unsigned)u;
        x[13] = slice_j;
        x[14] = prev[8] ^ ref[8];
        x[15] = prev[9] ^ ref[9];
        chacha_core(x);
        for (int w = 0; w < 16; ++w) {
            out_slice[u * 16 + w] = x[w] ^ prev[w] ^ ref[w];
        }
    }
}

// The fold. Every 16-word chunk of C (int32; the position is XORed into word 0) goes through the
// ChaCha20 permutation, and its words are added into 8 sums (word i and word i+8 into sum i).
// (Identical to tenero.matmulhash.fold_sums.) grid = (blocks per attempt, number of attempts).
// Integer addition is exact and order-independent, so the atomics stay deterministic.
__global__ void fold_kernel(const unsigned long long c_ptr,      // int[num_attempts][chunks * 16]
                            const unsigned long long sums_ptr,   // unsigned long long[num_attempts][8]
                            const unsigned long long chunks)
{
    const unsigned long long b = blockIdx.y;
    const unsigned* C = (const unsigned*)c_ptr + b * chunks * 16ull;
    unsigned long long* sums = (unsigned long long*)sums_ptr + b * 8ull;
    const unsigned long long t = (unsigned long long)blockIdx.x * blockDim.x + threadIdx.x;
    const unsigned long long stride = (unsigned long long)gridDim.x * blockDim.x;
    if (t >= chunks) {
        return;
    }
    unsigned long long acc[8];
    for (int i = 0; i < 8; ++i) {
        acc[i] = 0ull;
    }
    for (unsigned long long c = t; c < chunks; c += stride) {
        unsigned x[16];
        for (int i = 0; i < 16; ++i) {
            x[i] = C[c * 16ull + i];
        }
        x[0] ^= (unsigned)c;
        chacha_core(x);
        for (int i = 0; i < 8; ++i) {
            acc[i] += (unsigned long long)x[i] + (unsigned long long)x[i + 8];
        }
    }
    for (int i = 0; i < 8; ++i) {
        atomicAdd(&sums[i], acc[i]);
    }
}

}  // extern "C"
