"""Generates the golden test vectors in tests/vectors/ from the Python reference implementation.

A vector is a fixed input with the output the reference produces for it. They exist so that any
other implementation of this coin (a C++ or Rust rewrite, a GPU kernel) can be checked against the
reference BIT FOR BIT, the same way the numpy code, OpenSSL, the CUDA emulator and the real GPU were
checked against each other here.

    python tools/make_vectors.py --check        do the committed files match the reference?
    python tools/make_vectors.py --write        regenerate the fast vectors (a CONSENSUS CHANGE:
                                                explain it in the commit, update docs/CONSENSUS.md)
    python tools/make_vectors.py --write --deep also the deep vector (slices up to 77 at real size:
                                                about 20 s and 1.3 GiB of RAM)
    python tools/make_vectors.py --full         the FULL vector: a hash of every one of the 256
                                                slices of the real 4 GiB dataset (about 4.3 GiB of
                                                RAM; run it on a machine that has that)

Everything here is deterministic: no clock, no random numbers, no files outside the repository.
Large numbers are written as hex strings (or ints where they fit in 64 bits).
"""
import argparse
import contextlib
import copy
import dataclasses
import hashlib
import json
import os
import sys
import tempfile

import numpy as np
from ecdsa import SECP256k1

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if ROOT not in sys.path:
    sys.path.insert(0, ROOT)

from tenero import chacha  # noqa: E402
from tenero import chain as chain_module  # noqa: E402
from tenero import matmulhash as mh  # noqa: E402
from tenero import pow as powmod  # noqa: E402
from tenero.block import Block  # noqa: E402
from tenero.chain import Blockchain, min_fee_for  # noqa: E402
from tenero.transaction import COINBASE, Transaction  # noqa: E402
from tenero.units import UNIT, fmt, to_units  # noqa: E402
from tenero.wallet import Wallet, address_from_pubkey_hex  # noqa: E402

VECTOR_DIR = os.path.join(ROOT, "tests", "vectors")
SCHEMA = 1
FAST_FILES = ("chacha20", "matmulhash_small", "matmulhash_real", "pow_misc", "emission",
              "difficulty", "fees_and_size", "units", "legacy_account_model", "chains")


# ---------------------------------------------------------------- helpers

def det_bytes(label, n):
    """n deterministic bytes from a label (SHA-256 in counter mode: portable, no PRNG needed)."""
    out, i = b"", 0
    while len(out) < n:
        out += hashlib.sha256(f"tenero vectors {label} {i}".encode()).digest()
        i += 1
    return out[:n]


def det_int(label, bound):
    return int.from_bytes(det_bytes(label, 8), "little") % bound


def sha(b):
    return hashlib.sha256(b).hexdigest()


def jdump(obj):
    return json.dumps(obj, indent=1, sort_keys=True) + "\n"


def wrap(name, description, body):
    return {"schema": SCHEMA, "name": name, "description": description, **body}


@contextlib.contextmanager
def min_block_median(value):
    """The block-size floor is a module constant in the reference: vectors that need a different
    one state it, and this sets it for the duration."""
    old = chain_module.MIN_BLOCK_MEDIAN
    chain_module.MIN_BLOCK_MEDIAN = value
    try:
        yield
    finally:
        chain_module.MIN_BLOCK_MEDIAN = old


def load_chain(doc):
    """A Blockchain from a chain.json-shaped dict (the same path a real chain file takes)."""
    fd, path = tempfile.mkstemp(suffix=".json")
    os.close(fd)
    try:
        with open(path, "w") as f:
            json.dump(doc, f)
        return Blockchain.load(path)
    finally:
        os.remove(path)


def chain_doc(bc):
    """A chain as the dict that Blockchain.save writes."""
    fd, path = tempfile.mkstemp(suffix=".json")
    os.close(fd)
    try:
        bc.save(path)
        with open(path) as f:
            return json.load(f)
    finally:
        os.remove(path)


# ---------------------------------------------------------------- ChaCha20

def chacha_vectors():
    blocks = []

    def add(key, counter, nonce):
        out = chacha.chacha20_blocks(chacha.key_words(key)[None], np.array([counter]),
                                     np.frombuffer(nonce, dtype="<u4")[None])[0]
        blocks.append({"key": key.hex(), "counter": counter, "nonce": nonce.hex(),
                       "output": out.astype("<u4").tobytes().hex()})

    add(bytes(range(32)), 1, bytes.fromhex("000000090000004a00000000"))   # RFC 8439 section 2.3.2
    add(bytes(32), 0, bytes(12))
    add(b"\xff" * 32, 2**32 - 1, b"\xff" * 12)
    for i in range(12):
        add(det_bytes(f"chacha key {i}", 32), int.from_bytes(det_bytes(f"chacha ctr {i}", 4), "little"),
            det_bytes(f"chacha nonce {i}", 12))

    cores = []
    states = [[0] * 16, [0xFFFFFFFF] * 16, list(range(16))]
    states += [[int(w) for w in np.frombuffer(det_bytes(f"core {i}", 64), dtype="<u4")] for i in range(12)]
    for st in states:
        out = chacha.chacha_core(np.array([st], dtype=np.uint32))[0]
        cores.append({"state": st, "output": [int(w) for w in out]})

    streams = []
    for label, start, nonce, n in (("s0", 0, (0, 0, 0), 300), ("s1", 2**32 - 3, (0, 0, 0), 10),
                                   ("s2", 5, (1, 2, 3), 17)):
        key = det_bytes(f"keystream {label}", 32)
        ks = chacha.keystream(key, n, start, nonce).astype("<u4").tobytes()
        streams.append({"key": key.hex(), "start_counter": start, "nonce_words": list(nonce),
                        "blocks": n, "sha256": sha(ks)})
    return wrap("chacha20", "ChaCha20 (RFC 8439): the block function, the bare permutation with "
                "feed-forward on arbitrary states (what the dataset fill and the fold use), and "
                "keystream digests. Words are little-endian uint32.",
                {"blocks": blocks, "core": cores, "keystream": streams})


# ---------------------------------------------------------------- matmulhash v2

