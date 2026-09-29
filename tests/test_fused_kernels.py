"""The real CUDA kernel source, run on the CPU through the emulator, against the numpy reference.

This checks the kernels' LOGIC: the ChaCha20 maths, the data-dependent picks, the indexing, the
grid-stride loops and the fold's atomic sums. It does not prove CUDA compilation or GPU speed:
NVRTC compiles the same source for sm_120/89/86, and the self-test on a real GPU covers the rest.
"""
import pathlib
import shutil
import sys
import tempfile

import pytest

np = pytest.importorskip("numpy")

from tenero import chacha  # noqa: E402
from tenero import gpubackend as g  # noqa: E402
from tenero import matmulhash as mh  # noqa: E402

from . import cuda_emulator as emu  # noqa: E402
from .fake_torch import FakeTorch  # noqa: E402

pytestmark = pytest.mark.skipif(emu.compiler() is None, reason="needs a C++ compiler (g++)")


@pytest.fixture(scope="module")
def dll():
    d = pathlib.Path(tempfile.mkdtemp())
    yield emu.build(d)
    shutil.rmtree(d, ignore_errors=True)


@pytest.fixture
def fused(dll):
    return g.FusedKernels(emu.FakeCupy(dll), FakeTorch, "cuda")


def keys_of(seed, n):
    return np.random.RandomState(seed).randint(0, 2**32, size=(n, 8), dtype=np.int64)


def reference_keystream(keys, blocks, start):
    return [chacha.keystream(k.astype("<u4").tobytes(), blocks, start) for k in keys]


def kernel_keystream(f, keys, blocks, start):
    out = np.asarray(f.keystream(f.device_keys(keys), blocks, start))
    return [out[i].view(np.uint32).reshape(blocks, 16) for i in range(len(keys))]


# ---- the ChaCha20 keystream kernel ----

@pytest.mark.parametrize("threads,max_blocks", [(256, 1 << 20), (32, 1 << 20), (7, 3), (1, 2)])
@pytest.mark.parametrize("blocks,start", [(1, 0), (10, 0), (37, 12345), (50, 2**32 - 20)])
def test_keystream_matches_the_reference_for_any_grid_shape(fused, threads, max_blocks, blocks, start):
    fused.THREADS, fused.MAX_BLOCKS = threads, max_blocks     # small grids force grid-stride loops
    keys = keys_of(1, 3)
    got = kernel_keystream(fused, keys, blocks, start)
    for a, b in zip(got, reference_keystream(keys, blocks, start)):
        assert np.array_equal(a, b)


def test_keystream_each_key_gets_its_own_stream(fused):
    keys = keys_of(2, 4)
    got = kernel_keystream(fused, keys, 6, 0)
    assert len({a.tobytes() for a in got}) == 4


# ---- the dataset kernels ----

@pytest.mark.parametrize("params", [
    mh.Params(m=8, k=64, nb=32, num_blocks=6),        # 32 blocks per slice
    mh.Params(m=4, k=48, nb=40, num_blocks=5),        # 30 blocks per slice: not a multiple of anything
    mh.Params(m=4, k=16, nb=8, num_blocks=9),         # 2 blocks per slice, many slices
])
@pytest.mark.parametrize("threads,max_blocks", [(256, 1 << 20), (5, 2)])
def test_the_dataset_kernels_match_the_reference(fused, params, threads, max_blocks):
    fused.THREADS, fused.MAX_BLOCKS = threads, max_blocks
    W = FakeTorch.empty((params.num_blocks, params.nb, params.k), dtype=np.int8)
    fused.build_dataset(W, params, b"kernel epoch")
    got = np.asarray(W).view(np.uint32).reshape(params.num_blocks, -1)
    assert np.array_equal(got, mh.build_dataset(params, b"kernel epoch"))


def test_the_dataset_depends_on_the_epoch_seed(fused):
    p = mh.Params(m=8, k=64, nb=32, num_blocks=4)
    out = []
    for seed in (b"a", b"b"):
        W = FakeTorch.empty((p.num_blocks, p.nb, p.k), dtype=np.int8)
        fused.build_dataset(W, p, seed)
        out.append(np.asarray(W).copy())
    assert not np.array_equal(out[0], out[1])


