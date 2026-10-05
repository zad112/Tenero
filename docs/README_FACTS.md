# Facts check of the README and the testers' guide (M11.4, 2026-10-04)

Every number and claim in `README.md` and `docs/TESTING.md`, and where it comes from. **Measured** = a number from a run on the owner's machine (one machine); **read from code** = checked
against the source on this date; **argued** = a design argument, not shown; **arithmetic** = worked out from other numbers here. If a source changes, this file and the README change together.

| Claim | Kind | Source |
|---|---|---|
| Block target 60 s, difficulty window 30 blocks (LWMA) | read from code | `crates/tenero-chain/src/params.rs` `version_2` (`block_time: 60`, `window: 30`) |
| A block's timestamp must be later than its parent's; a 30 % backdating miner leaves difficulty at 1.02x (was 0.40x) | measured (simulation of the real rule, 20 runs of 3,000 blocks) | `crates/tenero-core/tests/difficulty_sim.rs`, `docs/KNOWN_ISSUES.md` item 12 |
| 1 coin = 100,000,000 units; ticker TNR | read from code / docs | `crates/tenero-wallet/src/amount.rs` (`UNITS_PER_COIN`), `docs/CONSENSUS_V2.md` section 3, `docs/RUNNING.md` ("TNR") |
| 20 coins per block at the start, halving every 525,600 blocks, main supply 20,000,000 coins, then a 0.5-coin tail | read from code | `params.rs`: `initial_reward 2_000_000_000`, `halving_interval 525_600`, `max_supply 2_000_000_000_000_000`, `tail_reward 50_000_000` (units; divide by 10^8) |
| 525,600 blocks is "one year at 60 seconds" | arithmetic | 525,600 x 60 s = 365 days |
| No premine: the alpha genesis creates no output | read from code + test | `crates/tenero-app/tests/alpha_network.rs`, `crates/tenero-core/tests/v2_vectors.rs` |
| Mined coins mature after 60 blocks, received coins after 10 | read from code | `params.rs`: `COINBASE_MATURITY 60`, `SPEND_MATURITY 10` |
| Block size: median of the last 10 blocks, floor 150,000 bytes, penalty grows with the square of the excess, limit twice the median and at most 4 MiB | read from code | `crates/tenero-core/src/fees.rs` (`MEDIAN_WINDOW 10`, `V2_MIN_BLOCK_MEDIAN 150_000`, `penalty`, `v2_block_limit`, `V2_MAX_BLOCK_BODY`) |
| Ring of 16; CLSAG and Bulletproofs+ from `monero-oxide` | read from code / docs | `params.rs` `RING_SIZE 16`; `docs/CONSENSUS_V2.md` (decision table) |
| What the interim scheme lacks (no Janus protection, one address, no outgoing view key, unaudited composition) | read from code | top of `crates/tenero-wallet/src/interim.rs` |
| One attempt: 64 x 8192 int8 matrix from ChaCha20, times one 16 MiB slice chosen by the attempt, exact int32 arithmetic, folded and hashed | read from docs | `docs/CONSENSUS.md` section 8.2 (parameters `m=64, k=8192, nb=2048, num_blocks=256`) |
| Dataset 4 GiB (256 slices), rebuilt every epoch, slices built in order with data-dependent picks | read from docs | `docs/CONSENSUS.md` section 8.2 |
| Epoch 100 blocks on `alpha` | read from code | `crates/tenero-app/src/config.rs` (`REAL_POW_EPOCH_BLOCKS`) |
| A check needs about 4.3 GiB of RAM and about 0.1 s (the first block of an epoch about 3 s more, for the dataset build) | **measured** 2026-10-04 (4.01 GiB for a whole check process across three epoch boundaries with the node's one-dataset mode; 8.0 GiB with the old two-dataset mode; 2.8 to 3.0 s for a build with 6 threads) | `crates/tenero-chain/tests/real_pow_sync.rs` with `TENERO_POW_LOW_MEMORY=1`; `docs/RUNNING.md` "Memory and disk" |
| 33,000 to 36,000 attempts/s on one RTX 5070 Ti (batch 128 to 256) | **measured** | `docs/BENCHMARKS.md` |
| 550 to 590 GB/s of slice reads | arithmetic (rate x 16 MiB), **not** a bus measurement | `docs/BENCHMARKS.md` (the same caveat is there) |
| CPU: 164 attempts/s (tuned build) and 31.7 (default build), 6 threads of a Ryzen 9 5900X | **measured** | `docs/BENCHMARKS.md` |
| Start target 2^237, about 524,000 attempts a block | read from code | `crates/tenero-app/src/daemon.rs` (`ALPHA_START_TARGET_POW2 = 237`); 2^19 = 524,288 attempts (`alpha_network.rs`) |
| About 15 s a block on one GPU at the start; about 53 min and about 4.6 h on the CPU builds | arithmetic | 524,288 / 34,000; 524,288 / 164; 524,288 / 31.7 (the difficulty adjusts, so these move) |
| 16 blocks in 12 minutes; gaps 11 to 183 s, mean 48 s; not settled | **measured**, one run | `docs/M10_M11_PLAN.md` M11.2 |
| Chain id `430ca700...69d3`, label "tenero alpha network 1", control port 38332 | read from code | `v2_genesis.json`, `config.rs` |
| Peers use the Noise protocol through `snow` (no formal audit) | docs | `CLAUDE.md`, `docs/THREAT_MODEL.md` |
| The GPU miner keeps up to two 4 GiB datasets in video memory; only a 16 GB card tried | read from code (the "up to two"); the "only 16 GB tried" is the owner's one card | `crates/tenero-miner/src/gpu.rs` header |
| NVRTC and cuBLASLt come with the CUDA Toolkit, not the driver | read from code + the sizes on the owner's machine | `crates/tenero-gpu/src/lib.rs`, `gemm.rs`; `cublasLt64_13.dll` 470 MB there |
| About 900 tests (898 passing, 24 skipped); six fuzz targets, 30 min each, no crash | **measured** 2026-10-04 (count taken before the last few tests were added) | M11.2 test run; workflow run 37199833458 |
| `--version` on every command-line program | read from code + test | `crates/tenero-app/tests/version.rs` |
| Linux: built for Ubuntu 22.04 or newer; glibc 2.34 (programs), 2.35 (app) | **measured** in CI (`objdump`) | release workflow run 37233836092 |
| The Linux node was run by hand once (WSL2, Ubuntu 26.04): it served a 16-block alpha chain, an empty Linux node synced it with the real proof-of-work check (about 4.1 GiB resident), and a Windows node from the CI package synced from it; the miner, wallet and app were not run on Linux | **measured** 2026-10-04, one machine | `docs/M10_M11_PLAN.md` M11.3 |
| Windows 11 is the only Windows tried | the owner's machine | |
| The wallet app uses 24 words; the command-line wallet a raw 64-digit seed; they are different wallets | read from code | `crates/tenero-wallet/src/mnemonic.rs`, `purse.rs`; `docs/RUNNING.md` |
| The command-line wallet and miner default to control port 18332 and need `--control 127.0.0.1:38332` on alpha | read from code | `wallet_cli.rs` `DEFAULT_CONTROL`, `tenero_miner.rs` |
| `alpha` refuses private peer addresses unless `allow_private_peers yes` | read from code | `config.rs`; `docs/RUNNING.md` |
| There is no public seed node and no diagnostics bundle | true today | nothing exists; `docs/SEED_POLICY.md` (the plan) |
| Memory-hardness is simulated, not proven; the construction has had no review | docs | `docs/KNOWN_ISSUES.md` item 11, `docs/THREAT_MODEL.md` F4 |

**Claims the README must never make (and does not):** money, private in Monero's sense, audited, ASIC-proof, secure, "decentralised", or any date. A check before publishing any change to the README: search it for those words.
