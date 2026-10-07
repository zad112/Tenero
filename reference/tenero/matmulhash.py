"""matmulhash v2: a prototype GPU proof of work. Integer matrix multiplication against a large,
SEQUENTIALLY DEPENDENT dataset, built from real cryptographic primitives (ChaCha20, SHA-256).

This is a LEARNING PROTOTYPE, not an audited algorithm.

The dataset (about 4 GiB by default) is a stack of `num_blocks` slices. It is fixed for an epoch.
  slice 0        ChaCha20 keystream, keyed from the epoch seed.
  slice j >= 1   built block by block (a block is 64 bytes = 16 words). Block u of slice j mixes
                 the block u of slice j-1 with three DATA-DEPENDENT picks from anywhere in
                 slices 0..j-1 (the words of the previous block choose which earlier blocks),
                 using one ChaCha20 block. So slice j cannot be produced without the earlier slices, and
                 which earlier blocks it needs depends on their contents. Storing less than
                 most of the dataset makes rebuilding a missing slice snowball. (Measured on a
                 256-slice dataset: rebuilding one missing slice costs about 1.5 slices' worth of
                 work if 90% of the dataset is stored, 5 if 70%, 20 if 50%.)

One attempt (a "hash"):
  1. seed   = SHA-256(header_hash + nonce)
  2. X      = an m x k int8 matrix: the ChaCha20 keystream keyed by the seed
  3. b      = which slice to use (from the seed): attempts read different memory
  4. C      = X @ W_b, int8 x int8 accumulated in int32: EXACT, so every machine agrees
  5. mix    = a fold of C: every 16-word chunk of C goes through the ChaCha20 permutation and
              is added into 8 sums (64 bytes). It depends on the whole product.
  6. digest = SHA-256(seed + mix). A solution is digest < target.

The block carries `mix`. That allows a cheap first check (see `precheck`): SHA-256(seed + mix)
must equal the block's hash and meet the target. Only after that is the expensive recomputation
(the matmul against the dataset) worth doing.

Memory layout: a slice's bytes are the matrix W_b TRANSPOSED: W_b[t][n] = raw[n * k + t]. The GPU
stores it exactly that way (the layout the int8 tensor-core matmul is fastest with), so a slice
is used without any copy.
"""
import hashlib
import threading
from collections import OrderedDict
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass

import numpy as np

from . import chacha

BLOCK_WORDS = 16      # a dataset block: 16 words = 64 bytes = one ChaCha20 block
PICKS = 3             # earlier blocks each new block picks (data-dependently) and mixes in

# How the CPU reference builds the dataset. These change speed only, never the result.
CHUNK_BLOCKS = 16384  # blocks per work item: small enough to stay in the CPU cache (about twice
                      # as fast as whole slices at once), and the unit handed to a thread
BUILD_THREADS = 1     # threads used to build a dataset; the miner sets this from its core budget


class BuildSlot:
    """Hands out turns for dataset builds, in the order they were asked for, so that only ONE
    build runs at a time in a process. The core budget is per build (BUILD_THREADS), so two builds
    at once would use twice the cores and slow the GPU search badly."""

    def __init__(self):
        self._cond = threading.Condition()
        self._next = 0
        self._serving = 0
        self._skipped = set()

    def take(self):
        with self._cond:
            ticket = self._next
            self._next += 1
            return ticket

    def wait_turn(self, ticket):
        with self._cond:
            while self._serving != ticket:
                self._cond.wait()

    def finish(self, ticket):
        # called when the build is done, failed, or never started (then its turn is skipped)
        with self._cond:
            if ticket == self._serving:
                self._serving += 1
                while self._serving in self._skipped:
                    self._skipped.discard(self._serving)
                    self._serving += 1
            else:
                self._skipped.add(ticket)
            self._cond.notify_all()


BUILD_SLOT = BuildSlot()


def set_build_threads(n):
    """How many threads dataset builds may use in this process (at least 1)."""
    global BUILD_THREADS
    BUILD_THREADS = max(1, int(n))
DATASET_LABEL = b"tenero matmulhash v2 dataset"


