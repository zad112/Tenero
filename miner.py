"""The miner. A matmul chain (the default for a new chain) is mined on the GPU.

    python miner.py <payout-address>                 mine until you press Ctrl+C
    python miner.py <payout-address> --blocks 3      mine three blocks
    python miner.py <address> --pow sha256           a NEW chain that a CPU can mine (testing)

An existing chain keeps whatever proof of work it was created with. Run only ONE miner.

On a matmul chain, each block is re-checked on the CPU before it is kept. By default that check
runs in the background while the GPU already searches for the next block (--no-overlap turns
that off; --no-double-check skips the check). The check needs the epoch's dataset in RAM (about
4.3 GiB at full size, built in the background, one to two minutes per epoch). The next epoch's is
prepared during the last 10 blocks, so both are in RAM (about 8.6 GiB) for those blocks only; a
finished epoch's dataset is freed as soon as a check for the next epoch starts.
"""
import os

# The math library behind numpy (used by the CPU double-check) starts one thread per core unless
# told otherwise. The miner has a core budget (--max-cores), so it gets ONE: set before numpy loads.
for _var in ("OPENBLAS_NUM_THREADS", "OMP_NUM_THREADS", "MKL_NUM_THREADS"):
    os.environ.setdefault(_var, "1")

import argparse  # noqa: E402
import re  # noqa: E402
import sys  # noqa: E402
import threading  # noqa: E402
import time  # noqa: E402
from concurrent.futures.process import BrokenProcessPool  # noqa: E402

from tenero import config, paths  # noqa: E402
from tenero.chain import Blockchain, format_hashrate, format_tops  # noqa: E402
from tenero.pow import GpuFault, epoch_seed  # noqa: E402
from tenero.mempool import Mempool  # noqa: E402
from tenero.units import fmt  # noqa: E402


MAX_FAULTS = 3   # stop after this many GPU results in a row that the CPU rejects
BASELINE_SECONDS = 2.0   # how long the startup search-speed measurement runs
PREFETCH_BLOCKS = 10     # this close to the end of an epoch, prepare the next epoch's CPU dataset


def build_threads_for(max_cores):
    """Threads a CPU dataset build may use within a budget of `max_cores` cores in total. One core
    is the GPU search loop (it spins while waiting for the GPU) and one is the CPU double-check,
    which can run while the next epoch's dataset is being built; the rest go to the build."""
    return max(1, max_cores - 2)


def wants_process_check(args, searcher):
    """Whether the background check runs in a separate process. That is the default for a real
    GPU miner: the CPU dataset build, done in a thread of the miner, took most of the search's
    speed while it ran. --check-thread opts out; --check-process forces it (also for the CPU
    search used in tests)."""
    if args.check_thread:
        return False
    if args.check_process:
        return True
    from tenero.gpubackend import GpuSearcher
    return isinstance(searcher, GpuSearcher)


def is_address(s):
    return bool(re.fullmatch(r"[0-9a-fA-F]{40}", s))


def make_gpu_searcher(skip_selftest=False, log=print):
    """Sets up the GPU and checks it against the CPU reference. Raises RuntimeError with a
    plain message if it cannot (no PyTorch, no CUDA GPU, no CuPy for the CUDA kernels, or the
    GPU disagrees with the CPU)."""
    from tenero import gpubackend as gb
    try:
        import torch
    except ImportError:
        raise RuntimeError("PyTorch is not installed in this Python, and this chain needs a "
                           "GPU miner (a CUDA build of PyTorch)")
    if not torch.cuda.is_available():
        raise RuntimeError("PyTorch cannot see a CUDA GPU (is it a CPU-only build?), and this "
                           "chain needs a GPU miner")
    if hasattr(torch, "set_num_threads"):
        torch.set_num_threads(1)          # CPU-side torch work is tiny: stay within the core budget
    device = torch.device("cuda")
    torch.backends.cuda.matmul.allow_tf32 = False
    props = torch.cuda.get_device_properties(0)
    fused = gb.make_fused(torch, device)     # raises RuntimeError, saying what to install
    backend = gb.TorchBackend(torch, device, "auto", fused=fused)
    log(f"GPU: {props.name} ({props.total_memory / 2**30:.1f} GiB)")
    log(f"matmul: {backend.name}; ChaCha20 generation, dataset fill and fold: "
        f"fused CUDA kernels (CuPy)")
    if not skip_selftest:
        title = "checking the GPU against the CPU reference (a few seconds)..."
        ok = gb.self_test(backend, title=title)
        if not ok:
            raise RuntimeError("the GPU disagrees with the CPU reference, so it would produce "
                               "invalid blocks: refusing to mine")
        log("GPU check passed.\n")
    return gb.GpuSearcher(backend, log=lambda msg: log("  " + msg))


