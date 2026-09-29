"""Tests for miner.py: new chains, the GPU path (through the numpy stand-in), refusals."""
import json
import sys
import types

import pytest

np = pytest.importorskip("numpy")

import miner  # noqa: E402
from tenero import config  # noqa: E402
from tenero import chain as chain_module  # noqa: E402
from tenero import gpubackend as gb  # noqa: E402
from tenero.chain import Blockchain  # noqa: E402
from tenero.mempool import Mempool  # noqa: E402

from . import cuda_emulator as emu  # noqa: E402
from .fake_torch import FakeTorch  # noqa: E402

ADDRESS = "c" * 40


@pytest.fixture(scope="module")
def dll():
    import pathlib
    import shutil
    import tempfile
    if emu.compiler() is None:
        pytest.skip("needs a C++ compiler (g++)")
    d = pathlib.Path(tempfile.mkdtemp())
    yield emu.build(d)
    shutil.rmtree(d, ignore_errors=True)


@pytest.fixture
def gpu_searcher(dll):
    """A real GpuSearcher whose CUDA kernels run on the CPU through the emulator."""
    fused = gb.FusedKernels(emu.FakeCupy(dll), FakeTorch, "cuda")
    return gb.GpuSearcher(gb.TorchBackend(FakeTorch, "cuda", "int_mm", fused=fused), batch=4)


def fake_cuda_torch():
    class FakeDevice:
        pass

    class FakeProps:
        name, total_memory = "Fake GPU", 8 * 2**30

    return types.SimpleNamespace(
        cuda=types.SimpleNamespace(is_available=lambda: True,
                                   get_device_properties=lambda i: FakeProps()),
        device=lambda kind: FakeDevice(),
        backends=types.SimpleNamespace(
            cuda=types.SimpleNamespace(matmul=types.SimpleNamespace(allow_tf32=True))))


@pytest.fixture
def sandbox(tmp_path, monkeypatch):
    # point the miner at a scratch folder and tiny, instant settings
    chain_path = str(tmp_path / "chain.json")
    monkeypatch.setattr(Blockchain.load_or_new.__func__, "__defaults__", (chain_path, None, None))
    monkeypatch.setattr(Blockchain.save, "__defaults__", (chain_path,))
    monkeypatch.setattr(Mempool.__init__, "__defaults__", (str(tmp_path / "mempool.json"),))
    monkeypatch.setattr(chain_module, "DEFAULT_TARGET", 2**250)
    monkeypatch.setattr(chain_module, "MATMUL_START_ATTEMPTS", 8)
    monkeypatch.setattr(config, "POW_DATASET_GIB", 0.00002)
    monkeypatch.setattr(config, "POW_M", 8)
    monkeypatch.setattr(config, "POW_K", 64)
    monkeypatch.setattr(config, "POW_NB", 32)
    monkeypatch.setattr(config, "POW_EPOCH_BLOCKS", 2)
    monkeypatch.setattr(miner, "BASELINE_SECONDS", 0.05)
    return chain_path


def saved(path):
    return Blockchain.load(path)


def test_mines_a_new_sha256_chain(sandbox, capsys):
    assert miner.main([ADDRESS, "--blocks", "2", "--pow", "sha256"]) == 0
    out = capsys.readouterr().out
    assert "SHA-256" in out and "found block 2" in out
    bc = saved(sandbox)
    assert len(bc.chain) == 3 and bc.pow.name == "sha256" and bc.is_valid()


def test_mines_a_matmul_chain_on_the_cpu(sandbox, capsys):
    assert miner.main([ADDRESS, "--blocks", "3", "--cpu"]) == 0
    out = capsys.readouterr().out
    assert "matmul int8" in out and "dataset epoch" in out and "CPU search" in out
    bc = saved(sandbox)
    assert bc.pow.name == "matmul" and len(bc.chain) == 4
    assert bc.is_valid()                                   # every block re-checked by the CPU
    assert bc.balance_of(ADDRESS) > 0


def test_mines_through_an_injected_gpu_searcher(gpu_searcher, sandbox, capsys):
    searcher = gpu_searcher
    assert miner.main([ADDRESS, "--blocks", "3"], searcher=searcher) == 0
    out = capsys.readouterr().out
    assert "attempts" in out and "double-check runs in the background" in out
    assert "double-checked by the CPU" in out and "and saved" in out
    assert searcher.attempts > 0
    assert saved(sandbox).is_valid()


def test_the_double_check_can_be_skipped(gpu_searcher, sandbox, capsys):
    searcher = gpu_searcher
    miner.main([ADDRESS, "--blocks", "1", "--no-double-check"], searcher=searcher)
    assert "double-check" not in capsys.readouterr().out


def test_a_later_run_continues_the_same_chain(sandbox):
    miner.main([ADDRESS, "--blocks", "2", "--cpu"])
    miner.main([ADDRESS, "--blocks", "2", "--cpu"])
    bc = saved(sandbox)
    assert len(bc.chain) == 5 and bc.is_valid()
    assert [b.index for b in bc.chain] == [0, 1, 2, 3, 4]