def params_dict(p):
    return {"m": p.m, "k": p.k, "nb": p.nb, "num_blocks": p.num_blocks}


def attempt_record(params, data, header_hash, nonce):
    seed = mh.attempt_seed(header_hash, nonce)
    b = mh.attempt_slice(seed, params.num_blocks)
    X = mh.make_x(seed, params)
    W = mh.slice_matrix(data, b, params)
    C = (X.astype(np.float64) @ W.astype(np.float64)).astype(np.int64).astype(np.int32)
    sums = mh.fold_sums(C.reshape(1, -1))[0]
    mix = mh.mix_bytes(sums)
    digest = mh.digest_of(seed, mix)
    assert (digest, mix) == mh.compute_attempts(params, data, header_hash, [nonce])[0]
    return {"header_hash": header_hash.hex(), "nonce": nonce, "seed": seed.hex(),
            "slice_index": b, "x_sha256": sha(X.astype(np.int8).tobytes()),
            "c_sha256": sha(C.astype("<i4").tobytes()),
            "sums": [format(int(s), "016x") for s in sums], "mix": mix.hex(), "digest": digest.hex()}


def slice_hash(data, j):
    return sha(data[j].astype("<u4").tobytes())


SMALL_PARAMS = (mh.Params(m=4, k=16, nb=8, num_blocks=5), mh.Params(m=8, k=64, nb=32, num_blocks=6),
                mh.Params(m=4, k=48, nb=40, num_blocks=7), mh.Params(m=8, k=24, nb=40, num_blocks=6),
                mh.Params(m=8, k=64, nb=64, num_blocks=32))
SMALL_NONCES = (0, 1, 2, 37, 2**32, 2**63, 2**64 - 1)


def small_vectors():
    cases = []
    for i, p in enumerate(SMALL_PARAMS):
        seed = det_bytes(f"small epoch seed {i}", 32)
        data = mh.build_dataset(p, seed, threads=1)
        attempts = []
        for n, nonce in enumerate(SMALL_NONCES):
            header = hashlib.sha256(f"small header {i} {n % 3}".encode()).digest()
            attempts.append(attempt_record(p, data, header, nonce))
        cases.append({"params": params_dict(p), "epoch_seed": seed.hex(),
                      "dataset_key": mh.dataset_key(seed).hex(), "dataset_sha256": sha(data.astype("<u4").tobytes()),
                      "slice_sha256": [slice_hash(data, j) for j in range(p.num_blocks)],
                      "attempts": attempts})
    folds = []
    extremes = [-2**31, 2**31 - 1, 0, -1, 1, 2**31 - 1, -2**31, 12345]
    for label, chunks in (("zeros", 1), ("extremes", 2), ("a", 5), ("b", 3)):
        if label == "zeros":
            c = [0] * (16 * chunks)
        elif label == "extremes":
            c = (extremes * 4)[:16 * chunks]
        else:
            raw = np.frombuffer(det_bytes(f"fold {label}", 64 * chunks), dtype="<i4")
            c = [int(v) for v in raw]
        sums = mh.fold_sums(np.array([c], dtype=np.int32))[0]
        folds.append({"c": c, "sums": [format(int(s), "016x") for s in sums]})
    return wrap("matmulhash_small", "matmulhash v2 at small sizes: dataset hashes and full attempts "
                "(seed, slice, X, C, fold sums, mix, digest). dataset_sha256 hashes the (num_blocks, "
                "slice_bytes) array of raw slice bytes; a slice's bytes are the transposed matrix, "
                "W[t][n] = raw[n*k + t]. C is int32 little-endian, row-major (m, nb). sums are uint64.",
                {"cases": cases, "fold": folds})


REAL_HEADER = hashlib.sha256(b"tenero vectors real header").digest()


def real_case(slices, hash_indices, low, high, count=3):
    """Real-size (64 x 8192 @ 8192 x 2048, 256 slices) vectors using only the first `slices`
    slices, which are exactly the first slices of the full dataset (the fill of slice j does not
    depend on how many slices there are)."""
    p = mh.Params()
    seed = powmod.epoch_seed(0)
    data = mh.build_dataset(dataclasses.replace(p, num_blocks=slices), seed)
    attempts, nonce = [], 0
    while len(attempts) < count:
        b = mh.attempt_slice(mh.attempt_seed(REAL_HEADER, nonce), p.num_blocks)
        if low <= b <= high:
            attempts.append(attempt_record(p, data, REAL_HEADER, nonce))
        nonce += 1
    return {"params": params_dict(p), "epoch_seed": seed.hex(), "dataset_key": mh.dataset_key(seed).hex(),
            "slices_built": slices, "slice_sha256": {str(j): slice_hash(data, j) for j in hash_indices},
            "attempts": attempts}


def real_vectors():
    return wrap("matmulhash_real", "matmulhash v2 at the real parameters (epoch 0 seed): the first 8 "
                "slices of the 4 GiB dataset and full attempts whose slice is among them.",
                real_case(8, range(8), 0, 7))


def deep_vectors():
    return wrap("matmulhash_deep", "matmulhash v2 at the real parameters: slices up to 77 (each built "
                "from all the earlier ones through the data-dependent picks) and attempts on slices "
                "8 to 77. Slow: checked only when TENERO_SLOW_VECTORS=1.",
                real_case(78, (0, 1, 2, 3, 7, 15, 31, 63, 77), 8, 77))


def full_vectors(params=None, seed=None, threads=4, progress=None):
    """A hash of EVERY slice of the real dataset. Needs the whole dataset in RAM (4.3 GiB)."""
    p = params or mh.Params()
    seed = seed or powmod.epoch_seed(0)
    data = mh.build_dataset(p, seed, progress=progress, threads=threads)
    return wrap("matmulhash_full", "matmulhash v2 at the real parameters: the sha256 of every one of "
                "the 256 slices of the epoch-0 dataset. Slow and RAM-hungry (4.3 GiB): checked only "
                "when TENERO_SLOW_VECTORS=1.",
                {"params": params_dict(p), "epoch_seed": seed.hex(),
                 "dataset_sha256": sha(data.astype("<u4").tobytes()),
                 "slice_sha256": [slice_hash(data, j) for j in range(p.num_blocks)]})


