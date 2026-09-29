"""The proof-of-work algorithms a chain can use.

sha256  block hash = SHA-256(header + nonce). Mined on a CPU, and cheap to check.
matmul  block hash = the matmulhash digest (see toycoin.matmulhash): an int8 matrix
        multiplication against a big sequentially dependent dataset, built for GPUs. Finding a
        block takes a GPU. Checking one takes a CPU that holds the dataset in RAM (built once per
        epoch) a fraction of a second. Blocks carry `mix` (the fold sums the hash commits to), so
        a node can first run a microsecond check, and only then the expensive one.

Either way a block is valid when its hash, read as a number, is below the target, so the
difficulty adjustment, the fork rules and everything else work the same for both.

For matmul chains the dataset changes every `epoch_blocks` blocks. The seed for each epoch
depends only on the epoch number (a chain of hashes from a fixed start), never on block
contents, so miners can prepare the next dataset in advance and a reorganisation can never
change which dataset a block is checked against.
"""
import hashlib
import time

from . import config

_EPOCH_SEEDS = [hashlib.sha256(b"toycoin matmulhash epoch 0").digest()]


def epoch_seed(epoch):
    while len(_EPOCH_SEEDS) <= epoch:
        _EPOCH_SEEDS.append(hashlib.sha256(_EPOCH_SEEDS[-1]).digest())
    return _EPOCH_SEEDS[epoch]


class GpuFault(RuntimeError):
    """The search returned a result the CPU reference disagrees with. The block is discarded."""


def _matmulhash():
    from . import matmulhash  # imported on use, so SHA-256 chains do not need numpy
    return matmulhash


class Sha256Pow:
    name = "sha256"
    cheap = True   # re-checking a block costs microseconds

    def to_dict(self):
        return {"name": "sha256"}

    def describe(self):
        return "SHA-256 (CPU)"

    def hash_of(self, block):
        return block.compute_hash()

    def precheck(self, block, target):
        # for SHA-256 the cheap check IS the whole check
        try:
            return block.hash == block.compute_hash() and int(block.hash, 16) < target
        except (ValueError, TypeError):
            return False

    def mine(self, block, target, refresh_timestamp=False, min_timestamp=0, verify=None,
             on_chunk=None):
        # verify / on_chunk exist for interface parity with MatmulPow: checking is instant here
        block.mine(target, refresh_timestamp=refresh_timestamp, min_timestamp=min_timestamp)


class CpuSearcher:
    """The exact CPU search for matmul chains. Correct but far too slow at full size, so it is
    for tests and tiny datasets. It builds the dataset itself (once per epoch)."""

    def __init__(self, batch=4):
        self.batch = batch
        self.attempts = 0

    def reset(self):
        pass  # nothing is cached here (the dataset cache is shared and is not the problem)

    def search(self, params, epoch_seed_bytes, header_hash, target, start_nonce, seconds=None):
        # returns (nonce, digest, mix, next_nonce); nonce is None if `seconds` ran out first
        mh = _matmulhash()
        data = mh.cached_dataset(params, epoch_seed_bytes)
        t0 = time.perf_counter()
        nonce = start_nonce
        while True:
            nonces = list(range(nonce, nonce + self.batch))
            results = mh.compute_attempts(params, data, header_hash, nonces)
            self.attempts += len(nonces)
            for n, (digest, mix) in zip(nonces, results):
                if mh.meets_target(digest, target):
                    return n, digest, mix, n + 1
            nonce += self.batch
            if seconds is not None and time.perf_counter() - t0 >= seconds:
                return None, None, None, nonce