class BackgroundCheck:
    """Runs fn(*args) in a daemon thread. result() waits for it (in short slices, so Ctrl+C
    still works on Windows) and re-raises anything the function raised."""

    def __init__(self, fn, *args, on_finish=None):
        self._result = self._error = None
        self._on_finish = on_finish
        self._done = threading.Event()
        threading.Thread(target=self._run, args=(fn,) + args, daemon=True).start()

    def _run(self, fn, *args):
        try:
            self._result = fn(*args)
        except BaseException as e:  # noqa: BLE001 - handed to whoever calls result()
            self._error = e
        finally:
            if self._on_finish:
                self._on_finish()
            self._done.set()

    def done(self):
        return self._done.is_set()

    def result(self):
        while not self._done.wait(0.2):
            pass
        if self._error is not None:
            raise self._error
        return self._result


def measure_baseline(searcher, work, seconds=None):
    """The GPU search speed (attempts per second) with nothing else running, so the speed
    during a background check can be compared with it. None if the searcher cannot tell."""
    from tenero.gpubackend import GpuSearcher
    if not isinstance(searcher, GpuSearcher):
        return None      # only the real GPU searcher has a meaningful baseline
    seconds = BASELINE_SECONDS if seconds is None else seconds
    seed, header = epoch_seed(0), b"\x00" * 32
    searcher.search(work.params, seed, header, 0, 0, min(seconds, 0.3))   # build/warm up first
    a0, t0 = searcher.attempts, time.perf_counter()
    searcher.search(work.params, seed, header, 0, 0, seconds)             # target 0: never found
    elapsed = time.perf_counter() - t0
    return (searcher.attempts - a0) / elapsed if elapsed > 0 else None


class Pending:
    """The one unchecked block, with what is needed to report how the search fared meanwhile."""

    def __init__(self, check, block, started, attempts):
        self.check, self.block = check, block
        self.started, self.attempts = started, attempts     # when it was submitted
        self.finished = self.finished_attempts = None       # set when the check completes
        self.idle = 0.0                                     # seconds the GPU waited for it


