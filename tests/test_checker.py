"""The separate-process checker: a real worker process is started for these tests."""
import hashlib

import pytest

np = pytest.importorskip("numpy")

from concurrent.futures import Future  # noqa: E402

from tenero import checker  # noqa: E402
from tenero import pow as powmod  # noqa: E402
from tenero.pow import GpuFault  # noqa: E402

from .test_pow import MINER, TINY, forged_block, matmul_chain, mine  # noqa: E402


def mh_params():
    return TINY


@pytest.fixture(scope="module")
def process_checker():
    c = checker.ProcessChecker(build_threads=3)
    yield c
    c.close()


def test_digest_for_matches_hash_of():
    bc = mine(matmul_chain(), 3)
    for block in bc.chain[1:]:
        assert bc.pow.digest_for(block.index, block._header_bytes(), block.nonce) == block.hash
        assert bc.pow.hash_of(block) == block.hash


@pytest.mark.parametrize("bad", [-1, 2**64, 1.5, "7", True, None])
def test_digest_for_refuses_unusable_nonces(bad):
    work = powmod.MatmulPow(TINY, 3)
    assert work.digest_for(1, b"header", bad) is None


def test_compute_in_process_gives_the_reference_digest():
    bc = mine(matmul_chain(), 2)
    block = bc.chain[2]
    digest, seconds = checker.compute(bc.pow.to_dict(), block.index, block._header_bytes(),
                                      block.nonce)
    assert digest == block.hash and seconds >= 0


def test_lowering_priority_never_raises():
    checker._lower_priority()


def test_a_real_worker_process_confirms_a_good_block(process_checker):
    bc = mine(matmul_chain(), 3)
    block = bc.chain[3]
    check = process_checker.submit(block, bc.pow)
    seconds = check.result()
    assert seconds >= 0 and check.done()
    assert bc.pow.last_check_seconds == seconds


def test_a_real_worker_process_rejects_a_bad_block(process_checker):
    bc = mine(matmul_chain(), 2)
    block = bc.chain[2]
    good_hash = block.hash
    block.hash = "f" * 64                            # inconsistent with its own mix: caught cheaply
    with pytest.raises(GpuFault, match="does not match its own"):
        process_checker.submit(block, bc.pow).result()
    block.hash = good_hash
    block.nonce += 1                                 # the nonce no longer produces this hash
    with pytest.raises(GpuFault, match="does not match its own"):
        process_checker.submit(block, bc.pow).result()


def test_a_consistent_but_untrue_block_is_rejected_by_the_worker(process_checker):
    bc = mine(matmul_chain(), 2)
    fake = forged_block(bc)                          # its hash follows from its (made-up) mix
    assert bc.pow.precheck(fake, 2**256)             # so the cheap check cannot tell ...
    with pytest.raises(GpuFault, match="disagrees"):
        process_checker.submit(fake, bc.pow).result()    # ... but the recomputation does


def test_the_cheap_check_rejects_without_waiting_for_the_worker():
    bc = mine(matmul_chain(), 1)
    block = bc.chain[1]
    block.mix = "00" * 64
    check = checker.ProcessCheck(Future(), block, bc.pow)      # a future that never completes
    with pytest.raises(GpuFault, match="does not match its own"):
        check.result()


def test_start_prepare_builds_the_dataset_in_a_background_thread():
    from tenero import matmulhash as mh
    work = powmod.MatmulPow(TINY, 3)
    spec = work.to_dict()
    checker._PREPARING.clear()
    assert checker.start_prepare(spec, 7) is True
    assert checker.start_prepare(spec, 7) is False             # already under way
    [t.join(10) for t in checker._PREPARING.values()]
    assert (TINY, powmod.epoch_seed(7)) in mh.DEFAULT_CACHE._items
    assert checker.start_prepare(powmod.Sha256Pow().to_dict(), 0) is False   # nothing to prepare


def test_a_real_worker_can_prepare_an_epoch_ahead(process_checker):
    work = powmod.MatmulPow(TINY, 3)
    assert process_checker.prepare(work, 4).result(30) is True
    assert process_checker.prepare(work, 4).result(30) is False