# ---------------------------------------------------------------- proof of work, misc

def pow_misc_vectors():
    p = SMALL_PARAMS[1]
    seed = powmod.epoch_seed(0)
    data = mh.build_dataset(p, seed)
    header = hashlib.sha256(b"precheck header").digest()
    nonce, digest, mix = mh.find_solution(p, data, header, mh.bits_to_target(3))
    d_int = int.from_bytes(digest, "big")
    pre = []

    def case(name, header_hash, n, mix_bytes, hash_hex, target, expect):
        assert mh.precheck(header_hash, n, mix_bytes, hash_hex, target) is expect, name
        pre.append({"name": name, "header_hash": header_hash.hex(), "nonce": n, "mix": mix_bytes.hex(),
                    "hash": hash_hex, "target": str(target), "expect": expect})

    case("valid: hash is just below the target", header, nonce, mix, digest.hex(), d_int + 1, True)
    case("the target is compared strictly (hash == target fails)", header, nonce, mix, digest.hex(), d_int, False)
    case("wrong nonce", header, nonce + 1, mix, digest.hex(), 2**256, False)
    case("a different header", hashlib.sha256(b"other").digest(), nonce, mix, digest.hex(), 2**256, False)
    case("mix with one bit flipped", header, nonce, bytes([mix[0] ^ 1]) + mix[1:], digest.hex(), 2**256, False)
    case("mix one byte short", header, nonce, mix[:63], digest.hex(), 2**256, False)
    case("mix one byte long", header, nonce, mix + b"\x00", digest.hex(), 2**256, False)
    case("hash does not follow from seed and mix", header, nonce, mix, "0" * 64, 2**256, False)
    case("hash in upper case (comparison is on the lower-case hex)", header, nonce, mix, digest.hex().upper(), 2**256, False)
    case("nonce 2**64 does not fit", header, 2**64, mix, digest.hex(), 2**256, False)
    case("the largest nonce is fine when the hash follows", header, 2**64 - 1, b"\x00" * 64,
         mh.digest_of(mh.attempt_seed(header, 2**64 - 1), b"\x00" * 64).hex(), 2**256, True)
    return wrap("pow_misc", "Epoch seeds, epoch numbering, bits_to_target and the cheap pre-check.",
                {"epoch_seed_0_label": "tenero matmulhash epoch 0",
                 "epoch_seeds": {str(e): powmod.epoch_seed(e).hex() for e in (*range(13), 100, 1000)},
                 "epoch_of": [{"epoch_blocks": eb, "index": h,
                               "epoch": powmod.MatmulPow(SMALL_PARAMS[1], eb).epoch_of(h)}
                              for eb in (1, 2, 3, 4, 100) for h in (1, 2, 3, 4, 5, 99, 100, 101, 102, 199, 200, 201)],
                 "bits_to_target": [{"bits": b, "target": str(mh.bits_to_target(b))} for b in (1, 2, 8, 20, 64, 128, 255)],
                 "precheck": pre})


# ---------------------------------------------------------------- emission

EMISSION_SETS = {
    "default": dict(initial_reward=to_units(20), halving_interval=525_600, max_supply=to_units(20_000_000),
                    tail_reward=to_units("0.5")),
    "toy_128_2_500": dict(initial_reward=to_units(128), halving_interval=2, max_supply=to_units(500),
                          tail_reward=to_units(1)),
    "trimmed_final_reward": dict(initial_reward=to_units(10), halving_interval=10,
                                 max_supply=to_units(137) + 5000, tail_reward=to_units("0.01")),
    "tail_takes_over_early_20_500k": dict(initial_reward=to_units(20), halving_interval=500_000,
                                          max_supply=to_units(20_000_000), tail_reward=to_units("0.5")),
    "tail_takes_over_early_50_200k": dict(initial_reward=to_units(50), halving_interval=200_000,
                                          max_supply=to_units(20_000_000), tail_reward=to_units("0.5")),
    "no_tail": dict(initial_reward=to_units(10), halving_interval=7, max_supply=to_units(100), tail_reward=0),
    "never_halves_2_years": dict(initial_reward=to_units(20), halving_interval=1_051_200,
                                 max_supply=to_units(20_000_000), tail_reward=to_units("0.5")),
}


def emission_vectors():
    out = {}
    for name, params in EMISSION_SETS.items():
        bc = Blockchain(target=2**240, block_time=60, difficulty_window=0, **params)
        end = bc.main_emission_end(1)
        heights = {1, 2, 3, 4, 5, 10, 11, 12, 13, 100, 10**6, 10**9, 10**12,
                   end - 1, end, end + 1, end + 2}
        for k in range(1, 9):
            for delta in (0, 1):
                heights.add(k * params["halving_interval"] + delta)
        rows = []
        for h in sorted(x for x in heights if x >= 1):
            rows.append({"height": h, "scheduled": bc.scheduled_reward(h), "issued_before": bc.issued_before(h),
                         "main_reward": bc.main_reward_at(h), "reward": bc.reward_at(h),
                         "in_tail": bc.in_tail(h)})
        out[name] = {"params": params, "main_emission_end_from_1": end, "rows": rows}
    return wrap("emission", "Block rewards by height. All amounts are in units (0.0001 coins). "
                "scheduled = initial >> era; main_reward = min(scheduled, cap - issued_before); "
                "reward = max(main_reward, tail_reward); issued_before counts the main emission only.",
                {"sets": out})


# ---------------------------------------------------------------- difficulty

def fake_chain(params, timestamps):
    bc = Blockchain(target=int(params["start_target"]), block_time=params["block_time"],
                    difficulty_window=params["window"])
    for i, t in enumerate(timestamps, start=1):
        bc.chain.append(Block(i, [], bc.chain[-1].hash, timestamp=t))
    return bc


def difficulty_record(name, params, timestamps):
    bc = fake_chain(params, timestamps)
    ts, targets = bc._history()
    nxt = bc.next_target()
    required = [str(t) for t in targets[1:]] + [str(nxt)]
    medians = [bc._median_time(ts, pos) for pos in range(1, len(ts) + 1)]
    assert medians[-1] == bc.min_timestamp()
    return {"name": name, "params": {**params, "start_target": str(params["start_target"])},
            "timestamps": timestamps, "required_targets": required, "median_times": medians}