@dataclass(frozen=True)
class Params:
    m: int = 64             # rows of X per attempt
    k: int = 8192           # shared dimension
    nb: int = 2048          # columns of one slice (a slice is k x nb bytes = 16 MiB)
    num_blocks: int = 256   # slices in the dataset (256 x 16 MiB = 4 GiB)

    def validate(self):
        if min(self.m, self.k, self.nb, self.num_blocks) < 1:
            raise ValueError("m, k, nb and num_blocks must be positive")
        if self.k % 8 or self.nb % 8:
            raise ValueError("k and nb must be multiples of 8 (a GPU int8 matmul requirement)")
        if (self.m * self.k) % 64 or (self.m * self.nb) % 16:
            raise ValueError("m*k must be a multiple of 64 and m*nb a multiple of 16")
        if self.k * 128 * 128 >= 2**31:
            raise ValueError("k is too large: int32 accumulation could overflow")
        if self.blocks_per_slice >= 2**32 or self.num_blocks >= 2**32:
            raise ValueError("dataset too large for 32-bit block counters")
        return self

    @property
    def slice_bytes(self):
        return self.k * self.nb

    @property
    def slice_words(self):
        return self.slice_bytes // 4

    @property
    def blocks_per_slice(self):
        return self.slice_bytes // (4 * BLOCK_WORDS)

    @property
    def dataset_bytes(self):
        return self.num_blocks * self.slice_bytes

    def ops_per_attempt(self):
        return 2 * self.m * self.k * self.nb


