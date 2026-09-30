# CPU benchmarks (the Rust code)

**Measured** with `cargo run --release --example bench -- --full` (`crates/tenero-core/examples/bench.rs`),
2026-09-29, on the owner's machine: AMD Ryzen 9 5900X (12 cores, 24 threads), Windows 11, Rust stable
(1.98), portable code with no `unsafe` and no hand-written SIMD. Nothing here is a GPU number, and one machine
is one data point (KNOWN_ISSUES 13). Rerun it before relying on any figure.

## Where the time goes (real parameters: m=64, k=8192, nb=2048, 256 slices)

| | default build (generic x86-64) | `RUSTFLAGS="-C target-cpu=native"` |
|---|---|---|
| ChaCha20 core, one thread | 82 ns (12.2 M/s) | 88 ns (11.4 M/s) |
| dataset build, 1 GiB-scale prefix, 1 thread | 313 MiB/s | 318 MiB/s |
| **full 4 GiB dataset, 4 threads** | **3.9 s** | not measured |
| **full 4 GiB dataset, 6 threads** | **2.7 s** | not measured |
| make X (512 KiB keystream) | 0.63 ms | 0.71 ms |
| **matmul `C = X @ W` (1.07 G multiply-adds)** | **175 ms (6.2 G/s)** | **35 ms (31 G/s)** |
| fold (8,192 ChaCha cores) | 0.66 ms | 0.73 ms |
| **one whole attempt, one thread** | **176 ms (5.7/s)** | **36 ms (27.8/s)** |
| cheap precheck (no dataset) | 0.2 us | not measured |
| attempts/s, 6 threads | 31.7 | 164 |

## GPU (the Rust engine, `crates/tenero-gpu`)

**Measured** with `cargo run --release -p tenero-gpu --example gpu_bench` on the same machine (RTX 5070 Ti,
compute capability 12.0, driver 617.14, CUDA toolkit 13.4), 2026-09-29. The engine is the first working
version and is **untuned**: one CUDA stream, one cuBLASLt call per attempt, the CPU hashing between batches
without overlapping the GPU. The search used a target that is never met, so every batch is fully computed.

| batch of attempts | attempts/s | ms per batch |
|---|---|---|
| 8 | 30,315 | 0.26 |
| 16 | 32,508 | 0.49 |
| 32 | 33,316 | 0.96 |
| 64 | 34,130 | 1.88 |
| 128 | 34,890 | 3.67 |
| 256 | 35,247 | 7.26 |

The whole 4 GiB dataset builds on the GPU in **0.116 s** (measured; the README's earlier figure is about
0.10 s). All 256 slices match `matmulhash_full.json`.

- **The Python miner's documented figure is about 22,000 attempts/s.** That was measured earlier, and the
  Python GPU path cannot run here now (no torch or CuPy in the venv), so this is **not** a same-day,
  same-setup comparison. The Rust engine is about 1.4 to 1.6 times that documented number.
- **It is close to memory bound.** Each attempt reads one 16 MiB slice, so 35,000 attempts/s is about
  590 GB/s of slice reads. Compare it with the card's spec-sheet bandwidth (about 900 GB/s, from the
  manufacturer, not measured here) before hoping for a big further gain: the remaining headroom is probably
  tens of percent, from overlapping the CPU work and the copies with the GPU.
- The correctness of every one of these runs is not assumed: the same engine passes 11 GPU tests (see
  `crates/tenero-gpu/tests/gpu_selftest.rs`), including the golden vectors and 256-attempt batches compared
  with the CPU one by one.

## What this says

- **The matmul is 99% of an attempt.** Everything else (the ChaCha keystream, the fold, SHA-256) is about
  1 ms. The dataset build is a once-per-epoch cost (every 100 blocks) of a few seconds.
- **The compiler's own AVX2 gives 5x on the matmul.** No `unsafe` and no code change: just letting it use
  the CPU's instructions. The default build targets generic x86-64 (SSE2 only), which is why it is slower.
- **Checking a block costs one attempt plus the epoch's dataset**: about 0.18 s (default) or 0.04 s
  (native) once the dataset exists, so validation is not a bottleneck either way.
- For scale only: the GPU baseline in `REWRITE_PLAN.md` (about 22,000 attempts/s, measured earlier on the
  owner's RTX 5070 Ti) is roughly 130 times the 6-thread native CPU figure. CPU mining is a test-chain
  curiosity, not a competitor.

## Syncing a chain with the real proof of work (measured on the owner's machine, 2026-10)

`cargo test --release -p tenero-chain --test real_pow_sync -- --ignored --nocapture`: 30 blocks mined on the CPU with
the real parameters (m 64, k 8192, nb 2048, 256 slices) at an easy target, then taken by fresh nodes. **Generic
x86-64 build (no `target-cpu=native`)**, so the per-block figure is the "default" one above. CPU time in
`Chain::submit_block` only: coinbase-only blocks, no transaction proofs, no network.

| | 100 blocks per dataset | 10 blocks per dataset (3 datasets) |
|---|---|---|
| everything checked, total for 30 blocks | 8.02 s | 13.55 s |
| a block, median (one attempt, dataset present) | 179.4 ms | 179.6 ms |
| the first block (builds the dataset) | 2.82 s | 2.81 s |
| so one dataset build is about | 2.64 s | 2.63 s |
| assume-valid, the 25 assumed blocks | 0.05 s (2.0 ms each) | 0.05 s (1.9 ms each) |
| assume-valid, the last 5 checked in full | 3.66 s (incl. one dataset build) | 3.54 s |
| assume-valid, whole run | 3.71 s | 3.59 s |

- **Agrees with the benchmarks above:** 0.18 s a block and 2.6 s a dataset (earlier: 0.18 s, 2.7 to 3.9 s).
  The 10-block-epoch run fits too: 3 builds (7.9 s) + 30 x 0.18 s = 13.3 s against 13.55 s measured.
- **What an assumed block costs:** about 2 ms, which is the non-proof checks and the state update, against 179 ms
  for a full check: about 90 times less, per block, on these coinbase-only blocks. Blocks with transactions cost
  more to apply; that is not measured here.
- **Estimates, not measurements** (a straight line from 30 blocks; a year is 525,600 blocks at one a minute, 5,256
  datasets): checking a year in full is about 26 hours of attempts plus about 4 hours of dataset builds on this
  build, against about 18 minutes to apply a year of assumed blocks, plus the tail checked in full. A
  `target-cpu=native` build should cut the attempt part several times (0.04 s a block in the table above); that
  was not run here.
- **What it does not include:** the network, the transaction proofs (their own cost, measured in M7), disk beyond
  the store, and any block with transactions in it.

## Caveats

- Multi-thread attempt timings are noisy: each thread does only 3 attempts, and one run gave 2 threads at
  6.4 attempts/s where a second run gave 11.3. Read the 1-thread rows and the trend, not the third digit.
- `target-cpu=native` makes a binary that only runs on CPUs with the same instruction sets (illegal
  instruction otherwise). The portable choice is `-C target-feature=+avx2`, which any 2013-or-later
  Intel/AMD desktop CPU has; that is a decision to make before shipping binaries, not measured here.
- The 6-thread rows use up to the miner's 6-core budget (`CLAUDE.md` rule 8); the machine has more.
- Later slices read from a larger part of the dataset, so per-slice build cost is not constant; the "full
  4 GiB" rows are real timings of the whole build, not extrapolations.