def test_a_fill_launch_writes_only_its_own_slice(fused):
    p = mh.Params(m=8, k=64, nb=32, num_blocks=6)
    blocks = p.blocks_per_slice
    W = FakeTorch.zeros((p.num_blocks, p.nb, p.k), dtype=np.int8)
    key = fused.device_keys(np.frombuffer(mh.dataset_key(b"e"), dtype="<u4")[None])
    fused.keystream_kernel((1,), (64,), (np.uint64(key.data_ptr()), np.uint64(W.data_ptr()),
                                         np.uint64(blocks), np.uint64(0), np.uint64(blocks)))
    before = np.asarray(W).copy()
    fused.fill_kernel((1,), (64,), (np.uint64(W.data_ptr()), np.uint64(blocks), np.uint32(1)))
    after = np.asarray(W)
    assert np.array_equal(after[0], before[0])            # earlier slices untouched
    assert (after[1] != 0).any() and not after[2:].any()  # only slice 1 was written


# ---- the fold kernel ----

@pytest.mark.parametrize("attempts,chunks", [(1, 1), (1, 7), (3, 64), (2, 100)])
@pytest.mark.parametrize("threads,fold_blocks", [(256, 4), (16, 3), (1, 1), (5, 2)])
def test_fold_matches_the_reference_for_any_grid_shape(fused, attempts, chunks, threads, fold_blocks):
    fused.THREADS, fused.FOLD_BLOCKS = threads, fold_blocks
    rng = np.random.RandomState(4)
    C = rng.randint(-2**31, 2**31, size=(attempts, chunks * 16), dtype=np.int64).astype(np.int32)
    C[0, :4] = [-2**31, 2**31 - 1, 0, -1]
    got = np.asarray(fused.fold_sums(FakeTorch.from_numpy(C), chunks))
    assert np.array_equal(got.view(np.uint64), mh.fold_sums(C))


def test_fold_sums_do_not_depend_on_how_the_work_is_split(fused):
    C = np.random.RandomState(9).randint(-2**31, 2**31, size=(2, 16 * 50), dtype=np.int64).astype(np.int32)
    results = []
    for threads, fold_blocks in ((256, 4), (1, 1), (3, 5)):
        fused.THREADS, fused.FOLD_BLOCKS = threads, fold_blocks
        results.append(np.asarray(fused.fold_sums(FakeTorch.from_numpy(C), 50)).copy())
    assert np.array_equal(results[0], results[1]) and np.array_equal(results[0], results[2])


# ---- everything together, through the backend ----

def test_whole_attempts_through_the_backend_match_the_cpu(fused):
    p = mh.Params(m=8, k=256, nb=64, num_blocks=6)
    backend = g.TorchBackend(FakeTorch, "cuda", "int_mm", fused=fused)
    W = backend.build_dataset(p, b"whole")
    header = mh.attempt_seed(b"h", 1)
    nonces = [0, 1, 2, 99, 2**33, 7, 8, 9]
    assert backend.attempts(p, W, header, nonces) == \
        mh.compute_attempts(p, mh.build_dataset(p, b"whole"), header, nonces)


# ---- the kernel source itself ----

def test_the_kernel_source_uses_the_pick_count_of_the_reference():
    assert f"#define PICKS {mh.PICKS}" in g.KERNEL_SOURCE
    assert "__PICKS__" not in g.KERNEL_SOURCE


def test_the_kernel_source_needs_no_includes():
    assert "#include" not in g.KERNEL_SOURCE


# ---- streams, smoke test and the missing-CuPy message ----

def test_the_stream_helper_prefers_the_current_cupy_api(dll):
    cp = emu.FakeCupy(dll, new_streams=True)
    g.FusedKernels(cp, FakeTorch, "cuda").keystream(
        g.FusedKernels(cp, FakeTorch, "cuda").device_keys(keys_of(1, 1)), 2)
    assert cp.used == ["from_external"]


def test_the_stream_helper_falls_back_to_the_old_cupy_api(dll):
    cp = emu.FakeCupy(dll, new_streams=False)
    f = g.FusedKernels(cp, FakeTorch, "cuda")
    f.keystream(f.device_keys(keys_of(1, 1)), 2)
    assert cp.used == ["ExternalStream"]


def test_smoke_test_runs_all_three_kernels(fused):
    fused.smoke_test()


