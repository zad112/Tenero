"""Runs the CPU double-check of a mined block in a separate process.

Why: while the GPU searches for the next block, a check running in a thread of the same process
competes for the interpreter and the CPU, and slows the search. A separate process, at lower
priority and with a single math-library thread, keeps the two apart.

Only plain values (the algorithm settings, block index, header bytes and nonce) are sent to the
worker, never the chain. It starts with the "spawn" method, which behaves the same on every
platform (including Windows).
"""
import concurrent.futures
import multiprocessing
import os
import sys
import threading
import time
from concurrent.futures import ProcessPoolExecutor

from .pow import GpuFault, make_pow

_WORK = {}


def _lower_priority():
    try:
        if sys.platform == "win32":
            import ctypes
            kernel = ctypes.windll.kernel32
            kernel.SetPriorityClass(kernel.GetCurrentProcess(), 0x00004000)  # below normal
        else:
            os.nice(10)
    except Exception:  # noqa: BLE001 - priority is a nicety; never fail because of it
        pass


def _init_worker(build_threads=1):
    # one math-library thread, set BEFORE numpy is imported, a lower priority, and the number of
    # threads a dataset build may use here (the miner's core budget, minus the search and check)
    for var in ("OPENBLAS_NUM_THREADS", "OMP_NUM_THREADS", "MKL_NUM_THREADS"):
        os.environ[var] = "1"
    _lower_priority()
    from . import matmulhash
    matmulhash.set_build_threads(build_threads)


def _ping():
    from . import matmulhash  # noqa: F401 - forces the numpy import now, while the GPU is busy
    return True


def worker_threads():
    """Runs in the worker: how many threads a dataset build may use there."""
    from . import matmulhash
    return matmulhash.BUILD_THREADS


def worker_info():
    """Runs in the worker: (process id, niceness on POSIX or None, the math-library thread
    settings). For checking that the worker really is separate and set up as intended."""
    nice = os.nice(0) if hasattr(os, "nice") else None
    return os.getpid(), nice, {v: os.environ.get(v) for v in
                               ("OPENBLAS_NUM_THREADS", "OMP_NUM_THREADS", "MKL_NUM_THREADS")}


def _work_for(spec):
    key = tuple(sorted(spec.items()))
    work = _WORK.get(key)
    if work is None:
        work = _WORK[key] = make_pow(spec)
    return work


def compute(spec, index, header, nonce):
    """Runs in the worker: (the hash the block should have, seconds it took, not counting any
    wait for the epoch's dataset to finish building)."""
    work = _work_for(spec)
    if hasattr(work, "prepare"):
        work.prepare(work.epoch_of(index))
    t0 = time.perf_counter()
    digest = work.digest_for(index, header, nonce)
    return digest, time.perf_counter() - t0


_PREPARING = {}


def start_prepare(spec, epoch):
    """Runs in the worker: starts building this epoch's dataset in a background THREAD and
    returns at once, so the checks of blocks in the current epoch are not held up. Returns
    False if that build was already started."""
    work = _work_for(spec)
    key = (tuple(sorted(spec.items())), epoch)
    if key in _PREPARING or not hasattr(work, "prepare"):
        return False
    thread = threading.Thread(target=work.prepare, args=(epoch,), daemon=True)
    _PREPARING[key] = thread
    thread.start()
    return True


class ProcessCheck:
    """One block's check in the worker. Same interface as the miner's thread-based check."""

    def __init__(self, future, block, work, on_finish=None):
        self.future, self.block, self.work = future, block, work
        if on_finish:
            future.add_done_callback(lambda f: on_finish())

    def done(self):
        return self.future.done()

    def result(self):
        # the cheap consistency check first (microseconds, right here), then wait for the worker
        # in short slices so Ctrl+C still works on Windows; a dead worker raises
        # concurrent.futures.process.BrokenProcessPool for the caller to handle
        if hasattr(self.work, "precheck") and not self.work.precheck(self.block, 2**256):
            raise GpuFault("the search produced a block whose hash does not match its own "
                           "contents, nonce and mix: the block was discarded")
        while True:
            try:
                digest, seconds = self.future.result(timeout=0.2)
                break
            except concurrent.futures.TimeoutError:
                continue
        self.work.last_check_seconds = seconds
        if digest != self.block.hash:
            raise GpuFault("the search produced a result the CPU reference disagrees "
                           "with: the block was discarded")
        return seconds


class ProcessChecker:
    def __init__(self, build_threads=1):
        ctx = multiprocessing.get_context("spawn")
        self.executor = ProcessPoolExecutor(max_workers=1, mp_context=ctx,
                                            initializer=_init_worker,
                                            initargs=(build_threads,))
        self.executor.submit(_ping)      # start the worker now; do not wait for it

    def submit(self, block, work, on_finish=None):
        future = self.executor.submit(compute, work.to_dict(), block.index,
                                      block._header_bytes(), block.nonce)
        return ProcessCheck(future, block, work, on_finish)

    def prepare(self, work, epoch):
        """Asks the worker to build this epoch's dataset in the background (it holds the
        dataset in its own memory, so the main process never needs the 4 GiB)."""
        return self.executor.submit(start_prepare, work.to_dict(), epoch)

    def close(self):
        self.executor.shutdown(wait=False, cancel_futures=True)
