# M10 and M11 plan: a usable program, then a first test release (a PROPOSAL, written 2026-10-02 from the owner's request)

Tenero is still an experiment: **unaudited, not for real value.** A release to other people is a *test network build*, and
every screen, file name and page that a tester sees must say so. Coins on it have no value, and the chain may be reset
at any time.

## Before M10 starts: things from M8 that must be finished first

These are not new work; they are open items that a release to others would otherwise ship broken.

1. **Proof checks on by default.** The node validates blocks with a validator that still *accepts every transaction proof*
   unless `RingCtProofs` is switched on (`tenero-crypto`). A search of `tenero-app` and `tenero-node` found no use of it, so
   the running nodes most likely do not check signatures or range proofs. Testers must not run a node that does not. (To
   be confirmed by a test that sends a transaction with a broken proof to a real `tenerod` and sees it refused.)
2. **The day-long three-node run** (M8.4) finishing with matching tips and 0 bans (the lighter run 3 started 2026-10-02).
3. **The difficulty settling** at one block a minute on a real-PoW network, watched for at least an hour (never yet seen).

## M10: a program people can read and use

### M10.1 Command-line output that reads like a product (size S to M)

* **One format for `tenerod` and `tenero-miner`:** a short banner (name, version, network, "EXPERIMENTAL, no value"), then
  a status block that *redraws in place* on a terminal (height, peers, sync progress with an estimate, mempool, disk,
  uptime) and plain lines when output goes to a file. Colours only on a terminal, switched off by `NO_COLOR`. Block and
  event messages in plain words ("block 1,204 mined, 0.4 s, reward 12.3 TNR", not `found a block at height ... (nonce
  11400714819...)`). Errors say what to do next. Dependency: a terminal-colour crate; **ask first** (rule 3).
* **The log file keeps the full detail** (nonces, ids) in the format it has now; the screen is the summary.
* **Sync progress** (percent, blocks a second, time left) and a `--quiet` and a `--verbose`.
* Test: the output is produced by one formatting module with tests per line type (golden text), so it cannot drift.

### M10.2 A hash rate that means what other miners' numbers mean (size M)

What the owner asked for is a rate that can be compared with other GPU miners. The honest part first:

* **Our unit is one "attempt" (one matmulhash evaluation), and it is a different amount of work from one hash of any other
  algorithm** (each attempt reads a 16 MiB slice of a 4 GiB dataset). **A number in H/s on this chain cannot be put next to
  a number from another coin's miner as a speed comparison, and the project should not claim it can.** What can be
  made consistent is the *reporting*, the way miners such as T-Rex, lolMiner and XMRig do it:
* **Rates over 10 s, 60 s and 15 min** (and the run's average), the *current* GPU rate separate from the *effective* rate
  (accepted work over time, which also depends on luck and on node round trips); no averaging over pauses or over a
  dataset build (the first status line today read 3,046/s, a measurement artefact).
* **Per-GPU lines:** attempts/s, the GPU's name, and (with NVML, an optional dependency, **to be approved**) temperature,
  power, fan, clocks and memory use, plus the **memory bandwidth in use**, which is the number that actually explains our
  speed (the work is close to memory-bound: about 35,000 attempts/s * 16 MiB is about 0.55 TB/s read, close to the card's
  limit; this was an estimate in `BENCHMARKS.md`, now to be measured).
* **Shares and results:** found, accepted, lost a race, refused (already counted), and an expected-versus-actual block
  count for the run.
* **Tuning:** `--gpu-batch` auto-selection (a short benchmark at start-up; 128 to 256 is currently best), several GPUs
  (`--gpu-device` list; **not built or measured**, one machine with one GPU only).
* **Measured on the owner's machine only** (rule 5); after the work, the number is re-measured and recorded. If it is not
  faster than today's 35,000, the report says so: this milestone is about *reporting*, not a promise of speed.
* Possible real speed work, **not promised**: overlapping the CPU work with the GPU, a faster fold, a kernel tuned for the
  card. Each is a consensus-neutral change that must still pass `tests/test_fused_kernels.py` and the owner's GPU check.

### M10.3 The wallet app: a GUI that can run the node (size L)

* **A desktop program with the wallet and, inside it, the node:** it starts `tenerod` as a child process of its own (a
  separate process, over the control interface that exists; the app never links the node into its own memory), stops it
  cleanly on exit, and shows its state. An "external node" option for a node already running.
* **Screens:** first run (create or restore a wallet, **write down the seed words, confirmed by asking for a few of
  them**, set a passphrase), balance and sync state, receive (the address, copy, a QR code), send (address, amount, the
  fee shown before sending, a confirmation screen), history, settings (data folder, network, pruned or archive node,
  mining on or off and which backend), a log view, and an about box with the version and the labels.
* **Mining from the app:** a start/stop toggle that runs `tenero-miner` (the process of its own), shows the rate from
  M10.2, with a visible "this uses the GPU" notice. Default **off**.
* **The labels are part of the design:** a permanent banner "TEST NETWORK. NO VALUE. UNAUDITED." and an address and
  wallet that say "interim output scheme, not private in Monero's sense" until Carrot replaces it. Nothing in the app may
  say or imply that coins are money or that transactions are anonymous.
* **Safety:** the seed and passphrase never appear in a log, a crash report or the clipboard without a click; the
  passphrase is not stored; the wallet file keeps its current encryption; the control cookie stays loopback only; the
  window shows when it is *not* synced and refuses to say "balance" as final then.
* **Decision needed (a dependency, rule 3):** the toolkit. Options, with the trade-off, to be chosen by the owner:
  (a) **egui/eframe** (pure Rust, one small exe, immediate-mode, plain look, fewest moving parts, my recommendation for a
  first release); (b) **Tauri** (web UI in the system's WebView2, nicer look, but a JavaScript toolchain and a much larger
  supply chain); (c) **iced** (pure Rust, retained-mode, younger). A QR crate is also needed. Licences to be listed
  before anything is added.
* Test: the app's logic (state, validation of inputs, the amount parser, the child-process lifecycle: start, crash,
  restart, stop, an orphaned node) lives in a library tested without a window; the window itself is checked by hand on the
  owner's machine, and I will say which parts were not.

### M10.4 Other things that fit here (my suggestions; the owner picks)

* **Transaction history and a payment *request* with a label** (what the wallet needs for people to actually test paying
  each other); a way to export the history as a file.
* **A tiny block explorer in the app** (the last blocks, a block's details, a transaction's status) from the control
  interface, so testers can see the chain without a web service.
* **A faucet is not needed if mining is easy**; a tester with no GPU should be able to mine on the CPU at a low,
  easy test difficulty, or receive coins from the owner by address. (Decide in M11 with the network parameters.)
* **A "diagnostics bundle" button/command** (versions, settings with secrets removed, the last log lines, peer counts) that
  a tester can send back when something fails; **it must not contain keys, seeds, passphrases, cookies or addresses unless
  the tester ticks a box**, and nothing is ever sent by the program itself (no telemetry).