class MatmulPow:
    name = "matmul"
    VERSION = 2
    cheap = False        # fully re-checking a block needs the dataset in memory
    CHUNK_SECONDS = 1.0  # while mining, the timestamp is refreshed this often

    def __init__(self, params, epoch_blocks):
        if epoch_blocks < 1:
            raise ValueError("epoch_blocks must be at least 1")
        self.params = params.validate()
        self.epoch_blocks = epoch_blocks
        self.searcher = None            # a GPU searcher; None means the slow CPU search
        self.verify_solutions = True    # re-check every mined block on the CPU before keeping it
        self.last_check_seconds = 0.0
        self._cache = {}

    def to_dict(self):
        p = self.params
        return {"name": "matmul", "version": self.VERSION, "epoch_blocks": self.epoch_blocks,
                "m": p.m, "k": p.k, "nb": p.nb, "num_blocks": p.num_blocks}

    def describe(self):
        p = self.params
        return (f"matmul int8 v2 (GPU): {p.dataset_bytes / 2**30:.2f} GiB dataset in "
                f"{p.num_blocks} slices of {p.slice_bytes / 2**20:.0f} MiB, each built from "
                f"earlier ones, rebuilt every {self.epoch_blocks} blocks; a full CPU check needs "
                f"the dataset in RAM (built once per epoch), then costs a fraction of a second")

    def epoch_of(self, index):
        return (index - 1) // self.epoch_blocks   # blocks 1..epoch_blocks are epoch 0

    def prepare(self, epoch, progress=None):
        """Builds (or fetches) this epoch's dataset for CPU checks. At full size it takes about
        one to two minutes and 4 GiB of RAM; ask ahead of time so a check never waits for it. Safe to call
        from a background thread."""
        return _matmulhash().cached_dataset(self.params, epoch_seed(epoch), progress)

    def precheck(self, block, target):
        """The CHEAP check (microseconds, no dataset): the block's hash follows from its header,
        nonce and mix, and meets the target."""
        try:
            mix = bytes.fromhex(block.mix)
            header_hash = hashlib.sha256(block._header_bytes()).digest()
            return _matmulhash().precheck(header_hash, block.nonce, mix, block.hash, target)
        except (TypeError, ValueError):
            return False

    def digest_for(self, index, header, nonce):
        """The hash a block at `index` with these header bytes and nonce SHOULD have (a full CPU
        recomputation), from plain parts so another process can compute it. None for a nonce that
        cannot be used."""
        if not isinstance(nonce, int) or isinstance(nonce, bool) or not 0 <= nonce < 2**64:
            return None
        mh = _matmulhash()
        data = mh.cached_dataset(self.params, epoch_seed(self.epoch_of(index)))
        digest = mh.compute_attempts(self.params, data, hashlib.sha256(header).digest(),
                                     [nonce])[0][0]
        return digest.hex()

    def hash_of(self, block):
        # what this block's hash SHOULD be, from its contents. Cached per block content.
        nonce = block.nonce
        if not isinstance(nonce, int) or isinstance(nonce, bool) or not 0 <= nonce < 2**64:
            return None
        header = block._header_bytes()
        key = hashlib.sha256(header + str(nonce).encode()).hexdigest()
        if key not in self._cache:
            self._cache[key] = self.digest_for(block.index, header, nonce)
        return self._cache[key]

    def check(self, block):
        """Fully re-checks a mined block against the CPU reference. Returns the seconds it took,
        or raises GpuFault if it is wrong (a GPU bug would otherwise put a bad block into the
        chain). The cheap consistency check runs first. Safe to run in a background thread."""
        if not self.precheck(block, 2**256):        # consistent with its own mix and nonce?
            raise GpuFault("the search produced a block whose hash does not match its own "
                           "contents, nonce and mix: the block was discarded")
        self.prepare(self.epoch_of(block.index))    # waits if the dataset is still being built,
        t0 = time.perf_counter()                    # and does not count that wait as check time
        expected = self.hash_of(block)
        seconds = time.perf_counter() - t0
        self.last_check_seconds = seconds
        if expected != block.hash:
            raise GpuFault("the search produced a result the CPU reference disagrees "
                           "with: the block was discarded")
        return seconds

    def mine(self, block, target, refresh_timestamp=False, min_timestamp=0, verify=None,
             on_chunk=None):
        """Searches for a nonce. verify=None checks the result if self.verify_solutions is set;
        verify=False skips the check (the caller then calls check() itself, possibly in the
        background). on_chunk, if given, is called between search chunks and may raise to
        abort the search (the miner uses it to notice a failed background check)."""
        seed = epoch_seed(self.epoch_of(block.index))
        searcher = self.searcher or CpuSearcher()
        nonce = block.nonce
        if refresh_timestamp:
            block.timestamp = max(int(time.time()), min_timestamp)
        chunk = self.CHUNK_SECONDS if (refresh_timestamp or on_chunk) else None
        while True:
            header_hash = hashlib.sha256(block._header_bytes()).digest()
            found, digest, mix, nonce = searcher.search(self.params, seed, header_hash, target,
                                                        nonce, chunk)
            if found is not None:
                block.nonce, block.hash, block.mix = found, digest.hex(), bytes(mix).hex()
                break
            if refresh_timestamp:
                block.timestamp = max(int(time.time()), min_timestamp)  # the header changes
            if on_chunk:
                on_chunk()
        if self.verify_solutions if verify is None else verify:
            self.check(block)


def make_pow(spec):
    """Rebuilds an algorithm from what a chain saved (None means an old chain: SHA-256)."""
    name = (spec or {}).get("name", "sha256")
    if name == "sha256":
        return Sha256Pow()
    if name == "matmul":
        if spec.get("version") != MatmulPow.VERSION:
            raise ValueError("this chain was made with matmulhash v1, which this version no "
                             "longer supports (the algorithm changed). Delete chain.json and "
                             "mempool.json to start a fresh chain.")
        mh = _matmulhash()
        params = mh.Params(m=spec["m"], k=spec["k"], nb=spec["nb"], num_blocks=spec["num_blocks"])
        return MatmulPow(params, spec["epoch_blocks"])
    raise ValueError(f"unknown proof of work '{name}'")


def default_pow(algorithm=None, epoch_blocks=None):
    """The algorithm a NEW chain uses, from config.py (or `algorithm` / `epoch_blocks` if given)."""
    algorithm = algorithm or config.POW_ALGORITHM
    if algorithm == "sha256":
        return Sha256Pow()
    if algorithm == "matmul":
        mh = _matmulhash()
        params = mh.params_for_dataset(config.POW_DATASET_GIB, m=config.POW_M, k=config.POW_K,
                                       nb=config.POW_NB)
        return MatmulPow(params, epoch_blocks or config.POW_EPOCH_BLOCKS)
    raise ValueError(f"unknown proof of work '{algorithm}' (use matmul or sha256)")