def test_an_existing_chain_keeps_its_own_proof_of_work(sandbox, capsys):
    miner.main([ADDRESS, "--blocks", "1", "--pow", "sha256"])
    miner.main([ADDRESS, "--blocks", "1", "--pow", "matmul"])   # ignored: the chain is SHA-256
    bc = saved(sandbox)
    assert bc.pow.name == "sha256" and len(bc.chain) == 3 and bc.is_valid()


def test_a_bad_address_is_refused(sandbox, capsys):
    assert miner.main(["not-an-address", "--blocks", "1"]) == 1
    assert "not a valid address" in capsys.readouterr().out


def test_a_matmul_chain_without_a_gpu_is_refused_clearly(sandbox, monkeypatch, capsys):
    monkeypatch.setitem(sys.modules, "torch", None)             # `import torch` fails
    assert miner.main([ADDRESS, "--blocks", "1"]) == 1
    out = capsys.readouterr().out
    assert "PyTorch is not installed" in out and "needs a GPU miner" in out
    import os
    assert not os.path.exists(sandbox)                          # nothing was written


def test_torch_without_cuda_is_refused_clearly(sandbox, monkeypatch, capsys):
    no_cuda = types.SimpleNamespace(cuda=types.SimpleNamespace(is_available=lambda: False))
    monkeypatch.setitem(sys.modules, "torch", no_cuda)
    assert miner.main([ADDRESS, "--blocks", "1"]) == 1
    assert "cannot see a CUDA GPU" in capsys.readouterr().out


def test_a_gpu_that_fails_the_check_is_refused(sandbox, monkeypatch, capsys):
    monkeypatch.setitem(sys.modules, "torch", fake_cuda_torch())
    monkeypatch.setattr(gb, "make_fused", lambda torch, device: None)
    monkeypatch.setattr(gb, "TorchBackend",
                        lambda *a, **k: types.SimpleNamespace(name="stub", fused=None))
    monkeypatch.setattr(gb, "self_test", lambda backend, title="": False)
    assert miner.main([ADDRESS, "--blocks", "1"]) == 1
    out = capsys.readouterr().out
    assert "disagrees with the CPU reference" in out


def test_a_missing_cupy_is_explained_with_the_install_command(sandbox, monkeypatch, capsys):
    monkeypatch.setitem(sys.modules, "torch", fake_cuda_torch())
    monkeypatch.setitem(sys.modules, "cupy", None)                # `import cupy` fails
    assert miner.main([ADDRESS, "--blocks", "1"]) == 1
    out = capsys.readouterr().out
    assert "needs CuPy" in out and 'pip install "cupy-cuda13x[ctk]"' in out
    import os
    assert not os.path.exists(sandbox)                            # nothing was written


def test_the_old_kernels_option_is_gone(sandbox):
    with pytest.raises(SystemExit):
        miner.main([ADDRESS, "--blocks", "1", "--kernels", "torch"])


class LyingSearcher:
    """Always claims a perfect digest: the CPU double-check must reject every block."""
    attempts = 0
    resets = 0

    def search(self, params, seed, header_hash, target, start, seconds=None):
        return start, b"\x00" * 32, b"\x00" * 64, start + 1

    def reset(self):
        self.resets += 1


class FlakySearcher:
    """Lies on its first `lies` searches, then behaves (a GPU that glitches once)."""
    attempts = 0

    def __init__(self, lies):
        from tenero.pow import CpuSearcher
        self.lies, self.real, self.resets = lies, CpuSearcher(), 0

    def search(self, params, seed, header_hash, target, start, seconds=None):
        if self.lies > 0:
            self.lies -= 1
            return start, b"\x00" * 32, b"\x00" * 64, start + 1
        return self.real.search(params, seed, header_hash, target, start, seconds)

    def reset(self):
        self.resets += 1


def test_repeated_gpu_faults_stop_the_miner_cleanly_and_save_nothing(sandbox, capsys):
    searcher = LyingSearcher()
    assert miner.main([ADDRESS, "--blocks", "1"], searcher=searcher) == 3
    out = capsys.readouterr().out
    assert "WARNING" in out and "3 in a row" in out and "stopping" in out
    assert "overclock" in out
    assert searcher.resets == 2                    # reset after the 1st and 2nd fault
    import os
    assert not os.path.exists(sandbox)


def test_one_gpu_fault_is_survived(sandbox, capsys):
    searcher = FlakySearcher(lies=1)
    assert miner.main([ADDRESS, "--blocks", "2"], searcher=searcher) == 0
    out = capsys.readouterr().out
    assert out.count("WARNING") == 1 and "retrying with a fresh dataset" in out
    assert searcher.resets == 1
    bc = saved(sandbox)
    assert len(bc.chain) == 3 and bc.is_valid()    # the faulty block was never kept