class Session:
    """One mining run.

    Synchronous mode (SHA-256 chains, --no-overlap, --no-double-check): find a block, check it,
    save it, then start the next one.

    Overlapped mode (matmul chains): the GPU finds block N, then starts on block N+1 AT ONCE
    while a background thread double-checks block N on the CPU. Rules that keep it safe:
      - only the main thread saves, and only blocks whose check has passed (the saved chain
        never contains an unchecked block);
      - at most ONE unchecked block exists at a time (block N+1 is not checked until N is);
      - if a check fails, the bad block and everything built on top of it are thrown away and
        the chain is reloaded from disk.
    """

    def __init__(self, args, address, searcher, mempool):
        self.args, self.address, self.searcher, self.mempool = args, address, searcher, mempool
        self.overlap = False
        self.checker = None      # a ProcessChecker, or None to check in a thread
        self.baseline = None     # search speed with nothing else running
        self.bc = None
        self.pending = None      # a Pending for the one unchecked block, or None
        self.saved = 0
        self.faults = 0
        self.idle = False
        self.prepared = set()    # epochs whose CPU dataset has been asked for

    def prepare_epoch(self, epoch):
        """Starts building this epoch's dataset for the CPU double-check, in the background
        (once per epoch). Only matmul chains that double-check need it."""
        bc = self.bc
        if (bc is None or bc.pow.name != "matmul" or self.args.no_double_check
                or epoch in self.prepared):
            return
        self.prepared.add(epoch)
        work, p = bc.pow, bc.pow.params
        print(f"  preparing the CPU reference dataset for epoch {epoch} in the background "
              f"(about {p.dataset_bytes * 1.08 / 2**30:.1f} GiB of RAM; one to two minutes at "
              f"full size; the finished epoch's is freed)")
        if self.checker is not None:
            try:
                self.checker.prepare(work, epoch)  # built in the checker process's memory
                return
            except BrokenProcessPool:
                print("  WARNING: the checker process died; checking in this process instead")
                self.checker = None                # never skip a check: build it here instead

        def build():
            t0 = time.perf_counter()
            try:
                work.prepare(epoch)
            except MemoryError:
                print(f"  WARNING: not enough RAM for the CPU reference dataset "
                      f"({p.dataset_bytes / 2**30:.2f} GiB); the double-check cannot run")
                return
            print(f"  CPU reference dataset for epoch {epoch} ready "
                  f"({time.perf_counter() - t0:.0f}s)")

        threading.Thread(target=build, daemon=True).start()

    def prefetch(self, height):
        """Before mining block `height`: the CPU dataset of its epoch must be under way, and
        near the end of an epoch so must the next one (a block of the new epoch would otherwise
        wait a minute or two for its check)."""
        work = self.bc.pow
        if work.name != "matmul":
            return
        epoch = work.epoch_of(height)
        self.prepare_epoch(epoch)
        left = work.epoch_blocks - ((height - 1) % work.epoch_blocks)   # blocks left, this one included
        if left <= PREFETCH_BLOCKS:
            self.prepare_epoch(epoch + 1)

    def reload(self):
        a = self.args
        self.bc = Blockchain.load_or_new(searcher=self.searcher, algorithm=a.pow,
                                         epoch_blocks=a.epoch_blocks)
        if hasattr(self.bc.pow, "verify_solutions"):
            self.bc.pow.verify_solutions = not a.no_double_check
        return self.bc

    # ---- the one unchecked block ----
    def settle(self, wait):
        """If the unchecked block's check has finished (or wait=True: once it does), save it.
        Raises GpuFault if the check failed."""
        if self.pending is None:
            return
        pending = self.pending
        block = pending.block
        if not wait and not pending.check.done():
            return
        waited0 = time.perf_counter()
        try:
            seconds = pending.check.result()      # raises GpuFault for a bad block
        except BrokenProcessPool:
            # the checker process died: check in this process instead (never skip a check)
            print("  WARNING: the checker process died; checking in this process instead")
            self.checker = None
            seconds = self.bc.pow.check(block)
        pending.idle += time.perf_counter() - waited0 if wait else 0.0
        self.pending = None
        self.bc.save(upto=block.index + 1)   # only checked blocks, never a newer unchecked one
        self.mempool.remove(block.transactions[1:])
        self.saved += 1
        self.faults = 0
        print(f"  block {block.index} double-checked by the CPU in {seconds:.1f}s and saved"
              + self.speed_note(pending))

    def speed_note(self, pending):
        # how fast the GPU search ran while this check was running (the cost of overlapping)
        if pending.finished is None or pending.finished_attempts is None:
            return ""
        window = pending.finished - pending.started - pending.idle
        if window < 0.5:
            return ""                       # too short to mean anything
        rate = (pending.finished_attempts - pending.attempts) / window
        note = f" (GPU search ran at {rate:,.0f}/s meanwhile"
        if self.baseline:
            note += f", {100 * rate / self.baseline:.0f}% of the {self.baseline:,.0f}/s baseline"
        return note + ")"

    def start_check(self, bc, block):
        """Starts the background check of a just-found block."""
        searcher = self.searcher
        holder = {}

        def finished():
            p = holder.get("pending")
            if p is not None:
                p.finished = time.perf_counter()
                p.finished_attempts = getattr(searcher, "attempts", 0)

        started, attempts = time.perf_counter(), getattr(searcher, "attempts", 0)
        check = None
        if self.checker is not None:
            try:
                check = self.checker.submit(block, bc.pow, on_finish=finished)
            except BrokenProcessPool:
                print("  WARNING: the checker process died; checking in this process instead")
                self.checker = None                # never skip a check: do it here instead
        if check is None:
            check = BackgroundCheck(bc.pow.check, block, on_finish=finished)
        holder["pending"] = Pending(check, block, started, attempts)
        return holder["pending"]

    def discard_after_fault(self, error):
        self.faults += 1
        print(f"  WARNING: {error}")
        print(f"  ({self.faults} in a row.) A wrong GPU result can mean an unstable GPU: an "
              f"overclock, an undervolt or overheating.")
        self.pending = None
        if self.faults >= MAX_FAULTS:
            print("stopping: the GPU keeps producing results the CPU rejects. Nothing bad was "
                  "saved.")
            return False
        if hasattr(self.searcher, "reset"):
            self.searcher.reset()   # rebuild the dataset in case it was corrupted
        print("  retrying with a fresh dataset...\n")
        self.reload()               # back to the last checked block on disk
        return True

    # ---- one block ----
    def mine_one(self):
        a, bc, mempool = self.args, self.bc, self.mempool
        args_blocks = a.blocks
        if args_blocks is not None and self.saved + (self.pending is not None) >= args_blocks:
            self.settle(True)          # enough blocks are found: just finish checking the last
            return
        txs = mempool.usable(bc)
        height = len(bc.chain)
        self.prefetch(height)
        base = bc.reward_at(height)

        if base == 0 and not txs:
            # no reward and no fees to earn: don't mine empty blocks
            self.settle(True)          # save what has been checked before going quiet
            if not self.idle:
                print("all coins have been issued. Waiting for transactions "
                      "(miners now earn fees only)...")
                self.idle = True
            time.sleep(2)
            return
        self.idle = False

        fees = sum(t.fee for t in txs)
        size = sum(t.size() for t in txs)
        label = "tail reward" if bc.in_tail(height) else "reward"
        print(f"mining block {height}: {label} {fmt(base)} + {fmt(fees)} fees, "
              f"{len(txs)} transaction(s), {size} bytes...")
        if self.pending is not None:
            print(f"  (block {self.pending.block.index} is being double-checked on the CPU in "
                  f"the background)")
        stats = bc.recent_stats()
        line = f"  difficulty: ~{2**256 // bc.next_target():,} attempts per block"
        if bc.difficulty_window == 0:
            line += " (fixed)"
        if stats:
            line += (f"; recent blocks {stats[0]:.0f}s on average, "
                     f"network ~{format_hashrate(stats[1])}")
        print(line)
        if bc.pow.name == "matmul":
            print(f"  dataset epoch {bc.pow.epoch_of(height)}")
        t = time.time()
        median = bc.median_size()
        before = getattr(self.searcher, "attempts", 0)

        if self.overlap:
            # search without the check; between search chunks, save the previous block as
            # soon as its check has passed (or abort if it failed)
            block = bc.mine_block(self.address, txs, verify=False,
                                  on_chunk=lambda: self.settle(False))
            elapsed = search_seconds = time.time() - t
            self.settle(True)          # the previous block must be checked before this one starts
            self.pending = self.start_check(bc, block)
            note = "; the CPU double-check runs in the background"
        else:
            block = bc.mine_block(self.address, txs)
            elapsed = time.time() - t
            check = getattr(bc.pow, "last_check_seconds", 0.0) if bc.pow.name == "matmul" else 0.0
            search_seconds = elapsed - check
            bc.save()
            mempool.remove(block.transactions[1:])
            self.saved += 1
            self.faults = 0
            note = f"; then a {check:.1f}s double-check on the CPU" if check else ""

        self.report(bc, block, elapsed, search_seconds, before, size, median, base, note)

    def report(self, bc, block, elapsed, search_seconds, before, size, median, base, note):
        print(f"  found block {block.index}: nonce={block.nonce:,} "
              f"time={elapsed:.1f}s hash={block.hash[:16]}...")
        if bc.pow.name == "matmul":
            attempts = getattr(self.searcher, "attempts", 0) - before
            rate = attempts / max(search_seconds, 1e-9)
            p = bc.pow.params
            print(f"  {attempts:,} attempts ({rate:,.0f}/s = "
                  f"{format_tops(rate * p.ops_per_attempt())}, streaming "
                  f"{rate * p.slice_bytes / 1e9:.0f} GB/s){note}")
        penalty = bc.penalty(base, size, median)
        print(f"  size: {size} bytes (free up to {median}, hard limit {2 * median})")
        if penalty:
            print(f"  oversize penalty: -{fmt(penalty)} (the extra fees were worth more)")
        print(f"  paid out: {fmt(block.transactions[0].amount)}")
        print(f"  payout balance: {fmt(bc.balance_of(self.address))}")
        print(f"  supply issued: {fmt(bc.supply())}")
        if bc.main_reward_at(block.index) > 0 and bc.main_reward_at(len(bc.chain)) == 0:
            if bc.tail_reward:
                print(f"  *** main emission finished ({fmt(bc.max_supply)} coins): "
                      f"tail emission of {fmt(bc.tail_reward)} per block begins ***")
            else:
                print("  *** that was the last block reward: the supply cap is reached ***")
        included = {t.signature for t in block.transactions[1:]}
        left = len([t for t in self.mempool.load() if t.signature not in included])
        if left:
            print(f"  {left} transaction(s) still waiting in the mempool")
        print()

    def run(self):
        a = self.args
        self.reload()
        try:
            while a.blocks is None or self.saved < a.blocks:
                try:
                    self.mine_one()
                except GpuFault as e:
                    if not self.discard_after_fault(e):
                        return 3
        except MemoryError:
            print("error: not enough free RAM to build the CPU reference dataset "
                  "(4 GiB at full size) for the double-check. Close other programs, or use "
                  "--no-double-check (blocks are then saved WITHOUT a CPU check). Nothing "
                  "unchecked was saved.")
            return 4
        except KeyboardInterrupt:
            print("\nminer stopping...")
            if self.pending is not None:
                print(f"finishing the CPU check of block {self.pending.block.index} "
                      f"(press Ctrl+C again to skip it)...")
                try:
                    self.settle(True)
                except GpuFault:
                    print("  that block FAILED its check and was discarded")
                except KeyboardInterrupt:
                    print("  skipped: the unchecked block was NOT saved")
            print("miner stopped")
        return 0


