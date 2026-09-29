"""Checks the logic of the GPU layer (tenero/gpubackend.py) WITHOUT a GPU.

A numpy-backed stand-in plays the part of torch and the real CUDA kernel source runs on the CPU
through the emulator, so this catches typos and logic mistakes in the whole GPU path (dataset
build, per-attempt slices, matmul, fold, batching, the fp32 fallback, the searcher). It does NOT
prove real GPU behaviour. That is what `python gpu_pow_test.py` and the miner's startup self-test
check on your machine.
"""
import hashlib
import pathlib
import shutil
import tempfile

import pytest

np = pytest.importorskip("numpy")

from tenero import gpubackend as g  # noqa: E402
from tenero import matmulhash as mh  # noqa: E402
from tenero import pow as powmod  # noqa: E402
from tenero.chain import Blockchain  # noqa: E402
from tenero.units import UNIT  # noqa: E402

from . import cuda_emulator as emu  # noqa: E402
from .fake_torch import FT, FakeTorch, NoIntMm  # noqa: E402

pytestmark = pytest.mark.skipif(emu.compiler() is None, reason="needs a C++ compiler (g++)")

HEADER = hashlib.sha256(b"plumbing").digest()
EPOCH = b"epoch-plumbing"
P = mh.Params(m=8, k=1024, nb=64, num_blocks=6)


@pytest.fixture(scope="module")
def dll():
    d = pathlib.Path(tempfile.mkdtemp())
    yield emu.build(d)
    shutil.rmtree(d, ignore_errors=True)


@pytest.fixture
def fused(dll):
    return g.FusedKernels(emu.FakeCupy(dll), FakeTorch, "cuda")


def make_backend(fused, name="int_mm", torch=FakeTorch):
    return g.TorchBackend(torch, "cuda", name, fused=fused)


@pytest.mark.parametrize("backend_name", ["int_mm", "fp32"])
def test_backend_matches_cpu_reference(fused, backend_name):
    b = make_backend(fused, backend_name)
    W = b.build_dataset(P, EPOCH)
    cpu = mh.build_dataset(P, EPOCH)
    assert np.asarray(W).shape == (6, 64, 1024)          # stored transposed: (slices, nb, k)
    for blk in range(P.num_blocks):
        view = b.view(W, blk)                             # the logical (k, nb) slice
        assert np.asarray(view).shape == (1024, 64)
        assert np.array_equal(np.asarray(view), mh.slice_matrix(cpu, blk, P))
    nonces = [0, 1, 2, 99, 2**33, 7, 8, 9]
    assert b.attempts(P, W, HEADER, nonces) == mh.compute_attempts(P, cpu, HEADER, nonces)


def test_attempts_really_use_different_slices():
    blocks = {mh.attempt_slice(mh.attempt_seed(HEADER, n), P.num_blocks) for n in range(40)}
    assert blocks == set(range(P.num_blocks))             # all six slices are touched


def test_the_backend_needs_the_fused_kernels():
    with pytest.raises(ValueError, match=r"cupy-cuda13x"):
        g.TorchBackend(FakeTorch, "cuda", "int_mm")


def test_fp32_fallback_is_exact_at_the_worst_case(fused):
    b = make_backend(fused, "fp32")
    X = np.full((32, 4096), -128, dtype=np.int8).view(FT)
    W = np.full((4096, 64), -128, dtype=np.int8).view(FT)
    assert (np.asarray(b.matmul(X, W)) == 4096 * 16384).all()


def test_auto_falls_back_when_int_mm_is_missing(fused, capsys):
    b = make_backend(fused, "auto", NoIntMm)
    assert "fp32" in b.name
    assert "falling back" in capsys.readouterr().out
    with pytest.raises(RuntimeError):
        make_backend(fused, "int_mm", NoIntMm)             # asking for it explicitly is an error


def test_stages_compose_to_the_same_result(fused):
    b = make_backend(fused)
    W = b.build_dataset(P, EPOCH)
    seeds = [mh.attempt_seed(HEADER, n) for n in range(5)]
    from tenero import chacha
    keys = np.stack([chacha.key_words(s) for s in seeds])
    blocks = [mh.attempt_slice(s, P.num_blocks) for s in seeds]
    X = b.stage_generate(P, keys)
    assert np.asarray(X).shape == (5, P.m, P.k)
    Cs = b.stage_matmul(P, X, W, blocks)
    assert np.asarray(Cs).shape == (5, P.m, P.nb)
    staged = np.asarray(b.stage_fold(P, Cs))
    assert np.array_equal(staged, b.sums(P, W, keys, blocks))
    reference = mh.compute_attempts(P, mh.build_dataset(P, EPOCH), HEADER, range(5))
    assert [mh.mix_bytes(row) for row in staged] == [mix for _, mix in reference]


def test_the_random_dataset_for_sweeps_has_the_right_shape_and_differs_per_slice(fused):
    b = make_backend(fused)
    W = np.asarray(b.build_random_dataset(4, 128, 64))
    assert W.shape == (4, 128, 64) and W.dtype == np.int8
    assert len({W[i].tobytes() for i in range(4)}) == 4
    assert np.array_equal(W, np.asarray(b.build_random_dataset(4, 128, 64)))   # deterministic