* **Config file and first-run defaults** that work with no editing (ports, folders under the user's profile), plus a
  check that the data folder has room (the chain, and 4.3 GiB of RAM for the CPU check, rule 8: **the minimum
  requirements must be stated, and the app should say so if the machine is below them**).
* **Log rotation** (the logs grow without limit now), and a check that a second copy of the node cannot open the same data
  folder.
* **A version number and a protocol/network id in the peer handshake**, so a tester on an old build or on another chain
  is refused with a clear message instead of a ban or a split.
* **Localisation is NOT planned** (English only).

### M10 done when

The four parts above each have the check stated in them, the CLI output and the wallet's logic have tests, a person who
did not write the code can install the build on a clean Windows machine, create a wallet, receive coins from a miner and
send some on, **by following only what is on screen**, and the owner has confirmed the GPU numbers on the owner's machine.

## M11: tidy the repository and ship a first test release

### M11.1 Retire the old Python (size M, deliberate, in steps)

* **What goes:** the Python *node, miner, wallet and account-model chain* (`miner.py`, the old node and wallet code, the
  legacy chain tests, `chains.json`, the GPU Python miner and its `.bat` files) and the tests that exist only for them.
* **What stays, on purpose:** the **independent Python references** (`tools/make_vectors*.py`, the Python implementations
  of the data model, the wire format, the interim scheme, the control protocol) and the **golden vectors** they make.
  They are the second implementation that the Rust is checked against ("bit for bit, or it is wrong"); deleting them
  would leave one implementation checking itself. They move into a clearly named folder (for example `reference/`) with a
  README that says what they are for, and the commands in `CLAUDE.md` are updated.
* **Known issues:** every item in `docs/KNOWN_ISSUES.md` that exists only in the old Python gets a written note that it is
  *retired because the code is gone* (not "fixed"), and its xfail test goes with it, in the same commit (rule 7). Items that
  the Rust code also has stay and stay visible.
* **Done in steps**, each with the test suite green: first move, then delete, never both at once; the old code stays in the
  git history and on the `main` branch (tag it, e.g. `python-final`, before deleting).
* `CLAUDE.md` is rewritten for the Rust-first project (its rules 1 to 9 stay in spirit; rule 4's `gpu_test.bat` becomes the
  Rust GPU check), and `main` is replaced by the `rewrite` branch only when the owner says so.

### M11.2 A fresh chain, with no premine (size S to M)

* **A new genesis block:** a new chain id and genesis message and a fresh data model start, chosen once and written in
  `CONSENSUS_V2.md`, with **no coinbase output and no premine of any kind** (today's genesis already has no coinbase output;
  this is checked by a test and by a vector, and stated in the release notes). Every coin comes from a mined block,
  *including the owner's*: the owner starts as one miner among the testers, with no head start and no reserved coins.
* **Real network parameters, chosen and written down:** the proof of work (real matmulhash, not SHA-256), the **starting
  difficulty** (the placeholder 2^253 makes the first blocks free, as the 50 blocks in 20 seconds showed; it must be set so
  that a first block is not found in under a second, but also so that a tester with one modest GPU, and a tester with only a
  CPU, can both find blocks in the first hours), the **epoch length** (the dev network uses 100 blocks; the real value is
  a consensus decision to make with the dataset build time in mind), and the **difficulty window**. These are consensus
  choices: made deliberately, in the reference first, with new vectors (rule 1).
* **What "no premine" does and does not mean here:** it is about how the chain starts, and it is true by construction
  and checkable by anyone. It says **nothing about value or safety**: the network has no value, a small chain can be rewritten
  by anyone with a GPU, and the chain can restart again (the testnet will be reset when Carrot replaces the interim scheme).
* The old test and dev networks are *kept* as development networks; the release network gets a new, distinct name, and
  a peer on another chain is refused at the handshake (M10.4).

### M11.3 Packaging for Windows and Linux (size L)

* **Artifacts:** `tenerod`, `tenero-miner`, `tenero-wallet` (CLI) and the wallet app, as a **zip (and for Linux a tar.gz) with the
  executables, a README, the LICENCE and THIRD-PARTY-LICENCES, and a checksum file (SHA-256)**. An installer only if it is
  cheap; the zip comes first.
* **Windows:** release build, with the **CUDA and NVRTC libraries handled deliberately** (the GPU engine needs CUDA runtime
  pieces that NVIDIA's licence lets us redistribute only in certain ways; **check the licence before bundling anything**, or
  require the driver and the user's CUDA install and say so). The exe is **not code-signed unless a certificate is bought**,
  so Windows SmartScreen and some antivirus programs (as already happened on the owner's machine) will warn; the release
  notes must say so and show how to verify the checksum, and the build is reproducible from a tagged commit.
* **Linux:** the same, built in a clean container or a CI runner (**no Linux machine has run any of this yet; the first
  Linux build is a first test, not a known-good**). The GPU path needs the NVIDIA driver and CUDA libraries there too; a
  CPU-only Linux build is the fallback and should be offered. glibc version floor stated.
* **CI** (GitHub Actions): build and test on Windows and Linux for each tag, run the Python reference checks and the
  vectors, run `clippy -D warnings`, and publish the artifacts to a draft release. **The GPU tests cannot run in CI**
  (rule 5): they are done by the owner before a tag, and the release notes say which release was GPU-tested and on what.
* **Supply chain:** `cargo audit`/`cargo deny` over the dependencies, a list of every dependency and licence (the
  `snow`, `monero-oxide` git pin and `argon2` notes carry over), the lock file committed, nothing fetched at build time
  but from the registry or the pinned commit.
* **Reproducibility:** the version and git commit are printed by every program (`--version`) and appear in the handshake.

### M11.4 What testers need (my suggestions; the owner picks)

* **A TESTING.md for testers:** requirements (a GPU, RAM, disk, what happens without a GPU), how to start, how to find
  peers, how to report a problem and what to attach (the diagnostics bundle), what is expected to break, and the labels:
  unaudited, no value, may be reset.
* **Seed nodes:** a tester on another network needs somewhere to connect. For the first release, **the owner's node
  reachable from the internet** (a port forward, a stable address; this exposes the owner's machine to every kind of
  hostile traffic that M9 has *not* hardened against, so it should run on a separate machine or a cheap server, with the
  firewall limited to the one port) and a peers list, and **testers can also connect to each other by address**.
  A node that accepts inbound connections from strangers is the part of this plan I would be most careful about.
* **A small group first** (three to five known people) before anything public, with the release marked "pre-release".
* **A status page or a plain-text network summary** (height, peers, hash rate) so testers can tell if the network is alive.
* **An upgrade and reset story:** how a tester moves to the next build, and the plan for a chain reset (a new genesis id
  announced, old data refused cleanly).
* **Security contact and a licence file** (what licence the project itself carries is the owner's decision; none is chosen
  in the repository today).
* **A rollback rule:** the release is withdrawn, not patched silently, if a consensus or key-handling bug is found.

### M11 done when

The repository contains only the Rust code, the independent references, the vectors, the docs and the packaging; the build
from a tagged commit gives the Windows and Linux archives with checksums; a clean Windows machine and a clean Linux machine
(Linux **untested until then**) each install it from the archive, sync from the owner's seed node, and mine or receive a
block; the new chain has the genesis stated in the docs and the owner has verified the no-premine check for themselves
(the first block is height 1, and the genesis holds no outputs); and the owner has chosen who to send it to.

## What this does not change

Section 7 of `docs/M8_PLAN.md` still holds: **a test release to friends is not a launch.** M9 (fuzzing, threat model, review
plan), an independent review, a long public test and an emergency-fork plan are still ahead of anything that could carry
value. **Recommended order: finish the three "before M10" items, then M9's fuzzing of the decoders and network handlers
(anything strangers can reach should be fuzzed before it is exposed), then M10, then M11.** If the owner prefers to
send a build to a few trusted people sooner, I would do it after the "before M10" items and with a seed node that is not
the owner's own computer.

## Decisions for the owner

1. The GUI toolkit (egui recommended; Tauri; iced), and approval of its dependencies and a QR crate (rule 3).
2. Approval of an NVML crate for GPU temperature and power, and a terminal-colour crate (rule 3).
3. Whether M9's fuzzing comes before M10 (my recommendation) or after M11.
4. Which of the "my suggestions" in M10.4 and M11.4 to keep, cut or defer.
5. The project's licence (none chosen yet), and whether to buy a code-signing certificate.
6. Where the seed node will run.