def test_make_fused_says_what_to_install_when_cupy_is_missing(monkeypatch):
    monkeypatch.setitem(sys.modules, "cupy", None)         # makes "import cupy" fail
    with pytest.raises(RuntimeError, match=r'pip install "cupy-cuda13x\[ctk\]"'):
        g.make_fused(FakeTorch, "cuda")


def test_make_fused_reports_kernels_that_will_not_compile(monkeypatch):
    class Broken:
        def RawKernel(self, source, name):
            raise RuntimeError("nvrtc: error: expected a ';'")
    monkeypatch.setitem(sys.modules, "cupy", Broken())
    with pytest.raises(RuntimeError, match="would not run on this GPU"):
        g.make_fused(FakeTorch, "cuda")


# ---- mutation tests: break the kernel on purpose; the tests above must notice ----

MUTATIONS = [
    ("a rotate amount in the quarter round", "d = rotl32(d, 16);", "d = rotl32(d, 15);", "keystream"),
    ("the keystream ignores the start counter", "x[12] = (unsigned)(start_counter + local);",
     "x[12] = (unsigned)local;", "keystream"),
    ("the fill counter is off by one", "x[12] = (unsigned)u;", "x[12] = (unsigned)u + 1u;", "dataset"),
    ("the fill forgets the slice number", "x[13] = slice_j;", "x[13] = 0u;", "dataset"),
    ("a pick uses the wrong word for the block", "prev[2 * i + 1] % (unsigned)blocks_per_slice",
     "prev[2 * i] % (unsigned)blocks_per_slice", "dataset"),
    ("only the last pick counts", "ref[w] ^= picked[w];", "ref[w] = picked[w];", "dataset"),
    ("the picks may not reach the previous slice", "prev[2 * i] % slice_j;",
     "prev[2 * i] % (slice_j > 1u ? slice_j - 1u : 1u);", "dataset"),
    ("the fill skips the final XOR with its inputs", "out_slice[u * 16 + w] = x[w] ^ prev[w] ^ ref[w];",
     "out_slice[u * 16 + w] = x[w];", "dataset"),
    ("the fold ignores the chunk position", "x[0] ^= (unsigned)c;", "", "fold"),
    ("the fold drops the upper half of each chunk", "(unsigned long long)x[i] + (unsigned long long)x[i + 8]",
     "(unsigned long long)x[i]", "fold"),
    ("the fold adds into the wrong sum", "atomicAdd(&sums[i], acc[i]);",
     "atomicAdd(&sums[(i + 1) % 8], acc[i]);", "fold"),
]


def caught(source, what):
    """True if the (broken) kernel source disagrees with the reference for `what`."""
    d = pathlib.Path(tempfile.mkdtemp())
    try:
        f = g.FusedKernels(emu.FakeCupy(emu.build(d, source)), FakeTorch, "cuda")
        f.THREADS = 16
        if what == "keystream":
            keys = keys_of(5, 2)
            got = kernel_keystream(f, keys, 20, 3)
            return not all(np.array_equal(a, b) for a, b in zip(got, reference_keystream(keys, 20, 3)))
        if what == "dataset":
            p = mh.Params(m=8, k=64, nb=32, num_blocks=6)
            W = FakeTorch.empty((p.num_blocks, p.nb, p.k), dtype=np.int8)
            f.build_dataset(W, p, b"mutant")
            return not np.array_equal(np.asarray(W).view(np.uint32).reshape(p.num_blocks, -1),
                                      mh.build_dataset(p, b"mutant"))
        C = np.random.RandomState(6).randint(-2**31, 2**31, size=(2, 16 * 20), dtype=np.int64).astype(np.int32)
        return not np.array_equal(np.asarray(f.fold_sums(FakeTorch.from_numpy(C), 20)).view(np.uint64),
                                  mh.fold_sums(C))
    finally:
        shutil.rmtree(d, ignore_errors=True)


@pytest.mark.parametrize("label,old,new,what", MUTATIONS, ids=[m[0] for m in MUTATIONS])
def test_the_tests_catch_a_deliberately_broken_kernel(label, old, new, what):
    assert g.KERNEL_SOURCE.count(old) >= 1, f"mutation target not found: {old!r}"
    assert caught(g.KERNEL_SOURCE.replace(old, new, 1), what), f"NOT caught: {label}"


def test_the_unmodified_kernel_is_not_flagged_by_the_same_checks():
    for what in ("keystream", "dataset", "fold"):
        assert not caught(g.KERNEL_SOURCE, what)