def test_the_fault_count_is_of_faults_in_a_row(sandbox, capsys):
    # two faults, a success, two more faults: never three in a row, so it keeps going
    class Pattern:
        attempts = 0

        def __init__(self):
            from tenero.pow import CpuSearcher
            self.real, self.calls, self.resets = CpuSearcher(), 0, 0

        def search(self, params, seed, header_hash, target, start, seconds=None):
            self.calls += 1
            if self.calls in (1, 2, 4, 5):
                return start, b"\x00" * 32, b"\x00" * 64, start + 1
            return self.real.search(params, seed, header_hash, target, start, seconds)

        def reset(self):
            self.resets += 1

    assert miner.main([ADDRESS, "--blocks", "2"], searcher=Pattern()) == 0
    assert saved(sandbox).is_valid()


def test_the_miner_shows_effective_tops_and_explains_the_units(gpu_searcher, sandbox, capsys):
    searcher = gpu_searcher
    miner.main([ADDRESS, "--blocks", "1"], searcher=searcher)
    out = capsys.readouterr().out
    assert "TOPS" in out and "GB/s" in out
    assert "billion int8 operations" in out and "not comparable with SHA-256" in out


def test_the_epoch_length_of_a_new_chain_can_be_chosen(sandbox, capsys):
    miner.main([ADDRESS, "--blocks", "5", "--cpu", "--epoch-blocks", "2"])
    out = capsys.readouterr().out
    assert "rebuilt every 2 blocks" in out
    assert "dataset epoch 0" in out and "dataset epoch 1" in out and "dataset epoch 2" in out
    bc = saved(sandbox)
    assert bc.pow.epoch_blocks == 2 and bc.is_valid()


def test_an_existing_chain_ignores_a_new_epoch_length(sandbox):
    miner.main([ADDRESS, "--blocks", "1", "--cpu", "--epoch-blocks", "2"])
    miner.main([ADDRESS, "--blocks", "1", "--cpu", "--epoch-blocks", "50"])
    assert saved(sandbox).pow.epoch_blocks == 2


def test_the_scratch_folder_is_announced(sandbox, monkeypatch, capsys):
    from tenero import paths
    monkeypatch.setattr(paths, "DATA_DIR", "/somewhere/scratch")
    miner.main([ADDRESS, "--blocks", "1", "--pow", "sha256"])
    assert "scratch data folder: /somewhere/scratch" in capsys.readouterr().out


def test_saved_chain_records_the_algorithm(sandbox):
    miner.main([ADDRESS, "--blocks", "1", "--cpu"])
    params = json.load(open(sandbox))["params"]
    assert params["pow"]["name"] == "matmul" and params["pow"]["epoch_blocks"] == 2



# ---------------- overlapping the CPU check with the next search ----------------

import threading  # noqa: E402

from tenero.pow import CpuSearcher, GpuFault, MatmulPow  # noqa: E402


class CountingSearcher(CpuSearcher):
    """A CPU searcher that counts (and can observe) each search call."""

    def __init__(self, on_call=None):
        super().__init__()
        self.calls = 0
        self.on_call = on_call

    def search(self, params, seed, header_hash, target, start, seconds=None):
        self.calls += 1
        if self.on_call:
            self.on_call(self.calls)
        return super().search(params, seed, header_hash, target, start, seconds)


def test_the_next_search_starts_before_the_previous_check_finishes(sandbox, monkeypatch):
    second_search_started = threading.Event()
    real_check = MatmulPow.check

    def gated_check(self, block):
        if block.index == 1:
            # block 1's check cannot finish until the search for block 2 has begun
            assert second_search_started.wait(10), \
                "block 2's search never started while block 1 was being checked"
        return real_check(self, block)

    monkeypatch.setattr(MatmulPow, "check", gated_check)
    searcher = CountingSearcher(lambda n: n >= 2 and second_search_started.set())
    assert miner.main([ADDRESS, "--blocks", "2"], searcher=searcher) == 0
    bc = saved(sandbox)
    assert len(bc.chain) == 3 and bc.is_valid()


def test_the_file_never_holds_an_unchecked_block(sandbox, monkeypatch):
    import os
    started = threading.Event()
    seen = []
    real_check = MatmulPow.check

    def spying_check(self, block):
        started.wait(10)
        on_disk = len(json.load(open(sandbox))["chain"]) - 1 if os.path.exists(sandbox) else 0
        seen.append((block.index, on_disk))     # blocks on disk while THIS block is unchecked
        return real_check(self, block)

    monkeypatch.setattr(MatmulPow, "check", spying_check)
    searcher = CountingSearcher(lambda n: n >= 2 and started.set())
    assert miner.main([ADDRESS, "--blocks", "3"], searcher=searcher) == 0
    assert seen == [(1, 0), (2, 1), (3, 2)]     # only checked blocks are ever saved
    assert saved(sandbox).is_valid()


