import hashlib
import struct

import pytest

np = pytest.importorskip("numpy")

from toycoin import chacha  # noqa: E402
from toycoin import matmulhash as mh  # noqa: E402

from .test_chacha import py_core  # noqa: E402

TINY = mh.Params(m=4, k=16, nb=8, num_blocks=5)     # 5 slices of 128 bytes (2 blocks each)
HEADER = hashlib.sha256(b"test header").digest()
EPOCH = b"epoch-0"
M32 = 0xFFFFFFFF


# ---- an independent implementation for tiny sizes: OpenSSL for the standard ChaCha20 blocks,
#      plain Python integers for everything else ----

def openssl_block(key, counter, nonce_words):
    ciphers = pytest.importorskip("cryptography.hazmat.primitives.ciphers")
    iv = struct.pack("<4I", counter, *nonce_words)
    enc = ciphers.Cipher(ciphers.algorithms.ChaCha20(key, iv), mode=None).encryptor()
    return list(struct.unpack("<16I", enc.update(b"\x00" * 64)))


def py_dataset(params, epoch_seed):
    B, N = params.blocks_per_slice, params.num_blocks
    dkey = hashlib.sha256(mh.DATASET_LABEL + epoch_seed).digest()
    data = [[None] * B for _ in range(N)]
    for u in range(B):
        data[0][u] = openssl_block(dkey, u, (0, 0, 0))
    for j in range(1, N):
        for u in range(B):
            prev = data[j - 1][u]
            ref = [0] * 16
            for i in range(mh.PICKS):
                picked = data[prev[2 * i] % j][prev[2 * i + 1] % B]
                ref = [a ^ b for a, b in zip(ref, picked)]
            key = struct.pack("<8I", *[prev[w] ^ ref[w] for w in range(8)])
            out = openssl_block(key, u, (j, prev[8] ^ ref[8], prev[9] ^ ref[9]))
            data[j][u] = [o ^ p ^ r for o, p, r in zip(out, prev, ref)]
    return data