# ---- the self-test ----

def test_self_test_passes_with_the_stand_in(fused, capsys):
    assert g.self_test(make_backend(fused, "auto"))
    out = capsys.readouterr().out
    assert "FAIL" not in out and out.count("PASS") >= 9


def test_self_test_fails_when_the_matmul_is_wrong(fused, capsys):
    class WrongMatmul(FakeTorch):
        @staticmethod
        def _int_mm(a, b):
            return (np.asarray(a).astype(np.int32) @ np.asarray(b).astype(np.int32) + 1).view(FT)

    assert not g.self_test(make_backend(fused, "int_mm", WrongMatmul))
    assert "FAIL" in capsys.readouterr().out


def test_self_test_fails_when_a_kernel_is_wrong(capsys):
    broken = g.KERNEL_SOURCE.replace("x[13] = slice_j;", "x[13] = 0u;", 1)
    assert broken != g.KERNEL_SOURCE
    d = pathlib.Path(tempfile.mkdtemp())
    try:
        f = g.FusedKernels(emu.FakeCupy(emu.build(d, broken)), FakeTorch, "cuda")
        assert not g.self_test(make_backend(f))
    finally:
        shutil.rmtree(d, ignore_errors=True)
    out = capsys.readouterr().out
    assert "FAIL" in out and "dataset built on the GPU" in out


@pytest.fixture
def windows_style_randint(monkeypatch):
    # on Windows numpy's RandomState.randint defaults to 32-bit integers, so any call that
    # asks for values above 2**31 without an explicit dtype crashes there (not on Linux)
    class WindowsRandomState(np.random.RandomState):
        def randint(self, low, high=None, size=None, dtype=None):
            return super().randint(low, high, size, np.int32 if dtype is None else dtype)

    monkeypatch.setattr(np.random, "RandomState", WindowsRandomState)


def test_the_windows_style_randint_really_fails_without_a_dtype(windows_style_randint):
    with pytest.raises(ValueError):
        np.random.RandomState(0).randint(0, 2**32, size=(2, 2))


def test_self_test_survives_32_bit_default_integers(fused, windows_style_randint):
    assert g.self_test(make_backend(fused, "auto"))


# ---- the searcher ----

def test_the_searcher_finds_a_valid_solution_and_counts_attempts(fused):
    s = g.GpuSearcher(make_backend(fused), batch=4)
    target = mh.bits_to_target(5)
    nonce, digest, mix, nxt = s.search(P, EPOCH, HEADER, target, 0)
    assert nxt == nonce + 1 and s.attempts >= nonce + 1
    assert mh.precheck(HEADER, nonce, mix, digest.hex(), target)
    assert mh.verify(P, mh.build_dataset(P, EPOCH), HEADER, nonce, target)


def test_the_gpu_and_cpu_searchers_find_the_same_first_nonce(fused):
    target = mh.bits_to_target(4)
    gpu = g.GpuSearcher(make_backend(fused), batch=4).search(P, EPOCH, HEADER, target, 0)
    cpu = powmod.CpuSearcher(batch=4).search(P, EPOCH, HEADER, target, 0)
    assert gpu == cpu


def test_the_searcher_gives_up_when_the_time_runs_out_and_says_where_to_resume(fused):
    s = g.GpuSearcher(make_backend(fused), batch=4)
    nonce, digest, mix, nxt = s.search(P, EPOCH, HEADER, 0, 100, seconds=0.0)   # target 0: never
    assert nonce is None and digest is None and mix is None
    assert nxt == 104                                     # one batch tried, resume after it


def test_the_dataset_is_built_once_per_epoch_and_rebuilt_when_it_changes(fused):
    logs = []
    s = g.GpuSearcher(make_backend(fused), batch=4, log=logs.append)
    target = mh.bits_to_target(3)
    s.search(P, EPOCH, HEADER, target, 0)
    s.search(P, EPOCH, HEADER, target, 100)
    assert s.dataset_builds == 1 and len(logs) == 1 and "dataset" in logs[0]
    s.search(P, b"the next epoch", HEADER, target, 0)
    assert s.dataset_builds == 2
    s.reset()
    s.search(P, b"the next epoch", HEADER, target, 0)
    assert s.dataset_builds == 3                          # reset forces a rebuild
    assert s.last_build_seconds >= 0


def test_a_chain_mined_on_the_emulated_gpu_validates_on_the_cpu(fused):
    params = mh.Params(m=8, k=64, nb=32, num_blocks=6)
    bc = Blockchain(target=2**256 // 16, initial_reward=50 * UNIT, halving_interval=10_000,
                    max_supply=10**9 * UNIT, tail_reward=0, difficulty_window=0,
                    pow=powmod.MatmulPow(params, epoch_blocks=2))
    searcher = g.GpuSearcher(make_backend(fused), batch=4)
    bc.pow.searcher = searcher
    for i in range(5):                                    # crosses two epoch changes
        bc.mine_block("c" * 40, timestamp=1_000_000 + 30 * (i + 1))
    assert searcher.dataset_builds == 3                   # epochs 0, 1 and 2
    assert bc.is_valid()                                  # the CPU agrees with every block
    assert all(len(b.mix) == 128 for b in bc.chain[1:])
