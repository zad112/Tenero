"""GPU proof-of-work prototype test, v2 (int8 matmul against a big, sequentially dependent dataset).

Every attempt multiplies its own small int8 matrix (ChaCha20 keystream) by ONE slice of a large
dataset chosen by the attempt's seed, so different attempts read different memory. The dataset
lives in your GPU's VRAM (4 GiB by default, sized so a card with 8 GB holds it with room for the
desktop, the CUDA context and working buffers) and is built from ChaCha20 in a way that makes
each slice depend on the earlier ones (see tenero/matmulhash.py).

It needs CuPy for the CUDA kernels:  pip install "cupy-cuda13x[ctk]"

What it does, in order:
  1. Shows your GPU and matmul backend, and applies the VRAM budget (--vram-gb).
  2. SELF-TEST (small dataset): GPU vs CPU reference, bit for bit.
  3. Builds the full dataset in VRAM (timed) and compares the first slices, and real-size
     attempts on them, with the CPU reference. --full-cpu-check compares every slice (needs
     4 GiB of RAM and one to two minutes).
  4. MINE + VERIFY at full size: the GPU finds a solution; the cheap check accepts it in
     microseconds; the CPU then recomputes it (building the dataset up to the slice it used).
  5. BENCHMARK: memory bandwidth, matmul on cached vs random slices, the full pipeline.
  6. MEMORY HARDNESS: what not keeping the dataset in memory would cost, from this GPU's own
     build and attempt times and a simulation of the dependency structure.
  7. Memory summary.

  COMPARE MODE (--compare-m 64,128,192): the full pipeline for each number of rows per attempt
  on the same dataset, side by side, so you can pick POW_M for config.py.

  SWEEP MODE (--sweep): tries different rows-per-attempt, slice shapes and memory layouts to find
  where the matmul really streams near your card's bandwidth.

Usage (from the project folder):
    python gpu_pow_test.py                       # 4 GiB dataset, 8 GB budget
    python gpu_pow_test.py --sweep               # find the best shape
    python gpu_pow_test.py --dataset-gib 2       # try other dataset sizes
    python gpu_pow_test.py --full-cpu-check      # compare EVERY slice with the CPU (4 GiB RAM)
    python gpu_pow_test.py --cpu-only            # no GPU needed: CPU side only
"""
import argparse
import dataclasses
import sys
import time

import numpy as np

from tenero import analysis, chacha
from tenero import matmulhash as mh
from tenero.gpubackend import (  # noqa: F401 - re-exported: the tests use g.<name>
    CHUNK, EPOCH, HEADER, KERNEL_SOURCE, FusedKernels, TorchBackend, check, make_fused,
    self_test)

GIB = 2**30


# ---------------------------------------------------------------- helpers

def fmt_rate(x):
    for unit, size in (("G", 1e9), ("M", 1e6), ("k", 1e3)):
        if x >= size:
            return f"{x / size:.2f} {unit}"
    return f"{x:.1f} "


def search(attempts_fn, target, batch, start=0, seconds=None):
    # runs batches until a digest is below the target (or `seconds` pass). attempts_fn maps a
    # list of nonces to [(digest, mix)]. Returns (nonce, digest, mix, attempts, elapsed); nonce
    # is None if nothing was found.
    t0 = time.perf_counter()
    nonce, attempts = start, 0
    while True:
        nonces = list(range(nonce, nonce + batch))
        for n, (d, mix) in zip(nonces, attempts_fn(nonces)):
            if mh.meets_target(d, target):
                attempts += n - nonce + 1
                return n, d, mix, attempts, time.perf_counter() - t0
        nonce += batch
        attempts += batch
        if seconds is not None and time.perf_counter() - t0 >= seconds:
            return None, None, None, attempts, time.perf_counter() - t0


def verdict(frac_of_bandwidth):
    # is the matmul limited by memory? Judged against the MEASURED bandwidth, not by
    # comparing cached and random reads (which only shows that cache misses cost something)
    if frac_of_bandwidth >= 0.6:
        return f"memory-bound: streaming at {frac_of_bandwidth:.0%} of the measured bandwidth"
    return (f"NOT memory-bound: it streams only {frac_of_bandwidth:.0%} of the measured "
            f"bandwidth, so the kernel is the limit at this shape (try --sweep)")


