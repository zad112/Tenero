"""Tests for the --sweep logic in gpu_pow_test.py, without a GPU.

The timing numbers here are meaningless (a numpy stand-in plays torch). These tests check
the bookkeeping: every shape/layout/rows combination is measured, unsupported layouts are
skipped cleanly, the best configuration is chosen correctly, and the compare mode behaves
sensibly. Real numbers come from `python gpu_pow_test.py --sweep`.
"""
import pathlib
import shutil
import tempfile

import pytest

np = pytest.importorskip("numpy")

import gpu_pow_test as g  # noqa: E402
from tenero import gpubackend as gb  # noqa: E402
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
def backend_for(dll):
    def make(torch=None):
        fused = gb.FusedKernels(emu.FakeCupy(dll), FakeTorch, "cuda")
        return gb.TorchBackend(torch or FakeTorch, "cuda", "int_mm", fused=fused)
    return make


class NoTransposedMm(FakeTorch):
    """A build whose int8 matmul only accepts contiguous inputs."""

    @staticmethod
    def _int_mm(a, b):
        if not np.asarray(b).flags["C_CONTIGUOUS"]:
            raise RuntimeError("mat2 must be contiguous")
        return FakeTorch._int_mm(a, b)


def fake_result(m, pct_bw, pct_peak=50.0, k=64, nb=64, layout="row", dt=1e-4):
    return dict(k=k, nb=nb, layout=layout, m=m, dt=dt, gbs=1.0, tops=1.0,
                pct_bw=pct_bw, pct_peak=pct_peak, regime=g.classify(pct_bw, pct_peak))


def test_classify():
    assert g.classify(80, 10) == "memory-bound"
    assert g.classify(60, 99) == "memory-bound"
    assert g.classify(30, 70) == "compute-bound"
    assert g.classify(15, 12).startswith("neither")


def test_verdict_uses_the_measured_bandwidth():
    assert "memory-bound" in g.verdict(0.85) and "NOT" not in g.verdict(0.85)
    text = g.verdict(0.18)
    assert "NOT memory-bound" in text and "18%" in text and "--sweep" in text


def test_pick_best_prefers_bandwidth_then_fewer_rows():
    results = [fake_result(32, 30), fake_result(64, 72), fake_result(128, 72),
               fake_result(256, 55)]
    assert g.pick_best(results)["m"] == 64            # ties on bandwidth go to fewer rows
    assert g.pick_best([]) is None


def test_sweep_measures_every_combination(backend_for):
    b = backend_for()
    shapes = [(64, 32), (32, 64)]
    results, skipped = g.run_sweep(b, bw=5e11, peak=100.0, shapes=shapes, ms=(32, 48),
                                   gib=0.0, reps=3, log=lambda *a: None)
    assert skipped == []
    assert len(results) == len(shapes) * 2 * 2       # shapes x layouts x rows
    assert {r["layout"] for r in results} == {"row", "col"}
    assert {r["m"] for r in results} == {32, 48}
    for r in results:
        assert r["dt"] > 0 and r["tops"] > 0 and r["gbs"] > 0
        assert r["regime"] in ("memory-bound", "compute-bound", "neither (kernel/latency)")


def test_unsupported_layout_is_skipped_not_fatal(backend_for):
    b = backend_for(NoTransposedMm)
    results, skipped = g.run_sweep(b, bw=5e11, peak=100.0, shapes=[(64, 32)], ms=(32,),
                                   gib=0.0, reps=3, log=lambda *a: None)
    assert [r["layout"] for r in results] == ["row"]
    assert len(skipped) == 1 and skipped[0][2] == "col"
    assert "RuntimeError" in skipped[0][3]


def test_report_lists_everything_and_names_the_best(capsys):
    results = [fake_result(32, 20), fake_result(64, 75, layout="col")]
    best = g.report_sweep(results, [(8, 8, "col", "RuntimeError: nope")], 100.0, 5e11)
    out = capsys.readouterr().out
    assert best["m"] == 64 and best["layout"] == "col"
    assert "BEST" in out and "not supported here" in out and "balance point" in out
    assert "memory-bound" in out


def test_report_warns_when_nothing_reaches_the_bandwidth(capsys):
    g.report_sweep([fake_result(32, 18), fake_result(64, 22)], [], 100.0, 5e11)
    assert "nothing streams near the bandwidth" in capsys.readouterr().out


def test_report_handles_no_results(capsys):
    assert g.report_sweep([], [], 100.0, 5e11) is None


def test_cpu_verify_timing_really_runs():
    assert g.time_cpu_verify(64, 32, 32) > 0


def test_peak_tops_ignores_unsupported_layouts(backend_for):
    b = backend_for(NoTransposedMm)
    assert g.measure_peak_tops(b, size=64, reps=2) > 0   # the contiguous layout still counts


