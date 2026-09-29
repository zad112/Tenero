"""The GPU side of the prototype: CUDA kernels, the PyTorch backend, the self-test and the searcher.

Kept in the package (not in the test script) so the miner can use it too. Nothing here imports
torch or cupy at import time: both are passed in, so this module loads on any machine and can be
tested without a GPU.

Three CUDA kernels (compiled at run time by CuPy) do all the generating; PyTorch does the int8
matrix multiply on the tensor cores. Every kernel is an exact integer version of a function in
tenero.chacha / tenero.matmulhash, and the self-test checks each one bit for bit.
"""
import hashlib
import time

import numpy as np

from . import chacha
from . import matmulhash as mh

CHUNK = 512  # fp32 fallback matmul: partial sums stay below 2**23, so float32 is still exact

HEADER = hashlib.sha256(b"tenero gpu pow test header").digest()
EPOCH = b"tenero-epoch-0"

INSTALL_HINT = 'pip install "cupy-cuda13x[ctk]"'

# The CUDA kernels. Written without any #include so NVRTC can compile them anywhere. PICKS is
# filled in from tenero.matmulhash so the two can never disagree.
KERNEL_SOURCE = r'''
#define PICKS __PICKS__

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
'''.replace("__PICKS__", str(mh.PICKS))


