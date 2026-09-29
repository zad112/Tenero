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

## Caveats

- Multi-thread attempt timings are noisy: each thread does only 3 attempts, and one run gave 2 threads at
  6.4 attempts/s where a second run gave 11.3. Read the 1-thread rows and the trend, not the third digit.
- `target-cpu=native` makes a binary that only runs on CPUs with the same instruction sets (illegal
  instruction otherwise). The portable choice is `-C target-feature=+avx2`, which any 2013-or-later
  Intel/AMD desktop CPU has; that is a decision to make before shipping binaries, not measured here.
- The 6-thread rows use up to the miner's 6-core budget (`CLAUDE.md` rule 8); the machine has more.
- Later slices read from a larger part of the dataset, so per-slice build cost is not constant; the "full
  4 GiB" rows are real timings of the whole build, not extrapolations.