def difficulty_vectors():
    T0 = 1_700_000_000
    start = 2**240
    real_start = 2**256 // 1_400_000
    base = {"block_time": 60, "window": 30, "start_target": start}
    scen = []

    def seq(solves, t0=T0):
        out, t = [], t0
        for s in solves:
            t += s
            out.append(t)
        return out

    scen.append(difficulty_record("steady 60 s", base, seq([60] * 80)))
    scen.append(difficulty_record("steady 60 s from the real start target", {**base, "start_target": real_start}, seq([60] * 60)))
    scen.append(difficulty_record("blocks twice as fast (30 s)", base, seq([30] * 80)))
    scen.append(difficulty_record("blocks much faster (5 s): the 4x per-block clamp", base, seq([5] * 60)))
    scen.append(difficulty_record("blocks three times slower (180 s)", base, seq([180] * 80)))
    scen.append(difficulty_record("blocks very slow (10000 s): the 6x solve-time clamp", base, seq([10000] * 60)))
    scen.append(difficulty_record("alternating 10 s and 110 s", base, seq([10, 110] * 40)))
    scen.append(difficulty_record("one huge gap in the middle", base, seq([60] * 40 + [100_000] + [60] * 40)))
    scen.append(difficulty_record("hashrate jump then drop", base, seq([60] * 30 + [15] * 30 + [240] * 30)))
    rnd = [1 + det_int(f"solve {i}", 120) for i in range(120)]
    scen.append(difficulty_record("uniform pseudo-random solve times 1..120 s", base, seq(rnd)))
    jitter = [T0 + 60 * i + (det_int(f"jitter {i}", 181) - 90) for i in range(1, 80)]
    scen.append(difficulty_record("timestamps out of order (jitter of +-90 s)", base, jitter))
    for w in (1, 2, 4, 10):
        scen.append(difficulty_record(f"window {w}, 30 s blocks", {"block_time": 30, "window": w, "start_target": start},
                                      seq([20 + det_int(f"w{w} {i}", 40) for i in range(50)])))
    scen.append(difficulty_record("fixed difficulty (window 0)", {"block_time": 60, "window": 0, "start_target": start},
                                  seq([30 + det_int(f"f {i}", 60) for i in range(20)])))
    scen.append(difficulty_record("very short chain (2 blocks)", base, seq([60] * 2)))
    return wrap("difficulty", "The LWMA difficulty adjustment and the median-time rule. timestamps[i] is "
                "block i+1's timestamp (the genesis block has timestamp 0). required_targets[i] is the "
                "target block i+1 must be BELOW, and the last entry is for the next block. "
                "median_times[i] is the earliest timestamp block i+1 may carry (when window > 0). "
                "Targets are decimal strings.", {"scenarios": scen})


# ---------------------------------------------------------------- fees, block size, units

def fees_and_size_vectors():
    sizes = [0, 1, 100, 427, 428, 432, 999, 1000, 1001, 2200, 4400, 300_000, 600_000]
    penalty_cases = []
    for base, size, median in ((0, 5000, 2200), (1280000, 2200, 2200), (1280000, 1000, 2200),
                               (1000000, 3300, 2200), (1000000, 4400, 2200), (1, 2201, 2200),
                               (200000, 300_000, 300_000), (200000, 450_000, 300_000),
                               (200000, 600_000, 300_000), (200000, 300_001, 300_000),
                               (5000, 300_001, 300_000), (123456789, 777_777, 500_000)):
        penalty_cases.append({"base": base, "size": size, "median": median,
                              "penalty": Blockchain.penalty(base, size, median)})
    median_cases = []
    lists = ([], [100] * 10, [10**6] * 10, [0] * 9 + [10**7], [400_000] * 4 + [0] * 6,
             [400_000] * 5 + [0] * 5, [400_000] * 6 + [0] * 4, [1, 2, 3], [500_000], [300_000, 300_001])
    for floor in (2200, 300_000):
        with min_block_median(floor):
            for sizes_list in lists:
                median_cases.append({"floor": floor, "sizes": sizes_list, "median": Blockchain._median(sizes_list)})
    # the median a block is judged against: the previous MEDIAN_WINDOW blocks, genesis excluded
    history = [0, 300, 500_000, 0, 620_000, 10, 700_000, 400_000, 350_000, 900, 450_000, 5, 320_000, 600_000]
    window = []
    with min_block_median(300_000):
        for pos in range(1, len(history) + 1):
            window.append({"pos": pos, "median": Blockchain._median(history[max(1, pos - chain_module.MEDIAN_WINDOW):pos])})
    return wrap("fees_and_size", "The minimum fee, the oversize penalty and the block-size median. "
                "penalty = ceil(base * over^2 / median^2) for size > median, else 0; the hard limit is "
                "2 * median. min_fee = max(1, ceil(rate * size / 1000)). The median floor is a consensus "
                "constant (a field in each case).",
                {"constants": {"min_fee_rate_units_per_1000_bytes": chain_module.MIN_FEE_RATE_UNITS,
                               "median_window": chain_module.MEDIAN_WINDOW, "default_min_block_median": 300_000},
                 "min_fee": [{"size": s, "fee": min_fee_for(s)} for s in sizes],
                 "penalty": penalty_cases, "median": median_cases,
                 "median_history": {"sizes_by_position_0_is_genesis": history, "windowed": window}})


def units_vectors():
    parse = []
    for text in ("1", "0.0001", "20", "0.5", "1.2345", " 3 ", "0", "-1", "0.10", "100.0000", "1e2", "1e-4",
                 "0.00001", "1.23456", "abc", "", "nan", "inf", "1,5", "20000000", "0.00009", "+2"):
        try:
            parse.append({"text": text, "units": to_units(text)})
        except ValueError:
            parse.append({"text": text, "error": True})
    return wrap("units", "Parsing coins to units and formatting units as coins (4 decimals). Python "
                "also accepts exponent notation ('1e2') and surrounding spaces; a rewrite may choose "
                "to reject those.",
                {"decimals": 4, "parse": parse,
                 "format": [{"units": u, "text": fmt(u)} for u in (0, 1, 9, 10_000, 12_345, 123_456_789, -5,
                                                                    -10_000, 200_000_000_000, 10**15)]})