def py_attempt(params, data, header_hash, nonce):
    seed = hashlib.sha256(header_hash + nonce.to_bytes(8, "little")).digest()
    b = int.from_bytes(hashlib.sha256(seed + b"\x01").digest()[:8], "little") % params.num_blocks
    ks = b""
    for c in range(params.m * params.k // 64):
        ks += struct.pack("<16I", *openssl_block(seed, c, (0, 0, 0)))
    X = struct.unpack(f"<{params.m * params.k}b", ks)
    raw = struct.pack(f"<{params.slice_words}I", *[w for blk in data[b] for w in blk])
    raw = struct.unpack(f"<{params.slice_bytes}b", raw)          # W[t][n] = raw[n * k + t]
    C = []
    for i in range(params.m):
        for n in range(params.nb):
            C.append(sum(X[i * params.k + t] * raw[n * params.k + t] for t in range(params.k)) & M32)
    sums = [0] * 8
    for c in range(len(C) // 16):
        x = C[16 * c:16 * c + 16]
        x[0] ^= c
        y = py_core(x)
        for i in range(16):
            sums[i % 8] += y[i]
    mix = struct.pack("<8Q", *sums)
    return hashlib.sha256(seed + mix).digest(), mix


def test_the_dataset_matches_an_independent_implementation():
    data = mh.build_dataset(TINY, EPOCH)
    expected = py_dataset(TINY, EPOCH)
    for j in range(TINY.num_blocks):
        got = data[j].reshape(TINY.blocks_per_slice, 16)
        for u in range(TINY.blocks_per_slice):
            assert [int(w) for w in got[u]] == expected[j][u], (j, u)


@pytest.mark.parametrize("nonce", [0, 1, 12345, 2**40 + 7])
def test_an_attempt_matches_an_independent_implementation(nonce):
    data = mh.build_dataset(TINY, EPOCH)
    digest, mix = py_attempt(TINY, py_dataset(TINY, EPOCH), HEADER, nonce)
    assert mh.compute_attempts(TINY, data, HEADER, [nonce])[0] == (digest, mix)


def test_a_bigger_shape_also_matches_the_independent_implementation():
    p = mh.Params(m=4, k=32, nb=16, num_blocks=6)         # 8 blocks per slice, more picks land
    data = mh.build_dataset(p, b"another epoch")
    for nonce in (3, 99):
        digest, mix = py_attempt(p, py_dataset(p, b"another epoch"), HEADER, nonce)
        assert mh.compute_attempts(p, data, HEADER, [nonce])[0] == (digest, mix)


def test_batching_does_not_change_results():
    p = mh.Params(m=8, k=64, nb=32, num_blocks=6)
    data = mh.build_dataset(p, EPOCH)
    together = mh.compute_attempts(p, data, HEADER, [5, 6, 7, 8])
    alone = [mh.compute_attempts(p, data, HEADER, [n])[0] for n in (5, 6, 7, 8)]
    assert together == alone


def test_results_depend_on_nonce_header_and_epoch():
    p = mh.Params(m=8, k=64, nb=32, num_blocks=6)
    data = mh.build_dataset(p, EPOCH)
    r = mh.compute_attempts(p, data, HEADER, [1, 2])
    assert r[0] != r[1] and r[0][1] != r[1][1]
    assert r[0] == mh.compute_attempts(p, data, HEADER, [1])[0]              # deterministic
    other_header = hashlib.sha256(b"other").digest()
    assert mh.compute_attempts(p, data, other_header, [1])[0] != r[0]
    other_data = mh.build_dataset(p, b"epoch-1")
    assert mh.compute_attempts(p, other_data, HEADER, [1])[0] != r[0]


def test_every_slice_is_different_and_uses_the_whole_int8_range():
    p = mh.Params(m=8, k=64, nb=32, num_blocks=8)
    data = mh.build_dataset(p, EPOCH)
    assert len({data[j].tobytes() for j in range(p.num_blocks)}) == 8
    raw = data.view(np.int8)
    assert raw.min() == -128 and raw.max() == 127


def test_the_slice_matrix_is_a_transposed_view_of_the_raw_bytes():
    p = mh.Params(m=4, k=16, nb=8, num_blocks=3)
    data = mh.build_dataset(p, EPOCH)
    W = mh.slice_matrix(data, 2, p)
    raw = data[2].view(np.int8)
    assert W.shape == (16, 8) and not W.flags["C_CONTIGUOUS"]
    for t, n in ((0, 0), (5, 3), (15, 7), (9, 1)):
        assert W[t, n] == raw[n * p.k + t]
    assert np.shares_memory(W, data)                     # a view, not a copy


def test_extreme_values_stay_exact():
    p = mh.Params(m=4, k=4096, nb=8, num_blocks=1)
    X = np.full((p.m, p.k), -128, dtype=np.int8)
    W = np.full((p.k, p.nb), -128, dtype=np.int8)
    C = (X.astype(np.float64) @ W.astype(np.float64)).astype(np.int64)
    assert (C == p.k * 16384).all() and p.k * 16384 < 2**31


# ---- the fold ----

def test_the_fold_matches_a_scalar_version_on_extreme_values():
    rng = np.random.default_rng(5)
    C = rng.integers(-2**31, 2**31, size=(2, 64), dtype=np.int64).astype(np.int32)
    C[0, :4] = [-2**31, 2**31 - 1, 0, -1]
    got = mh.fold_sums(C)
    for row_c, row_s in zip(C, got):
        sums = [0] * 8
        words = [int(v) & M32 for v in row_c]
        for c in range(len(words) // 16):
            x = words[16 * c:16 * c + 16]
            x[0] ^= c
            y = py_core(x)
            for i in range(16):
                sums[i % 8] += y[i]
        assert [int(v) for v in row_s] == sums


def test_the_fold_is_not_a_linear_shortcut():
    rng = np.random.default_rng(0)
    C1 = rng.integers(-1000, 1000, size=(1, 64), dtype=np.int64).astype(np.int32)
    C2 = rng.integers(-1000, 1000, size=(1, 64), dtype=np.int64).astype(np.int32)
    assert not np.array_equal(mh.fold_sums(C1 + C2), mh.fold_sums(C1) + mh.fold_sums(C2))


def test_the_fold_depends_on_every_element_and_on_position():
    rng = np.random.default_rng(1)
    C = rng.integers(-2**31, 2**31, size=(1, 64), dtype=np.int64).astype(np.int32)
    base = mh.fold_sums(C)
    for i in (0, 17, 63):                                 # changing any single element changes it
        changed = C.copy()
        changed[0, i] ^= 1
        assert not np.array_equal(mh.fold_sums(changed), base)
    swapped = C.copy()
    swapped[0, :16], swapped[0, 16:32] = C[0, 16:32].copy(), C[0, :16].copy()
    assert not np.array_equal(mh.fold_sums(swapped), base)   # moving a chunk changes it


# ---- what makes the dataset memory hard ----

def closure(data, params, j, blocks_wanted):
    """Every (slice, block) needed to compute the given blocks of slice j, by following the
    dependencies (the previous block, and the data-dependent picks)."""
    B = params.blocks_per_slice
    view = data.reshape(params.num_blocks, B, 16)
    seen, stack = set(), [(j, u) for u in blocks_wanted]
    while stack:
        node = stack.pop()
        if node in seen:
            continue
        seen.add(node)
        jj, uu = node
        if jj == 0:
            continue
        prev = view[jj - 1, uu]
        stack.append((jj - 1, uu))
        for i in range(mh.PICKS):
            stack.append((int(prev[2 * i] % jj), int(prev[2 * i + 1] % B)))
    return seen


@pytest.fixture(scope="module")
def deep_dataset():
    p = mh.Params(m=8, k=64, nb=64, num_blocks=32)        # 32 slices of 64 blocks
    return p, mh.build_dataset(p, b"depth")


def test_a_slice_depends_on_all_of_the_earlier_data(deep_dataset):
    p, data = deep_dataset
    for j in (8, 16, 31):
        needed = {n for n in closure(data, p, j, range(p.blocks_per_slice)) if n[0] < j}
        assert len(needed) == j * p.blocks_per_slice      # 100% of everything before it


def test_even_one_block_depends_on_a_large_part_of_the_earlier_data(deep_dataset):
    p, data = deep_dataset
    needed = {n for n in closure(data, p, 31, [0]) if n[0] < 31}
    assert len(needed) / (31 * p.blocks_per_slice) > 0.15


def test_changing_the_first_slice_changes_everything_after_it(deep_dataset):
    p, data = deep_dataset
    other = mh.build_dataset(p, b"different")
    for j in range(p.num_blocks):
        differing = (data[j] != other[j]).mean()
        assert differing > 0.99                            # avalanche: no slice keeps old data


def rebuild_cost(data, p, stored, b):
    """Slices' worth of blocks to recompute to get slice b when only `stored` slices are kept."""
    B = p.blocks_per_slice
    view = data.reshape(p.num_blocks, B, 16)
    needed, stack = set(), [(b, u) for u in range(B)]
    while stack:
        node = stack.pop()
        jj, uu = node
        if node in needed or jj in stored or jj == 0:
            continue
        needed.add(node)
        prev = view[jj - 1, uu]
        stack.append((jj - 1, uu))
        for i in range(mh.PICKS):
            stack.append((int(prev[2 * i] % jj), int(prev[2 * i + 1] % B)))
    return len(needed) / B


def test_storing_less_of_the_dataset_makes_a_missing_slice_costlier_to_rebuild(deep_dataset):
    import random
    p, data = deep_dataset
    rng = random.Random(4)
    average = {}
    for f in (0.9, 0.7, 0.5):
        costs = []
        for _ in range(15):
            stored = set(rng.sample(range(1, p.num_blocks), int(f * (p.num_blocks - 1)))) | {0}
            missing = [b for b in range(1, p.num_blocks) if b not in stored]
            costs.append(rebuild_cost(data, p, stored, rng.choice(missing)))
        average[f] = sum(costs) / len(costs)
    assert average[0.9] < average[0.7] < average[0.5]
    assert average[0.5] > 2 * average[0.9]                # and it climbs steeply, not gently


# ---- the cheap check ----

def test_precheck_accepts_a_true_solution_and_rejects_everything_else():
    p = mh.Params(m=8, k=64, nb=32, num_blocks=6)
    data = mh.build_dataset(p, EPOCH)
    target = mh.bits_to_target(5)
    nonce, digest, mix = mh.find_solution(p, data, HEADER, target)
    assert mh.precheck(HEADER, nonce, mix, digest.hex(), target)
    assert not mh.precheck(HEADER, nonce, mix, digest.hex(), 1)                   # above the target
    assert not mh.precheck(HEADER, nonce + 1, mix, digest.hex(), target)          # wrong nonce
    assert not mh.precheck(hashlib.sha256(b"x").digest(), nonce, mix, digest.hex(), target)
    forged = bytes([mix[0] ^ 1]) + mix[1:]
    assert not mh.precheck(HEADER, nonce, forged, digest.hex(), target)           # tampered mix
    assert not mh.precheck(HEADER, nonce, mix, "0" * 64, target)                  # wrong hash
    for bad_mix in (b"", mix[:63], mix + b"x", None, "abc"):
        assert not mh.precheck(HEADER, nonce, bad_mix, digest.hex(), target)
    for bad_nonce in (-1, 2**64, 1.5, "7", True, None):
        assert not mh.precheck(HEADER, bad_nonce, mix, digest.hex(), target)


def test_precheck_needs_no_dataset_and_is_fast():
    import time
    mix = bytes(64)
    t0 = time.perf_counter()
    for n in range(2000):
        mh.precheck(HEADER, n, mix, "0" * 64, 2**255)
    assert (time.perf_counter() - t0) / 2000 < 0.001     # well under a millisecond each


def test_a_made_up_mix_only_passes_by_grinding_to_the_target():
    # the cheap check is a real (if modest) cost for a forger: the mix must hash under the target
    target = mh.bits_to_target(8)
    passes = sum(mh.precheck(HEADER, n, hashlib.sha256(bytes([n % 256, n // 256])).digest() * 2,
                             hashlib.sha256(b"never matches").hexdigest(), target)
                 for n in range(3000))
    assert passes == 0                                    # a random mix does not satisfy it


def test_solution_search_and_verification():
    p = mh.Params(m=8, k=64, nb=32, num_blocks=6)
    data = mh.build_dataset(p, EPOCH)
    target = mh.bits_to_target(6)                         # about 64 attempts
    nonce, digest, mix = mh.find_solution(p, data, HEADER, target)
    assert mh.meets_target(digest, target) and mh.verify(p, data, HEADER, nonce, target)


# ---- parameters and the dataset cache ----

def test_params_validation_and_sizing():
    mh.Params().validate()
    p = mh.Params()
    assert (p.m, p.k, p.nb, p.num_blocks) == (64, 8192, 2048, 256)
    assert p.slice_bytes == 16 * 2**20 and p.dataset_bytes == 4 * 2**30
    assert p.blocks_per_slice == 262144 and p.slice_words == 4 * 2**20
    assert mh.params_for_dataset(2).dataset_bytes == 2 * 2**30
    assert mh.params_for_dataset(0.001, k=1024, nb=1024).num_blocks == 1
    for bad in (mh.Params(k=4095), mh.Params(nb=1001), mh.Params(m=0), mh.Params(k=200_000),
                mh.Params(num_blocks=0), mh.Params(m=3, k=8), mh.Params(m=1, k=64, nb=8)):
        with pytest.raises(ValueError):
            bad.validate()


def test_the_dataset_cache_keeps_the_newest_and_counts_builds():
    cache = mh.DatasetCache(keep=2)
    p = mh.Params(m=4, k=16, nb=8, num_blocks=3)
    a1 = cache.get(p, b"a")
    assert cache.get(p, b"a") is a1 and cache.builds == 1
    cache.get(p, b"b")
    cache.get(p, b"c")                                    # evicts "a", the oldest
    assert cache.builds == 3
    assert cache.get(p, b"a") is not a1 and cache.builds == 4
    cache.clear()
    cache.get(p, b"a")
    assert cache.builds == 5


def test_the_dataset_cache_builds_once_when_two_threads_ask():
    import threading
    cache = mh.DatasetCache()
    p = mh.Params(m=8, k=64, nb=32, num_blocks=4)
    results = []
    threads = [threading.Thread(target=lambda: results.append(cache.get(p, b"same")))
               for _ in range(4)]
    [t.start() for t in threads]
    [t.join() for t in threads]
    assert cache.builds == 1 and all(r is results[0] for r in results)


def test_a_cached_dataset_is_not_held_up_by_another_epochs_build(monkeypatch):
    import threading
    import time
    cache = mh.DatasetCache(keep=2)
    p = mh.Params(m=4, k=16, nb=8, num_blocks=3)
    ready = cache.get(p, b"current")
    started, release = threading.Event(), threading.Event()
    real = mh.build_dataset

    def slow_build(params, seed, progress=None):
        started.set()
        release.wait(10)
        return real(params, seed, progress)

    monkeypatch.setattr(mh, "build_dataset", slow_build)
    builder = threading.Thread(target=lambda: cache.get(p, b"next"))
    builder.start()
    assert started.wait(5)                              # the next epoch's build is under way
    t0 = time.perf_counter()
    assert cache.get(p, b"current") is ready            # the current one comes straight back
    assert time.perf_counter() - t0 < 0.5
    release.set()
    builder.join(10)
    assert cache.builds == 2


def test_a_dataset_is_evicted_before_the_next_one_is_built(monkeypatch):
    # peak memory: with keep=2, asking for a third must free the oldest BEFORE building
    cache = mh.DatasetCache(keep=2)
    p = mh.Params(m=4, k=16, nb=8, num_blocks=3)
    cache.get(p, b"a")
    cache.get(p, b"b")
    sizes = []
    real = mh.build_dataset

    def watching(params, seed, progress=None):
        sizes.append(len(cache._items))                 # how many are held while building
        return real(params, seed, progress)

    monkeypatch.setattr(mh, "build_dataset", watching)
    cache.get(p, b"c")
    assert sizes == [1]


# ---- building the dataset on several threads ----

@pytest.mark.parametrize("threads", [1, 2, 3, 8])
@pytest.mark.parametrize("chunk", [1, 3, 5, 7, 16, 10_000])
def test_the_result_is_identical_for_any_thread_count_and_chunk_size(threads, chunk):
    p = mh.Params(m=8, k=64, nb=32, num_blocks=7)                 # 32 blocks per slice
    assert np.array_equal(mh.build_dataset(p, EPOCH, threads=threads, chunk_blocks=chunk),
                          mh.build_dataset(p, EPOCH, threads=1, chunk_blocks=10_000))


def test_the_threaded_build_still_matches_the_independent_implementation():
    data = mh.build_dataset(TINY, EPOCH, threads=3, chunk_blocks=1)
    expected = py_dataset(TINY, EPOCH)
    for j in range(TINY.num_blocks):
        got = data[j].reshape(TINY.blocks_per_slice, 16)
        for u in range(TINY.blocks_per_slice):
            assert [int(w) for w in got[u]] == expected[j][u], (j, u)


def watch_threads(monkeypatch):
    """Wraps the ChaCha core to record which threads run it and how many run at once."""
    import threading
    import time
    seen, lock = set(), threading.Lock()
    state = {"now": 0, "peak": 0}
    real = chacha.chacha_core

    def watched(*a, **k):
        with lock:
            seen.add(threading.get_ident())
            state["now"] += 1
            state["peak"] = max(state["peak"], state["now"])
        time.sleep(0.01)                     # long enough for the other threads to overlap
        try:
            return real(*a, **k)
        finally:
            with lock:
                state["now"] -= 1

    monkeypatch.setattr(chacha, "chacha_core", watched)
    return seen, state


def test_a_threaded_build_really_uses_several_threads_but_never_more_than_asked(monkeypatch):
    import threading
    p = mh.Params(m=8, k=64, nb=32, num_blocks=4)
    seen, state = watch_threads(monkeypatch)
    mh.build_dataset(p, EPOCH, threads=3, chunk_blocks=4)         # 8 ranges per slice
    assert 2 <= state["peak"] <= 3
    assert 2 <= len(seen) <= 3 and threading.get_ident() not in seen


def test_one_thread_builds_in_the_calling_thread_and_spawns_nothing(monkeypatch):
    import threading
    p = mh.Params(m=8, k=64, nb=32, num_blocks=3)
    seen, state = watch_threads(monkeypatch)
    mh.build_dataset(p, EPOCH, threads=1, chunk_blocks=4)
    assert seen == {threading.get_ident()} and state["peak"] == 1


def test_a_single_chunk_needs_no_threads_even_if_they_are_allowed(monkeypatch):
    import threading
    p = mh.Params(m=8, k=64, nb=32, num_blocks=3)
    seen, _ = watch_threads(monkeypatch)
    mh.build_dataset(p, EPOCH, threads=8, chunk_blocks=10_000)    # one range per slice
    assert seen == {threading.get_ident()}


def test_an_error_in_a_worker_thread_reaches_the_caller(monkeypatch):
    p = mh.Params(m=8, k=64, nb=32, num_blocks=4)

    def boom(view, j, blocks, lo=0, hi=None):
        raise RuntimeError("a chunk failed")

    monkeypatch.setattr(mh, "fill_slice", boom)
    with pytest.raises(RuntimeError, match="a chunk failed"):
        mh.build_dataset(p, EPOCH, threads=3, chunk_blocks=4)


def test_the_default_thread_count_is_set_by_set_build_threads(monkeypatch):
    import threading
    monkeypatch.setattr(mh, "BUILD_THREADS", 1)
    mh.set_build_threads(3)
    assert mh.BUILD_THREADS == 3
    mh.set_build_threads(0)
    assert mh.BUILD_THREADS == 1                          # never less than one
    mh.set_build_threads(3)
    monkeypatch.setattr(mh, "CHUNK_BLOCKS", 4)
    p = mh.Params(m=8, k=64, nb=32, num_blocks=3)
    seen, state = watch_threads(monkeypatch)
    mh.DatasetCache().get(p, b"uses the default")         # the cache builds with the default
    assert 2 <= state["peak"] <= 3


def test_progress_is_still_reported_by_a_threaded_build():
    p = mh.Params(m=8, k=64, nb=32, num_blocks=16)
    seen = []
    mh.build_dataset(p, EPOCH, progress=lambda d, n: seen.append((d, n)), threads=3, chunk_blocks=4)
    assert seen == [(2 * i, 16) for i in range(1, 9)]


def test_the_cpu_fill_of_a_partial_range_touches_only_that_range():
    p = mh.Params(m=8, k=64, nb=32, num_blocks=4)
    blocks = p.blocks_per_slice
    data = mh.build_dataset(p, EPOCH)
    view = data.reshape(p.num_blocks, blocks, 16).copy()
    before = view[2].copy()
    view[2, 4:9] = 0
    mh.fill_slice(view, 2, blocks, 4, 9)
    assert np.array_equal(view[2], before)                # the range was rebuilt to the same values
    view[2, 4:9] = 0
    mh.fill_slice(view, 2, blocks, 4, 6)
    assert (view[2, 6:9] == 0).all() and not (view[2, 4:6] == 0).all()   # and nothing outside it


# ---- one dataset build at a time ----

def test_the_build_slot_serves_turns_in_the_order_they_were_asked_for():
    import threading
    slot = mh.BuildSlot()
    tickets = [slot.take() for _ in range(3)]
    assert tickets == [0, 1, 2]
    order = []

    def worker(t):
        slot.wait_turn(t)
        order.append(t)
        slot.finish(t)

    threads = [threading.Thread(target=worker, args=(t,)) for t in reversed(tickets)]   # arrive backwards
    [t.start() for t in threads]
    [t.join(5) for t in threads]
    assert order == [0, 1, 2]


def test_a_turn_that_is_never_used_is_skipped_not_waited_for():
    import threading
    slot = mh.BuildSlot()
    a, b, c = slot.take(), slot.take(), slot.take()
    slot.finish(b)                                        # b gives up before its turn
    done = []
    t = threading.Thread(target=lambda: (slot.wait_turn(c), done.append(c), slot.finish(c)))
    t.start()
    slot.wait_turn(a)
    slot.finish(a)                                        # a runs, then b is skipped, then c
    t.join(5)
    assert done == [c]


def test_two_datasets_are_never_built_at_the_same_time(monkeypatch):
    import threading
    import time
    cache = mh.DatasetCache(keep=3)
    p = mh.Params(m=4, k=16, nb=8, num_blocks=3)
    real = mh.build_dataset
    state = {"now": 0, "peak": 0}
    lock = threading.Lock()

    def slow(params, seed, progress=None, **kw):
        with lock:
            state["now"] += 1
            state["peak"] = max(state["peak"], state["now"])
        time.sleep(0.05)
        try:
            return real(params, seed, progress, **kw)
        finally:
            with lock:
                state["now"] -= 1

    monkeypatch.setattr(mh, "build_dataset", slow)
    threads = [threading.Thread(target=lambda s=s: cache.get(p, s)) for s in (b"a", b"b", b"c")]
    [t.start() for t in threads]
    [t.join(10) for t in threads]
    assert cache.builds == 3 and state["peak"] == 1


def test_builds_run_in_the_order_they_were_requested(monkeypatch):
    import threading
    cache = mh.DatasetCache(keep=3)
    p = mh.Params(m=4, k=16, nb=8, num_blocks=3)
    real = mh.build_dataset
    gate, started, order = threading.Event(), threading.Event(), []

    def gated(params, seed, progress=None, **kw):
        order.append(seed)
        if seed == b"first":
            started.set()
            gate.wait(10)                                 # holds the slot until released
        return real(params, seed, progress, **kw)

    monkeypatch.setattr(mh, "build_dataset", gated)
    threads = []
    for name in (b"first", b"second", b"third"):
        before = mh.BUILD_SLOT._next
        t = threading.Thread(target=lambda n=name: cache.get(p, n))
        t.start()
        threads.append(t)
        if name == b"first":
            assert started.wait(5)
        while mh.BUILD_SLOT._next == before:              # wait until it has queued
            pass
    gate.set()
    [t.join(10) for t in threads]
    assert order == [b"first", b"second", b"third"]


def test_a_failed_build_does_not_jam_the_queue_and_can_be_retried(monkeypatch):
    cache = mh.DatasetCache()
    p = mh.Params(m=4, k=16, nb=8, num_blocks=3)
    real = mh.build_dataset
    calls = {"n": 0}

    def flaky(params, seed, progress=None, **kw):
        calls["n"] += 1
        if calls["n"] == 1:
            raise MemoryError("not enough RAM")
        return real(params, seed, progress, **kw)

    monkeypatch.setattr(mh, "build_dataset", flaky)
    with pytest.raises(MemoryError):
        cache.get(p, b"x")
    assert cache.get(p, b"x") is not None                 # retry works, nothing is stuck
    assert cache.get(p, b"y") is not None and cache.builds == 2


def test_two_callers_for_the_same_dataset_share_one_build_and_one_turn(monkeypatch):
    import threading
    cache = mh.DatasetCache()
    p = mh.Params(m=4, k=16, nb=8, num_blocks=3)
    results = []
    threads = [threading.Thread(target=lambda: results.append(cache.get(p, b"same"))) for _ in range(4)]
    [t.start() for t in threads]
    [t.join(10) for t in threads]
    assert cache.builds == 1 and all(r is results[0] for r in results)
    assert cache.get(p, b"another") is not None           # the queue was not left waiting on a turn