def test_at_most_one_block_is_unchecked_at_a_time(sandbox, monkeypatch):
    # block 2 must not start its own check until block 1's has finished and been saved
    order = []
    real_check = MatmulPow.check

    def logging_check(self, block):
        order.append(f"check-start-{block.index}")
        result = real_check(self, block)
        order.append(f"check-end-{block.index}")
        return result

    monkeypatch.setattr(MatmulPow, "check", logging_check)
    miner.main([ADDRESS, "--blocks", "3"], searcher=CountingSearcher())
    assert order == ["check-start-1", "check-end-1", "check-start-2", "check-end-2",
                     "check-start-3", "check-end-3"]


def test_no_search_is_wasted_beyond_the_last_block(sandbox):
    for blocks in (1, 3):
        import os
        if os.path.exists(sandbox):
            os.remove(sandbox)
        searcher = CountingSearcher()
        assert miner.main([ADDRESS, "--blocks", str(blocks)], searcher=searcher) == 0
        assert searcher.calls == blocks                 # no speculative search after the last
        assert len(saved(sandbox).chain) == blocks + 1


def test_a_failed_check_also_discards_the_block_built_on_top_of_it(sandbox, capsys):
    class LiesOnce(CpuSearcher):
        def __init__(self):
            super().__init__()
            self.calls, self.resets = 0, 0

        def search(self, params, seed, header_hash, target, start, seconds=None):
            self.calls += 1
            if self.calls == 1:                          # block 1 is bad ...
                return start, b"\x00" * 32, b"\x00" * 64, start + 1   # ... and block 2 is then built on it
            return super().search(params, seed, header_hash, target, start, seconds)

        def reset(self):
            self.resets += 1

    searcher = LiesOnce()
    assert miner.main([ADDRESS, "--blocks", "2"], searcher=searcher) == 0
    out = capsys.readouterr().out
    assert out.count("WARNING") == 1 and searcher.resets == 1
    bc = saved(sandbox)
    assert len(bc.chain) == 3 and bc.is_valid()          # nothing built on the bad block survived


def test_a_failed_check_aborts_the_search_on_top_of_it(sandbox, capsys):
    class StallsOnTopOfABadBlock(CpuSearcher):
        """Block 1 is bad. The search on top of it never finds anything, so the only way out
        is the miner noticing the failed check between chunks."""

        def __init__(self):
            super().__init__()
            self.calls, self.stalled, self.repaired = 0, 0, False

        def search(self, params, seed, header_hash, target, start, seconds=None):
            self.calls += 1
            if self.calls == 1:
                return start, b"\x00" * 32, b"\x00" * 64, start + 1
            if not self.repaired:
                self.stalled += 1
                import time as _t
                _t.sleep(0.01)
                return None, None, None, start + 1
            return super().search(params, seed, header_hash, target, start, seconds)

        def reset(self):
            self.repaired = True

    searcher = StallsOnTopOfABadBlock()
    # two blocks, so a speculative search on top of the (bad) first block really happens
    assert miner.main([ADDRESS, "--blocks", "2"], searcher=searcher) == 0
    assert 1 <= searcher.stalled < 200                    # aborted promptly, not stuck forever
    assert "WARNING" in capsys.readouterr().out
    bc = saved(sandbox)
    assert len(bc.chain) == 3 and bc.is_valid()


def test_ctrl_c_finishes_the_pending_check_and_saves_that_block(sandbox, capsys):
    class InterruptedDuringTheSecondSearch(CountingSearcher):
        def search(self, params, seed, header_hash, target, start, seconds=None):
            if self.calls == 1:
                raise KeyboardInterrupt
            return super().search(params, seed, header_hash, target, start, seconds)

    assert miner.main([ADDRESS], searcher=InterruptedDuringTheSecondSearch()) == 0
    out = capsys.readouterr().out
    assert "finishing the CPU check of block 1" in out
    assert "block 1 double-checked by the CPU" in out and "miner stopped" in out
    bc = saved(sandbox)
    assert len(bc.chain) == 2 and bc.is_valid()           # the found block was not lost


def test_a_second_ctrl_c_skips_the_check_and_saves_nothing_unchecked(sandbox, capsys, monkeypatch):
    class InterruptedDuringTheSecondSearch(CountingSearcher):
        def search(self, params, seed, header_hash, target, start, seconds=None):
            if self.calls == 1:
                raise KeyboardInterrupt
            return super().search(params, seed, header_hash, target, start, seconds)

    def interrupted_wait(self):
        raise KeyboardInterrupt

    monkeypatch.setattr(miner.BackgroundCheck, "result", interrupted_wait)
    assert miner.main([ADDRESS], searcher=InterruptedDuringTheSecondSearch()) == 0
    out = capsys.readouterr().out
    assert "NOT saved" in out and "miner stopped" in out
    import os
    assert not os.path.exists(sandbox)