# ---------------------------------------------------------------- the legacy account model

def make_wallet(name):
    return Wallet(det_bytes(f"wallet {name}", 32).hex())


ALICE, BOB, CAROL = (make_wallet(n) for n in ("alice", "bob", "carol"))
WALLETS = {"alice": ALICE, "bob": BOB, "carol": CAROL}


def stx(wallet, recipient, amount, fee, memo="", signer=None):
    """A transaction signed with a DETERMINISTIC (RFC 6979) ECDSA signature, so the vectors are
    reproducible. `signer` signs instead of `wallet` (to make a bad signature)."""
    tx = Transaction(wallet.address, recipient, amount, fee=fee, memo=memo, public_key=wallet.public_key)
    tx.signature = (signer or wallet).signing_key.sign_deterministic(tx.payload()).hex()
    return tx


def tx_fields(tx):
    return tx.to_dict()


def legacy_vectors():
    wallets = [{"name": n, "private_key": w.private_key_hex(), "public_key": w.public_key, "address": w.address}
               for n, w in WALLETS.items()]
    bc = Blockchain()
    txs = []

    def add(name, tx, note=""):
        problem = bc.tx_problem(tx)
        txs.append({"name": name, "tx": tx_fields(tx), "payload_hex": tx.payload().hex(), "size": tx.size(),
                    "min_fee": min_fee_for(tx.size()), "signature_valid": tx.is_signature_valid(),
                    "acceptable": problem is None, "problem": problem, "note": note})

    fee = to_units("0.05")
    add("plain payment", stx(ALICE, BOB.address, to_units(5), fee))
    add("ascii memo", stx(ALICE, BOB.address, to_units("0.0001"), fee, "rent"))
    add("unicode memo (JSON escapes it: the payload has \\u00e9 and \\u2713)", stx(ALICE, BOB.address, to_units(1), fee, "h\u00e9llo \u2713"))
    add("quotes and backslash in the memo", stx(ALICE, BOB.address, to_units(1), fee, 'say "hi" \\ ok'))
    add("longest allowed memo (100 bytes)", stx(ALICE, BOB.address, to_units(1), fee, "x" * 100))
    add("memo one byte too long (101)", stx(ALICE, BOB.address, to_units(1), fee, "x" * 101))
    add("smallest amount, fee below the minimum", stx(ALICE, BOB.address, 1, 1))
    add("fee exactly the minimum", stx(ALICE, BOB.address, to_units(1), min_fee_for(
        Transaction(ALICE.address, BOB.address, to_units(1), fee=to_units("0.0043")).size())))
    add("amount zero", stx(ALICE, BOB.address, 0, fee))
    add("negative amount", stx(ALICE, BOB.address, -5, fee))
    good = stx(ALICE, BOB.address, to_units(2), fee, "tamper base")
    changed = Transaction(good.sender, good.recipient, good.amount + 1, fee=good.fee, memo=good.memo,
                          public_key=good.public_key, signature=good.signature)
    add("amount changed after signing", changed)
    add("signed by someone else", stx(ALICE, BOB.address, to_units(2), fee, "forged", signer=CAROL))
    swapped = Transaction(ALICE.address, BOB.address, to_units(2), fee=fee, public_key=CAROL.public_key)
    swapped.signature = CAROL.signing_key.sign_deterministic(swapped.payload()).hex()
    add("public key does not belong to the sender address", swapped)
    unsigned = Transaction(ALICE.address, BOB.address, to_units(2), fee=fee)
    add("no signature", unsigned)
    N = SECP256k1.order
    raw = bytes.fromhex(good.signature)
    twin_sig = raw[:32] + (N - int.from_bytes(raw[32:], "big")).to_bytes(32, "big")
    twin = Transaction(good.sender, good.recipient, good.amount, fee=good.fee, memo=good.memo,
                       public_key=good.public_key, signature=twin_sig.hex())
    add("KNOWN ISSUE: the signature with s flipped (n - s) also verifies", twin,
        "ECDSA malleability: a second valid signature for the same payload. See docs/KNOWN_ISSUES.md #1; "
        "a rewrite must not accept this as a distinct transaction.")
    coinbase = Transaction(COINBASE, ALICE.address, to_units(50))
    add("coinbase transaction (unsigned reward)", coinbase)
    blocks = []

    def add_block(name, index, timestamp, block_txs, previous, nonce):
        b = Block(index, block_txs, previous, timestamp=timestamp, nonce=nonce)
        blocks.append({"name": name, "index": index, "timestamp": timestamp,
                       "transactions": [t.to_dict() for t in block_txs], "previous_hash": previous,
                       "nonce": nonce, "header_bytes_hex": b._header_bytes().hex(), "hash": b.compute_hash()})

    genesis = Block(0, [], "0" * 64, timestamp=0)
    add_block("genesis", 0, 0, [], "0" * 64, 0)
    add_block("empty block", 1, 1_700_000_060, [], genesis.hash, 12345)
    add_block("block with two transactions", 2, 1_700_000_120,
              [Transaction(COINBASE, ALICE.address, to_units(50)), stx(ALICE, BOB.address, to_units(5), fee)],
              genesis.hash, 7)
    add_block("float timestamp: JSON prints 1700000000.5", 3, 1_700_000_000.5, [], genesis.hash, 1)
    add_block("integral float timestamp: JSON prints 1700000000.0, not 1700000000", 4, 1_700_000_000.0, [], genesis.hash, 1)
    add_block("unicode memo", 5, 1_700_000_300,
              [stx(ALICE, BOB.address, to_units(1), fee, "h\u00e9llo \u2713")], genesis.hash, 2**63)
    return wrap("legacy_account_model", "The CURRENT account-model transaction and block formats. LEGACY: "
                "a rewrite to an output model replaces these, so treat them as documentation of what the "
                "reference does today (Python-JSON serialization, ECDSA secp256k1 over SHA-1, raw 64-byte "
                "r||s signatures) and NOT as a design to copy. Signatures here are deterministic "
                "(RFC 6979) so the vectors are reproducible.",
                {"wallets": wallets, "transactions": txs, "blocks": blocks,
                 "block_hash_rule": "sha256(header_bytes + decimal(nonce)) as lower-case hex"})


