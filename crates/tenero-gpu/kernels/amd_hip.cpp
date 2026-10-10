// AMD HIP/rocWMMA implementation of Tenero matmulhash v2.
//
// The consensus operations are the same as matmulhash.cu and gather.cu.  The multiply is native
// signed INT8 WMMA on RDNA3 (gfx1100): one 128-thread workgroup computes all 64 rows and 64
// columns in its tile, so every gathered 8 KiB dataset column is read exactly once per attempt.

#include <hip/hip_runtime.h>
#include <rocwmma/rocwmma.hpp>

#include <cstdint>
#include <cstdio>
#include <cstring>
#include <new>

namespace {

#ifndef TENERO_AMD_GEMM_THREADS
#define TENERO_AMD_GEMM_THREADS 128
#endif
#ifndef TENERO_AMD_M_TILE
#define TENERO_AMD_M_TILE 64
#endif
#ifndef TENERO_AMD_N_TILE
#define TENERO_AMD_N_TILE 64
#endif
#ifndef TENERO_AMD_K_STAGE
#define TENERO_AMD_K_STAGE 32
#endif
#ifndef TENERO_AMD_A_PAD
#define TENERO_AMD_A_PAD 0
#endif
#ifndef TENERO_AMD_B_PAD
#define TENERO_AMD_B_PAD 0
#endif
#ifndef TENERO_AMD_MIN_BLOCKS_PER_CU
#define TENERO_AMD_MIN_BLOCKS_PER_CU 0
#endif
#ifndef TENERO_AMD_COL_TILES_PER_WAVE
#define TENERO_AMD_COL_TILES_PER_WAVE 1
#endif
#ifndef TENERO_AMD_SPECIALIZE_DEFAULT
#define TENERO_AMD_SPECIALIZE_DEFAULT 1
#endif
#ifndef TENERO_AMD_DIRECT_A
#define TENERO_AMD_DIRECT_A 0
#endif
#ifndef TENERO_AMD_RESTRICT_POINTERS
#define TENERO_AMD_RESTRICT_POINTERS 0
#endif
#ifndef TENERO_AMD_HOIST_LOADS
#define TENERO_AMD_HOIST_LOADS 1
#endif
#ifndef TENERO_AMD_PREFETCH_LOADS
#define TENERO_AMD_PREFETCH_LOADS 1
#endif
#ifndef TENERO_AMD_PREFETCH_DISTANCE
#define TENERO_AMD_PREFETCH_DISTANCE 1
#endif
#ifndef TENERO_AMD_PREFETCH_A
#define TENERO_AMD_PREFETCH_A 1
#endif
#ifndef TENERO_AMD_FUSE_FOLD
#define TENERO_AMD_FUSE_FOLD 0
#endif
#ifndef TENERO_AMD_LDS_DOUBLE_BUFFER
#define TENERO_AMD_LDS_DOUBLE_BUFFER 0
#endif
#ifndef TENERO_AMD_DIRECT_B_NATIVE
#define TENERO_AMD_DIRECT_B_NATIVE 1
#endif
#ifndef TENERO_AMD_DIRECT_AB_NATIVE
#define TENERO_AMD_DIRECT_AB_NATIVE 0
#endif
#ifndef TENERO_AMD_GRID_ATTEMPT_X
#define TENERO_AMD_GRID_ATTEMPT_X 0
#endif
#ifndef TENERO_AMD_X_STAGE_MAJOR
#define TENERO_AMD_X_STAGE_MAJOR 1
#endif
#ifndef TENERO_AMD_B_NONTEMPORAL
#define TENERO_AMD_B_NONTEMPORAL 0
#endif

constexpr uint32_t THREADS = 256;
constexpr uint32_t GEMM_THREADS = TENERO_AMD_GEMM_THREADS;
constexpr uint32_t M = 64;
constexpr uint32_t M_TILE = TENERO_AMD_M_TILE;
constexpr uint32_t M_TILES = M / M_TILE;
constexpr uint32_t N_TILE = TENERO_AMD_N_TILE;
constexpr uint32_t K_STAGE = TENERO_AMD_K_STAGE;
constexpr uint32_t A_LD = K_STAGE + TENERO_AMD_A_PAD;
constexpr uint32_t B_LD = K_STAGE + TENERO_AMD_B_PAD;
constexpr uint32_t COL_TILES_PER_WAVE = TENERO_AMD_COL_TILES_PER_WAVE;
constexpr bool DIRECT_A = TENERO_AMD_DIRECT_A != 0;
constexpr bool HOIST_LOADS = TENERO_AMD_HOIST_LOADS != 0;
constexpr bool PREFETCH_LOADS = TENERO_AMD_PREFETCH_LOADS != 0;
constexpr uint32_t PREFETCH_DISTANCE = TENERO_AMD_PREFETCH_DISTANCE;
constexpr bool PREFETCH_A = TENERO_AMD_PREFETCH_A != 0;
constexpr bool FUSE_FOLD = TENERO_AMD_FUSE_FOLD != 0;
constexpr bool LDS_DOUBLE_BUFFER = TENERO_AMD_LDS_DOUBLE_BUFFER != 0;
constexpr bool DIRECT_B_NATIVE = TENERO_AMD_DIRECT_B_NATIVE != 0;
constexpr bool DIRECT_AB_NATIVE = TENERO_AMD_DIRECT_AB_NATIVE != 0;
constexpr bool GRID_ATTEMPT_X = TENERO_AMD_GRID_ATTEMPT_X != 0;
constexpr bool X_STAGE_MAJOR = TENERO_AMD_X_STAGE_MAJOR != 0;
constexpr bool B_NONTEMPORAL = TENERO_AMD_B_NONTEMPORAL != 0;
constexpr uint32_t PICKS = 3;

static_assert(GEMM_THREADS >= 32 && GEMM_THREADS <= 1024 && GEMM_THREADS % 32 == 0,
              "GEMM_THREADS must contain complete wave32s");
static_assert(M_TILE > 0 && M % M_TILE == 0 && M_TILE % 16 == 0,
              "M_TILE must be a 16-row divisor of M");
static_assert(COL_TILES_PER_WAVE > 0, "each wave32 must compute at least one column tile");
static_assert(N_TILE == (GEMM_THREADS / 32) * 16 * COL_TILES_PER_WAVE,
              "N_TILE must match the wave32-to-column-tile mapping");
static_assert(K_STAGE > 0 && K_STAGE % 16 == 0, "K_STAGE must be a multiple of 16");
static_assert(A_LD % 4 == 0 && B_LD % 4 == 0,
              "LDS leading dimensions must preserve 32-bit alignment");

#if TENERO_AMD_MIN_BLOCKS_PER_CU > 0
#define TENERO_GEMM_LAUNCH_BOUNDS \
    __launch_bounds__(TENERO_AMD_GEMM_THREADS, TENERO_AMD_MIN_BLOCKS_PER_CU)
#else
#define TENERO_GEMM_LAUNCH_BOUNDS __launch_bounds__(TENERO_AMD_GEMM_THREADS)
#endif

#if TENERO_AMD_RESTRICT_POINTERS
#define TENERO_RESTRICT __restrict__
#else
#define TENERO_RESTRICT
#endif

inline void set_error(char* out, size_t cap, const char* what, hipError_t e = hipSuccess) {
    if (!out || cap == 0) return;
    if (e == hipSuccess) {
        std::snprintf(out, cap, "%s", what);
    } else {
        std::snprintf(out, cap, "%s: %s", what, hipGetErrorString(e));
    }
}

#define HIP_OR_RETURN(call, msg)                                \
    do {                                                        \
        const hipError_t tenero_hip_e = (call);                \
        if (tenero_hip_e != hipSuccess) {                      \
            set_error(error, error_cap, (msg), tenero_hip_e);  \
            return 1;                                          \
        }                                                       \
    } while (0)

__device__ __forceinline__ uint32_t rotl32(uint32_t x, int n) {
    return (x << n) | (x >> (32 - n));
}

__device__ __forceinline__ void quarter_round(uint32_t& a, uint32_t& b, uint32_t& c,
                                               uint32_t& d) {
    a += b; d ^= a; d = rotl32(d, 16);
    c += d; b ^= c; b = rotl32(b, 12);
    a += b; d ^= a; d = rotl32(d, 8);
    c += d; b ^= c; b = rotl32(b, 7);
}

__device__ __forceinline__ void chacha_core(uint32_t x[16]) {
    uint32_t original[16];
#pragma unroll
    for (int i = 0; i < 16; ++i) original[i] = x[i];
#pragma unroll
    for (int round = 0; round < 10; ++round) {
        quarter_round(x[0], x[4], x[8], x[12]);
        quarter_round(x[1], x[5], x[9], x[13]);
        quarter_round(x[2], x[6], x[10], x[14]);
        quarter_round(x[3], x[7], x[11], x[15]);
        quarter_round(x[0], x[5], x[10], x[15]);
        quarter_round(x[1], x[6], x[11], x[12]);
        quarter_round(x[2], x[7], x[8], x[13]);
        quarter_round(x[3], x[4], x[9], x[14]);
    }
#pragma unroll
    for (int i = 0; i < 16; ++i) x[i] += original[i];
}

__device__ __forceinline__ void set_constants(uint32_t x[16]) {
    x[0] = 0x61707865u;
    x[1] = 0x3320646eu;
    x[2] = 0x79622d32u;
    x[3] = 0x6b206574u;
}

__device__ __forceinline__ uint2 load_dataset_b64(const int8_t* source) {
    if constexpr (B_NONTEMPORAL) {
        using U32x2 = uint32_t __attribute__((ext_vector_type(2)));
        const U32x2 value = __builtin_nontemporal_load(
            reinterpret_cast<const U32x2*>(source));
        return uint2{value[0], value[1]};
    }
    return *reinterpret_cast<const uint2*>(source);
}

// ChaCha20 keystream, byte-for-byte identical to the official CUDA and CPU versions.
template<bool StageMajorX>
__global__ __launch_bounds__(THREADS) void keystream_kernel(const uint32_t* keys, uint32_t* out,
                                                             uint64_t blocks_per_key,
                                                             uint64_t start_counter,
                                                             uint64_t total_blocks) {
    const uint64_t stride = uint64_t(gridDim.x) * blockDim.x;
    for (uint64_t idx = uint64_t(blockIdx.x) * blockDim.x + threadIdx.x; idx < total_blocks;
         idx += stride) {
        const uint64_t key_index = idx / blocks_per_key;
        const uint64_t scheduled_local = idx - key_index * blocks_per_key;
        uint64_t local = scheduled_local;
        uint64_t x_row = 0;
        uint64_t x_block = 0;
        if constexpr (StageMajorX) {
            const uint64_t blocks_per_row = blocks_per_key / M;
            x_row = scheduled_local % M;
            x_block = scheduled_local / M;
            local = x_row * blocks_per_row + x_block;
        }
        uint32_t x[16];
        set_constants(x);
#pragma unroll
        for (int i = 0; i < 8; ++i) x[4 + i] = keys[key_index * 8 + i];
        x[12] = uint32_t(start_counter + local);
        x[13] = 0;
        x[14] = 0;
        x[15] = 0;
        chacha_core(x);
        if constexpr (StageMajorX) {
            uint32_t* key_out = out + key_index * blocks_per_key * 16;
            uint32_t* first = key_out + (2 * x_block * M + x_row) * 8;
            uint32_t* second = first + M * 8;
            *reinterpret_cast<uint4*>(first) = uint4{x[0], x[1], x[2], x[3]};
            *reinterpret_cast<uint4*>(first + 4) = uint4{x[4], x[5], x[6], x[7]};
            *reinterpret_cast<uint4*>(second) = uint4{x[8], x[9], x[10], x[11]};
            *reinterpret_cast<uint4*>(second + 4) = uint4{x[12], x[13], x[14], x[15]};
        } else {
#pragma unroll
            for (int i = 0; i < 16; ++i) out[idx * 16 + i] = x[i];
        }
    }
}

// Dataset slices 1..255, identical to the official construction.
__global__ __launch_bounds__(THREADS) void fill_kernel(uint32_t* data, uint64_t blocks_per_slice,
                                                       uint32_t slice_j) {
    const uint64_t slice_words = blocks_per_slice * 16;
    const uint32_t* prev_slice = data + uint64_t(slice_j - 1) * slice_words;
    uint32_t* out_slice = data + uint64_t(slice_j) * slice_words;
    const uint64_t stride = uint64_t(gridDim.x) * blockDim.x;
    for (uint64_t u = uint64_t(blockIdx.x) * blockDim.x + threadIdx.x; u < blocks_per_slice;
         u += stride) {
        uint32_t prev[16], ref[16];
#pragma unroll
        for (int w = 0; w < 16; ++w) {
            prev[w] = prev_slice[u * 16 + w];
            ref[w] = 0;
        }
#pragma unroll
        for (int i = 0; i < int(PICKS); ++i) {
            const uint32_t pick_slice = prev[2 * i] % slice_j;
            const uint32_t pick_block = prev[2 * i + 1] % uint32_t(blocks_per_slice);
            const uint32_t* picked = data + uint64_t(pick_slice) * slice_words
                                     + uint64_t(pick_block) * 16;
#pragma unroll
            for (int w = 0; w < 16; ++w) ref[w] ^= picked[w];
        }
        uint32_t x[16];
        set_constants(x);
#pragma unroll
        for (int w = 0; w < 8; ++w) x[4 + w] = prev[w] ^ ref[w];
        x[12] = uint32_t(u);
        x[13] = slice_j;
        x[14] = prev[8] ^ ref[8];
        x[15] = prev[9] ^ ref[9];
        chacha_core(x);
#pragma unroll
        for (int w = 0; w < 16; ++w) out_slice[u * 16 + w] = x[w] ^ prev[w] ^ ref[w];
    }
}

// C = X @ W. One workgroup owns 64 rows and 64 columns of one attempt. Four wave32s each own
// 16 columns and four 16x16 accumulators. K is staged 32 bytes at a time in LDS while retaining
// one global dataset read per attempt.
template<bool DefaultGather>
__global__ TENERO_GEMM_LAUNCH_BOUNDS void gather_wmma_kernel(
    const int8_t* TENERO_RESTRICT x, const int8_t* TENERO_RESTRICT data,
    const uint32_t* TENERO_RESTRICT idx, const uint32_t* TENERO_RESTRICT slices,
    int32_t* TENERO_RESTRICT c, uint64_t* TENERO_RESTRICT sums, uint32_t k, uint32_t nb,
    uint32_t columns, uint32_t gathered) {
    constexpr uint32_t A_BUFFER_SIZE = (DIRECT_A || DIRECT_AB_NATIVE) ? 16 : M_TILE * A_LD;
    constexpr uint32_t B_BUFFER_SIZE = DIRECT_B_NATIVE ? 16 : N_TILE * B_LD;
    constexpr uint32_t LDS_BUFFERS = LDS_DOUBLE_BUFFER ? 2 : 1;
    __shared__ __align__(16) int8_t sx[LDS_BUFFERS * A_BUFFER_SIZE];
    __shared__ __align__(16) int8_t sw[LDS_BUFFERS * B_BUFFER_SIZE];
    __shared__ __align__(16) int32_t sc[FUSE_FOLD ? M_TILE * N_TILE : 1];
    __shared__ __align__(16) uint64_t wave_sums[FUSE_FOLD ? (GEMM_THREADS / 32) * 8 : 1];

    const uint32_t attempt = GRID_ATTEMPT_X ? blockIdx.x : blockIdx.y;
    const uint32_t row_tile = blockIdx.z;
    const uint32_t row0 = row_tile * M_TILE;
    const uint32_t n0 = (GRID_ATTEMPT_X ? blockIdx.y : blockIdx.x) * N_TILE;
    const uint32_t tid = threadIdx.x;
    const uint32_t wave = tid >> 5;
    const uint32_t lane = tid & 31;
    const uint32_t effective_k = DefaultGather ? 8192u : k;
    const uint32_t effective_nb = DefaultGather ? 2048u : nb;
    const int8_t* attempt_x = x + uint64_t(attempt) * M * effective_k
                              + (X_STAGE_MAJOR ? 0 : uint64_t(row0) * effective_k);
    int32_t* attempt_c = c + uint64_t(attempt) * M * effective_nb
                         + uint64_t(row0) * effective_nb;

    using AFrag = rocwmma::fragment<rocwmma::matrix_a, 16, 16, 16, int8_t,
                                    rocwmma::row_major>;
    using BFrag = rocwmma::fragment<rocwmma::matrix_b, 16, 16, 16, int8_t,
                                    rocwmma::col_major>;
    using CFrag = rocwmma::fragment<rocwmma::accumulator, 16, 16, 16, int32_t>;
    CFrag accum[COL_TILES_PER_WAVE][M_TILE / 16];
#pragma unroll
    for (int ct = 0; ct < int(COL_TILES_PER_WAVE); ++ct) {
#pragma unroll
        for (int r = 0; r < int(M_TILE / 16); ++r) {
            rocwmma::fill_fragment(accum[ct][r], int32_t(0));
        }
    }

    constexpr uint32_t stage_parts = K_STAGE / 16;
    constexpr uint32_t A_STAGE_VECTORS = M_TILE * stage_parts;
    constexpr bool A_TWO_VECTORS_PER_THREAD = A_STAGE_VECTORS == 2 * GEMM_THREADS;
    static_assert(!HOIST_LOADS || (((A_STAGE_VECTORS <= GEMM_THREADS
                                    && GEMM_THREADS % A_STAGE_VECTORS == 0)
                                   || A_TWO_VECTORS_PER_THREAD)
                                   && N_TILE * stage_parts == GEMM_THREADS),
                  "hoisted loads require one or two A vectors and exactly one B vector per thread");
    uint32_t cached_xrow = 0;
    uint32_t cached_xpart = 0;
    uint32_t cached_bpart = 0;
    uint64_t cached_dataset_col = 0;
    if constexpr (HOIST_LOADS) {
        if constexpr (A_TWO_VECTORS_PER_THREAD) {
            cached_xrow = tid % M_TILE;
            cached_xpart = (tid / M_TILE) * 2;
        } else {
            const uint32_t a_load = tid % A_STAGE_VECTORS;
            cached_xrow = a_load / stage_parts;
            cached_xpart = a_load % stage_parts;
        }
        const uint32_t local_col = DIRECT_B_NATIVE
            ? wave * 16 + (lane & 15)
            : tid / stage_parts;
        cached_bpart = DIRECT_B_NATIVE ? (lane >> 4) * 8 : tid % stage_parts;
        const uint32_t n = n0 + local_col;
        if constexpr (DefaultGather) {
            cached_dataset_col = uint64_t(idx[uint64_t(attempt) * 2048u + n] & 0x7ffffu);
        } else {
            cached_dataset_col = gathered
                ? uint64_t(idx[uint64_t(attempt) * effective_nb + n] % columns)
                : uint64_t(slices[attempt]) * effective_nb + n;
        }
    }

    static_assert(!PREFETCH_LOADS || (HOIST_LOADS && !DIRECT_A && K_STAGE == 32
                                      && A_LD % 16 == 0 && B_LD % 16 == 0),
                  "software prefetch needs the hoisted, aligned 32-byte stage");
    static_assert(!LDS_DOUBLE_BUFFER || PREFETCH_LOADS,
                  "LDS double buffering requires software prefetch");
    static_assert(!DIRECT_B_NATIVE || (PREFETCH_LOADS && PREFETCH_DISTANCE == 1
                                       && K_STAGE == 32 && COL_TILES_PER_WAVE == 1
                                       && A_STAGE_VECTORS <= GEMM_THREADS
                                       && GEMM_THREADS % A_STAGE_VECTORS == 0),
                  "native direct-B currently requires the single-stage register pipeline");
    static_assert(!DIRECT_AB_NATIVE || (DIRECT_B_NATIVE && !LDS_DOUBLE_BUFFER
                                        && M_TILE == 64 && N_TILE == 64
                                        && GEMM_THREADS == 128),
                  "native direct-A+B currently requires the four-wave direct-B pipeline");
    static_assert(PREFETCH_DISTANCE == 1 || PREFETCH_DISTANCE == 2,
                  "prefetch distance must be one or two stages");
    static_assert(!FUSE_FOLD || (M_TILE == 64 && N_TILE == 64 && GEMM_THREADS == 128
                                 && COL_TILES_PER_WAVE == 1),
                  "fused fold currently requires the 64x64 four-wave tile");
    static_assert(!X_STAGE_MAJOR || (PREFETCH_LOADS && DIRECT_B_NATIVE
                                     && !DIRECT_AB_NATIVE && !LDS_DOUBLE_BUFFER
                                     && M_TILE == 64 && A_STAGE_VECTORS == GEMM_THREADS),
                  "stage-major X currently targets the single-buffer direct-B pipeline");
    if constexpr (PREFETCH_LOADS) {
        uint4 staged_a{};
        if constexpr (!DIRECT_AB_NATIVE) {
            if constexpr (A_STAGE_VECTORS < GEMM_THREADS) {
                if (tid < A_STAGE_VECTORS) {
                    staged_a = *reinterpret_cast<const uint4*>(
                        attempt_x + uint64_t(cached_xrow) * effective_k
                        + cached_xpart * 16);
                }
            } else {
                if constexpr (X_STAGE_MAJOR) {
                    const uint32_t x_byte = cached_xpart * 16;
                    staged_a = *reinterpret_cast<const uint4*>(
                        attempt_x + uint64_t(x_byte / 32) * M * 32
                        + uint64_t(row0 + cached_xrow) * 32 + x_byte % 32);
                } else {
                    staged_a = *reinterpret_cast<const uint4*>(
                        attempt_x + uint64_t(cached_xrow) * effective_k
                        + cached_xpart * 16);
                }
            }
        }
        uint4 staged_a_extra{};
        if constexpr (A_TWO_VECTORS_PER_THREAD && !DIRECT_AB_NATIVE) {
            if constexpr (X_STAGE_MAJOR) {
                const uint32_t x_byte = (cached_xpart + 1) * 16;
                staged_a_extra = *reinterpret_cast<const uint4*>(
                    attempt_x + uint64_t(x_byte / 32) * M * 32
                    + uint64_t(row0 + cached_xrow) * 32 + x_byte % 32);
            } else {
                staged_a_extra = *reinterpret_cast<const uint4*>(
                    attempt_x + uint64_t(cached_xrow) * effective_k
                    + (cached_xpart + 1) * 16);
            }
        }
        uint4 staged_b{};
        if constexpr (DIRECT_B_NATIVE) {
            const int8_t* source = data + cached_dataset_col * effective_k + cached_bpart;
            const uint2 first = load_dataset_b64(source);
            const uint2 second = load_dataset_b64(source + 16);
            staged_b = uint4{first.x, first.y, second.x, second.y};
        } else {
            staged_b = *reinterpret_cast<const uint4*>(
                data + cached_dataset_col * effective_k + cached_bpart * 16);
        }
        if constexpr (!DIRECT_AB_NATIVE) {
            if constexpr (A_STAGE_VECTORS < GEMM_THREADS) {
                if (tid < A_STAGE_VECTORS) {
                    *reinterpret_cast<uint4*>(sx + cached_xrow * A_LD
                                              + cached_xpart * 16) = staged_a;
                }
            } else {
                *reinterpret_cast<uint4*>(sx + cached_xrow * A_LD + cached_xpart * 16)
                    = staged_a;
            }
            if constexpr (A_TWO_VECTORS_PER_THREAD) {
                *reinterpret_cast<uint4*>(sx + cached_xrow * A_LD + 16) = staged_a_extra;
            }
        }
        const uint32_t cached_local_col = tid / stage_parts;
        if constexpr (!DIRECT_B_NATIVE) {
            *reinterpret_cast<uint4*>(sw + cached_local_col * B_LD + cached_bpart * 16)
                = staged_b;
        }
        if constexpr (!DIRECT_AB_NATIVE) __syncthreads();

        if constexpr (PREFETCH_DISTANCE == 1) {
        if constexpr (LDS_DOUBLE_BUFFER && DIRECT_B_NATIVE) {
        uint4 current_b = staged_b;
        for (uint32_t k0 = 0; k0 < effective_k; k0 += 2 * K_STAGE) {
            {
                const bool has_next = k0 + K_STAGE < effective_k;
                uint4 next_a{}, next_b{};
                if (has_next) {
                    next_a = *reinterpret_cast<const uint4*>(
                        attempt_x + uint64_t(cached_xrow) * effective_k + k0 + K_STAGE
                        + cached_xpart * 16);
                    const int8_t* source = data + cached_dataset_col * effective_k
                                           + k0 + K_STAGE + cached_bpart;
                    const uint2 first = load_dataset_b64(source);
                    const uint2 second = load_dataset_b64(source + 16);
                    next_b = uint4{first.x, first.y, second.x, second.y};
                }
#pragma unroll
                for (uint32_t ks = 0; ks < K_STAGE; ks += 16) {
#pragma unroll
                    for (int r = 0; r < int(M_TILE / 16); ++r) {
                        AFrag afrag;
                        rocwmma::load_matrix_sync(afrag, sx + r * 16 * A_LD + ks, A_LD);
                        using I32x2 = int32_t __attribute__((ext_vector_type(2)));
                        using I32x4 = int32_t __attribute__((ext_vector_type(4)));
                        using I32x8 = int32_t __attribute__((ext_vector_type(8)));
                        const uint32_t own0 = ks == 0 ? current_b.x : current_b.z;
                        const uint32_t own1 = ks == 0 ? current_b.y : current_b.w;
                        const I32x4 b_native{
                            int32_t(own0), int32_t(own1),
                            __shfl_xor(int32_t(own0), 16, 32),
                            __shfl_xor(int32_t(own1), 16, 32)};
                        const I32x2& a_packed = reinterpret_cast<const I32x2&>(*afrag);
                        const I32x4 a_native{
                            a_packed[0], a_packed[1],
                            __shfl_xor(a_packed[0], 16, 32),
                            __shfl_xor(a_packed[1], 16, 32)};
                        I32x8& c_native = reinterpret_cast<I32x8&>(*accum[0][r]);
                        c_native = __builtin_amdgcn_wmma_i32_16x16x16_iu8_w32(
                            true, a_native, true, b_native, c_native, true);
                    }
                }
                if (!has_next) break;
                *reinterpret_cast<uint4*>(sx + A_BUFFER_SIZE + cached_xrow * A_LD
                                          + cached_xpart * 16) = next_a;
                current_b = next_b;
                __syncthreads();
            }
            {
                const bool has_next = k0 + 2 * K_STAGE < effective_k;
                uint4 next_a{}, next_b{};
                if (has_next) {
                    next_a = *reinterpret_cast<const uint4*>(
                        attempt_x + uint64_t(cached_xrow) * effective_k + k0 + 2 * K_STAGE
                        + cached_xpart * 16);
                    const int8_t* source = data + cached_dataset_col * effective_k
                                           + k0 + 2 * K_STAGE + cached_bpart;
                    const uint2 first = load_dataset_b64(source);
                    const uint2 second = load_dataset_b64(source + 16);
                    next_b = uint4{first.x, first.y, second.x, second.y};
                }
#pragma unroll
                for (uint32_t ks = 0; ks < K_STAGE; ks += 16) {
#pragma unroll
                    for (int r = 0; r < int(M_TILE / 16); ++r) {
                        AFrag afrag;
                        rocwmma::load_matrix_sync(
                            afrag, sx + A_BUFFER_SIZE + r * 16 * A_LD + ks, A_LD);
                        using I32x2 = int32_t __attribute__((ext_vector_type(2)));
                        using I32x4 = int32_t __attribute__((ext_vector_type(4)));
                        using I32x8 = int32_t __attribute__((ext_vector_type(8)));
                        const uint32_t own0 = ks == 0 ? current_b.x : current_b.z;
                        const uint32_t own1 = ks == 0 ? current_b.y : current_b.w;
                        const I32x4 b_native{
                            int32_t(own0), int32_t(own1),
                            __shfl_xor(int32_t(own0), 16, 32),
                            __shfl_xor(int32_t(own1), 16, 32)};
                        const I32x2& a_packed = reinterpret_cast<const I32x2&>(*afrag);
                        const I32x4 a_native{
                            a_packed[0], a_packed[1],
                            __shfl_xor(a_packed[0], 16, 32),
                            __shfl_xor(a_packed[1], 16, 32)};
                        I32x8& c_native = reinterpret_cast<I32x8&>(*accum[0][r]);
                        c_native = __builtin_amdgcn_wmma_i32_16x16x16_iu8_w32(
                            true, a_native, true, b_native, c_native, true);
                    }
                }
                if (!has_next) break;
                *reinterpret_cast<uint4*>(sx + cached_xrow * A_LD + cached_xpart * 16)
                    = next_a;
                current_b = next_b;
                __syncthreads();
            }
        }
        } else if constexpr (LDS_DOUBLE_BUFFER) {
        for (uint32_t k0 = 0; k0 < effective_k; k0 += 2 * K_STAGE) {
            {
            const bool has_next1 = k0 + K_STAGE < effective_k;
            uint4 next_a1{}, next_b1{};
            if (has_next1) {
                if constexpr (PREFETCH_A && !DIRECT_AB_NATIVE) {
                    next_a1 = *reinterpret_cast<const uint4*>(
                        attempt_x + uint64_t(cached_xrow) * effective_k + k0 + K_STAGE
                        + cached_xpart * 16);
                }
                next_b1 = *reinterpret_cast<const uint4*>(
                    data + cached_dataset_col * effective_k + k0 + K_STAGE
                    + cached_bpart * 16);
            }
#pragma unroll
            for (uint32_t ks = 0; ks < K_STAGE; ks += 16) {
#pragma unroll
                for (int ct = 0; ct < int(COL_TILES_PER_WAVE); ++ct) {
                    BFrag bfrag;
                    const uint32_t local_n = (wave * COL_TILES_PER_WAVE + ct) * 16;
                    rocwmma::load_matrix_sync(bfrag, sw + local_n * B_LD + ks, B_LD);
#pragma unroll
                    for (int r = 0; r < int(M_TILE / 16); ++r) {
                        AFrag afrag;
                        rocwmma::load_matrix_sync(afrag, sx + r * 16 * A_LD + ks, A_LD);
                        rocwmma::mma_sync(accum[ct][r], afrag, bfrag, accum[ct][r]);
                    }
                }
            }
            if (!has_next1) break;
            if constexpr (!PREFETCH_A) {
                next_a1 = *reinterpret_cast<const uint4*>(
                    attempt_x + uint64_t(cached_xrow) * effective_k + k0 + K_STAGE
                    + cached_xpart * 16);
            }
            *reinterpret_cast<uint4*>(sx + A_BUFFER_SIZE + cached_xrow * A_LD
                                      + cached_xpart * 16) = next_a1;
            *reinterpret_cast<uint4*>(sw + B_BUFFER_SIZE + cached_local_col * B_LD
                                      + cached_bpart * 16) = next_b1;
            __syncthreads();
            }

            {
            const bool has_next2 = k0 + 2 * K_STAGE < effective_k;
            uint4 next_a2{}, next_b2{};
            if (has_next2) {
                if constexpr (PREFETCH_A && !DIRECT_AB_NATIVE) {
                    next_a2 = *reinterpret_cast<const uint4*>(
                        attempt_x + uint64_t(cached_xrow) * effective_k + k0 + 2 * K_STAGE
                        + cached_xpart * 16);
                }
                next_b2 = *reinterpret_cast<const uint4*>(
                    data + cached_dataset_col * effective_k + k0 + 2 * K_STAGE
                    + cached_bpart * 16);
            }
#pragma unroll
            for (uint32_t ks = 0; ks < K_STAGE; ks += 16) {
#pragma unroll
                for (int ct = 0; ct < int(COL_TILES_PER_WAVE); ++ct) {
                    BFrag bfrag;
                    const uint32_t local_n = (wave * COL_TILES_PER_WAVE + ct) * 16;
                    rocwmma::load_matrix_sync(
                        bfrag, sw + B_BUFFER_SIZE + local_n * B_LD + ks, B_LD);
#pragma unroll
                    for (int r = 0; r < int(M_TILE / 16); ++r) {
                        AFrag afrag;
                        rocwmma::load_matrix_sync(
                            afrag, sx + A_BUFFER_SIZE + r * 16 * A_LD + ks, A_LD);
                        rocwmma::mma_sync(accum[ct][r], afrag, bfrag, accum[ct][r]);
                    }
                }
            }
            if (!has_next2) break;
            if constexpr (!PREFETCH_A) {
                next_a2 = *reinterpret_cast<const uint4*>(
                    attempt_x + uint64_t(cached_xrow) * effective_k + k0 + 2 * K_STAGE
                    + cached_xpart * 16);
            }
            *reinterpret_cast<uint4*>(sx + cached_xrow * A_LD + cached_xpart * 16) = next_a2;
            *reinterpret_cast<uint4*>(sw + cached_local_col * B_LD + cached_bpart * 16) = next_b2;
            __syncthreads();
            }
        }
        } else {
        uint4 current_b = staged_b;
        for (uint32_t k0 = 0; k0 < effective_k; k0 += K_STAGE) {
            const bool has_next = k0 + K_STAGE < effective_k;
            uint4 next_a{};
            uint4 next_a_extra{};
            uint4 next_b{};
            if (has_next) {
                if constexpr (PREFETCH_A && !DIRECT_AB_NATIVE) {
                    if constexpr (A_STAGE_VECTORS < GEMM_THREADS) {
                        if (tid < A_STAGE_VECTORS) {
                            next_a = *reinterpret_cast<const uint4*>(
                                attempt_x + uint64_t(cached_xrow) * effective_k + k0
                                + K_STAGE + cached_xpart * 16);
                        }
                    } else {
                        if constexpr (X_STAGE_MAJOR) {
                            const uint32_t x_byte
                                = k0 + K_STAGE + cached_xpart * 16;
                            next_a = *reinterpret_cast<const uint4*>(
                                attempt_x + uint64_t(x_byte / 32) * M * 32
                                + uint64_t(row0 + cached_xrow) * 32 + x_byte % 32);
                        } else {
                            next_a = *reinterpret_cast<const uint4*>(
                                attempt_x + uint64_t(cached_xrow) * effective_k + k0
                                + K_STAGE + cached_xpart * 16);
                        }
                    }
                    if constexpr (A_TWO_VECTORS_PER_THREAD) {
                        if constexpr (X_STAGE_MAJOR) {
                            const uint32_t x_byte
                                = k0 + K_STAGE + (cached_xpart + 1) * 16;
                            next_a_extra = *reinterpret_cast<const uint4*>(
                                attempt_x + uint64_t(x_byte / 32) * M * 32
                                + uint64_t(row0 + cached_xrow) * 32 + x_byte % 32);
                        } else {
                            next_a_extra = *reinterpret_cast<const uint4*>(
                                attempt_x + uint64_t(cached_xrow) * effective_k + k0
                                + K_STAGE + (cached_xpart + 1) * 16);
                        }
                    }
                }
                if constexpr (DIRECT_B_NATIVE) {
                    const int8_t* source = data + cached_dataset_col * effective_k
                                           + k0 + K_STAGE + cached_bpart;
                    const uint2 first = load_dataset_b64(source);
                    const uint2 second = load_dataset_b64(source + 16);
                    next_b = uint4{first.x, first.y, second.x, second.y};
                } else {
                    next_b = *reinterpret_cast<const uint4*>(
                        data + cached_dataset_col * effective_k + k0 + K_STAGE
                        + cached_bpart * 16);
                }
            }

#pragma unroll
            for (uint32_t ks = 0; ks < K_STAGE; ks += 16) {
#pragma unroll
                for (int ct = 0; ct < int(COL_TILES_PER_WAVE); ++ct) {
                    BFrag bfrag;
                    const uint32_t local_n = (wave * COL_TILES_PER_WAVE + ct) * 16;
                    if constexpr (!DIRECT_B_NATIVE) {
                        rocwmma::load_matrix_sync(bfrag, sw + local_n * B_LD + ks, B_LD);
                    }
#pragma unroll
                    for (int r = 0; r < int(M_TILE / 16); ++r) {
                        AFrag afrag;
                        if constexpr (!DIRECT_AB_NATIVE) {
                            rocwmma::load_matrix_sync(
                                afrag, sx + r * 16 * A_LD + ks, A_LD);
                        }
                        if constexpr (DIRECT_B_NATIVE) {
                            using I32x4 = int32_t __attribute__((ext_vector_type(4)));
                            using I32x2 = int32_t __attribute__((ext_vector_type(2)));
                            using I32x8 = int32_t __attribute__((ext_vector_type(8)));
                            const uint32_t own0 = ks == 0 ? current_b.x : current_b.z;
                            const uint32_t own1 = ks == 0 ? current_b.y : current_b.w;
                            const I32x4 b_native{
                                int32_t(own0), int32_t(own1),
                                __shfl_xor(int32_t(own0), 16, 32),
                                __shfl_xor(int32_t(own1), 16, 32)};
                            I32x4 a_native{};
                            if constexpr (DIRECT_AB_NATIVE) {
                                const uint32_t a_row = uint32_t(r) * 16 + (lane & 15);
                                const uint32_t a_half = (lane >> 4) * 8;
                                const uint2 a_words = *reinterpret_cast<const uint2*>(
                                    attempt_x + uint64_t(a_row) * effective_k + k0 + ks
                                    + a_half);
                                a_native = I32x4{
                                    int32_t(a_words.x), int32_t(a_words.y),
                                    __shfl_xor(int32_t(a_words.x), 16, 32),
                                    __shfl_xor(int32_t(a_words.y), 16, 32)};
                            } else {
                                const I32x2& a_packed
                                    = reinterpret_cast<const I32x2&>(*afrag);
                                a_native = I32x4{
                                    a_packed[0], a_packed[1],
                                    __shfl_xor(a_packed[0], 16, 32),
                                    __shfl_xor(a_packed[1], 16, 32)};
                            }
                            I32x8& c_native = reinterpret_cast<I32x8&>(*accum[ct][r]);
                            c_native = __builtin_amdgcn_wmma_i32_16x16x16_iu8_w32(
                                true, a_native, true, b_native, c_native, true);
                        } else {
                            rocwmma::mma_sync(accum[ct][r], afrag, bfrag, accum[ct][r]);
                        }
                    }
                }
            }
            if constexpr (!DIRECT_AB_NATIVE) __syncthreads();
            if (!has_next) break;
            if constexpr (!PREFETCH_A && !DIRECT_AB_NATIVE) {
                next_a = *reinterpret_cast<const uint4*>(
                    attempt_x + uint64_t(cached_xrow) * effective_k + k0 + K_STAGE
                    + cached_xpart * 16);
                if constexpr (A_TWO_VECTORS_PER_THREAD) {
                    next_a_extra = *reinterpret_cast<const uint4*>(
                        attempt_x + uint64_t(cached_xrow) * effective_k + k0 + K_STAGE
                        + 16);
                }
            }
            if constexpr (!DIRECT_AB_NATIVE) {
                if constexpr (A_STAGE_VECTORS < GEMM_THREADS) {
                    if (tid < A_STAGE_VECTORS) {
                        *reinterpret_cast<uint4*>(sx + cached_xrow * A_LD
                                                  + cached_xpart * 16) = next_a;
                    }
                } else {
                    *reinterpret_cast<uint4*>(sx + cached_xrow * A_LD
                                              + cached_xpart * 16) = next_a;
                }
                if constexpr (A_TWO_VECTORS_PER_THREAD) {
                    *reinterpret_cast<uint4*>(sx + cached_xrow * A_LD + 16) = next_a_extra;
                }
            }
            if constexpr (DIRECT_B_NATIVE) {
                current_b = next_b;
            } else {
                *reinterpret_cast<uint4*>(sw + cached_local_col * B_LD + cached_bpart * 16)
                    = next_b;
            }
            if constexpr (!DIRECT_AB_NATIVE) __syncthreads();
        }
        }
        } else {
            for (uint32_t k0 = 0; k0 < effective_k; k0 += 2 * K_STAGE) {
                const bool has_next1 = k0 + K_STAGE < effective_k;
                const bool has_next2 = k0 + 2 * K_STAGE < effective_k;
                uint4 next_a1{}, next_b1{}, next_a2{}, next_b2{};
                if (has_next1) {
                    next_a1 = *reinterpret_cast<const uint4*>(
                        attempt_x + uint64_t(cached_xrow) * effective_k + k0 + K_STAGE
                        + cached_xpart * 16);
                    next_b1 = *reinterpret_cast<const uint4*>(
                        data + cached_dataset_col * effective_k + k0 + K_STAGE
                        + cached_bpart * 16);
                }
                if (has_next2) {
                    next_a2 = *reinterpret_cast<const uint4*>(
                        attempt_x + uint64_t(cached_xrow) * effective_k + k0 + 2 * K_STAGE
                        + cached_xpart * 16);
                    next_b2 = *reinterpret_cast<const uint4*>(
                        data + cached_dataset_col * effective_k + k0 + 2 * K_STAGE
                        + cached_bpart * 16);
                }

#pragma unroll
                for (uint32_t ks = 0; ks < K_STAGE; ks += 16) {
#pragma unroll
                    for (int ct = 0; ct < int(COL_TILES_PER_WAVE); ++ct) {
                        BFrag bfrag;
                        const uint32_t local_n = (wave * COL_TILES_PER_WAVE + ct) * 16;
                        rocwmma::load_matrix_sync(bfrag, sw + local_n * B_LD + ks, B_LD);
#pragma unroll
                        for (int r = 0; r < int(M_TILE / 16); ++r) {
                            AFrag afrag;
                            rocwmma::load_matrix_sync(afrag, sx + r * 16 * A_LD + ks, A_LD);
                            rocwmma::mma_sync(accum[ct][r], afrag, bfrag, accum[ct][r]);
                        }
                    }
                }
                __syncthreads();
                if (!has_next1) break;
                *reinterpret_cast<uint4*>(sx + cached_xrow * A_LD + cached_xpart * 16)
                    = next_a1;
                *reinterpret_cast<uint4*>(sw + cached_local_col * B_LD + cached_bpart * 16)
                    = next_b1;
                __syncthreads();

#pragma unroll
                for (uint32_t ks = 0; ks < K_STAGE; ks += 16) {
#pragma unroll
                    for (int ct = 0; ct < int(COL_TILES_PER_WAVE); ++ct) {
                        BFrag bfrag;
                        const uint32_t local_n = (wave * COL_TILES_PER_WAVE + ct) * 16;
                        rocwmma::load_matrix_sync(bfrag, sw + local_n * B_LD + ks, B_LD);
#pragma unroll
                        for (int r = 0; r < int(M_TILE / 16); ++r) {
                            AFrag afrag;
                            rocwmma::load_matrix_sync(afrag, sx + r * 16 * A_LD + ks, A_LD);
                            rocwmma::mma_sync(accum[ct][r], afrag, bfrag, accum[ct][r]);
                        }
                    }
                }
                __syncthreads();
                if (!has_next2) break;
                *reinterpret_cast<uint4*>(sx + cached_xrow * A_LD + cached_xpart * 16)
                    = next_a2;
                *reinterpret_cast<uint4*>(sw + cached_local_col * B_LD + cached_bpart * 16)
                    = next_b2;
                __syncthreads();
            }
        }
    } else {
    for (uint32_t k0 = 0; k0 < effective_k; k0 += K_STAGE) {
        // 2 KiB of X, copied through aligned 16-byte vectors.
        if constexpr (!DIRECT_A) {
            for (uint32_t load = tid; load < (M_TILE * K_STAGE) / 16;
                 load += GEMM_THREADS) {
                constexpr uint32_t parts = K_STAGE / 16;
                const uint32_t xrow = HOIST_LOADS ? cached_xrow : load / parts;
                const uint32_t xpart = HOIST_LOADS ? cached_xpart : load % parts;
                const uint4 value = *reinterpret_cast<const uint4*>(
                    attempt_x + uint64_t(xrow) * effective_k + k0 + xpart * 16);
                int8_t* dst = sx + xrow * A_LD + xpart * 16;
                if constexpr (A_LD % 16 == 0) {
                    *reinterpret_cast<uint4*>(dst) = value;
                } else {
                    uint32_t* words = reinterpret_cast<uint32_t*>(dst);
                    words[0] = value.x;
                    words[1] = value.y;
                    words[2] = value.z;
                    words[3] = value.w;
                }
            }
        }

        // 2 KiB of W, copied through aligned 16-byte vectors. Each logical column can be
        // anywhere in the 4 GiB dataset in gathered mode.
#pragma unroll
        for (uint32_t load = tid; load < (N_TILE * K_STAGE) / 16; load += GEMM_THREADS) {
            constexpr uint32_t parts = K_STAGE / 16;
            const uint32_t local_col = load / parts;
            const uint32_t part = HOIST_LOADS ? cached_bpart : load % parts;
            const uint32_t n = n0 + local_col;
            uint64_t dataset_col;
            if constexpr (HOIST_LOADS) {
                dataset_col = cached_dataset_col;
            } else if constexpr (DefaultGather) {
                // DEFAULT has 256 * 2,048 = 524,288 columns, so modulo is a mask.
                dataset_col = uint64_t(idx[uint64_t(attempt) * 2048u + n] & 0x7ffffu);
            } else {
                dataset_col = gathered
                    ? uint64_t(idx[uint64_t(attempt) * effective_nb + n] % columns)
                    : uint64_t(slices[attempt]) * effective_nb + n;
            }
            const uint4 value = *reinterpret_cast<const uint4*>(
                data + dataset_col * effective_k + k0 + part * 16);
            int8_t* dst = sw + local_col * B_LD + part * 16;
            if constexpr (B_LD % 16 == 0) {
                *reinterpret_cast<uint4*>(dst) = value;
            } else {
                uint32_t* words = reinterpret_cast<uint32_t*>(dst);
                words[0] = value.x;
                words[1] = value.y;
                words[2] = value.z;
                words[3] = value.w;
            }
        }
        __syncthreads();

#pragma unroll
        for (uint32_t ks = 0; ks < K_STAGE; ks += 16) {
#pragma unroll
            for (int ct = 0; ct < int(COL_TILES_PER_WAVE); ++ct) {
                BFrag bfrag;
                const uint32_t local_n = (wave * COL_TILES_PER_WAVE + ct) * 16;
                rocwmma::load_matrix_sync(bfrag, sw + local_n * B_LD + ks, B_LD);
#pragma unroll
                for (int r = 0; r < int(M_TILE / 16); ++r) {
                    AFrag afrag;
                    if constexpr (DIRECT_A) {
                        rocwmma::load_matrix_sync(
                            afrag,
                            attempt_x + uint64_t(r * 16) * effective_k + k0 + ks,
                            effective_k);
                    } else {
                        rocwmma::load_matrix_sync(afrag, sx + r * 16 * A_LD + ks, A_LD);
                    }
                    rocwmma::mma_sync(accum[ct][r], afrag, bfrag, accum[ct][r]);
                }
            }
        }
        __syncthreads();
    }
    }

    if constexpr (FUSE_FOLD) {
#pragma unroll
        for (int r = 0; r < int(M_TILE / 16); ++r) {
            const uint32_t local_n = wave * 16;
            rocwmma::store_matrix_sync(sc + uint64_t(r * 16) * N_TILE + local_n,
                                       accum[0][r], N_TILE, rocwmma::mem_row_major);
        }
        __syncthreads();

        uint64_t local_sums[8]{};
        constexpr uint32_t local_chunks = M_TILE * N_TILE / 16;
        for (uint32_t local_chunk = tid; local_chunk < local_chunks;
             local_chunk += GEMM_THREADS) {
            constexpr uint32_t chunks_per_row = N_TILE / 16;
            const uint32_t local_row = local_chunk / chunks_per_row;
            const uint32_t local_col_chunk = local_chunk % chunks_per_row;
            uint32_t state[16];
#pragma unroll
            for (int i = 0; i < 16; ++i) {
                state[i] = uint32_t(sc[uint64_t(local_row) * N_TILE
                                       + local_col_chunk * 16 + i]);
            }
            const uint64_t global_chunk
                = uint64_t(row0 + local_row) * (effective_nb / 16)
                  + n0 / 16 + local_col_chunk;
            state[0] ^= uint32_t(global_chunk);
            chacha_core(state);
#pragma unroll
            for (int i = 0; i < 8; ++i) {
                local_sums[i] += uint64_t(state[i]) + uint64_t(state[i + 8]);
            }
        }
#pragma unroll
        for (int i = 0; i < 8; ++i) {
            uint64_t value = local_sums[i];
#pragma unroll
            for (int offset = 16; offset > 0; offset >>= 1) {
                value += __shfl_down(value, offset, 32);
            }
            if (lane == 0) wave_sums[wave * 8 + i] = value;
        }
        __syncthreads();
        if (wave == 0 && lane < 8) {
            uint64_t value = 0;
#pragma unroll
            for (int w = 0; w < int(GEMM_THREADS / 32); ++w) {
                value += wave_sums[w * 8 + lane];
            }
            atomicAdd(sums + uint64_t(attempt) * 8 + lane, value);
        }
    } else {
#pragma unroll
        for (int ct = 0; ct < int(COL_TILES_PER_WAVE); ++ct) {
#pragma unroll
            for (int r = 0; r < int(M_TILE / 16); ++r) {
                const uint32_t local_n = (wave * COL_TILES_PER_WAVE + ct) * 16;
                rocwmma::store_matrix_sync(
                    attempt_c + uint64_t(r * 16) * effective_nb + n0 + local_n,
                    accum[ct][r], effective_nb, rocwmma::mem_row_major);
            }
        }
    }
    (void)lane;
}

// Consensus fold. Unsigned addition is associative modulo 2^64, so wave reduction and atomics
// preserve the exact value regardless of scheduling.
__global__ __launch_bounds__(THREADS) void fold_kernel(const uint32_t* c, uint64_t* sums,
                                                       uint64_t chunks) {
    const uint64_t attempt = blockIdx.y;
    const uint32_t* input = c + attempt * chunks * 16;
    uint64_t* output = sums + attempt * 8;
    const uint64_t first = uint64_t(blockIdx.x) * blockDim.x + threadIdx.x;
    const uint64_t stride = uint64_t(gridDim.x) * blockDim.x;
    uint64_t acc[8]{};
    for (uint64_t chunk = first; chunk < chunks; chunk += stride) {
        uint32_t state[16];
#pragma unroll
        for (int i = 0; i < 16; ++i) state[i] = input[chunk * 16 + i];
        state[0] ^= uint32_t(chunk);
        chacha_core(state);
#pragma unroll
        for (int i = 0; i < 8; ++i) acc[i] += uint64_t(state[i]) + uint64_t(state[i + 8]);
    }
#pragma unroll
    for (int i = 0; i < 8; ++i) {
        uint64_t value = acc[i];
#pragma unroll
        for (int offset = 16; offset > 0; offset >>= 1) value += __shfl_down(value, offset, 32);
        if ((threadIdx.x & 31) == 0 && value != 0) atomicAdd(output + i, value);
    }
}

struct HipContext {
    int device = -1;
    uint32_t batch = 0;
    uint32_t k = 0;
    uint32_t nb = 0;
    uint32_t num_blocks = 0;
    char name[256]{};
    char arch[64]{};
    hipStream_t stream = nullptr;
    uint32_t* keys = nullptr;
    uint32_t* pick_keys = nullptr;
    int8_t* x = nullptr;
    uint32_t* idx = nullptr;
    uint32_t* slices = nullptr;
    int32_t* c = nullptr;
    uint64_t* sums = nullptr;
    int8_t* dataset = nullptr;

