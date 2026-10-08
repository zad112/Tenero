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

## GPU: the driver alone, no CUDA Toolkit (measured on the owner's machine, 2026-10-07)

The kernels are compiled ahead of time (`crates/tenero-gpu/kernels/build_kernels.py`: NVRTC, `ptxas` and `fatbinary` from CUDA
13.4; machine code for sm_80, 86, 89, 90, 100, 103 and 120 and PTX for compute_80) and embedded in the program, and the int8
multiply of BOTH designs is our own kernel (`gather.cu`; cuBLASLt is gone). RTX 5070 Ti, driver 617.14, card otherwise idle,
single runs, **with every CUDA Toolkit folder removed from PATH and `CUDA_PATH` cleared** (NVRTC and cuBLASLt could not be found):

* **All 16 GPU tests passed** (`gpu_selftest` 12, `gather_selftest` 4: the golden vectors, every one of the 256 slices, both designs
  at the real parameters, bit for bit with the CPU), and `gpu_mining` mined 20 blocks, all accepted by the node's CPU check, and 20
  more across a fork at height 10.
* **The only CUDA libraries in the running benchmark's process** were the driver's `nvcuda.dll` and `nvcuda64.dll` (read from the
  process's module list): no `nvrtc`, `cublas` or `cudart`.

| | attempts/s |
|---|---|
| the gathered attempt (`beta` and `dev` from block 500), batch 128 to 512 | 43,080 to 43,523 (unchanged: it never used cuBLASLt) |
| the first design, grouped 16 to a slice and pipelined, batch 256 / 512 (`gpu_bench`) | 79,080 / 78,879 |
| the same, the miner backend (`gpu_mining`), batch 32 / 64 / 128 / 256 | 71,550 / 76,660 / 79,516 / 80,956 |
| the first design, not grouped, batch 256 / 512 | 61,867 / 73,190 (47,000 with cuBLASLt) |
| **for comparison, the first design grouped on cuBLASLt (the section below)** | about 125,000 |

* **What it costs:** the first design (`alpha`, and `dev` below block 500) is about 37 % slower when grouped, because our multiply's
  compute ceiling (about 76,000 to 80,000 attempts/s when every attempt reads the same columns, `gather_bench`) is about half of
  cuBLASLt's. The gathered attempt is limited by memory, not by the multiply, so it loses nothing.
* **What was tried and dropped:** launching the attempts side by side on one column tile (so that attempts sharing a slice read it
  together) made the gathered attempt slower, about 32,000 against 43,000, because each attempt's X was then read once per tile.
* **Not tested:** any GPU but the owner's RTX 5070 Ti. The machine code for the other generations was built, not run.

## GPU: attempts grouped by slice (measured on the owner's machine, 2026-10-07)

**About 120,000 to 130,000 attempts/s on the RTX 5070 Ti, against 33,000 to 36,000 before: about 3.5 times.** Measured with
`cargo run --release -p tenero-gpu --example gpu_bench -- --seconds 3` (and `--groups`, `--batches`, `--no-pipeline`, `--tune`)
and with the miner's own speed test (`gpu_mining`, `how_many_attempts_per_second...`, 10 s per batch size), the card otherwise
idle (the owner stopped their miner for it). One machine, one evening, single runs; earlier runs on this card varied by up to
about 7 %, so read the second digit, not the third. **The proof of work is unchanged**: every attempt is the same attempt, the 12
GPU tests (golden vectors, the full 256-slice vector, every attempt of grouped and pipelined batches against the CPU) pass, and a
GPU-mined chain of 20 blocks was accepted in full by the node's CPU check.

**What changed, and what each step gave** (pipelined, batch 256 to 512, except where said):

| step | attempts/s |
|---|---|
| before (one multiply per attempt, batch 256) | about 35,000 |
| attempts of a batch that happen to share a slice multiplied together (batch 256, nonces in plain order) | 47,000 |
| nonces chosen 16 to a slice (`group::SliceGrouper`) | 97,000 to 98,000 |
| + keystream and fold kernels with coalesced memory access (`kernels/fast.cu`) | 121,000 to 124,000 |
| + two batches in flight (the CPU's hashing overlaps the GPU) | 126,000 to 127,000 |
| groups of 32 instead of 16 | 129,000 to 130,000 (within noise of 16) |
| the miner backend itself (`GpuBackend`, group 16, batch 32 / 64 / 128 / 256) | 119,966 / 123,888 / 122,210 / 124,847 |

* **Why it works.** An attempt's slice is `attempt_slice(attempt_seed(header, nonce))`, two SHA-256 hashes, known before any
  matrix work, and the miner chooses its nonces. So it can collect nonces by slice and multiply 16 attempts' X matrices (stacked:
  a 1024 x 8192 matrix) by one read of the 16 MiB slice. The product of stacked rows is the stacked products, bit for bit.
* **Where the time goes now** (Nsight Systems, group 16, per 512 attempts): the int8 multiply 3.1 ms (about 6 us an attempt;
  34 G multiply-adds in about 96 us is about 350 T int8 operations a second, so the tensor cores, not memory, are the limit),
  the fold 0.34 ms and the keystream 0.30 ms (each about 800 to 900 GB/s of memory traffic), the rest gaps.
* **Choosing the multiply algorithm by timing** (`AttemptEngine::tune`) does not help at groups of 16 or 32: cuBLASLt's first
  choice was the fastest once the timing went round different slices. Timed on one slice it picked a worse one (the slice stays
  in the 48 MB L2 cache), which cost about 7 % in a real run. The miner does not tune.
* **Video memory:** two sets of batch buffers (2 MiB per attempt of the batch), so 512 MiB at batch 256 on top of the dataset.
* **This undercuts the README's ASIC argument** ("an attempt is limited by how fast memory can be read"): with grouping, an
  attempt costs about 1 MiB of slice reads (16 MiB / 16) and its speed is set by int8 multiply throughput. A chip with fast
  int8 units and modest memory bandwidth gains from this as much as a GPU does. Making it a consensus matter (for example a
  slice that cannot be known without doing the work) is a design decision, not made here.

## GPU: the gathered attempt (measured on the owner's machine, 2026-10-07, as a prototype; the rule on `beta` and `dev` from height 500, `CONSENSUS.md` 8.3)

`cargo run --release -p tenero-gpu --example gather_bench -- --seconds 3`, the card otherwise idle, one run each. The design
and why it was adopted are in `CONSENSUS.md` 8.3 and `THREAT_MODEL.md` E11. One batch at a time, no pipeline (as the miner does it).
The column numbering was settled after these runs (`ceil(nb/16)` keystream blocks, the same at the real parameters), so the
kernel measured is the one the miner uses.

| | |
|---|---|
| device-to-device copy of 1 GiB (reads plus writes) | 823 to 847 GB/s |
| reading gathered columns only, no multiply: 64 bytes of each column per step | 589 GB/s |
| the same, 128 / 256 / 512 / 1024 bytes per step | 860 to 878 GB/s |
| **honest gathered attempts** (batch 32 / 128 / 512) | **40,283 / 44,416 / 45,412 attempts/s** (676 to 762 GB/s of column reads) |
| impossible best case, every attempt reads the same columns | 69,363 / 79,367 / 80,831 attempts/s |
| the chain's design, for comparison (from the section above) | about 35,000 ungrouped, about 127,000 grouped |

* **Memory is the limit of the honest gathered attempt**: 45,400 attempts/s is about 16 MiB of column reads plus about 2 MiB of
  X and C traffic each, roughly 870 GB/s in all, the card's practical bandwidth.
* **The first kernel read 64 bytes of each column per step** and reached about 28,000 attempts/s; the read-only test above showed
  why (589 GB/s for that pattern) and the second kernel reads 128.
* The kernel is hand-written (`kernels/gather.cu`: cp.async, ldmatrix, mma.sync m16n8k32), and its best case is about half of
  what cuBLAS reaches on the slice design; a better kernel would raise the "same columns" row, not the honest one.
* Grouping does not apply: no two attempts share more than a few of their 2,048 columns (8 on average), and each column has a
  different set of sharers.

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

## The GPU miner again, with the rate meter and NVML (measured on the owner's machine, RTX 5070 Ti, 2026-10-02, evening)

Same machine and same test as below, run again after M10.2 (`how_many_attempts_per_second_the_gpu_backend_does_at_each_batch_size`, 15 s
per batch size after 5 s of warm-up, one run each):

| batch | sustained attempts/s |
|---|---|
| 32 | 32,353 |
| 64 | 31,758 |
| 128 | 33,348 |
| 256 | 32,997 |

* **That first run was about 6 to 7 % below the earlier run on the same card (34,058 to 35,959), but the gap did not repeat.** I ran the same
  test twice more, a few minutes later, with the 8 run 4 nodes still mining on the CPU exactly as in the first run (I was not allowed to pause
  them, so I could not test their effect directly):

  | batch | run 1 | run 2 | run 3 | the earlier run |
  |---|---|---|---|---|
  | 32 | 32,353 | 32,942 | 32,533 | 34,058 |
  | 64 | 31,758 | 33,966 | 33,595 | 34,790 |
  | 128 | 33,348 | 34,457 | 34,824 | 35,745 |
  | 256 | 32,997 | 35,344 | 35,566 | 35,959 |

  **What this shows:** at the larger batch sizes runs 2 and 3 are back within about 1.5 % of the earlier run (256: 35,344 and 35,566 against
  35,959), and run 1 was the low outlier (at batch 256, 6.6 % under run 3). Run 4 was running in all three, so it does not explain run 1 (my
  first guess), and I do not know what did. **Batch 32 is lower than the earlier run in all three (32,353 to 32,942 against 34,058, 3 to 5 %), and I
  have no explanation for that either**; the larger batches show the same order in every run (the speed rises with the batch size, as
  before). **Run-to-run variation on this machine is about 7 % at one batch size, so a difference of a few per cent between two single runs
  means nothing.** The number to quote is "about 33,000 to 36,000 attempts/s at batch 128 to 256", **not** a single figure, and no run has
  been faster than the earlier ones: this milestone is about reporting, not speed.
* **The card while it ran** (`nvidia-smi` once a second, 80 samples under load): 47 C on average (up to 50), 236 W on average (up to 257;
  the average includes the warm-up and the dataset builds), graphics clock 2,985 MHz, memory clock 15,801 MHz, GPU busy 89 % on average
  (94 % at most), memory controller busy 62 % on average (67 % at most), 6.0 GiB of video memory used. The driver reported no
  throttling reason other than idle.
* **The memory-bound estimate holds up roughly:** 33,000 attempts/s of 16 MiB slices is about 554 GB/s of reads, against a spec-sheet
  bandwidth I have not looked up for this card (earlier text here used about 900 GB/s), and the memory controller was busy 62 to 67 % of the
  time. "Busy" is a share of time, not a share of bandwidth, so these two numbers do not have to agree and I do not claim they do.
* **The in-process miner** (`tenerod --network dev --mine gpu`, batch 128, a scratch chain on the development network, 3 minutes, the
  node's own screen): average 34,417 attempts/s over the run (its 10 s figure moved between 33,900 and 35,300), card at 50 to 51 C and 256 to
  264 W. Block times on that chain were not steady (the difficulty was still moving from its placeholder start), so the attempt rate
  is the thing to read, not the blocks. One run.
* **The separate miner program under load** (`tenero-miner --backend gpu --gpu-batch auto` against a scratch `tenerod` on the development network, about
  90 s, one run): connected, found and submitted 44 blocks, every one in the chain (none lost to a race, none refused); the rate was 30.0 to 32.6k
  attempts/s in the 10 s window once blocks slowed down (the first 12 s read 15.6k because blocks came every 0.2 s at the placeholder
  difficulty and the round trip to the node dominates); the card 44 to 47 C and 170 to 245 W. `auto` measured 34.9k, 34.9k and 36.2k for batch 128,
  256 and 512 and chose 512 (a 3.7 % lead in one run: noise). **This run showed the `luck` row reading 0.2x** (40 found, 184 expected):
  on that easy target a batch of 512 holds dozens of solutions and only the first becomes a block. Fixed (the attempts after a batch's first
  solution are left out of the expectation); on the real card at an easy target, 300 blocks found against 320.4 expected (1.07x; the same
  test before the fix would have expected about 19,200).
* **The `reads` row on the screen is attempts a second times 16 MiB, not a measurement of the memory bus.** NVML does not report
  bandwidth.

## The GPU miner end to end (measured on the owner's machine, RTX 5070 Ti, 2026-10)

`cargo test --release -p tenero-miner --test gpu_mining -- --ignored --nocapture --test-threads=1`. The miner
(`tenero-miner`) finds blocks on the GPU; the node checks every one with the full CPU proof of work.

| batch | sustained attempts/s (the backend alone, a target never met) |
|---|---|
| 32 | 34,058 |
| 64 | 34,790 |
| 128 | 35,745 |
| 256 | 35,959 |

* **Every block the GPU found was accepted by the CPU check; none was refused** (20 blocks in one epoch, 30 across six
  epoch boundaries, 60 across two).
* **Epoch boundaries:** the GPU builds a 4 GiB dataset in about 0.1 s and does it ahead of time. The stall was the NODE
  building its CPU dataset (about 3 s) when the first block of a new epoch arrived: with epochs of 25 blocks and 60
  blocks, the slowest later block was 3.42 s with no prefetch and showed no stall with the node looking 20 blocks
  ahead (total 20.8 s against 15.7 s). On the real chain (a block a minute, a lookahead of 10) the next epoch's dataset
  has ten minutes to build.
* A run at an easy target does not measure hashing: each block was found within one batch, so its time (about 0.2 s)
  is the node's CPU check.
* Not measured: a block at the real difficulty, more than one GPU, other batch sizes or settings, any other machine.

## The GPU miner in its own process (measured on the owner's machine, RTX 5070 Ti, 2026-10-02)

`tenerod` on the dev network (real matmulhash, start target 2^253) and `tenero-miner --backend gpu` (batch 128) as two
separate programs on one machine, started by the owner, run about 6.5 minutes and stopped with Ctrl-C.

* **64 blocks found: 64 in the chain, 0 lost a race, 0 refused.** The node (a CPU check of every block) logged 0 bans and
  stayed in sync; it stopped cleanly at height 64.
* **Steady speed: 32,600 to 35,600 attempts/s** (the miner's own status line, every 15 s, from about block 35 on). That
  matches the in-process figure above (34,000 to 36,000 at batch 128), so the separate process costs nothing measurable.
* The first status line (3,046/s) and the first blocks include the one-time 4.3 GiB dataset build and an almost-free
  target; the first 35 blocks took about 20 s because at 2^253 a block needs about 8 attempts, so the pace was the node's
  check (about 0.2 s a block), not the GPU. The difficulty then rose on its own: from block 35 to block 64 a block took
  0.4 to 46 s, about 12 s on average (still well short of the real one-minute target).
* Not measured: the real difficulty (a block a minute) held for long enough to see the adjustment settle, a run of hours,
  a node and miner restart during mining on this arrangement, other machines. 6.5 minutes of one run.

## Memory of the node and the GPU miner (measured on the owner's machine, 2026-10-02)

`tenerod` on the dev network (real matmulhash) and `tenero-miner --backend gpu` (batch 128) as two processes, 62 blocks in about 7
minutes, sampled every 5 s with `Get-Process` (working set, private bytes) and `nvidia-smi` (video memory used). The machine has 96 GiB of RAM; the
GPU already held about 1.9 GiB for the desktop. These were the binaries built before the memory-budget change (`budget.rs`).

| process | working set (resident) | private bytes (committed) | notes |
|---|---|---|---|
| `tenerod` | **about 4.1 GiB**, flat for the whole run | about 4.1 GiB | the CPU check's 4 GiB dataset, built when the first block arrived; 4 MB before |
| `tenero-miner` (gpu) | **about 364 MB** | about 4.9 GB | the dataset itself is in video memory, so the resident part is small, but the process commits about 4.9 GB of address space (the CUDA context and its mappings); **the commit charge, not the resident set, is what a low-memory machine's limits count** |
| video memory | the whole GPU went from about 1.9 GiB to about 6.4 to 6.6 GiB: **the miner adds about 4.5 GiB** | | one 4 GiB dataset plus the CUDA context and cuBLAS |

* The miner's GPU rate in this run was a median of 34,100 attempts/s (the first sample, 5,700, includes the dataset build), as in
  the first run.
* **The 8.6 GiB peak was not observed.** The node builds the next epoch's dataset in the background 10 blocks before an epoch ends; the
  dev network's epoch is 100 blocks and this run stopped at block 62. The documented peak (two 4.3 GiB datasets) is from the design, not this measurement.
* The test network (SHA-256) has no dataset: its three nodes held 8 MB each over four hours (run 3's monitor log).
* So: **a node on the real proof of work needs about 4.3 GiB of RAM, and about 8.6 GiB around an epoch boundary; a GPU miner needs
  about 4.5 GiB of video memory and under 0.5 GiB of RAM resident.** One machine, one run, one GPU.

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