# ---------------------------------------------------------------- chains

T0 = 1_700_000_000
EASY = 2**248


def new_sha_chain(**over):
    params = dict(target=EASY, initial_reward=to_units(50), halving_interval=1000,
                  max_supply=to_units(1_000_000), tail_reward=to_units(1), block_time=60, difficulty_window=4)
    params.update(over)
    return Blockchain(**params)


def mine(bc, miner, txs, ts):
    n = len(bc.chain)
    bc.mine_block(miner.address, txs, timestamp=ts)
    assert len(bc.chain[-1].transactions) - 1 == len(txs), "the miner left a transaction out"
    assert bc.chain[-1].index == n


def basic_chain():
    bc = new_sha_chain()
    fee = to_units
    mine(bc, ALICE, [], T0 + 60)
    mine(bc, ALICE, [], T0 + 120)
    mine(bc, ALICE, [stx(ALICE, BOB.address, to_units(10), fee("0.05"), "rent"),
                     stx(ALICE, CAROL.address, to_units("3.5"), fee("0.02"))], T0 + 180)
    mine(bc, BOB, [stx(BOB, CAROL.address, to_units(1), fee("0.01"), "h\u00e9llo \u2713")], T0 + 240)
    mine(bc, CAROL, [stx(CAROL, ALICE.address, to_units("0.5"), fee("0.005"))], T0 + 300)
    mine(bc, ALICE, [], T0 + 365)
    return bc


def prefix(doc, blocks):
    d = copy.deepcopy(doc)
    d["chain"] = d["chain"][:blocks + 1]
    return d


def manual_block(bc, miner, txs, ts, claim=None, index=None, previous=None, mine_target=None, coinbase=None):
    """Builds, mines and appends a block by hand, bypassing the miner's own rules, so a rule can be
    broken on purpose. The default coinbase claim is the correct one."""
    height = len(bc.chain)
    if claim is None:
        base = bc.reward_at(height)
        claim = base - bc.penalty(base, sum(t.size() for t in txs), bc.median_size()) + sum(t.fee for t in txs)
    first = coinbase if coinbase is not None else Transaction(COINBASE, miner.address, claim)
    block = Block(height if index is None else index, [first] + txs if first is not None else txs,
                  bc.chain[-1].hash if previous is None else previous, timestamp=ts)
    block.mine(bc.next_target() if mine_target is None else mine_target)
    bc.chain.append(block)
    return block


def evaluate_chain_case(case):
    """What the reference says about a chain vector (shared by the generator and the tests)."""
    with min_block_median(case.get("constants", {}).get("min_block_median", 300_000)):
        bc = load_chain(case["doc"])
        out = {"valid": bc.is_valid(), "valid_without_pow_recheck": bc.is_valid(check_pow=False)}
        if out["valid"]:
            out["balances"] = dict(sorted(bc.all_balances().items()))
            out["supply"] = bc.supply()
        return out


CASES = []


def add_case(name, doc, expect_valid, note="", constants=None, expect_cheap=None, base_ok=None):
    case = {"name": name, "doc": doc, "constants": constants or {}, "note": note}
    got = evaluate_chain_case(case)
    assert got["valid"] is expect_valid, f"{name}: the reference says valid={got['valid']}"
    if expect_cheap is not None:
        assert got["valid_without_pow_recheck"] is expect_cheap, f"{name}: cheap-only verdict {got['valid_without_pow_recheck']}"
    if base_ok is not None:
        # the chain up to the block before the last must be valid, so the LAST block is the reason
        with min_block_median(case["constants"].get("min_block_median", 300_000)):
            assert load_chain(base_ok).is_valid(), f"{name}: the prefix is not valid"
    case["expect_valid"] = expect_valid
    case["expect_valid_without_pow_recheck"] = got["valid_without_pow_recheck"]
    if "balances" in got:
        case["balances"], case["supply"] = got["balances"], got["supply"]
    CASES.append(case)
    return case


def tamper(doc, fn):
    d = copy.deepcopy(doc)
    fn(d)
    return d