def params_for_dataset(gib, **kw):
    # the Params whose dataset is about `gib` GiB (rounded down to whole slices)
    p = Params(**kw)
    blocks = max(1, int(gib * 2**30) // p.slice_bytes)
    return Params(**{**kw, "num_blocks": blocks}).validate()


def bits_to_target(bits):
    # a target that needs about 2**bits attempts on average
    return 1 << (256 - bits)


# ---------------------------------------------------------------- the dataset

def dataset_key(epoch_seed):
    return hashlib.sha256(DATASET_LABEL + epoch_seed).digest()


def fill_slice(view, j, blocks, lo=0, hi=None):
    """Builds blocks lo..hi of slice j (>= 1) in place (default: the whole slice). `view` is the
    dataset as (num_blocks, blocks, 16) uint32. Blocks of one slice do not depend on each other,
    only on earlier slices, so ranges can be built in any order or at the same time."""
    hi = blocks if hi is None else hi
    n = hi - lo
    prev = view[j - 1, lo:hi]                                   # (n, 16)
    ref = np.zeros((n, BLOCK_WORDS), dtype=np.uint32)
    for i in range(PICKS):                                      # data-dependent picks: words
        pick_slice = (prev[:, 2 * i] % np.uint32(j)).astype(np.intp)          # 2i and 2i+1 of the
        pick_block = (prev[:, 2 * i + 1] % np.uint32(blocks)).astype(np.intp)  # previous block
        ref ^= view[pick_slice, pick_block]                     # choose any earlier block
    state = np.empty((n, BLOCK_WORDS), dtype=np.uint32)
    state[:, 0:4] = chacha.CONSTANTS
    state[:, 4:12] = prev[:, 0:8] ^ ref[:, 0:8]                 # key
    state[:, 12] = np.arange(lo, hi, dtype=np.uint32)           # counter: the block number
    state[:, 13] = np.uint32(j)                                 # nonce: the slice number and
    state[:, 14] = prev[:, 8] ^ ref[:, 8]                       # more words from both inputs
    state[:, 15] = prev[:, 9] ^ ref[:, 9]
    view[j, lo:hi] = chacha.chacha_core(state) ^ prev ^ ref     # every input word is used


def build_dataset(params, epoch_seed, progress=None, threads=None, chunk_blocks=None):
    """The whole dataset as a (num_blocks, slice_words) uint32 array. At full size this is 4 GiB
    and takes one to two minutes in numpy on one core, once per epoch. `threads` (default
    BUILD_THREADS) and `chunk_blocks` (default CHUNK_BLOCKS) change only the speed: the result
    is identical."""
    params.validate()
    threads = BUILD_THREADS if threads is None else max(1, int(threads))
    chunk = max(1, CHUNK_BLOCKS if chunk_blocks is None else int(chunk_blocks))
    blocks = params.blocks_per_slice
    ranges = [(lo, min(lo + chunk, blocks)) for lo in range(0, blocks, chunk)]
    data = np.empty((params.num_blocks, params.slice_words), dtype=np.uint32)
    view = data.reshape(params.num_blocks, blocks, BLOCK_WORDS)
    key = dataset_key(epoch_seed)
    pool = ThreadPoolExecutor(max_workers=threads) if threads > 1 and len(ranges) > 1 else None

    def run(fn):
        # every range of one slice, in parallel when there is a pool; waits for all of them
        # (the next slice needs this one complete) and re-raises any error
        if pool is None:
            for lo, hi in ranges:
                fn(lo, hi)
        else:
            list(pool.map(lambda r: fn(*r), ranges))

    def first(lo, hi):
        view[0, lo:hi] = chacha.keystream(key, hi - lo, lo)     # counter = block number

    try:
        run(first)
        for j in range(1, params.num_blocks):
            run(lambda lo, hi, j=j: fill_slice(view, j, blocks, lo, hi))
            if progress and (j + 1) % max(1, params.num_blocks // 8) == 0:
                progress(j + 1, params.num_blocks)
    finally:
        if pool is not None:
            pool.shutdown(wait=True)
    return data


def slice_matrix(data, b, params):
    """Slice b as the (k, nb) int8 matrix the attempt multiplies by (a transposed view of the
    raw bytes: no copy)."""
    return data[b].view(np.int8).reshape(params.nb, params.k).T


class DatasetCache:
    """Keeps the most recent few datasets built by this process (one per epoch). Thread safe.
    A caller asking for a dataset that is being built waits for that build; a caller asking for
    a dataset that is already there is never held up by the build of a different one (the miner
    prepares the next epoch's dataset while still checking blocks of the current one)."""

    def __init__(self, keep=2):
        self.keep = keep
        self.builds = 0
        self._items = OrderedDict()
        self._building = {}
        self._lock = threading.Lock()

    def get(self, params, epoch_seed, progress=None):
        key = (params, epoch_seed)
        with self._lock:
            if key in self._items:
                self._items.move_to_end(key)
                return self._items[key]
            entry = self._building.get(key)
            if entry is None:                         # first to ask: take a place in the queue
                entry = self._building[key] = (threading.Lock(), BUILD_SLOT.take())
            build_lock, ticket = entry
        with build_lock:                              # one build per dataset
            with self._lock:
                if key in self._items:                # built by another thread meanwhile
                    self._items.move_to_end(key)
                    return self._items[key]
                if self._building.get(key) is not entry:
                    retry = True                      # that build failed: ask again
                else:
                    retry = False
            if retry:
                return self.get(params, epoch_seed, progress)
            try:
                BUILD_SLOT.wait_turn(ticket)          # and one build at a time, in request order
                with self._lock:
                    while len(self._items) >= self.keep:
                        self._items.popitem(last=False)   # drop the oldest epoch to free memory
                data = build_dataset(params, epoch_seed, progress)
                with self._lock:
                    self._items[key] = data
                    self.builds += 1
                    self._building.pop(key, None)
                return data
            except BaseException:
                with self._lock:
                    self._building.pop(key, None)     # a failed build can be asked for again
                raise
            finally:
                BUILD_SLOT.finish(ticket)

    def discard(self, params, epoch_seed):
        """Frees one cached dataset (a finished epoch's). True if it was there."""
        with self._lock:
            return self._items.pop((params, epoch_seed), None) is not None

    def clear(self):
        with self._lock:
            self._items.clear()


DEFAULT_CACHE = DatasetCache()


def cached_dataset(params, epoch_seed, progress=None):
    return DEFAULT_CACHE.get(params, epoch_seed, progress)


# ---------------------------------------------------------------- one attempt

def attempt_seed(header_hash, nonce):
    return hashlib.sha256(header_hash + int(nonce).to_bytes(8, "little")).digest()


def attempt_slice(seed, num_blocks):
    # which dataset slice this attempt reads (independent of the bytes that make X)
    return int.from_bytes(hashlib.sha256(seed + b"\x01").digest()[:8], "little") % num_blocks


def make_x(seed, params):
    """The (m, k) int8 matrix of this attempt: the ChaCha20 keystream keyed by the seed."""
    ks = chacha.keystream(seed, params.m * params.k // 64)
    return ks.astype("<u4").reshape(-1).view(np.int8).reshape(params.m, params.k)


def fold_sums(C):
    """The fold. C: (n, m*nb) int32 holding n products. Returns (n, 8) uint64.
    Each 16-word chunk (with its position mixed in) goes through the ChaCha20 permutation; the
    words are added into 8 sums."""
    n, total = C.shape
    chunks = total // 16
    x = np.ascontiguousarray(C, dtype=np.int32).view(np.uint32).reshape(n * chunks, 16).copy()
    x[:, 0] ^= np.tile(np.arange(chunks, dtype=np.uint32), n)
    y = chacha.chacha_core(x).reshape(n, chunks, 16)
    return y[:, :, :8].sum(axis=1, dtype=np.uint64) + y[:, :, 8:].sum(axis=1, dtype=np.uint64)


def mix_bytes(sums_row):
    return np.asarray(sums_row, dtype="<u8").tobytes()


def digest_of(seed, mix):
    return hashlib.sha256(seed + mix).digest()


def compute_attempts(params, data, header_hash, nonces):
    """The CPU reference for a list of nonces: [(digest, mix)]. `data` is the dataset."""
    seeds = [attempt_seed(header_hash, n) for n in nonces]
    products = []
    for seed in seeds:
        W = slice_matrix(data, attempt_slice(seed, params.num_blocks), params)
        # float64 matmul is exact here: every product and partial sum is an integer < 2**53
        c = make_x(seed, params).astype(np.float64) @ W.astype(np.float64)
        products.append(c.astype(np.int64).astype(np.int32).reshape(-1))
    sums = fold_sums(np.stack(products))
    out = []
    for seed, row in zip(seeds, sums):
        mix = mix_bytes(row)
        out.append((digest_of(seed, mix), mix))
    return out


# ---------------------------------------------------------------- the gathered attempt (from the fork height)
#
# From a network's gather fork height on (beta and dev: 500; docs/CONSENSUS.md section 8.3), an attempt does not read one
# slice: it multiplies X by `nb` columns picked one by one from the WHOLE dataset. Why: the slice of the first design is
# two cheap hashes of the nonce, so a miner can try nonces 16 to a slice and read each slice once for all of them, which
# makes the proof of work limited by multiply speed, not memory (docs/THREAT_MODEL.md E11). Columns picked one by one
# from 2^19 leave two attempts sharing about 8 of 2048, each with different partners.
#
# Column j of the dataset is bytes [j*k, (j+1)*k) of the whole dataset (all slices one after the other), so column n of
# slice b is column b*nb + n. Everything else (seed, X, the fold, mix, digest) is as in the first design.

def pick_key(seed):
    return hashlib.sha256(seed + b"\x02").digest()


def pick_columns(seed, params):
    """The nb dataset columns a gathered attempt reads: little-endian u32 word n of the ChaCha20 keystream of
    pick_key(seed) (counter 0, nonce 0), modulo num_blocks * nb."""
    columns = params.num_blocks * params.nb
    words = chacha.keystream(pick_key(seed), -(-params.nb // 16)).astype("<u4").reshape(-1)[:params.nb]
    return [int(w) % columns for w in words]


def gathered_matrix(data, cols, params):
    """The (k, nb) int8 matrix of a gathered attempt: column n is dataset column cols[n]."""
    flat = np.ascontiguousarray(data, dtype="<u4").reshape(-1).view(np.int8)
    k = params.k
    return np.stack([flat[c * k:(c + 1) * k] for c in cols], axis=1)


def compute_gathered_attempts(params, data, header_hash, nonces):
    """The gathered attempt for a list of nonces: [(digest, mix)]. `data` is the WHOLE dataset."""
    if data.shape[0] != params.num_blocks:
        raise ValueError("a gathered attempt needs the whole dataset")
    seeds = [attempt_seed(header_hash, n) for n in nonces]
    products = []
    for seed in seeds:
        W = gathered_matrix(data, pick_columns(seed, params), params)
        # float64 matmul is exact here: every product and partial sum is an integer < 2**53
        c = make_x(seed, params).astype(np.float64) @ W.astype(np.float64)
        products.append(c.astype(np.int64).astype(np.int32).reshape(-1))
    sums = fold_sums(np.stack(products))
    out = []
    for seed, row in zip(seeds, sums):
        mix = mix_bytes(row)
        out.append((digest_of(seed, mix), mix))
    return out


def attempts_at(params, data, header_hash, nonces, height, gather_from):
    """The attempt the chain requires at `height`: gathered from `gather_from` on, the first design before."""
    if height >= gather_from:
        return compute_gathered_attempts(params, data, header_hash, nonces)
    return compute_attempts(params, data, header_hash, nonces)


def meets_target(digest, target):
    return int.from_bytes(digest, "big") < target


def precheck(header_hash, nonce, mix, block_hash_hex, target):
    """The CHEAP check (microseconds, no dataset): the claimed mix and nonce really produce this
    hash, and it meets the target. A block that fails cannot be valid. A block that passes still
    has to be re-computed, because the mix could be made up: making one up that meets the target
    costs as many SHA-256 hashes as the block's difficulty."""
    if not isinstance(mix, (bytes, bytearray)) or len(mix) != 64:
        return False
    if not isinstance(nonce, int) or isinstance(nonce, bool) or not 0 <= nonce < 2**64:
        return False
    try:
        digest = digest_of(attempt_seed(header_hash, nonce), bytes(mix))
        return digest.hex() == block_hash_hex and meets_target(digest, target)
    except (TypeError, ValueError):
        return False


def verify(params, data, header_hash, nonce, target):
    return meets_target(compute_attempts(params, data, header_hash, [nonce])[0][0], target)


def find_solution(params, data, header_hash, target, start=0, batch=4, max_attempts=10**7):
    """CPU search (slow: for tests and comparison). Returns (nonce, digest, mix) or None."""
    nonce = start
    while nonce < start + max_attempts:
        nonces = range(nonce, nonce + batch)
        for n, (d, mix) in zip(nonces, compute_attempts(params, data, header_hash, nonces)):
            if meets_target(d, target):
                return n, d, mix
        nonce += batch
    return None