def test_the_default_sweep_shapes_are_valid_for_the_gpu():
    for k, nb in g.SWEEP_SHAPES:
        assert k % 8 == 0 and nb % 8 == 0
        mh.Params(m=32, k=k, nb=nb).validate()
    assert all(m > 16 for m in g.SWEEP_MS)


def test_sweep_main_runs_end_to_end(backend_for, capsys, monkeypatch):
    import argparse
    monkeypatch.setattr(g, "measure_bandwidth", lambda backend, **kw: 5e11)
    monkeypatch.setattr(g, "measure_peak_tops", lambda backend, **kw: 100.0)
    monkeypatch.setattr(g, "SWEEP_SHAPES", [(64, 32), (32, 64)])
    args = argparse.Namespace(sweep_ms="32,48", sweep_gib=0.0, sweep_reps=3)
    b = backend_for()
    assert g.sweep_main(b, args) == 0
    out = capsys.readouterr().out
    for marker in ("[3] SWEEP", "[4] SWEEP", "BEST", "to run the full test with the best shape"):
        assert marker in out
    args.sweep_ms = "8"                       # GPU int8 matmul needs more than 16 rows
    assert g.sweep_main(b, args) == 1


def test_rescore_fixes_a_low_bandwidth_reading():
    # the copy test read 388 GB/s, but a configuration streamed 819 GB/s
    results = [fake_result(32, 0.0, pct_peak=15), fake_result(64, 0.0, pct_peak=30)]
    results[0]["gbs"], results[1]["gbs"] = 819.0, 400.0
    fixed, bw = g.rescore(results, 388e9, 350.0)
    assert bw == pytest.approx(819e9)
    assert fixed[0]["pct_bw"] == pytest.approx(100.0)          # never above 100%
    assert fixed[1]["pct_bw"] == pytest.approx(100 * 400 / 819)
    assert all(r["pct_bw"] <= 100.001 for r in fixed)
    assert fixed[0]["regime"] == "memory-bound"


def test_rescore_leaves_a_sensible_reading_alone():
    results = [fake_result(32, 0.0, pct_peak=15)]
    results[0]["gbs"] = 700.0
    _, bw = g.rescore(results, 859e9, 350.0)
    assert bw == 859e9
    assert g.rescore([], 859e9, 350.0) == ([], 859e9)


def test_compare_m_measures_each_value_and_reports(backend_for, capsys, monkeypatch):
    monkeypatch.setattr(g, "measure_bandwidth", lambda backend, **kw: 8.59e11)
    monkeypatch.setattr(g, "time_cpu_verify", lambda k, nb, m: 1.0 + m / 1000)
    p = mh.Params(m=32, k=256, nb=64, num_blocks=4)
    b = backend_for()
    logged = []
    rows = g.compare_m(b, p, [32, 48, 64], 0.1, 8, verify=True, log=logged.append)
    assert [r["m"] for r in rows] == [32, 48, 64]
    for r in rows:
        assert r["rate"] > 0 and r["tops"] > 0 and r["matmul_us"] > 0
        assert r["verify"] == pytest.approx(1.0 + r["m"] / 1000)
    # effective TOPS scales with rows for the same attempt rate: ops = 2*m*k*nb
    assert rows[0]["tops"] == pytest.approx(rows[0]["rate"] * p.ops_per_attempt() / 1e12)
    text = "\n".join(logged)
    assert "effective TOPS" in text and "POW_M" in text and "within 15%" in text


def test_compare_m_can_skip_cpu_verification(backend_for, monkeypatch):
    monkeypatch.setattr(g, "measure_bandwidth", lambda backend, **kw: 8.59e11)
    p = mh.Params(m=32, k=256, nb=64, num_blocks=4)
    b = backend_for()
    rows = g.compare_m(b, p, [32], 0.1, 8, verify=False, log=lambda *a: None)
    assert rows[0]["verify"] is None


def test_compare_main_rejects_too_few_rows(backend_for, capsys):
    import argparse
    args = argparse.Namespace(compare_m="8,64", dataset_gib=0.0, k=256, nb=64,
                              seconds=0.1, batch=4, no_cpu_verify=True)
    b = backend_for()
    assert g.compare_main(b, args) == 1
    assert "greater than 16" in capsys.readouterr().out


def test_compare_main_runs(backend_for, capsys, monkeypatch):
    import argparse
    monkeypatch.setattr(g, "measure_bandwidth", lambda backend, **kw: 8.59e11)
    args = argparse.Namespace(compare_m="32,48", dataset_gib=0.0, k=256, nb=64,
                              seconds=0.1, batch=4, no_cpu_verify=True)
    b = backend_for()
    assert g.compare_main(b, args) == 0
    assert "COMPARE rows per attempt" in capsys.readouterr().out
