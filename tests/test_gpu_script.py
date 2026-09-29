"""The stages of gpu_pow_test.py and tenero/analysis.py, without a GPU (emulated kernels)."""
import argparse
import dataclasses
import pathlib
import shutil
import sys
import tempfile

import pytest

np = pytest.importorskip("numpy")

import gpu_pow_test as g  # noqa: E402
from tenero import analysis  # noqa: E402
from tenero import gpubackend as gb  # noqa: E402
from tenero import matmulhash as mh  # noqa: E402

from . import cuda_emulator as emu  # noqa: E402
from .fake_torch import FakeTorch  # noqa: E402
from .test_matmulhash import rebuild_cost as independent_rebuild_cost  # noqa: E402

P = mh.Params(m=32, k=256, nb=64, num_blocks=16)      # 16 slices of 16 KiB


@pytest.fixture(scope="module")
def dll():
    if emu.compiler() is None:
        pytest.skip("needs a C++ compiler (g++)")
    d = pathlib.Path(tempfile.mkdtemp())
    yield emu.build(d)
    shutil.rmtree(d, ignore_errors=True)


@pytest.fixture
def backend(dll):
    fused = gb.FusedKernels(emu.FakeCupy(dll), FakeTorch, "cuda")
    return gb.TorchBackend(FakeTorch, "cuda", "int_mm", fused=fused)


# ---- the dataset property the script relies on ----

def test_a_small_dataset_is_exactly_the_first_slices_of_a_bigger_one():
    small = mh.build_dataset(dataclasses.replace(P, num_blocks=4), g.EPOCH)
    big = mh.build_dataset(P, g.EPOCH)
    assert np.array_equal(small, big[:4])        # the fill does not depend on the slice count


# ---- full-size checks ----

def test_full_size_checks_pass_and_only_compare_the_first_slices(backend, capsys):
    ok, W, build = g.full_size_checks(backend, P, checked=4)
    out = capsys.readouterr().out
    assert ok and build >= 0 and np.asarray(W).shape == (16, 64, 256)
    assert "FAIL" not in out and out.count("PASS") >= 3
    assert "only the first 4 slices are compared" in out and "--full-cpu-check" in out


def test_full_size_checks_can_compare_every_slice(backend, capsys):
    ok, _, _ = g.full_size_checks(backend, P, checked=4, full_cpu=True)
    out = capsys.readouterr().out
    assert ok and "ALL 16 slices equal the CPU reference" in out


def test_full_size_checks_fail_when_the_gpu_dataset_is_wrong(backend, capsys):
    real = backend.build_dataset

    def corrupted(params, epoch, progress=None):
        W = real(params, epoch, progress)
        raw = np.asarray(W)
        raw[1, 0, 0] ^= 1                              # one flipped bit in slice 1
        return W

    backend.build_dataset = corrupted
    ok, _, _ = g.full_size_checks(backend, P, checked=4)
    out = capsys.readouterr().out
    assert not ok and "[FAIL] slice 1" in out


def test_attempts_are_chosen_from_the_checked_slices():
    nonces = g.slices_with_attempts(P, checked=4)
    assert len(nonces) == 4
    for n in nonces:
        assert mh.attempt_slice(mh.attempt_seed(g.HEADER, n), P.num_blocks) < 4


# ---- mine and verify ----

def solution(target_bits=5):
    data = mh.build_dataset(P, g.EPOCH)
    target = mh.bits_to_target(target_bits)
    nonce, digest, mix = mh.find_solution(P, data, g.HEADER, target)
    return nonce, digest, mix, target


def test_verify_with_prefix_accepts_a_true_solution_and_rejects_forgeries():
    nonce, digest, mix, target = solution()
    ok, seconds, note = g.verify_with_prefix(P, g.HEADER, nonce, digest, mix, target)
    assert ok and seconds >= 0 and "cheap check took" in note and "rebuilt slices 0.." in note
    import re
    assert re.search(r"using \d+\.\d+s of CPU time = \d+\.\d cores on average", note)
    forged_mix = bytes([mix[0] ^ 1]) + mix[1:]
    assert not g.verify_with_prefix(P, g.HEADER, nonce, digest, forged_mix, target)[0]
    assert not g.verify_with_prefix(P, g.HEADER, nonce + 1, digest, mix, target)[0]
    assert not g.verify_with_prefix(P, g.HEADER, nonce, digest, mix, 1)[0]   # above the target