def sha_chain_cases():
    good = chain_doc(basic_chain())
    add_case("valid: six blocks, transactions, a memo with unicode, retargeting every block", good, True)
    add_case("valid: only the genesis block", chain_doc(new_sha_chain()), True)
    add_case("valid: one empty block", prefix(good, 1), True)

    # ---- a tampered field, with the block NOT re-mined (the hash no longer follows) ----
    add_case("tampered: an amount inside block 3", tamper(good, lambda d: d["chain"][3]["transactions"][1].__setitem__("amount", d["chain"][3]["transactions"][1]["amount"] + 1)), False)
    add_case("tampered: a memo inside block 4", tamper(good, lambda d: d["chain"][4]["transactions"][1].__setitem__("memo", "changed")), False)
    add_case("tampered: the nonce of block 2", tamper(good, lambda d: d["chain"][2].__setitem__("nonce", d["chain"][2]["nonce"] + 1)), False)
    add_case("tampered: the stored hash of block 2", tamper(good, lambda d: d["chain"][2].__setitem__("hash", "0" * 64)), False)
    add_case("tampered: the timestamp of block 5", tamper(good, lambda d: d["chain"][5].__setitem__("timestamp", d["chain"][5]["timestamp"] + 1)), False)
    add_case("tampered: the previous_hash of block 3", tamper(good, lambda d: d["chain"][3].__setitem__("previous_hash", "ab" * 32)), False)
    add_case("broken: block 3 removed (the index jumps)", tamper(good, lambda d: d["chain"].pop(3)), False)

    # ---- a properly mined last block that breaks exactly one rule ----
    base5 = prefix(good, 5)
    fee = to_units("0.05")

    def bad(name, note, builder, **kw):
        bc = load_chain(base5)
        builder(bc)
        add_case(name, chain_doc(bc), False, note, base_ok=base5, **kw)

    ts = T0 + 365
    bad("rule: coinbase claims one unit too much", "expected = base - penalty + fees, exactly",
        lambda bc: manual_block(bc, ALICE, [], ts, claim=bc.reward_at(6) + 1))
    bad("rule: coinbase claims one unit too little", "the claim must EQUAL the expected amount",
        lambda bc: manual_block(bc, ALICE, [], ts, claim=bc.reward_at(6) - 1))
    bad("rule: a block with no transactions at all", "the first transaction must be the reward",
        lambda bc: _empty_block(bc, ts))
    bad("rule: the first transaction is not from COINBASE", "a signed ordinary payment in the reward slot",
        lambda bc: manual_block(bc, ALICE, [], ts, coinbase=stx(ALICE, BOB.address, to_units(1), fee)))
    bad("rule: spending more than the sender has", "balances are tracked block by block",
        lambda bc: manual_block(bc, ALICE, [stx(BOB, CAROL.address, to_units(10_000), fee)], ts))
    bad("rule: fee below the minimum for the transaction's size", "min fee = ceil(rate * size / 1000)",
        lambda bc: manual_block(bc, ALICE, [stx(ALICE, BOB.address, to_units(1), 1)], ts))
    bad("rule: signature by the wrong key", "sender A, public key A, signed by C",
        lambda bc: manual_block(bc, ALICE, [stx(ALICE, BOB.address, to_units(1), fee, signer=CAROL)], ts))
    bad("rule: public key does not match the sender address", "sender A, public key C",
        lambda bc: manual_block(bc, ALICE, [_mismatched(fee)], ts))
    bad("rule: amount must be positive", "amount 0",
        lambda bc: manual_block(bc, ALICE, [stx(ALICE, BOB.address, 0, fee)], ts))
    bad("rule: a transaction may not appear twice", "the same signed transaction as in block 3, again",
        lambda bc: manual_block(bc, ALICE, [_transaction(bc, 3, 1)], ts))
    bad("rule: timestamp older than the median of the last 11", "median-time-past",
        lambda bc: manual_block(bc, ALICE, [], T0))
    bad("rule: timestamp too far in the future", "more than 120 s after the validator's clock (year 2100)",
        lambda bc: manual_block(bc, ALICE, [], 4_102_444_800))
    bad("rule: the hash does not meet the required target", "mined at an easier target than required",
        lambda bc: _mine_too_easy(bc, ts))
    bad("rule: previous_hash does not match", "properly mined, but on a different parent",
        lambda bc: manual_block(bc, ALICE, [], ts, previous="cd" * 32))
    bad("rule: block number is not the parent's + 1", "properly mined, index 7 instead of 6",
        lambda bc: manual_block(bc, ALICE, [], ts, index=7))


def _empty_block(bc, ts):
    block = Block(len(bc.chain), [], bc.chain[-1].hash, timestamp=ts)
    block.mine(bc.next_target())
    bc.chain.append(block)


def _mismatched(fee):
    tx = Transaction(ALICE.address, BOB.address, to_units(1), fee=fee, public_key=CAROL.public_key)
    tx.signature = CAROL.signing_key.sign_deterministic(tx.payload()).hex()
    return tx


def _transaction(bc, block_index, tx_index):
    return Transaction.from_dict(bc.chain[block_index].transactions[tx_index].to_dict())


def _mine_too_easy(bc, ts):
    required = bc.next_target()
    height = len(bc.chain)
    block = Block(height, [Transaction(COINBASE, ALICE.address, bc.reward_at(height))], bc.chain[-1].hash, timestamp=ts)
    start = 0
    while True:
        block.nonce = start
        block.mine(2**255)                       # far easier than required
        if int(block.hash, 16) >= required:
            break
        start = block.nonce + 1
    bc.chain.append(block)


def oversize_cases():
    constants = {"min_block_median": 2200}
    with min_block_median(2200):
        bc = new_sha_chain(initial_reward=to_units(50))
        mine(bc, ALICE, [], T0 + 60)
        mine(bc, ALICE, [], T0 + 120)
        base = chain_doc(bc)
        fee = to_units("0.05")

        def many(n, tag):
            return [stx(ALICE, BOB.address, to_units(1) + i, fee, f"{tag} {i}") for i in range(n)]

        def make(name, txs, expect, note, **kw):
            b = load_chain(base)
            manual_block(b, ALICE, txs, T0 + 180, **kw)
            add_case(name, chain_doc(b), expect, note, constants=constants, base_ok=base)

        small, over, huge = many(5, "s"), many(6, "o"), many(11, "h")
        size = lambda ts: sum(t.size() for t in ts)   # noqa: E731
        assert size(small) <= 2200 < size(over) and size(huge) > 4400
        make("size: five transactions fit under the floor of 2200 bytes: full reward", small, True,
             "block body size <= median: no penalty")
        make("size: six transactions are over the floor and pay the penalty", over, True,
             "coinbase = base - ceil(base * over^2 / median^2) + fees")
        make("size: six transactions with the penalty skipped", over, False,
             "claims base + fees although the block is over the median",
             claim=b_reward(base) + sum(t.fee for t in over))
        make("size: eleven transactions exceed the hard limit (2 x median)", huge, False,
             "body size > 2 * median is invalid whatever the coinbase claims", claim=sum(t.fee for t in huge))


def b_reward(doc):
    with min_block_median(2200):
        return load_chain(doc).reward_at(len(doc["chain"]))