def main(argv=None, searcher=None):
    ap = argparse.ArgumentParser(description="Tenero miner")
    ap.add_argument("address", nargs="?", help="payout address (40 hex characters)")
    ap.add_argument("--blocks", type=int, help="stop after this many blocks")
    ap.add_argument("--pow", choices=("matmul", "sha256"), default=None,
                    help="proof of work for a NEW chain (an existing chain keeps its own)")
    ap.add_argument("--epoch-blocks", type=int, default=None,
                    help="matmul, NEW chain only: blocks between dataset changes (default from "
                         "config.py). A small value lets you watch an epoch change quickly.")
    ap.add_argument("--cpu", action="store_true",
                    help="matmul chain: search on the CPU (exact, but far too slow at full "
                         "size: for testing with tiny settings)")
    ap.add_argument("--max-cores", type=int, default=config.MINER_MAX_CORES,
                    help=f"the most CPU cores the whole miner may use (default "
                         f"{config.MINER_MAX_CORES}): 1 for the GPU search loop, 1 for the "
                         f"double-check, the rest to build the CPU reference dataset")
    ap.add_argument("--skip-selftest", action="store_true", help="skip the GPU-vs-CPU check at start")
    ap.add_argument("--no-double-check", action="store_true",
                    help="do not re-check each mined block on the CPU before saving it")
    ap.add_argument("--check-process", action="store_true",
                    help="run the background check in a separate low-priority process. This is "
                         "already the default for a GPU miner; the option forces it for a CPU "
                         "search too (testing)")
    ap.add_argument("--check-thread", action="store_true",
                    help="run the background check in a thread of the miner instead of a "
                         "separate process. Slower for the GPU search: building the CPU "
                         "dataset then took 50-85%% of its speed for about a minute per epoch, "
                         "against 3-28%% in a separate process")
    ap.add_argument("--no-overlap", action="store_true",
                    help="matmul chain: check each block on the CPU BEFORE starting the next "
                         "(slower: the GPU idles during the check; simpler, no background thread)")
    args = ap.parse_args(argv)
    if args.max_cores < 2:
        ap.error("--max-cores must be at least 2 (one core drives the GPU, one does the checks)")

    address = args.address or input("payout address: ").strip()
    if not is_address(address):
        print("error: that is not a valid address (expected 40 hex characters)")
        return 1
    address = address.lower()

    mempool = Mempool()
    try:
        start = Blockchain.load_or_new(algorithm=args.pow, epoch_blocks=args.epoch_blocks)
    except ValueError as e:
        print("error:", e)
        return 1
    print(f"miner started, paying rewards to {address}")
    if paths.using_scratch_folder():
        print(f"scratch data folder: {paths.DATA_DIR}")
    print(f"proof of work: {start.pow.describe()}")
    if start.pow.name == "matmul":
        p = start.pow.params
        print(f"one attempt = {p.ops_per_attempt() / 1e9:.2f} billion int8 operations plus a "
              f"{p.slice_bytes / 2**20:.0f} MiB read, so rates below are not comparable with "
              f"SHA-256-style hashes")
    print(f"supply issued so far: {fmt(start.supply())} "
          f"(main emission cap: {fmt(start.max_supply)})")
    if start.tail_reward:
        print(f"tail emission: {fmt(start.tail_reward)} per block "
              f"after the main emission ends")

    if start.pow.name == "matmul" and searcher is None:
        if args.cpu:
            from tenero.pow import CpuSearcher
            searcher = CpuSearcher()
            print("CPU search: correct but very slow at full size.")
        else:
            try:
                searcher = make_gpu_searcher(args.skip_selftest)
            except RuntimeError as e:
                print("error:", e)
                return 1

    threads = build_threads_for(args.max_cores)
    if start.pow.name == "matmul":
        from tenero import matmulhash
        matmulhash.set_build_threads(threads)
        if args.no_double_check:
            print(f"CPU budget: at most {args.max_cores} cores; with no CPU checks the only "
                  f"CPU use is 1 core driving the GPU search")
        else:
            print(f"CPU budget: at most {args.max_cores} cores in total: 1 for the GPU search "
                  f"loop, 1 for the double-check, {threads} for building the CPU reference "
                  f"dataset")

    session = Session(args, address, searcher, mempool)
    session.overlap = (not start.pow.cheap and not args.no_overlap and not args.no_double_check)
    if session.overlap and wants_process_check(args, searcher):
        from tenero.checker import ProcessChecker
        session.checker = ProcessChecker(build_threads=threads)
    if start.pow.name == "matmul":
        if session.overlap:
            where = "in a separate low-priority process" if session.checker else "in a thread"
            print(f"overlap: each block is double-checked on the CPU {where} while the GPU "
                  f"searches for the next one (a check is fast once the CPU dataset is ready)")
            session.baseline = measure_baseline(searcher, start.pow)
            if session.baseline:
                print(f"GPU search speed with nothing else running: {session.baseline:,.0f} "
                      f"attempts/s")
        elif not args.no_double_check:
            print("no overlap: the GPU waits while each block is double-checked on the CPU")
    print("Ctrl+C to stop. Run only ONE miner at a time.\n")
    return session.run()


if __name__ == "__main__":
    sys.exit(main())