def test_verify_with_prefix_catches_a_ground_fake_mix():
    # consistent (passes the cheap check) but never computed: only the recomputation objects
    import hashlib
    target = mh.bits_to_target(5)
    n = 0
    while True:
        fake = hashlib.sha256(n.to_bytes(8, "little")).digest() * 2
        digest = mh.digest_of(mh.attempt_seed(g.HEADER, n), fake)
        if mh.meets_target(digest, target):
            break
        n += 1
    assert mh.precheck(g.HEADER, n, fake, digest.hex(), target)
    assert not g.verify_with_prefix(P, g.HEADER, n, digest, fake, target)[0]


def test_mine_and_verify_end_to_end_on_the_emulated_gpu(backend, capsys):
    W = backend.build_dataset(P, g.EPOCH)
    good, seconds = g.mine_and_verify(
        lambda n: backend.attempts(P, W, g.HEADER, n),
        lambda nonce, digest, mix, target: g.verify_with_prefix(P, g.HEADER, nonce, digest, mix, target),
        5, 4, "GPU")
    out = capsys.readouterr().out
    assert good and "[4] MINE + VERIFY" in out and "PASS" in out and "FAIL" not in out


def test_search_reports_what_it_found_and_gives_up_on_time():
    data = mh.build_dataset(P, g.EPOCH)
    fn = lambda n: mh.compute_attempts(P, data, g.HEADER, n)       # noqa: E731
    nonce, digest, mix, attempts, dt = g.search(fn, mh.bits_to_target(4), 4)
    assert mh.meets_target(digest, mh.bits_to_target(4)) and attempts == nonce + 1
    none = g.search(fn, 0, 4, seconds=0.0)
    assert none[0] is None and none[3] == 4


# ---- benchmark, breakdown, memory ----

def test_stage_breakdown_accounts_for_every_stage(backend):
    W = backend.build_dataset(P, g.EPOCH)
    stages, total = g.stage_breakdown(backend, P, W, batch=4, batches=2)
    assert set(stages) == {"cpu_seeds", "generate", "matmul", "fold", "cpu_finalize"}
    assert all(v >= 0 for v in stages.values())
    assert total == pytest.approx(sum(stages.values()))


def test_bandwidth_measurement_runs_and_is_positive(backend):
    assert g.measure_bandwidth(backend, size_mib=1, reps=2, trials=2, warmup_s=0) > 0


def test_the_whole_flow_runs_end_to_end(backend, capsys, monkeypatch):
    monkeypatch.setattr(g, "measure_bandwidth", lambda backend, **kw: 5e11)
    ok, W, build = g.full_size_checks(backend, P, checked=4)
    assert ok
    rate = g.benchmark(backend, P, W, 0.2, 8, build_seconds=max(build, 1e-3))
    assert rate > 0
    g.memory_summary(FakeTorch, P, 8)
    out = capsys.readouterr().out
    for marker in ("[3] FULL SIZE", "[5] BENCHMARK", "[6] MEMORY HARDNESS", "[7] MEMORY", "PASS",
                   "ChaCha20", "rebuild one missing slice"):
        assert marker in out
    assert "FAIL" not in out


def test_the_memory_hardness_report_uses_the_measured_times(capsys):
    table = {0.9: 1.5, 0.7: 5.0, 0.5: 20.0}
    logged = []
    out = g.memory_hardness_report(P, build_seconds=0.16, t_attempt=0.0005, table=table,
                                   log=logged.append)
    text = "\n".join(logged)
    assert out["t_slice"] == pytest.approx(0.16 / 16)
    assert out["prefix_seconds"] == pytest.approx(0.16 / 2)          # half the slices on average
    assert "rebuilding ONE slice from nothing" in text and "not a proof" in text
    slow = {f: analysis.estimated_slowdown(f, c, out["t_slice"], 0.0005) for f, c in table.items()}
    assert slow[0.9] < slow[0.7] < slow[0.5]
    assert out["within"] is None                                       # even 90% costs more than 2x here
    fast = g.memory_hardness_report(P, 0.0016, 0.0005, table, log=lambda *a: None)
    assert fast["within"] == 0.5 or fast["within"] is not None


def test_memory_summary_reports_headroom(capsys):
    g.memory_summary(FakeTorch, P, 8)
    out = capsys.readouterr().out
    assert "[7] MEMORY" in out and "OK" in out


# ---- CPU-only mode and the entry point ----