def matmul_cases():
    p = SMALL_PARAMS[1]
    bc = Blockchain(target=2**256 // 16, initial_reward=to_units(50), halving_interval=1000,
                    max_supply=to_units(1_000_000), tail_reward=to_units(1), block_time=60,
                    difficulty_window=4, pow=powmod.MatmulPow(p, 2))
    mine(bc, ALICE, [], T0 + 60)
    mine(bc, ALICE, [], T0 + 120)
    mine(bc, ALICE, [], T0 + 180)
    mine(bc, ALICE, [stx(ALICE, BOB.address, to_units(2), to_units("0.05"), "memo")], T0 + 240)
    mine(bc, BOB, [], T0 + 300)
    good = chain_doc(bc)
    add_case("matmul: valid, five blocks across three epochs (2 blocks each)", good, True,
             "tiny parameters (m=8 k=64 nb=32, 6 slices), epoch_blocks=2", expect_cheap=True)

    def flip(hexstr):
        return ("00" if hexstr[:2] != "00" else "01") + hexstr[2:]

    add_case("matmul: one byte of the mix changed", tamper(good, lambda d: d["chain"][3].__setitem__("mix", flip(d["chain"][3]["mix"]))),
             False, "the cheap check (hash from seed + mix) already rejects it", expect_cheap=False)
    add_case("matmul: the hash changed", tamper(good, lambda d: d["chain"][3].__setitem__("hash", "0" * 64)),
             False, "", expect_cheap=False)
    add_case("matmul: a missing mix", tamper(good, lambda d: d["chain"][3].pop("mix")), False, "", expect_cheap=False)
    add_case("matmul: a transaction amount changed", tamper(good, lambda d: d["chain"][4]["transactions"][1].__setitem__("amount", 1)),
             False, "the header hash covers the transactions", expect_cheap=False)

    base4 = prefix(good, 4)

    def block5(bc):
        height = len(bc.chain)
        return Block(height, [Transaction(COINBASE, ALICE.address, bc.reward_at(height))], bc.chain[-1].hash, timestamp=T0 + 300)

    b = load_chain(base4)
    blk = block5(b)                                            # block 5 belongs to epoch 2
    required = b.next_target()
    header = hashlib.sha256(blk._header_bytes()).digest()
    old = mh.cached_dataset(p, powmod.epoch_seed(1))           # ... but is solved with epoch 1's dataset
    nonce = 0
    while True:
        (digest, mix), = mh.compute_attempts(p, old, header, [nonce])
        if int.from_bytes(digest, "big") < required:
            break
        nonce += 1
    blk.nonce, blk.hash, blk.mix = nonce, digest.hex(), mix.hex()
    b.chain.append(blk)
    add_case("matmul: solved against the previous epoch's dataset", chain_doc(b), False,
             "internally consistent and under the target, so only the full recomputation rejects it",
             expect_cheap=True, base_ok=base4)

    b = load_chain(base4)
    blk = block5(b)
    required = b.next_target()
    header = hashlib.sha256(blk._header_bytes()).digest()
    nonce = 0
    while True:
        fake = hashlib.sha256(b"tenero vectors fake mix" + nonce.to_bytes(8, "little")).digest() * 2
        digest = mh.digest_of(mh.attempt_seed(header, nonce), fake)
        if int.from_bytes(digest, "big") < required:
            break
        nonce += 1
    blk.nonce, blk.hash, blk.mix = nonce, digest.hex(), fake.hex()
    b.chain.append(blk)
    add_case("matmul: a made-up mix ground until the hash meets the target", chain_doc(b), False,
             "passes the cheap check; costs a forger about `difficulty` SHA-256 hashes; only the full "
             "recomputation catches it", expect_cheap=True, base_ok=base4)


def chains_vectors():
    del CASES[:]
    sha_chain_cases()
    oversize_cases()
    matmul_cases()
    return wrap("chains", "Whole chains and whether the reference accepts them. Each case is a "
                "chain.json-shaped document (doc), the consensus constants it needs, and the verdicts: "
                "expect_valid is is_valid() with the full proof-of-work re-check; "
                "expect_valid_without_pow_recheck is the same without recomputing matmul proofs of work "
                "(everything cheap is still checked). Every 'rule:' case is a validly mined block "
                "that breaks exactly one rule, on top of a prefix that is valid by itself. The "
                "timestamp rule depends on the validator's clock, so the timestamps used are far in "
                "the past (or, for the future-timestamp case, in the year 2100).",
                {"cases": list(CASES)})


# ---------------------------------------------------------------- driver

BUILDERS = {"chacha20": chacha_vectors, "matmulhash_small": small_vectors, "matmulhash_real": real_vectors,
            "pow_misc": pow_misc_vectors, "emission": emission_vectors, "difficulty": difficulty_vectors,
            "fees_and_size": fees_and_size_vectors, "units": units_vectors,
            "legacy_account_model": legacy_vectors, "chains": chains_vectors}


def build_fast():
    return {name: BUILDERS[name]() for name in FAST_FILES}


def path_of(name):
    return os.path.join(VECTOR_DIR, name + ".json")


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--check", action="store_true", help="compare the committed files with the reference")
    ap.add_argument("--write", action="store_true", help="regenerate the files (a consensus change)")
    ap.add_argument("--deep", action="store_true", help="include the deep vector (slices up to 77)")
    ap.add_argument("--full", action="store_true", help="build the FULL vector (needs 4.3 GiB of RAM)")
    ap.add_argument("--threads", type=int, default=4, help="threads for --full (default 4)")
    args = ap.parse_args(argv)
    if not (args.check or args.write or args.full):
        ap.print_help()
        return 1
    docs = {}
    if args.check or args.write:
        docs.update(build_fast())
        if args.deep:
            docs["matmulhash_deep"] = deep_vectors()
    if args.full:
        print(f"building the full 4 GiB dataset on {args.threads} threads (needs about 4.3 GiB of RAM)...")
        docs["matmulhash_full"] = full_vectors(threads=args.threads,
                                               progress=lambda d, n: print(f"  {d}/{n} slices", flush=True))
    bad = 0
    os.makedirs(VECTOR_DIR, exist_ok=True)
    for name, doc in docs.items():
        text = jdump(doc)
        path = path_of(name)
        if args.check and not args.full:
            same = os.path.exists(path) and open(path).read() == text
            print(f"{'ok      ' if same else 'DIFFERS '}{name}")
            bad += 0 if same else 1
        else:
            with open(path, "w", newline="\n") as f:
                f.write(text)
            print(f"wrote {os.path.relpath(path, ROOT)}  ({len(text) / 1024:.0f} KiB)")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