def test_the_worker_reports_check_time_without_the_dataset_wait():
    work = powmod.MatmulPow(mh_params(), 3)
    bc = mine(matmul_chain(params=mh_params()), 1)
    checker._WORK.clear()
    from tenero import matmulhash as mh
    mh.DEFAULT_CACHE.clear()                                   # nothing cached: a build is needed
    digest, seconds = checker.compute(bc.pow.to_dict(), 1, bc.chain[1]._header_bytes(),
                                      bc.chain[1].nonce)
    assert digest == bc.chain[1].hash
    assert mh.DEFAULT_CACHE.builds >= 1 and seconds < 5        # the build was outside the timing


def test_the_worker_handles_several_blocks_in_a_row(process_checker):
    bc = mine(matmul_chain(epoch_blocks=2), 5)         # crosses epoch boundaries
    checks = [process_checker.submit(b, bc.pow) for b in bc.chain[1:]]
    assert all(c.result() >= 0 for c in checks)


def test_on_finish_is_called_when_the_check_completes(process_checker):
    bc = mine(matmul_chain(), 1)
    calls = []
    check = process_checker.submit(bc.chain[1], bc.pow, on_finish=lambda: calls.append(1))
    check.result()
    for _ in range(50):                                # the callback runs just after the result
        if calls:
            break
        import time
        time.sleep(0.02)
    assert calls == [1]


def test_result_waits_in_short_slices(monkeypatch):
    # a future that completes after a few polls: result() must keep waiting, not give up
    bc = mine(matmul_chain(), 1)
    block = bc.chain[1]
    future = Future()
    check = checker.ProcessCheck(future, block, bc.pow)
    polls = []
    real = future.result

    def slow_result(timeout=None):
        polls.append(timeout)
        if len(polls) < 3:
            import concurrent.futures
            raise concurrent.futures.TimeoutError
        return real(timeout)

    monkeypatch.setattr(future, "result", slow_result)
    future.set_result((block.hash, 0.25))
    assert check.result() == 0.25 and len(polls) == 3


def test_the_worker_is_a_separate_low_priority_single_thread_process(process_checker):
    import os
    pid, nice, env = process_checker.executor.submit(checker.worker_info).result()
    assert pid != os.getpid()                                   # really another process
    if nice is not None:                                        # POSIX: below normal priority
        assert nice >= 10
    assert env == {"OPENBLAS_NUM_THREADS": "1", "OMP_NUM_THREADS": "1", "MKL_NUM_THREADS": "1"}


def test_the_parent_is_not_affected_by_the_workers_settings(process_checker):
    import os
    before = (os.nice(0) if hasattr(os, "nice") else None, os.environ.get("OPENBLAS_NUM_THREADS"))
    process_checker.executor.submit(checker.worker_info).result()
    after = (os.nice(0) if hasattr(os, "nice") else None, os.environ.get("OPENBLAS_NUM_THREADS"))
    assert before == after        # the worker changed ITS priority and environment, not ours


def test_the_worker_is_told_how_many_threads_a_dataset_build_may_use(process_checker):
    assert process_checker.executor.submit(checker.worker_threads).result(30) == 3


def test_the_parent_keeps_its_own_build_thread_setting(process_checker):
    from tenero import matmulhash as mh
    before = mh.BUILD_THREADS
    process_checker.executor.submit(checker.worker_threads).result(30)
    assert mh.BUILD_THREADS == before                       # the worker changed ITS setting only


def test_the_worker_frees_finished_epochs_as_it_checks():
    from tenero import matmulhash as mh
    mh.DEFAULT_CACHE.clear()
    bc = mine(matmul_chain(epoch_blocks=2), 5)                  # epochs 0, 1, 2
    checker._WORK.clear()
    mh.DEFAULT_CACHE.clear()
    spec = bc.pow.to_dict()
    for epoch in (0, 1):
        mh.cached_dataset(TINY, powmod.epoch_seed(epoch))
    block = bc.chain[5]                                         # a block of epoch 2
    digest, _ = checker.compute(spec, block.index, block._header_bytes(), block.nonce)
    assert digest == block.hash
    cached = {e for e in range(6) if (TINY, powmod.epoch_seed(e)) in mh.DEFAULT_CACHE._items}
    assert cached == {2}                                        # epochs 0 and 1 were freed