def test_cpu_only_mode_runs(capsys):
    args = argparse.Namespace(m=32, difficulty_bits=4, seconds=0.2)
    assert g.run_cpu_only(args) == 0
    out = capsys.readouterr().out
    assert "PASS" in out and "attempts/s" in out and "FAIL" not in out


def test_main_refuses_without_pytorch(monkeypatch, capsys):
    monkeypatch.setattr(sys, "argv", ["gpu_pow_test.py"])
    monkeypatch.setitem(sys.modules, "torch", None)
    assert g.main() == 1
    assert "PyTorch is not installed" in capsys.readouterr().out


def test_main_rejects_too_few_rows(monkeypatch, capsys):
    monkeypatch.setattr(sys, "argv", ["gpu_pow_test.py", "--m", "8"])
    assert g.main() == 1
    assert "greater than 16" in capsys.readouterr().out


def test_the_defaults_are_the_documented_ones():
    p = mh.Params()
    assert (p.m, p.k, p.nb, p.num_blocks) == (64, 8192, 2048, 256)
    assert p.slice_bytes == 16 * 2**20 and p.dataset_bytes == 4 * 2**30


def test_raw_slice_words_are_the_cpu_words(backend):
    W = backend.build_dataset(P, g.EPOCH)
    cpu = mh.build_dataset(P, g.EPOCH)
    for b in (0, 5, 15):
        assert np.array_equal(g.raw_slice_words(W, b), cpu[b])


# ---- the analysis module ----

def test_rebuild_cost_agrees_with_an_independent_version():
    params, data = analysis.small_dataset(num_slices=32, blocks=32)
    import random
    rng = random.Random(3)
    for _ in range(10):
        stored = set(rng.sample(range(1, 32), 20)) | {0}
        b = rng.choice([x for x in range(1, 32) if x not in stored])
        assert analysis.rebuild_cost(data, params, stored, b) == independent_rebuild_cost(data, params, stored, b)


def test_a_stored_slice_costs_nothing_to_rebuild():
    params, data = analysis.small_dataset(num_slices=16, blocks=16)
    assert analysis.rebuild_cost(data, params, set(range(16)), 7) == 0
    assert analysis.rebuild_cost(data, params, {0}, 1) == 1.0            # only slice 0 is free


def test_the_cost_table_climbs_as_less_is_stored():
    table = analysis.rebuild_cost_table(fractions=(0.9, 0.7, 0.5), num_slices=64, blocks=16, trials=6)
    assert table[0.9] < table[0.7] < table[0.5]
    assert table[0.9] >= 1.0                                            # a missing slice costs at least itself
    assert table == analysis.rebuild_cost_table(fractions=(0.9, 0.7, 0.5), num_slices=64,
                                                blocks=16, trials=6)     # deterministic


def test_estimated_slowdown_and_the_within_helper():
    assert analysis.estimated_slowdown(1.0, 99.0, 1.0, 1.0) == 1.0     # nothing missing, nothing to pay
    assert analysis.estimated_slowdown(0.8, 5.0, 2.0, 1.0) == pytest.approx(1 + 0.2 * 5 * 2)
    table = {0.9: 1.0, 0.7: 3.0, 0.5: 20.0}
    assert analysis.fraction_within(table, 1.0, 1.0, factor=2.0) == 0.7   # 1 + 0.3*3 = 1.9
    assert analysis.fraction_within(table, 1.0, 1.0, factor=1.05) is None
    assert analysis.fraction_within({}, 1.0, 1.0) is None


def test_the_script_sets_its_cpu_thread_count(monkeypatch, capsys):
    monkeypatch.setattr(mh, "BUILD_THREADS", 1)
    monkeypatch.setattr(sys, "argv", ["gpu_pow_test.py", "--cpu-threads", "3", "--m", "8"])
    assert g.main() == 1                                  # refused (m too small), after setting it
    assert mh.BUILD_THREADS == 3


def test_the_bat_file_caps_the_cpu_and_lets_the_caller_override_it():
    text = open("gpu_test.bat", newline="").read()
    assert "\r\n" in text                                        # Windows line endings
    for var in ("OPENBLAS_NUM_THREADS", "OMP_NUM_THREADS", "MKL_NUM_THREADS"):
        assert f"if not defined {var} set {var}=6" in text          # a value set outside wins
    assert "--cpu-threads 6 %*" in text                            # extra options come last
    assert text.count("--cpu-threads 6 %*") == 2                   # both the .venv and plain python