def gen_matrix(backend, tag, rows, cols):
    """A deterministic (rows, cols) int8 matrix from the ChaCha20 kernel (for timing only)."""
    import hashlib
    key = np.frombuffer(hashlib.sha256(f"matrix {tag}".encode()).digest(), dtype="<u4")[None]
    flat = backend.fused.keystream(backend.to_device_keys(key), rows * cols // 64)[0]
    return flat.reshape(rows, cols).contiguous()


def raw_slice_words(W, b):
    # one GPU slice as the uint32 words the CPU reference uses
    return np.ascontiguousarray(W[b].cpu().numpy()).view(np.uint32).reshape(-1)


# ---------------------------------------------------------------- stages

def slices_with_attempts(params, checked, count=4, limit=200_000):
    # nonces whose attempt reads one of the first `checked` slices (so a partial CPU dataset
    # can recompute them): about count * num_blocks / checked nonces have to be scanned
    found, n = [], 0
    while len(found) < count and n < limit:
        if mh.attempt_slice(mh.attempt_seed(HEADER, n), params.num_blocks) < checked:
            found.append(n)
        n += 1
    return found


def full_size_checks(backend, params, checked=8, full_cpu=False):
    print(f"\n[3] FULL SIZE: building the {params.dataset_bytes / GIB:.2f} GiB dataset in VRAM")
    t0 = time.perf_counter()
    W = backend.build_dataset(params, EPOCH,
                              progress=lambda d, n: print(f"    {d}/{n} slices", flush=True))
    backend.sync()
    build = time.perf_counter() - t0
    print(f"  built {params.num_blocks} slices of {params.slice_bytes / 2**20:.0f} MiB in "
          f"{build:.2f}s = {build / params.num_blocks * 1e6:.0f} us per slice "
          f"(once per epoch; each slice is built from earlier ones)")
    ok = True
    checked = min(checked, params.num_blocks)
    # the fill of slice j does not depend on how many slices there are, so a small dataset is
    # exactly the first slices of the full one
    partial = mh.build_dataset(dataclasses.replace(params, num_blocks=checked), EPOCH)
    picks = sorted({0, 1, checked // 2, checked - 1} & set(range(checked)))
    for b in picks:
        ok &= check(f"slice {b} (real size) equals the CPU reference",
                    np.array_equal(raw_slice_words(W, b), partial[b]))
    nonces = slices_with_attempts(params, checked)
    if nonces:
        ok &= check(f"{len(nonces)} real-size attempts (hash and mix, on slices below {checked}) "
                    f"match the CPU reference",
                    backend.attempts(params, W, HEADER, nonces)
                    == mh.compute_attempts(params, partial, HEADER, nonces))
    if full_cpu:
        print(f"  building the whole dataset on the CPU ({params.dataset_bytes / GIB:.2f} GiB of "
              f"RAM, one to two minutes)...")
        t0 = time.perf_counter()
        full = mh.build_dataset(params, EPOCH)
        print(f"  CPU build took {time.perf_counter() - t0:.0f}s")
        same = all(np.array_equal(raw_slice_words(W, b), full[b])
                   for b in range(params.num_blocks))
        ok &= check(f"ALL {params.num_blocks} slices equal the CPU reference", same)
        del full
    else:
        print(f"  (only the first {checked} slices are compared with the CPU by default; "
              f"--full-cpu-check compares all {params.num_blocks})")
    return ok, W, build


def verify_with_prefix(params, header_hash, nonce, digest, mix, target):
    """Full CPU check of one solution. The CPU needs only slices 0..b, where b is the slice the
    attempt read. Returns (ok, seconds, note)."""
    seed = mh.attempt_seed(header_hash, nonce)
    b = mh.attempt_slice(seed, params.num_blocks)
    t0, cpu0 = time.perf_counter(), time.process_time()
    cheap_ok = mh.precheck(header_hash, nonce, mix, digest.hex(), target)
    cheap = time.perf_counter() - t0
    prefix = mh.build_dataset(dataclasses.replace(params, num_blocks=b + 1), EPOCH)
    (d, m), = mh.compute_attempts(params, prefix, header_hash, [nonce])
    seconds, cpu = time.perf_counter() - t0, time.process_time() - cpu0
    ok = cheap_ok and d == digest and m == mix and mh.meets_target(d, target)
    return ok, seconds, (f"the cheap check took {cheap * 1e6:.0f} us; the CPU then rebuilt "
                         f"slices 0..{b} and redid the attempt, using {cpu:.1f}s of CPU time "
                         f"= {cpu / max(seconds, 1e-9):.1f} cores on average")


def mine_and_verify(attempts_fn, verify_fn, bits, batch, label):
    print(f"\n[4] MINE + VERIFY: find a solution on the {label}, check it cheaply, then re-check "
          f"it on the CPU")
    target = mh.bits_to_target(bits)
    nonce, digest, mix, attempts, dt = search(attempts_fn, target, batch)
    print(f"  found nonce {nonce} after {attempts:,} attempts in {dt:.2f}s "
          f"(expected about {2**bits:,})")
    good, cpu_dt, note = verify_fn(nonce, digest, mix, target)
    check("the CPU independently confirms the solution", good,
          f"{cpu_dt:.2f}s in total; {note}")
    return good, cpu_dt


def measure_bandwidth(backend, size_mib=512, reps=10, trials=5, warmup_s=0.5):
    # a big copy reads AND writes every byte. It is warmed up first (a cold GPU runs
    # slow clocks and reads low) and the best of several trials is kept.
    t = backend.torch
    a = t.empty(size_mib * 2**20, dtype=t.int8, device=backend.device)
    b = t.empty_like(a)
    b.copy_(a)
    backend.sync()
    t0 = time.perf_counter()
    while time.perf_counter() - t0 < warmup_s:
        b.copy_(a)
    backend.sync()
    best = 0.0
    for _ in range(trials):
        t0 = time.perf_counter()
        for _ in range(reps):
            b.copy_(a)
        backend.sync()
        best = max(best, 2 * size_mib * 2**20 * reps / (time.perf_counter() - t0))
    del a, b
    return best


def stage_breakdown(backend, params, W, batch, batches=10):
    # where does a batch of attempts spend its time? each stage is timed on its own
    stages = dict(cpu_seeds=0.0, generate=0.0, matmul=0.0, fold=0.0, cpu_finalize=0.0)
    nonce = 10_000
    for _ in range(batches):
        nonces = list(range(nonce, nonce + batch))
        nonce += batch
        t0 = time.perf_counter()
        seeds = [mh.attempt_seed(HEADER, n) for n in nonces]
        blocks = [mh.attempt_slice(s, params.num_blocks) for s in seeds]
        keys = np.stack([chacha.key_words(s) for s in seeds])
        stages["cpu_seeds"] += time.perf_counter() - t0

        backend.sync()
        t0 = time.perf_counter()
        X = backend.stage_generate(params, keys)
        backend.sync()
        stages["generate"] += time.perf_counter() - t0

        t0 = time.perf_counter()
        Cs = backend.stage_matmul(params, X, W, blocks)
        backend.sync()
        stages["matmul"] += time.perf_counter() - t0

        t0 = time.perf_counter()
        sums = backend.stage_fold(params, Cs).cpu().numpy()
        stages["fold"] += time.perf_counter() - t0

        t0 = time.perf_counter()
        for seed, row in zip(seeds, sums):
            mh.digest_of(seed, mh.mix_bytes(row))
        stages["cpu_finalize"] += time.perf_counter() - t0
    total = sum(stages.values())
    return {name: v / batches for name, v in stages.items()}, total / batches


def memory_hardness_report(params, build_seconds, t_attempt, table=None, log=print):
    """What keeping less than the whole dataset would cost, from measured GPU times and a
    simulation of the dependency structure. Returns a dict of the numbers."""
    n = params.num_blocks
    t_slice = build_seconds / n
    log("\n[6] MEMORY HARDNESS: what would NOT keeping the whole dataset in memory cost?")
    log(f"  building all {n} slices on this GPU took {build_seconds:.2f}s = "
        f"{t_slice * 1e6:.0f} us per slice; one attempt takes {t_attempt * 1e6:.0f} us")
    prefix = t_slice * n / 2
    log(f"  rebuilding ONE slice from nothing needs every slice before it: on average {n // 2} "
        f"slices = {prefix * 1e3:.1f} ms = {prefix / t_attempt:,.0f}x the cost of an attempt")
    if table is None:
        table = analysis.rebuild_cost_table(num_slices=n)
    log(f"\n  keeping only PART of the dataset (a simulation of the dependency structure at the "
        f"real depth of {n} slices,")
    log("  12 random cases per row; the slowdown uses this GPU's measured slice and attempt times):")
    log(f"  {'kept':>6} {'kept GiB':>9} {'rebuild one missing slice':>27} {'est. slowdown':>14}")
    for f in sorted(table, reverse=True):
        cost = table[f]
        slow = analysis.estimated_slowdown(f, cost, t_slice, t_attempt)
        log(f"  {f:>6.0%} {f * params.dataset_bytes / GIB:>9.2f} {cost:>21.1f} slices' work "
            f"{slow:>12.1f}x")
    within = analysis.fraction_within(table, t_slice, t_attempt, 2.0)
    if within is not None:
        log(f"\n  within 2x of full speed a miner still needs about {within:.0%} of the dataset "
            f"({within * params.dataset_bytes / GIB:.1f} GiB of {params.dataset_bytes / GIB:.1f}); "
            f"below that it falls off steeply")
    else:
        log("\n  every fraction in the table costs more than 2x: the whole dataset is needed")
    log("  (a simulation of the dependency structure, not a proof; custom hardware with far more")
    log("   memory bandwidth per dollar would still gain, and the fill and fold are unaudited)")
    return dict(t_slice=t_slice, prefix_seconds=prefix, table=table, within=within)


def benchmark(backend, params, W, seconds, batch, build_seconds=None, table=None):
    slice_mib = params.slice_bytes / 2**20
    print(f"\n[5] BENCHMARK: m={params.m} k={params.k} nb={params.nb}, "
          f"{slice_mib:.0f} MiB slice per attempt, batch of {batch}")
    bw = measure_bandwidth(backend)
    print(f"  memory bandwidth (measured with a big copy): {bw / 1e9:8.0f} GB/s")

    X = gen_matrix(backend, "bench", params.m, params.k)
    rng = np.random.RandomState(1)
    reps = 100

    def timed(blocks):
        backend.matmul(X, backend.view(W, blocks[0]))
        backend.sync()
        t0 = time.perf_counter()
        for b in blocks:
            backend.matmul(X, backend.view(W, b))
        backend.sync()
        return (time.perf_counter() - t0) / len(blocks)

    dt_same = timed([0] * reps)
    dt_rand = timed(list(rng.randint(0, params.num_blocks, size=reps, dtype=np.int64)))
    for label, dt in (("same slice every time (cache-friendly)", dt_same),
                      ("random slices (the real workload)    ", dt_rand)):
        tops = params.ops_per_attempt() / dt / 1e12
        gbs = params.slice_bytes / dt / 1e9
        print(f"  matmul, {label}: {tops:6.1f} TOPS, streams {gbs:6.0f} GB/s, "
              f"{dt * 1e6:6.0f} us")
    slow = dt_rand / dt_same
    frac = params.slice_bytes / dt_rand / bw
    print(f"  -> random slices take {slow:.2f}x as long as cached ones; " + verdict(frac))

    # the full pipeline: seeds -> ChaCha20 X -> matmul on a slice -> fold -> digests
    _, _, _, attempts, elapsed = search(lambda n: backend.attempts(params, W, HEADER, n),
                                        0, batch, seconds=seconds)
    rate = attempts / elapsed
    t_attempt = 1 / rate
    tops = rate * params.ops_per_attempt() / 1e12
    print(f"  full pipeline: {rate:8.0f} attempts/s = {tops:5.1f} TOPS, streams "
          f"{rate * params.slice_bytes / 1e9:5.0f} GB/s ({100 * rate * params.slice_bytes / bw:.0f}% "
          f"of the copy bandwidth)")
    print(f"  pipeline efficiency: {100 * dt_rand / t_attempt:.0f}% of the raw random-slice matmul "
          f"(the rest is ChaCha20 generation, the fold and hashing)")

    stages, total = stage_breakdown(backend, params, W, batch)
    print(f"\n  where one batch of {batch} attempts spends its time ({total * 1e3:.2f} ms total):")
    labels = dict(cpu_seeds="CPU: seeds + hashing setup", generate="GPU: ChaCha20 X matrices",
                  matmul="GPU: matrix multiplies", fold="GPU: ChaCha20 fold + sums",
                  cpu_finalize="CPU: final SHA-256")
    for name, v in stages.items():
        print(f"    {labels[name]:<28} {v * 1e3:7.2f} ms  {100 * v / total:4.0f}%")
    other = 100 * (total - stages["matmul"]) / total
    print(f"  -> {other:.0f}% of the time is outside the matmul (what is left is the ChaCha20 "
          f"kernels, copies, launch gaps and CPU hashing)")

    if build_seconds:
        memory_hardness_report(params, build_seconds, t_attempt, table)
    return rate


def memory_summary(torch, params, vram_gb):
    peak = torch.cuda.max_memory_allocated() / GIB
    total = torch.cuda.get_device_properties(0).total_memory / GIB
    print(f"\n[7] MEMORY: peak PyTorch allocation {peak:.2f} GiB "
          f"(dataset {params.dataset_bytes / GIB:.2f} GiB + working buffers)")
    print(f"  budget simulated: {vram_gb:g} GB. The CUDA context and desktop use roughly "
          f"another 0.5-1.5 GB on top.")
    spare = vram_gb - peak
    print(f"  headroom on a {vram_gb:g} GB card: about {spare:.1f} GiB before the context/desktop"
          f" ({'OK' if spare >= 2 else 'TIGHT: consider a smaller dataset'}); "
          f"your card has {total:.1f} GiB in total")


SWEEP_SHAPES = [(2048, 2048), (4096, 4096), (8192, 2048), (2048, 8192), (8192, 8192)]
SWEEP_MS = (32, 64, 128, 256)


def classify(pct_bw, pct_peak):
    # what limits a configuration: memory bandwidth, tensor-core compute, or neither
    if pct_bw >= 60:
        return "memory-bound"
    if pct_peak >= 60:
        return "compute-bound"
    return "neither (kernel/latency)"


def time_matmuls(backend, X, mats):
    backend.matmul(X, mats[0])
    backend.sync()
    t0 = time.perf_counter()
    for w in mats:
        backend.matmul(X, w)
    backend.sync()
    return (time.perf_counter() - t0) / len(mats)


def measure_peak_tops(backend, size=4096, reps=20):
    # the best int8 speed this card reaches on one big matmul: the compute ceiling
    A = gen_matrix(backend, "peak a", size, size)
    B = gen_matrix(backend, "peak b", size, size)
    best = 0.0
    for mat in (B, B.t()):
        try:
            dt = time_matmuls(backend, A, [mat] * reps)
        except Exception:  # noqa: BLE001 - this layout is not supported here
            continue
        best = max(best, 2 * size**3 / dt / 1e12)
    return best


def run_sweep(backend, bw, peak, shapes, ms, gib, reps, log=print):
    # for every slice shape and memory layout, time the matmul on RANDOM slices of a
    # dataset in VRAM, for several rows-per-attempt. Returns (results, skipped).
    rng = np.random.RandomState(7)
    results, skipped = [], []
    for k, nb in shapes:
        slice_bytes = k * nb
        num_blocks = max(4, int(gib * GIB) // slice_bytes)
        for layout in ("row", "col"):
            rows, cols = (k, nb) if layout == "row" else (nb, k)
            W = None
            try:
                W = backend.build_random_dataset(num_blocks, rows, cols)
                for m in ms:
                    X = gen_matrix(backend, f"sweep {m} {k}", m, k)
                    picks = rng.randint(0, num_blocks, size=reps, dtype=np.int64)
                    mats = [W[int(b)] if layout == "row" else W[int(b)].t() for b in picks]
                    dt = time_matmuls(backend, X, mats)
                    gbs = slice_bytes / dt / 1e9
                    tops = 2 * m * slice_bytes / dt / 1e12
                    pct_bw = 100 * gbs * 1e9 / bw
                    pct_peak = 100 * tops / peak if peak else 0.0
                    results.append(dict(k=k, nb=nb, layout=layout, m=m, dt=dt, gbs=gbs,
                                        tops=tops, pct_bw=pct_bw, pct_peak=pct_peak,
                                        regime=classify(pct_bw, pct_peak)))
                log(f"  done {k}x{nb} ({slice_bytes / 2**20:.0f} MiB slices), layout {layout}")
            except Exception as e:  # noqa: BLE001 - record and carry on
                skipped.append((k, nb, layout, f"{type(e).__name__}: {str(e)[:70]}"))
                log(f"  skipped {k}x{nb}, layout {layout}: {type(e).__name__}")
            finally:
                del W
                empty = getattr(getattr(backend.torch, "cuda", None), "empty_cache", None)
                if empty:
                    empty()
    return results, skipped


def rescore(results, bw, peak):
    # a read can never stream faster than the memory can deliver. If the sweep saw
    # streaming faster than the copy test measured, the copy test read low: trust the
    # higher figure and recompute the percentages. Returns (results, bandwidth used).
    seen = max((r["gbs"] for r in results), default=0) * 1e9
    if seen > bw * 1.05:
        bw = seen
        for r in results:
            r["pct_bw"] = 100 * r["gbs"] * 1e9 / bw
            r["regime"] = classify(r["pct_bw"], r["pct_peak"])
    return results, bw


def pick_best(results):
    # the configuration that streams closest to the memory bandwidth (fewest rows on ties)
    return min(results, key=lambda r: (-round(r["pct_bw"], 1), r["m"])) if results else None


def report_sweep(results, skipped, peak, bw):
    print(f"\n  {'slice':>11} {'MiB':>4} {'layout':>6} {'m':>4} {'us':>7} {'TOPS':>6} "
          f"{'GB/s':>6} {'%BW':>5} {'%peak':>5}  limit")
    for r in results:
        print(f"  {r['k']:>5}x{r['nb']:<5} {r['k'] * r['nb'] / 2**20:>4.0f} {r['layout']:>6} "
              f"{r['m']:>4} {r['dt'] * 1e6:>7.0f} {r['tops']:>6.1f} {r['gbs']:>6.0f} "
              f"{r['pct_bw']:>5.0f} {r['pct_peak']:>5.0f}  {r['regime']}")
    for k, nb, layout, why in skipped:
        print(f"  {k:>5}x{nb:<5} {layout:>6}: not supported here ({why})")
    if not results:
        print("  no configuration ran")
        return None
    best = pick_best(results)
    print(f"\n  measured limits: bandwidth {bw / 1e9:.0f} GB/s, peak int8 {peak:.0f} TOPS")
    if peak:
        print(f"  balance point: about m = {peak * 1e12 / (2 * bw):.0f} rows "
              f"(below it a slice takes longer to read than to multiply)")
    print(f"  BEST: {best['k']}x{best['nb']} slices, layout {best['layout']}, m={best['m']}: "
          f"{best['dt'] * 1e6:.0f} us per attempt, {best['pct_bw']:.0f}% of bandwidth, "
          f"{best['pct_peak']:.0f}% of peak compute ({best['regime']})")
    if best["pct_bw"] < 60:
        print("  -> nothing streams near the bandwidth: the int8 matmul kernel, not memory,")
        print("     still limits every shape tried. A custom kernel would be the next step.")
    return best


def time_cpu_verify(k, nb, m):
    # one CPU attempt at this shape with the dataset already in memory (only 2 slices are
    # built: what an attempt costs does not depend on how many there are)
    p = mh.Params(m=m, k=k, nb=nb, num_blocks=2).validate()
    data = mh.build_dataset(p, EPOCH)
    t0 = time.perf_counter()
    mh.compute_attempts(p, data, HEADER, [0])
    return time.perf_counter() - t0


def raw_matmul_seconds(backend, W, X, num_blocks, reps=100, seed=5):
    # the matmul alone, on random slices: seconds per attempt
    rng = np.random.RandomState(seed)
    picks = [int(b) for b in rng.randint(0, num_blocks, size=reps, dtype=np.int64)]
    backend.matmul(X, backend.view(W, picks[0]))
    backend.sync()
    t0 = time.perf_counter()
    for b in picks:
        backend.matmul(X, backend.view(W, b))
    backend.sync()
    return (time.perf_counter() - t0) / reps


def compare_m(backend, params, ms, seconds, batch, verify=True, log=print):
    """Full-pipeline speed for several rows-per-attempt values on ONE dataset (the dataset does
    not depend on m). Returns a list of result dicts."""
    W = backend.build_dataset(params, EPOCH)
    backend.sync()
    bw = measure_bandwidth(backend)
    log(f"  memory bandwidth (big copy): {bw / 1e9:.0f} GB/s; slices of "
        f"{params.slice_bytes / 2**20:.0f} MiB")
    rows = []
    for m in ms:
        p = dataclasses.replace(params, m=m).validate()
        _, _, _, attempts, elapsed = search(lambda n: backend.attempts(p, W, HEADER, n), 0, batch,
                                            seconds=seconds)
        rate = attempts / elapsed
        X = gen_matrix(backend, f"compare {m}", m, p.k)
        raw = raw_matmul_seconds(backend, W, X, p.num_blocks)
        rows.append(dict(
            m=m, rate=rate, tops=rate * p.ops_per_attempt() / 1e12,
            gbs=rate * p.slice_bytes / 1e9, matmul_us=raw * 1e6,
            raw_tops=p.ops_per_attempt() / raw / 1e12,
            verify=time_cpu_verify(p.k, p.nb, m) if verify else None))
        log(f"  measured m={m}")
    log(f"\n  {'m':>5} {'attempts/s':>11} {'effective TOPS':>15} {'streams GB/s':>13} "
        f"{'%BW':>5} {'matmul us':>10} {'CPU check':>10}")
    for r in rows:
        log(f"  {r['m']:>5} {r['rate']:>11,.0f} {r['tops']:>15.1f} {r['gbs']:>13.0f} "
            f"{100 * r['gbs'] * 1e9 / bw:>5.0f} {r['matmul_us']:>10.0f} "
            f"{('%.2fs' % r['verify']) if r['verify'] else '-':>10}")
    if rows:
        base = rows[0]
        keep = [r["m"] for r in rows if r["rate"] >= 0.85 * base["rate"]]
        log(f"\n  the attempt rate stays within 15% of m={base['m']} up to m={max(keep)}: "
            f"beyond that the tensor cores, not memory, start to limit an attempt")
        log("  a larger m raises effective TOPS at a similar attempt rate (more math per slice "
            "read) and makes each CPU check a little slower. m is fixed when a chain is created:")
        log("  set POW_M in config.py, then start a new chain.")
    return rows


def compare_main(backend, args):
    ms = [int(x) for x in args.compare_m.split(",") if x.strip()]
    if any(m <= 16 for m in ms):
        print("every --compare-m value must be greater than 16")
        return 1
    params = mh.params_for_dataset(args.dataset_gib, m=ms[0], k=args.k, nb=args.nb)
    print(f"\n[3] COMPARE rows per attempt on one {params.dataset_bytes / GIB:.2f} GiB dataset "
          f"({args.seconds:g}s each)")
    compare_m(backend, params, ms, args.seconds, args.batch, verify=not args.no_cpu_verify)
    return 0


def run_cpu_only(args):
    print("[1] CPU-only mode: testing the CPU reference (no GPU stages)")
    p = mh.Params(m=args.m, k=1024, nb=1024, num_blocks=16).validate()
    print(f"  using a small dataset ({p.dataset_bytes / 2**20:.0f} MiB) so this runs quickly")
    t0 = time.perf_counter()
    data = mh.build_dataset(p, EPOCH)
    print(f"  built it on the CPU in {time.perf_counter() - t0:.2f}s")

    def verify_fn(nonce, digest, mix, target):
        t0 = time.perf_counter()
        cheap = mh.precheck(HEADER, nonce, mix, digest.hex(), target)
        (d, m), = mh.compute_attempts(p, data, HEADER, [nonce])
        return (cheap and d == digest and m == mix), time.perf_counter() - t0, \
            "the cheap check, then a full recomputation"

    mine_and_verify(lambda n: mh.compute_attempts(p, data, HEADER, n), verify_fn,
                    min(args.difficulty_bits, 6), 4, "CPU")
    _, _, _, attempts, elapsed = search(lambda n: mh.compute_attempts(p, data, HEADER, n),
                                        0, 4, seconds=min(args.seconds, 5))
    print(f"\n  CPU speed (numpy reference): {attempts / elapsed:,.1f} attempts/s")
    return 0


def sweep_main(backend, args):
    ms = [int(x) for x in args.sweep_ms.split(",") if x.strip()]
    if any(m <= 16 for m in ms):
        print("every --sweep-ms value must be greater than 16")
        return 1
    print("\n[3] SWEEP: measuring limits")
    bw = measure_bandwidth(backend)
    peak = measure_peak_tops(backend)
    print(f"  memory bandwidth (big copy): {bw / 1e9:.0f} GB/s")
    print(f"  peak int8 matmul (one big multiply): {peak:.0f} TOPS")
    print(f"\n[4] SWEEP: random slices from a {args.sweep_gib:g} GiB dataset, "
          f"{args.sweep_reps} per configuration")
    results, skipped = run_sweep(backend, bw, peak, SWEEP_SHAPES, ms, args.sweep_gib,
                                 args.sweep_reps)
    results, used_bw = rescore(results, bw, peak)
    if used_bw != bw:
        print(f"\n  note: the copy test read {bw / 1e9:.0f} GB/s but the sweep streamed up to "
              f"{used_bw / 1e9:.0f} GB/s,")
        print("  so the copy test was low (a cold GPU?). Using the higher figure for the "
              "percentages below.")
        bw = used_bw
    best = report_sweep(results, skipped, peak, bw)
    if best is None:
        return 1
    print(f"\n  to run the full test with the best shape: python gpu_pow_test.py "
          f"--m {best['m']} --k {best['k']} --nb {best['nb']}")
    return 0


def main():
    ap = argparse.ArgumentParser(description="int8 matmul + big sequential dataset proof-of-work "
                                             "prototype")
    ap.add_argument("--m", type=int, default=64, help="rows per attempt (GPU needs > 16; default 64)")
    ap.add_argument("--k", type=int, default=8192, help="shared dimension (default 8192)")
    ap.add_argument("--nb", type=int, default=2048, help="slice columns (default 2048: 16 MiB slices)")
    ap.add_argument("--dataset-gib", type=float, default=4.0,
                    help="dataset size in GiB (default 4, sized for an 8 GB card)")
    ap.add_argument("--vram-gb", type=float, default=8.0,
                    help="VRAM budget to simulate/enforce for PyTorch allocations (default 8)")
    ap.add_argument("--batch", type=int, default=32, help="attempts per GPU batch (default 32)")
    ap.add_argument("--seconds", type=float, default=10, help="benchmark length (default 10)")
    ap.add_argument("--difficulty-bits", type=int, default=8,
                    help="mine+verify demo needs about 2**bits attempts (default 8)")
    ap.add_argument("--checked-slices", type=int, default=8,
                    help="how many of the first slices to compare with the CPU (default 8)")
    ap.add_argument("--full-cpu-check", action="store_true",
                    help="build the whole dataset on the CPU (4 GiB of RAM, one to two minutes) and "
                         "compare EVERY slice with the GPU's")
    ap.add_argument("--cpu-threads", type=int, default=4,
                    help="threads for the CPU reference dataset builds (default 4)")
    ap.add_argument("--backend", choices=("auto", "int_mm", "fp32"), default="auto")
    ap.add_argument("--cpu-only", action="store_true", help="skip the GPU stages")
    ap.add_argument("--compare-m", default=None,
                    help="compare these rows-per-attempt values, e.g. 64,128,192")
    ap.add_argument("--sweep", action="store_true",
                    help="find the best rows/shape/layout for the matmul")
    ap.add_argument("--sweep-ms", default=",".join(map(str, SWEEP_MS)),
                    help="rows per attempt to try (default 32,64,128,256)")
    ap.add_argument("--sweep-gib", type=float, default=1.0,
                    help="dataset size used for each sweep measurement (default 1)")
    ap.add_argument("--sweep-reps", type=int, default=200,
                    help="random slices timed per configuration (default 200)")
    ap.add_argument("--no-cpu-verify", action="store_true",
                    help="skip timing the CPU check in compare mode")
    args = ap.parse_args()
    mh.set_build_threads(args.cpu_threads)

    if args.cpu_only:
        return run_cpu_only(args)
    if args.m <= 16:
        print("--m must be greater than 16 for the GPU int8 matmul")
        return 1
    params = mh.params_for_dataset(args.dataset_gib, m=args.m, k=args.k, nb=args.nb)

    try:
        import torch
    except ImportError:
        print("PyTorch is not installed in this Python. Install a CUDA build, or run with "
              "--cpu-only.")
        return 1
    if not torch.cuda.is_available():
        print("PyTorch is installed but cannot see a CUDA GPU (is it a CPU-only build?).")
        print("Run with --cpu-only to test the CPU side.")
        return 1

    device = torch.device("cuda")
    torch.backends.cuda.matmul.allow_tf32 = False  # keep the fp32 fallback exact
    props = torch.cuda.get_device_properties(0)
    total_gib = props.total_memory / GIB
    if args.vram_gb < total_gib:
        # makes PyTorch fail loudly if it needs more than an 8 GB card would have
        torch.cuda.set_per_process_memory_fraction(args.vram_gb / total_gib)
    print("[1] ENVIRONMENT")
    print(f"  GPU: {props.name}, compute capability {props.major}.{props.minor}, "
          f"{total_gib:.1f} GiB")
    print(f"  torch {torch.__version__}, CUDA {torch.version.cuda}")
    print(f"  VRAM budget for PyTorch: {min(args.vram_gb, total_gib):g} GB "
          f"(dataset: {params.dataset_bytes / GIB:.2f} GiB, {params.num_blocks} slices)")
    try:
        fused = make_fused(torch, device)
    except RuntimeError as e:
        print("error:", e)
        return 1
    backend = TorchBackend(torch, device, args.backend, fused=fused)
    print(f"  matmul backend: {backend.name}")
    print(f"  ChaCha20 generation, dataset fill and fold: {backend.kernels_name} (CuPy)")

    if not self_test(backend):
        print("\nSELF-TEST FAILED. The GPU and CPU disagree, so consensus would break. "
              "Please send me this output.")
        return 2
    if args.compare_m:
        return compare_main(backend, args)
    if args.sweep:
        return sweep_main(backend, args)
    try:
        ok, W, build_seconds = full_size_checks(backend, params, args.checked_slices,
                                                args.full_cpu_check)
    except torch.cuda.OutOfMemoryError:
        print(f"\nOut of memory at {params.dataset_bytes / GIB:.2f} GiB within the "
              f"{args.vram_gb:g} GB budget. Try a smaller --dataset-gib.")
        return 3
    if not ok:
        print("\nFULL-SIZE CHECK FAILED. Please send me this output.")
        return 2
    mine_and_verify(lambda n: backend.attempts(params, W, HEADER, n),
                    lambda nonce, digest, mix, target: verify_with_prefix(
                        params, HEADER, nonce, digest, mix, target),
                    args.difficulty_bits, args.batch, "GPU")
    benchmark(backend, params, W, args.seconds, args.batch, build_seconds)
    memory_summary(torch, params, args.vram_gb)
    return 0


if __name__ == "__main__":
    sys.exit(main())