class FusedKernels:
    """Runs the CUDA kernels on PyTorch tensors. `cp` is the cupy module and `torch` the torch
    module (both passed in so this can be tested without a GPU)."""

    THREADS = 256          # threads per block
    FOLD_BLOCKS = 4        # blocks per attempt in the fold
    MAX_BLOCKS = 1 << 20   # cap on a grid (kernels loop if there is more work)

    def __init__(self, cp, torch, device):
        self.cp, self.torch, self.device = cp, torch, device
        self.keystream_kernel = cp.RawKernel(KERNEL_SOURCE, "keystream_kernel")
        self.fill_kernel = cp.RawKernel(KERNEL_SOURCE, "fill_kernel")
        self.fold_kernel = cp.RawKernel(KERNEL_SOURCE, "fold_kernel")

    def _stream(self):
        # launch on PyTorch's current stream so the kernels stay in order with its work.
        # Newer CuPy wants Stream.from_external; older versions only have ExternalStream.
        ptr = self.torch.cuda.current_stream().cuda_stream
        cuda = self.cp.cuda
        try:
            return cuda.Stream.from_external(ptr)
        except Exception:  # noqa: BLE001 - not available in this CuPy: use the old name
            pass
        import warnings
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", DeprecationWarning)
            return cuda.ExternalStream(ptr)

    def _grid(self, total):
        return min((total + self.THREADS - 1) // self.THREADS, self.MAX_BLOCKS)

    def device_keys(self, keys_np):
        """(n, 8) uint32-valued numpy array -> the int64 device tensor the kernels read."""
        return self.torch.from_numpy(np.ascontiguousarray(keys_np, dtype=np.int64)).to(self.device)

    def keystream(self, keys, blocks_per_key, start_counter=0):
        # keys: (B, 8) int64 device tensor. Returns (B, blocks_per_key * 64) int8.
        t = self.torch
        keys = keys.contiguous()
        B = keys.shape[0]
        total = B * blocks_per_key
        out = t.empty((B, blocks_per_key * 64), dtype=t.int8, device=self.device)
        with self._stream():
            self.keystream_kernel((self._grid(total),), (self.THREADS,), (
                np.uint64(keys.data_ptr()), np.uint64(out.data_ptr()),
                np.uint64(blocks_per_key), np.uint64(start_counter), np.uint64(total)))
        return out

    def build_dataset(self, W, params, epoch_seed, progress=None):
        """Fills W, a (num_blocks, nb, k) int8 device tensor, with the epoch's dataset: slice 0
        from the keystream, then each later slice from the ones before it."""
        blocks = params.blocks_per_slice
        key = self.device_keys(np.frombuffer(mh.dataset_key(epoch_seed), dtype="<u4")[None])
        with self._stream():
            self.keystream_kernel((self._grid(blocks),), (self.THREADS,), (
                np.uint64(key.data_ptr()), np.uint64(W.data_ptr()), np.uint64(blocks),
                np.uint64(0), np.uint64(blocks)))
            for j in range(1, params.num_blocks):
                self.fill_kernel((self._grid(blocks),), (self.THREADS,), (
                    np.uint64(W.data_ptr()), np.uint64(blocks), np.uint32(j)))
                if progress and (j + 1) % max(1, params.num_blocks // 8) == 0:
                    progress(j + 1, params.num_blocks)
        return W

    def fold_sums(self, Cs, chunks):
        # Cs: (B, ...) int32 device tensor holding B products of chunks*16 values each.
        # Returns (B, 8) int64 (the sums are below 2**63, so they read the same as uint64).
        t = self.torch
        Cs = Cs.contiguous()
        B = Cs.shape[0]
        sums = t.zeros((B, 8), dtype=t.int64, device=self.device)
        with self._stream():
            self.fold_kernel((min(self.FOLD_BLOCKS, self._grid(chunks)), B), (self.THREADS,), (
                np.uint64(Cs.data_ptr()), np.uint64(sums.data_ptr()), np.uint64(chunks)))
        return sums

    def smoke_test(self):
        # compiles all three kernels and launches each once (raises if anything is wrong)
        t = self.torch
        keys = t.zeros((1, 8), dtype=t.int64, device=self.device)
        self.keystream(keys, 2)
        W = t.zeros((2, 64), dtype=t.int8, device=self.device)
        self.build_dataset(W, mh.Params(m=4, k=8, nb=8, num_blocks=2), b"smoke")
        self.fold_sums(t.zeros((1, 16), dtype=t.int32, device=self.device), 1)
        cuda = getattr(t, "cuda", None)
        if cuda is not None:
            cuda.synchronize()


def make_fused(torch, device):
    """A ready FusedKernels. Raises RuntimeError, saying what to install, if CuPy is missing or
    the kernels do not compile on this GPU."""
    try:
        import cupy as cp
    except ImportError:
        raise RuntimeError(f"the GPU miner needs CuPy for its CUDA kernels: {INSTALL_HINT}")
    try:
        fused = FusedKernels(cp, torch, device)
        fused.smoke_test()
    except Exception as e:  # noqa: BLE001 - report whatever went wrong
        raise RuntimeError(f"the CUDA kernels would not run on this GPU "
                           f"({type(e).__name__}: {str(e)[:160]})")
    return fused


class TorchBackend:
    """The same math as tenero.matmulhash, on the GPU. `torch` is passed in so this layer can
    be tested without a GPU."""

    def __init__(self, torch, device, backend="auto", fused=None):
        if fused is None:
            raise ValueError(f"the GPU backend needs the fused CUDA kernels: {INSTALL_HINT}")
        self.torch = torch
        self.device = device
        self.fused = fused
        self.name = None
        self._use_int_mm = False
        self.select(backend)

    @property
    def kernels_name(self):
        return "fused CUDA kernels"

    def to_device_keys(self, keys_np):
        return self.fused.device_keys(keys_np)

    # ---- the dataset: num_blocks slices, each stored TRANSPOSED as (nb, k) int8, in VRAM ----
    def build_dataset(self, params, epoch_seed, progress=None):
        t = self.torch
        W = t.empty((params.num_blocks, params.nb, params.k), dtype=t.int8, device=self.device)
        return self.fused.build_dataset(W, params, epoch_seed, progress)

    def build_random_dataset(self, num_blocks, rows, cols):
        # sweep only: the CONTENT does not matter for matmul speed, only the shape
        t = self.torch
        W = t.empty((num_blocks, rows, cols), dtype=t.int8, device=self.device)
        key = self.to_device_keys(np.frombuffer(hashlib.sha256(b"sweep").digest(),
                                                dtype="<u4")[None])
        blocks = rows * cols // 64
        for b in range(num_blocks):
            W[b] = self.fused.keystream(key, blocks, (b * blocks) % 2**32)[0].reshape(rows, cols)
        return W

    def view(self, W, b):
        # the logical k x nb slice b (a transposed view of the stored (nb, k) matrix)
        return W[b].t()

    # ---- the matrix multiply: int8 x int8 -> int32, exact ----
    def select(self, backend):
        t = self.torch
        if backend in ("auto", "int_mm"):
            try:
                a = t.zeros((32, 64), dtype=t.int8, device=self.device)
                b = t.zeros((64, 32), dtype=t.int8, device=self.device)
                t._int_mm(a, b)
                self._use_int_mm = True
                self.name = "torch._int_mm (int8 tensor cores)"
                return
            except Exception as e:  # noqa: BLE001 - any failure means "not available here"
                if backend == "int_mm":
                    raise
                print(f"  torch._int_mm is not available here ({type(e).__name__}); "
                      f"falling back to the exact fp32 path")
        self._use_int_mm = False
        self.name = "fp32 chunked (exact but slow: NOT the int8 tensor-core path)"

    def matmul(self, X, Wb):
        t = self.torch
        if self._use_int_mm:
            return t._int_mm(X, Wb)
        rows, k = X.shape
        acc = t.zeros((rows, Wb.shape[1]), dtype=t.int32, device=self.device)
        for s in range(0, k, CHUNK):
            part = X[:, s:s + CHUNK].to(t.float32) @ Wb[s:s + CHUNK].to(t.float32)
            acc += part.to(t.int32)
        return acc

    # ---- one batch of attempts, in stages (so the benchmark can time each one) ----
    def stage_generate(self, params, keys_np):
        keys = self.to_device_keys(keys_np)
        return self.fused.keystream(keys, params.m * params.k // 64).reshape(
            len(keys_np), params.m, params.k)

    def stage_matmul(self, params, X, W, blocks):
        # each attempt multiplies against its own slice
        return self.torch.stack([self.matmul(X[i], self.view(W, blocks[i]))
                                 for i in range(len(blocks))])

    def stage_fold(self, params, Cs):
        return self.fused.fold_sums(Cs, params.m * params.nb // 16)

    def sums(self, params, W, keys_np, blocks):
        X = self.stage_generate(params, keys_np)
        return self.stage_fold(params, self.stage_matmul(params, X, W, blocks)).cpu().numpy()

    def attempts(self, params, W, header_hash, nonces):
        """[(digest, mix)] for these nonces, computed on the GPU."""
        seeds = [mh.attempt_seed(header_hash, n) for n in nonces]
        keys = np.stack([chacha.key_words(s) for s in seeds])
        blocks = [mh.attempt_slice(s, params.num_blocks) for s in seeds]
        sums = self.sums(params, W, keys, blocks)
        out = []
        for seed, row in zip(seeds, sums):
            mix = mh.mix_bytes(row)
            out.append((mh.digest_of(seed, mix), mix))
        return out

    def sync(self):
        cuda = getattr(self.torch, "cuda", None)
        if cuda is not None:
            cuda.synchronize()


def check(label, ok, detail=""):
    print(f"  [{'PASS' if ok else 'FAIL'}] {label}{(' - ' + detail) if detail else ''}")
    return ok


def self_test(backend, title="[2] SELF-TEST (small dataset): does the GPU agree with the CPU, "
                              "bit for bit?"):
    print("\n" + title)
    p = mh.Params(m=32, k=1024, nb=1024, num_blocks=8).validate()
    ok = True
    W_gpu = backend.build_dataset(p, EPOCH)
    cpu_data = mh.build_dataset(p, EPOCH)
    gpu_data = np.ascontiguousarray(W_gpu.cpu().numpy()).view(np.uint32).reshape(p.num_blocks, -1)
    ok &= check(f"the {p.num_blocks}-slice dataset built on the GPU equals the CPU's "
                f"(every slice built from earlier ones)", np.array_equal(gpu_data, cpu_data))

    t = backend.torch
    for xv, wv in ((-128, -128), (127, -128), (127, 127)):
        X = t.full((32, 4096), xv, dtype=t.int8, device=backend.device)
        Wb = t.full((4096, 64), wv, dtype=t.int8, device=backend.device)
        C = backend.matmul(X, Wb).cpu().numpy()
        ok &= check(f"extreme values ({xv} x {wv}) accumulate exactly at k=4096",
                    (C == 4096 * xv * wv).all())

    # the ChaCha20 keystream kernel against the numpy version (itself checked against OpenSSL)
    rng = np.random.RandomState(3)
    keys_np = rng.randint(0, 2**32, size=(3, 8), dtype=np.int64)   # dtype: Windows defaults to int32
    for start in (0, 12345, 2**32 - 40):
        got = backend.fused.keystream(backend.to_device_keys(keys_np), 100, start).cpu().numpy()
        same = all(np.array_equal(
            got[i].view(np.uint32).reshape(100, 16),
            chacha.keystream(keys_np[i].astype("<u4").tobytes(), 100, start))
            for i in range(3))
        ok &= check(f"ChaCha20 keystream kernel matches the reference (counter starts at {start})",
                    same)

    # the fold on random values and the int32 extremes
    C_np = rng.randint(-2**31, 2**31, size=(3, 16 * 64), dtype=np.int64).astype(np.int32)
    C_np[0, :4] = [-2**31, 2**31 - 1, 0, -1]
    got = backend.fused.fold_sums(t.from_numpy(C_np).to(backend.device), 64).cpu().numpy()
    ok &= check("the fold matches the reference (random values and the int32 extremes)",
                np.array_equal(got.view(np.uint64), mh.fold_sums(C_np)))

    nonces = list(range(8))
    gpu = backend.attempts(p, W_gpu, HEADER, nonces)
    cpu = mh.compute_attempts(p, cpu_data, HEADER, nonces)
    ok &= check(f"{len(nonces)} full attempts (hash and mix) match the CPU reference", gpu == cpu,
                "" if gpu == cpu else "the GPU result differs: do not trust the benchmark")
    return ok


class GpuSearcher:
    """Finds proof-of-work nonces on the GPU for a matmul chain (see tenero.pow).

    Keeps the current epoch's dataset in VRAM and rebuilds it only when the epoch changes, so
    consecutive blocks in one epoch reuse it. `search` matches the interface of
    tenero.pow.CpuSearcher, so the chain code does not care which one it is given.
    """

    def __init__(self, backend, batch=32, log=None):
        self.backend = backend
        self.batch = batch
        self.log = log
        self.attempts = 0
        self.dataset_builds = 0
        self.last_build_seconds = 0.0
        self._key = None
        self._dataset = None

    def reset(self):
        # forget the dataset, so the next search rebuilds it (after a fault a bit flip in
        # VRAM could otherwise keep corrupting results)
        self._key = None
        self._dataset = None

    def _get_dataset(self, params, epoch_seed):
        key = (params, epoch_seed)
        if key != self._key:
            self._dataset = None            # free the old epoch's dataset first
            empty = getattr(getattr(self.backend.torch, "cuda", None), "empty_cache", None)
            if empty:
                empty()
            t0 = time.perf_counter()
            self._dataset = self.backend.build_dataset(params, epoch_seed)
            self.backend.sync()
            self._key = key
            self.dataset_builds += 1
            self.last_build_seconds = time.perf_counter() - t0
            if self.log:
                self.log(f"built the {params.dataset_bytes / 2**30:.2f} GiB dataset for this "
                         f"epoch in {self.last_build_seconds:.2f}s")
        return self._dataset

    def search(self, params, epoch_seed, header_hash, target, start_nonce, seconds=None):
        # returns (nonce, digest, mix, next_nonce); nonce is None if `seconds` ran out first
        W = self._get_dataset(params, epoch_seed)
        t0 = time.perf_counter()
        nonce = start_nonce
        while True:
            nonces = list(range(nonce, nonce + self.batch))
            results = self.backend.attempts(params, W, header_hash, nonces)
            self.attempts += len(nonces)
            for n, (digest, mix) in zip(nonces, results):
                if mh.meets_target(digest, target):
                    return n, digest, mix, n + 1
            nonce += self.batch
            if seconds is not None and time.perf_counter() - t0 >= seconds:
                return None, None, None, nonce