def test_no_overlap_checks_each_block_before_starting_the_next(sandbox, monkeypatch, capsys):
    searcher = CountingSearcher()
    real_check = MatmulPow.check

    def strict_check(self, block):
        # in this mode block N is fully checked before the search for N+1 begins
        assert searcher.calls == block.index
        return real_check(self, block)

    monkeypatch.setattr(MatmulPow, "check", strict_check)
    assert miner.main([ADDRESS, "--blocks", "3", "--no-overlap"], searcher=searcher) == 0
    out = capsys.readouterr().out
    assert "no overlap" in out and "while the GPU searches" not in out
    assert saved(sandbox).is_valid()


def test_sha256_chains_never_overlap(sandbox, capsys):
    miner.main([ADDRESS, "--blocks", "2", "--pow", "sha256"])
    out = capsys.readouterr().out
    assert "overlap" not in out and "background" not in out


def test_no_double_check_means_no_checking_and_no_overlap(sandbox, capsys):
    miner.main([ADDRESS, "--blocks", "2", "--no-double-check"],
               searcher=CountingSearcher())
    out = capsys.readouterr().out
    assert "double-check" not in out and "overlap" not in out
    assert saved(sandbox).is_valid()


def test_overlapped_mining_works_across_epoch_changes(sandbox, capsys):
    assert miner.main([ADDRESS, "--blocks", "6", "--cpu", "--epoch-blocks", "2"]) == 0
    out = capsys.readouterr().out
    for epoch in (0, 1, 2):
        assert f"dataset epoch {epoch}" in out
    bc = saved(sandbox)
    assert len(bc.chain) == 7 and bc.is_valid()


def test_transactions_are_never_mined_twice_when_blocks_overlap(sandbox):
    from tenero.mempool import Mempool
    from tenero.transaction import Transaction
    from tenero.units import UNIT
    from tenero.wallet import Wallet
    me, friend = Wallet(), Wallet()
    miner.main([me.address, "--blocks", "1", "--cpu"])         # gives `me` a block reward
    tx = Transaction(me.address, friend.address, 5 * UNIT, fee=UNIT)
    tx.sign(me)
    Mempool().add(tx, saved(sandbox))
    miner.main([me.address, "--blocks", "3", "--cpu"])
    bc = saved(sandbox)
    assert bc.is_valid()
    times = sum(1 for b in bc.chain for t in b.transactions if t.signature == tx.signature)
    assert times == 1                                          # once, not once per overlapped block
    assert bc.balance_of(friend.address) == 5 * UNIT
    assert Mempool().load() == []                              # and it left the mempool


def test_background_check_returns_raises_and_reports_done():
    ok = miner.BackgroundCheck(lambda x: x * 2, 21)
    assert ok.result() == 42 and ok.done()

    def boom():
        raise GpuFault("bad block")

    bad = miner.BackgroundCheck(boom)
    with pytest.raises(GpuFault, match="bad block"):
        bad.result()
    assert bad.done()


# ---------------- the process checker, the baseline and the speed readout ----------------

import types as _types  # noqa: E402


def test_mining_with_the_checker_in_a_separate_process(sandbox, capsys):
    assert miner.main([ADDRESS, "--blocks", "3", "--cpu", "--check-process"]) == 0
    out = capsys.readouterr().out
    assert "separate low-priority process" in out
    assert "preparing the CPU reference dataset for epoch 0" in out
    assert out.count("double-checked by the CPU") == 3
    bc = saved(sandbox)
    assert len(bc.chain) == 4 and bc.is_valid()


def test_a_bad_block_is_caught_by_the_process_checker_too(sandbox, capsys):
    searcher = FlakySearcher(lies=1)
    assert miner.main([ADDRESS, "--blocks", "2", "--check-process"], searcher=searcher) == 0
    out = capsys.readouterr().out
    assert out.count("WARNING") == 1
    bc = saved(sandbox)
    assert len(bc.chain) == 3 and bc.is_valid()


def test_a_dead_checker_process_falls_back_to_checking_here(sandbox, monkeypatch, capsys):
    from concurrent.futures.process import BrokenProcessPool
    import tenero.checker as checker_module

    class DeadCheck:
        def done(self):
            return True

        def result(self):
            raise BrokenProcessPool("the worker died")

    class DeadChecker:
        submitted = 0

        def __init__(self, **kw):
            pass

        def submit(self, block, work, on_finish=None):
            DeadChecker.submitted += 1
            return DeadCheck()

        def prepare(self, work, epoch):
            return None

        def close(self):
            pass

    monkeypatch.setattr(checker_module, "ProcessChecker", DeadChecker)
    assert miner.main([ADDRESS, "--blocks", "3", "--cpu", "--check-process"]) == 0
    out = capsys.readouterr().out
    assert out.count("the checker process died") == 1          # said once, then it switches
    assert DeadChecker.submitted == 1                          # later blocks use the thread
    bc = saved(sandbox)
    assert len(bc.chain) == 4 and bc.is_valid()                # and no block was skipped unchecked