    ~HipContext() {
        if (device >= 0) hipSetDevice(device);
        if (dataset) hipFree(dataset);
        if (sums) hipFree(sums);
        if (c) hipFree(c);
        if (slices) hipFree(slices);
        if (idx) hipFree(idx);
        if (x) hipFree(x);
        if (pick_keys) hipFree(pick_keys);
        if (keys) hipFree(keys);
        if (stream) hipStreamDestroy(stream);
    }
};

int finish_timed(hipEvent_t start, hipEvent_t stop, float* milliseconds, char* error,
                 size_t error_cap) {
    hipError_t e = hipEventRecord(stop);
    if (e == hipSuccess) e = hipEventSynchronize(stop);
    if (e == hipSuccess && milliseconds) e = hipEventElapsedTime(milliseconds, start, stop);
    hipEventDestroy(start);
    hipEventDestroy(stop);
    if (e != hipSuccess) {
        set_error(error, error_cap, "HIP execution failed", e);
        return 1;
    }
    return 0;
}

} // namespace

extern "C" {

int tenero_hip_create(uint32_t ordinal, uint32_t batch, uint32_t k, uint32_t nb,
                      uint32_t num_blocks, void** out, char* error, size_t error_cap) {
    if (!out || batch == 0 || k == 0 || nb == 0 || num_blocks == 0 || k % K_STAGE != 0
        || nb % N_TILE != 0) {
        set_error(error, error_cap, "invalid HIP engine parameters");
        return 1;
    }
    *out = nullptr;
    auto* ctx = new (std::nothrow) HipContext;
    if (!ctx) {
        set_error(error, error_cap, "cannot allocate the HIP engine");
        return 1;
    }
    ctx->device = int(ordinal);
    ctx->batch = batch;
    ctx->k = k;
    ctx->nb = nb;
    ctx->num_blocks = num_blocks;
    hipError_t e = hipSetDevice(ctx->device);
    if (e != hipSuccess) {
        set_error(error, error_cap, "cannot select the AMD GPU", e);
        delete ctx;
        return 1;
    }
    hipDeviceProp_t prop{};
    e = hipGetDeviceProperties(&prop, ctx->device);
    if (e != hipSuccess) {
        set_error(error, error_cap, "cannot read the AMD GPU properties", e);
        delete ctx;
        return 1;
    }
    std::snprintf(ctx->name, sizeof(ctx->name), "%s", prop.name);
    std::snprintf(ctx->arch, sizeof(ctx->arch), "%s", prop.gcnArchName);
    if (std::strncmp(ctx->arch, "gfx11", 5) != 0) {
        set_error(error, error_cap, "this AMD backend requires a gfx11 RDNA3/RDNA3.5 GPU");
        delete ctx;
        return 1;
    }
    e = hipStreamCreate(&ctx->stream);
    if (e == hipSuccess) e = hipMalloc(&ctx->keys, uint64_t(batch) * 32);
    if (e == hipSuccess) e = hipMalloc(&ctx->pick_keys, uint64_t(batch) * 32);
    if (e == hipSuccess) e = hipMalloc(&ctx->x, uint64_t(batch) * M * k);
    if (e == hipSuccess) e = hipMalloc(&ctx->idx, uint64_t(batch) * nb * sizeof(uint32_t));
    if (e == hipSuccess) e = hipMalloc(&ctx->slices, uint64_t(batch) * sizeof(uint32_t));
    if constexpr (!FUSE_FOLD) {
        if (e == hipSuccess) e = hipMalloc(&ctx->c, uint64_t(batch) * M * nb * sizeof(int32_t));
    }
    if (e == hipSuccess) e = hipMalloc(&ctx->sums, uint64_t(batch) * 8 * sizeof(uint64_t));
    if (e != hipSuccess) {
        set_error(error, error_cap, "cannot allocate the AMD attempt buffers", e);
        delete ctx;
        return 1;
    }
    *out = ctx;
    return 0;
}

void tenero_hip_destroy(void* opaque) {
    delete static_cast<HipContext*>(opaque);
}

int tenero_hip_name(void* opaque, char* name, size_t name_cap, char* arch, size_t arch_cap) {
    auto* ctx = static_cast<HipContext*>(opaque);
    if (!ctx || !name || name_cap == 0 || !arch || arch_cap == 0) return 1;
    std::snprintf(name, name_cap, "%s", ctx->name);
    std::snprintf(arch, arch_cap, "%s", ctx->arch);
    return 0;
}

int tenero_hip_build_dataset(void* opaque, const uint8_t* dataset_key, float* milliseconds,
                             char* error, size_t error_cap) {
    auto* ctx = static_cast<HipContext*>(opaque);
    if (!ctx || !dataset_key) {
        set_error(error, error_cap, "invalid dataset request");
        return 1;
    }
    HIP_OR_RETURN(hipSetDevice(ctx->device), "cannot select the AMD GPU");
    if (!ctx->dataset) {
        const uint64_t bytes = uint64_t(ctx->num_blocks) * ctx->k * ctx->nb;
        HIP_OR_RETURN(hipMalloc(&ctx->dataset, bytes), "cannot allocate the 4 GiB GPU dataset");
    }
    HIP_OR_RETURN(hipMemcpyAsync(ctx->keys, dataset_key, 32, hipMemcpyHostToDevice, ctx->stream),
                  "cannot upload the dataset key");
    hipEvent_t start = nullptr, stop = nullptr;
    HIP_OR_RETURN(hipEventCreate(&start), "cannot create a HIP timing event");
    HIP_OR_RETURN(hipEventCreate(&stop), "cannot create a HIP timing event");
    HIP_OR_RETURN(hipEventRecord(start, ctx->stream), "cannot start HIP timing");
    const uint64_t blocks = uint64_t(ctx->k) * ctx->nb / 64;
    const uint32_t grid = uint32_t((blocks + THREADS - 1) / THREADS);
    hipLaunchKernelGGL((keystream_kernel<false>), dim3(grid), dim3(THREADS), 0, ctx->stream, ctx->keys,
                       reinterpret_cast<uint32_t*>(ctx->dataset), blocks, uint64_t(0), blocks);
    for (uint32_t slice = 1; slice < ctx->num_blocks; ++slice) {
        hipLaunchKernelGGL(fill_kernel, dim3(grid), dim3(THREADS), 0, ctx->stream,
                           reinterpret_cast<uint32_t*>(ctx->dataset), blocks, slice);
    }
    hipError_t e = hipGetLastError();
    if (e != hipSuccess) {
        hipEventDestroy(start);
        hipEventDestroy(stop);
        set_error(error, error_cap, "cannot launch the HIP dataset kernels", e);
        return 1;
    }
    return finish_timed(start, stop, milliseconds, error, error_cap);
}

int tenero_hip_attempts(void* opaque, const uint8_t* keys, const uint8_t* pick_keys,
                        const uint32_t* slices, uint32_t count, uint32_t gathered,
                        uint64_t* host_sums, float* milliseconds, char* error, size_t error_cap) {
    auto* ctx = static_cast<HipContext*>(opaque);
    if (!ctx || !ctx->dataset || !keys || !host_sums || count == 0 || count > ctx->batch
        || (gathered && !pick_keys) || (!gathered && !slices)) {
        set_error(error, error_cap, "invalid AMD attempt request (or dataset not built)");
        return 1;
    }
    HIP_OR_RETURN(hipSetDevice(ctx->device), "cannot select the AMD GPU");
    HIP_OR_RETURN(hipMemcpyAsync(ctx->keys, keys, uint64_t(count) * 32, hipMemcpyHostToDevice,
                                 ctx->stream), "cannot upload attempt keys");
    if (gathered) {
        HIP_OR_RETURN(hipMemcpyAsync(ctx->pick_keys, pick_keys, uint64_t(count) * 32,
                                     hipMemcpyHostToDevice, ctx->stream),
                      "cannot upload gathered pick keys");
    } else {
        HIP_OR_RETURN(hipMemcpyAsync(ctx->slices, slices, uint64_t(count) * sizeof(uint32_t),
                                     hipMemcpyHostToDevice, ctx->stream),
                      "cannot upload slice numbers");
    }

    hipEvent_t start = nullptr, stop = nullptr;
    HIP_OR_RETURN(hipEventCreate(&start), "cannot create a HIP timing event");
    HIP_OR_RETURN(hipEventCreate(&stop), "cannot create a HIP timing event");
    HIP_OR_RETURN(hipEventRecord(start, ctx->stream), "cannot start HIP timing");

    const uint64_t x_blocks = uint64_t(M) * ctx->k / 64;
    const uint64_t total_x = uint64_t(count) * x_blocks;
    const uint32_t x_grid = uint32_t((total_x + THREADS - 1) / THREADS);
    hipLaunchKernelGGL((keystream_kernel<X_STAGE_MAJOR>), dim3(x_grid), dim3(THREADS), 0, ctx->stream, ctx->keys,
                       reinterpret_cast<uint32_t*>(ctx->x), x_blocks, uint64_t(0), total_x);

    if (gathered) {
        const uint64_t pick_blocks = uint64_t(ctx->nb) / 16;
        const uint64_t total_picks = uint64_t(count) * pick_blocks;
        const uint32_t pick_grid = uint32_t((total_picks + THREADS - 1) / THREADS);
        hipLaunchKernelGGL((keystream_kernel<false>), dim3(pick_grid), dim3(THREADS), 0, ctx->stream,
                           ctx->pick_keys, ctx->idx, pick_blocks, uint64_t(0), total_picks);
    }

    HIP_OR_RETURN(hipMemsetAsync(ctx->sums, 0, uint64_t(count) * 8 * sizeof(uint64_t), ctx->stream),
                  "cannot clear the fold sums");
    const bool default_gather = TENERO_AMD_SPECIALIZE_DEFAULT && gathered && ctx->k == 8192
                                && ctx->nb == 2048 && ctx->num_blocks == 256;
    if (default_gather) {
        const dim3 gather_grid = GRID_ATTEMPT_X
            ? dim3(count, ctx->nb / N_TILE, M_TILES)
            : dim3(ctx->nb / N_TILE, count, M_TILES);
        hipLaunchKernelGGL((gather_wmma_kernel<true>),
                           gather_grid, dim3(GEMM_THREADS), 0,
                           ctx->stream, ctx->x, ctx->dataset, ctx->idx, ctx->slices, ctx->c,
                           ctx->sums, ctx->k, ctx->nb, ctx->num_blocks * ctx->nb, gathered);
    } else {
        const dim3 gather_grid = GRID_ATTEMPT_X
            ? dim3(count, ctx->nb / N_TILE, M_TILES)
            : dim3(ctx->nb / N_TILE, count, M_TILES);
        hipLaunchKernelGGL((gather_wmma_kernel<false>),
                           gather_grid, dim3(GEMM_THREADS), 0,
                           ctx->stream, ctx->x, ctx->dataset, ctx->idx, ctx->slices, ctx->c,
                           ctx->sums, ctx->k, ctx->nb, ctx->num_blocks * ctx->nb, gathered);
    }
    if constexpr (!FUSE_FOLD) {
        const uint64_t chunks = uint64_t(M) * ctx->nb / 16;
        hipLaunchKernelGGL(fold_kernel, dim3(4, count), dim3(THREADS), 0, ctx->stream,
                           reinterpret_cast<const uint32_t*>(ctx->c), ctx->sums, chunks);
    }
    HIP_OR_RETURN(hipMemcpyAsync(host_sums, ctx->sums, uint64_t(count) * 8 * sizeof(uint64_t),
                                 hipMemcpyDeviceToHost, ctx->stream),
                  "cannot download the fold sums");
    hipError_t e = hipGetLastError();
    if (e != hipSuccess) {
        hipEventDestroy(start);
        hipEventDestroy(stop);
        set_error(error, error_cap, "cannot launch the HIP attempt kernels", e);
        return 1;
    }
    return finish_timed(start, stop, milliseconds, error, error_cap);
}

int tenero_hip_read_slice(void* opaque, uint32_t slice, uint8_t* out, char* error,
                          size_t error_cap) {
    auto* ctx = static_cast<HipContext*>(opaque);
    if (!ctx || !ctx->dataset || !out || slice >= ctx->num_blocks) {
        set_error(error, error_cap, "invalid dataset slice request");
        return 1;
    }
    const uint64_t bytes = uint64_t(ctx->k) * ctx->nb;
    HIP_OR_RETURN(hipMemcpy(out, ctx->dataset + uint64_t(slice) * bytes, bytes,
                            hipMemcpyDeviceToHost), "cannot download a dataset slice");
    return 0;
}

int tenero_hip_memory(void* opaque, uint64_t* free_bytes, uint64_t* total_bytes, char* error,
                      size_t error_cap) {
    auto* ctx = static_cast<HipContext*>(opaque);
    if (!ctx || !free_bytes || !total_bytes) {
        set_error(error, error_cap, "invalid memory query");
        return 1;
    }
    size_t free_value = 0, total_value = 0;
    HIP_OR_RETURN(hipSetDevice(ctx->device), "cannot select the AMD GPU");
    HIP_OR_RETURN(hipMemGetInfo(&free_value, &total_value), "cannot read AMD GPU memory");
    *free_bytes = free_value;
    *total_bytes = total_value;
    return 0;
}

} // extern "C"