def test_a_dead_checker_still_catches_a_bad_block(sandbox, monkeypatch, capsys):
    from concurrent.futures.process import BrokenProcessPool
    import tenero.checker as checker_module

    class DeadCheck:
        def done(self):
            return True

        def result(self):
            raise BrokenProcessPool("the worker died")

    class DeadChecker:
        def __init__(self, **kw):
            pass

        def submit(self, block, work, on_finish=None):
            return DeadCheck()

        def prepare(self, work, epoch):
            return None

        def close(self):
            pass

    monkeypatch.setattr(checker_module, "ProcessChecker", DeadChecker)
    searcher = FlakySearcher(lies=1)
    assert miner.main([ADDRESS, "--blocks", "2", "--check-process"], searcher=searcher) == 0
    assert "WARNING: the search produced a" in capsys.readouterr().out    # either wording
    assert saved(sandbox).is_valid()


def test_the_baseline_is_measured_for_a_real_gpu_searcher_only(gpu_searcher, sandbox):
    gpu = gpu_searcher
    from tenero.pow import default_pow
    work = default_pow("matmul")
    rate = miner.measure_baseline(gpu, work, seconds=0.1)
    assert rate is not None and rate > 0
    assert miner.measure_baseline(CountingSearcher(), work, seconds=0.1) is None   # not a GPU one
    assert miner.measure_baseline(object(), work, seconds=0.1) is None


def test_the_miner_prints_the_baseline_for_a_gpu_searcher(gpu_searcher, sandbox, capsys):
    gpu = gpu_searcher
    miner.main([ADDRESS, "--blocks", "2"], searcher=gpu)
    out = capsys.readouterr().out
    assert "GPU search speed with nothing else running:" in out and "attempts/s" in out


def make_session(baseline=None):
    args = _types.SimpleNamespace(blocks=None, pow=None, epoch_blocks=None, no_double_check=False)
    session = miner.Session(args, ADDRESS, None, None)
    session.baseline = baseline
    return session


def test_speed_note_reports_search_speed_during_a_check():
    session = make_session(baseline=23_000)
    p = miner.Pending(check=None, block=None, started=10.0, attempts=1_000)
    p.finished, p.finished_attempts = 12.0, 1_000 + 30_000
    assert session.speed_note(p) == " (GPU search ran at 15,000/s meanwhile, 65% of the 23,000/s baseline)"
    p.idle = 1.0                      # the GPU waited 1 s of the window: only 1 s was searching
    assert "30,000/s" in session.speed_note(p)


def test_speed_note_without_a_baseline_or_with_too_little_data():
    session = make_session(baseline=None)
    p = miner.Pending(check=None, block=None, started=10.0, attempts=0)
    p.finished, p.finished_attempts = 12.0, 24_000
    assert session.speed_note(p) == " (GPU search ran at 12,000/s meanwhile)"
    p.finished = 10.2                 # a window this short means nothing
    assert session.speed_note(p) == ""
    fresh = miner.Pending(check=None, block=None, started=10.0, attempts=0)
    assert session.speed_note(fresh) == ""          # the check has not finished yet


def test_waiting_for_a_slow_check_is_counted_as_idle_time():
    import time as _time
    session = make_session()
    saved_to = []
    session.bc = _types.SimpleNamespace(save=lambda upto=None: saved_to.append(upto))
    session.mempool = _types.SimpleNamespace(remove=lambda txs: None)
    block = _types.SimpleNamespace(index=4, transactions=[object()])
    check = miner.BackgroundCheck(lambda: (_time.sleep(0.3), 0.3)[1])
    session.pending = miner.Pending(check, block, started=_time.perf_counter(), attempts=0)
    session.settle(True)                                # blocks until the check is done
    assert session.saved == 1 and session.pending is None and saved_to == [5]


def test_background_check_calls_its_finish_hook_before_reporting_done():
    order = []
    hook = miner.BackgroundCheck(lambda: 7, on_finish=lambda: order.append("hook"))
    assert hook.result() == 7
    assert order == ["hook"]


# ---------------- preparing the CPU reference dataset ----------------

def test_the_cpu_dataset_is_prepared_in_the_background_and_the_next_epoch_ahead(sandbox, capsys):
    assert miner.main([ADDRESS, "--blocks", "3", "--cpu", "--epoch-blocks", "2"]) == 0
    out = capsys.readouterr().out
    for epoch in (0, 1, 2):
        assert out.count(f"preparing the CPU reference dataset for epoch {epoch} ") == 1


def test_no_double_check_and_sha256_never_prepare_a_dataset(sandbox, capsys):
    miner.main([ADDRESS, "--blocks", "2", "--no-double-check"], searcher=CountingSearcher())
    assert "preparing" not in capsys.readouterr().out
    import os
    os.remove(sandbox)
    miner.main([ADDRESS, "--blocks", "2", "--pow", "sha256"])
    assert "preparing" not in capsys.readouterr().out


def test_prefetch_starts_the_next_epoch_only_near_the_end_of_an_epoch():
    session = make_session()
    work = _types.SimpleNamespace(name="matmul", epoch_blocks=100,
                                  epoch_of=lambda h: (h - 1) // 100)
    session.bc = _types.SimpleNamespace(pow=work)
    asked = []
    session.prepare_epoch = asked.append
    for height, expected in ((1, [0]), (50, [0]), (90, [0]),      # 11 blocks left: not yet
                             (91, [0, 1]), (100, [0, 1]),          # 10 and 1 left: prepare epoch 1
                             (101, [1]), (191, [1, 2])):
        asked.clear()
        session.prefetch(height)
        assert asked == expected, height


def test_the_miner_asks_for_each_epoch_only_once():
    session = make_session()
    p = _types.SimpleNamespace(dataset_bytes=2**30)
    built = []
    session.bc = _types.SimpleNamespace(pow=_types.SimpleNamespace(
        name="matmul", params=p, prepare=built.append))
    session.args.no_double_check = False
    session.checker = None
    session.prepare_epoch(3)
    session.prepare_epoch(3)
    session.prepare_epoch(4)
    import time as _t
    for _ in range(100):
        if len(built) == 2:
            break
        _t.sleep(0.02)
    assert sorted(built) == [3, 4]


def test_running_out_of_ram_for_the_cpu_dataset_stops_cleanly(sandbox, monkeypatch, capsys):
    def no_memory(self, epoch, progress=None):
        raise MemoryError

    monkeypatch.setattr(MatmulPow, "prepare", no_memory)
    assert miner.main([ADDRESS, "--blocks", "2", "--cpu"]) == 4
    out = capsys.readouterr().out
    assert "not enough free RAM" in out and "--no-double-check" in out
    import os
    assert not os.path.exists(sandbox)                 # the unchecked block was not saved


def test_a_checker_that_died_before_preparing_falls_back_to_a_thread(sandbox, monkeypatch, capsys):
    from concurrent.futures.process import BrokenProcessPool
    import tenero.checker as checker_module

    class DeadChecker:
        def __init__(self, **kw):
            pass

        def submit(self, block, work, on_finish=None):
            raise AssertionError("no check should go to a dead checker")

        def prepare(self, work, epoch):
            raise BrokenProcessPool("the worker died")

        def close(self):
            pass

    monkeypatch.setattr(checker_module, "ProcessChecker", DeadChecker)
    assert miner.main([ADDRESS, "--blocks", "2", "--cpu", "--check-process"]) == 0
    assert "the checker process died" in capsys.readouterr().out
    bc = saved(sandbox)
    assert len(bc.chain) == 3 and bc.is_valid()                # still checked, and nothing skipped


# ---------------- the CPU core budget ----------------

def test_the_build_gets_what_is_left_of_the_core_budget():
    assert miner.build_threads_for(6) == 4          # 1 GPU loop + 1 check + 4
    assert miner.build_threads_for(12) == 10
    assert miner.build_threads_for(3) == 1
    assert miner.build_threads_for(2) == 1          # never less than one


def test_the_budget_line_is_honest_when_no_checks_run(sandbox, capsys):
    miner.main([ADDRESS, "--blocks", "1", "--cpu", "--no-double-check"])
    out = capsys.readouterr().out
    assert "with no CPU checks the only CPU use is 1 core" in out and "building" not in out


def test_the_default_budget_is_six_cores(sandbox, monkeypatch, capsys):
    from tenero import matmulhash as mh
    monkeypatch.setattr(mh, "BUILD_THREADS", 1)
    assert config.MINER_MAX_CORES == 6
    assert miner.main([ADDRESS, "--blocks", "1", "--cpu"]) == 0
    out = capsys.readouterr().out
    assert "CPU budget: at most 6 cores in total" in out and "4 for building" in out
    assert mh.BUILD_THREADS == 4


@pytest.mark.parametrize("cores,threads", [(4, 2), (2, 1), (12, 10)])
def test_max_cores_sets_the_build_threads(sandbox, monkeypatch, capsys, cores, threads):
    from tenero import matmulhash as mh
    monkeypatch.setattr(mh, "BUILD_THREADS", 1)
    miner.main([ADDRESS, "--blocks", "1", "--cpu", "--max-cores", str(cores)])
    assert mh.BUILD_THREADS == threads
    assert f"at most {cores} cores" in capsys.readouterr().out


def test_a_budget_too_small_to_run_is_refused(sandbox, capsys):
    with pytest.raises(SystemExit):
        miner.main([ADDRESS, "--blocks", "1", "--cpu", "--max-cores", "1"])
    assert "at least 2" in capsys.readouterr().err


def test_a_sha256_chain_does_not_touch_the_build_threads(sandbox, monkeypatch, capsys):
    from tenero import matmulhash as mh
    monkeypatch.setattr(mh, "BUILD_THREADS", 7)
    miner.main([ADDRESS, "--blocks", "1", "--pow", "sha256"])
    assert mh.BUILD_THREADS == 7 and "CPU budget" not in capsys.readouterr().out


def test_the_checker_process_is_given_the_build_threads(sandbox, monkeypatch):
    from concurrent.futures.process import BrokenProcessPool
    import tenero.checker as checker_module
    made = {}

    class DeadCheck:
        def done(self):
            return True

        def result(self):
            raise BrokenProcessPool("stop here")       # the miner then checks in this process

    class Recording:
        def __init__(self, **kw):
            made.update(kw)

        def submit(self, block, work, on_finish=None):
            return DeadCheck()

        def prepare(self, work, epoch):
            return None

        def close(self):
            pass

    monkeypatch.setattr(checker_module, "ProcessChecker", Recording)
    miner.main([ADDRESS, "--blocks", "2", "--cpu", "--check-process", "--max-cores", "8"])
    assert made == {"build_threads": 6}


def test_a_gpu_miner_checks_in_a_separate_process_by_default(sandbox, gpu_searcher, capsys):
    assert miner.main([ADDRESS, "--blocks", "2"], searcher=gpu_searcher) == 0
    out = capsys.readouterr().out
    assert "in a separate low-priority process" in out
    assert out.count("double-checked by the CPU") == 2 and saved(sandbox).is_valid()


def test_check_thread_opts_out_of_the_separate_process(sandbox, gpu_searcher, capsys):
    assert miner.main([ADDRESS, "--blocks", "2", "--check-thread"], searcher=gpu_searcher) == 0
    out = capsys.readouterr().out
    assert "in a thread" in out and "separate" not in out
    assert saved(sandbox).is_valid()


def test_the_cpu_search_keeps_checking_in_a_thread_unless_told_otherwise(sandbox, capsys):
    miner.main([ADDRESS, "--blocks", "1", "--cpu"])
    assert "in a thread" in capsys.readouterr().out
    import os
    os.remove(sandbox)
    miner.main([ADDRESS, "--blocks", "1", "--cpu", "--check-process"])
    assert "separate low-priority process" in capsys.readouterr().out


def test_the_decision_function():
    args = _types.SimpleNamespace(check_thread=False, check_process=False)
    assert miner.wants_process_check(args, CountingSearcher()) is False      # CPU search
    args.check_process = True
    assert miner.wants_process_check(args, CountingSearcher()) is True       # forced
    args.check_thread = True
    assert miner.wants_process_check(args, CountingSearcher()) is False      # opt-out wins


def test_a_checker_that_fails_at_submit_falls_back_to_a_thread(sandbox, monkeypatch, capsys):
    from concurrent.futures.process import BrokenProcessPool
    import tenero.checker as checker_module

    class BrokenAtSubmit:
        def __init__(self, **kw):
            pass

        def submit(self, block, work, on_finish=None):
            raise BrokenProcessPool("the worker died")

        def prepare(self, work, epoch):
            return None

        def close(self):
            pass

    monkeypatch.setattr(checker_module, "ProcessChecker", BrokenAtSubmit)
    assert miner.main([ADDRESS, "--blocks", "3", "--cpu", "--check-process"]) == 0
    out = capsys.readouterr().out
    assert out.count("the checker process died") == 1
    bc = saved(sandbox)
    assert len(bc.chain) == 4 and bc.is_valid()             # no block skipped its check


def test_the_miner_limits_the_math_library_to_one_thread():
    import os
    assert os.environ["OPENBLAS_NUM_THREADS"] == "1" and os.environ["OMP_NUM_THREADS"] == "1"


def test_torch_is_limited_to_one_cpu_thread(sandbox, monkeypatch):
    fake = fake_cuda_torch()
    calls = []
    fake.set_num_threads = calls.append
    monkeypatch.setitem(sys.modules, "torch", fake)
    monkeypatch.setattr(gb, "make_fused", lambda torch, device: None)
    monkeypatch.setattr(gb, "TorchBackend", lambda *a, **k: types.SimpleNamespace(name="stub"))
    monkeypatch.setattr(gb, "self_test", lambda backend, title="": False)
    miner.main([ADDRESS, "--blocks", "1"])
    assert calls == [1]


def test_the_miner_never_builds_two_cpu_datasets_at_once(sandbox, monkeypatch, capsys):
    # --epoch-blocks 2 makes the miner ask for two epochs' datasets at startup; within the core
    # budget they must be built one after the other, not side by side
    import time as _t
    from tenero import matmulhash as mh
    real = mh.build_dataset
    state = {"now": 0, "peak": 0, "built": 0}
    lock = threading.Lock()

    def watched(params, seed, progress=None, **kw):
        with lock:
            state["now"] += 1
            state["peak"] = max(state["peak"], state["now"])
        _t.sleep(0.05)
        try:
            return real(params, seed, progress, **kw)
        finally:
            with lock:
                state["now"] -= 1
                state["built"] += 1

    monkeypatch.setattr(mh, "build_dataset", watched)
    mh.DEFAULT_CACHE.clear()
    assert miner.main([ADDRESS, "--blocks", "3", "--cpu", "--epoch-blocks", "2"]) == 0
    assert state["built"] >= 2 and state["peak"] == 1
    assert saved(sandbox).is_valid()
