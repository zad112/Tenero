# M10 and M11 plan: a usable program, then a first test release (a PROPOSAL, written 2026-10-02 from the owner's request)

Tenero is still an experiment: **unaudited, not for real value.** A release to other people is a *test network build*, and
every screen, file name and page that a tester sees must say so. Coins on it have no value, and the chain may be reset
at any time.

## M9 groundwork: fuzzing (started 2026-10-02)

* **proptest, done:** a dev-dependency (approved 2026-10-02, MIT OR Apache-2.0, features cut to `std`, never in a shipped program; the
  nine crates it adds are all MIT OR Apache-2.0). Three test files run with `cargo test`: `tenero-core/tests/fuzz_decode.rs`
  (the nine version 2 decoders and the prunable part), `tenero-net/tests/fuzz_wire.rs` (the peer frame decoder and the streaming
  decoder in random chunking, a huge declared length) and `tenero-app/tests/fuzz_control.rs` (the control requests, responses and
  frames). Inputs: 4,000 random byte strings and 4,000 golden-vector objects with random edits per property. They check no panic, no hang, strict
  decoding (what decodes re-encodes to the same bytes), and that a failed stream decoder stays failed and stops buffering.
  **First run: no bug found.** Five deliberate faults (trailing bytes accepted, zero-length control frame accepted, a failed decoder not final or
  still buffering) were all caught after two properties were tightened. Not covered: the engine's message handling as a whole
  (what a peer can do with a *valid* message sent in a hostile order), the noise handshake, and the store's file readers.
* **cargo-fuzz (libFuzzer): BUILT 2026-10-03, run once for two minutes a target, no crash** (details and numbers in `THREAT_MODEL.md` section 5, item 2; four targets in `fuzz/`, their bodies in `crates/tenero-fuzzcases`, run by `.github/workflows/fuzz.yml` on a hosted Ubuntu runner). **Long runs are still to be done before the first release.** The original plan (approved in principle 2026-10-02): coverage-guided runs of the same
  decoders, then the engine's event handler, run under WSL or a Linux CI runner (the owner's Windows setup is the weak platform for it;
  it needs a nightly Rust). A corpus seeded from the golden vectors; crashes become regression tests. A run of hours with no
  finding is evidence, not proof.

## Before M10 starts: things from M8 that must be finished first

These are not new work; they are open items that a release to others would otherwise ship broken.

1. **Proof checks on by default: CHECKED 2026-10-02, and already true.** (The first draft of this plan said the nodes
   probably did not check proofs. That was wrong: it came from searching for one name in `tenero-app`.) `Node::new` uses the
   real `RingCtProofs` check, `Node::with_proof_check` refuses a checker that does not verify unless a test flag is set
   (`tenero-node/tests/mempool.rs`, `a_node_refuses_to_run_without_real_proof_checking`), and `tenerod` uses `Node::new`.
   The chain-level tests refuse a broken proof, a spend for another chain, a raised fee and a repeated key image
   (`tenero-crypto/tests/chain_spend.rs`). **The end-to-end test now exists** (`tenero-app/tests/daemon.rs`, `a_real_node_refuses_every_tampered_copy...`):
   a real node, a real wallet transaction, seven tampered copies (proof bytes at three places, the fee, an output address, an
   output commitment, the extra field), all refused with a reason, the untouched one accepted. With the node
   switched to "proofs not checked", the test fails (the tampered proof data is accepted): it does catch the fault.
2. **The day-long three-node run** (M8.4) finishing with matching tips and 0 bans (the lighter run 3 started 2026-10-02).
3. **The difficulty settling** at one block a minute on a real-PoW network, watched for at least an hour (never yet seen).

## M10: a program people can read and use

### M10.1 Command-line output that reads like a product (size S to M)

**Status 2026-10-02: built and tested; the owner has since run `tenerod` in a real Windows PowerShell window (one idle node on the test network): the status block redrew in place, the numbers ticked, and Ctrl-C ended cleanly.** Done: one formatting module (`ui.rs`) with golden
tests for every kind of line; the banner; the status block (redrawn in place on a terminal; plain lines with the time otherwise); events in
plain words, including "block N mined in S s, reward R"; errors with what to do for the common failures (port in use, data folder in use,
no cookie, node not running, wrong chain, bad address); sync percent, blocks a second and time left; `--quiet`, `--verbose`, `--color`; colour
only on a terminal and never with `NO_COLOR`; the same screen for `tenero-miner`; the log file unchanged. Run for real on scratch chains,
off a terminal (the plain mode): the node mining, the node syncing from the heavy test network, the miner program mining for a node, two
real errors, forced colour and `--quiet`. **Not done or not seen:** the redraw with a mining row, and `tenero-miner`, on a real console
(seen only for an idle node; otherwise tested by the exact bytes written), the hash rate meaning (M10.2: the miner shows raw
attempts a second until then), `tenero-wallet` (the plan names the node and the miner only), local time (the screen is UTC), terminal
width (assumed 80). One new dependency: `anstyle-query` 1.1.5 (MIT OR Apache-2.0, not audited as far as I know; it reads `NO_COLOR` and
turns on escape codes in the Windows console; its only dependency, `windows-sys`, was already in the tree).

* **One format for `tenerod` and `tenero-miner`:** a short banner (name, version, network, "EXPERIMENTAL, no value"), then
  a status block that *redraws in place* on a terminal (height, peers, sync progress with an estimate, mempool, disk,
  uptime) and plain lines when output goes to a file. Colours only on a terminal, switched off by `NO_COLOR`. Block and
  event messages in plain words ("block 1,204 mined, 0.4 s, reward 12.3 TNR", not `found a block at height ... (nonce
  11400714819...)`). Errors say what to do next. Dependency: a terminal-colour crate; **ask first** (rule 3).
* **The log file keeps the full detail** (nonces, ids) in the format it has now; the screen is the summary.
* **Sync progress** (percent, blocks a second, time left) and a `--quiet` and a `--verbose`.
* Test: the output is produced by one formatting module with tests per line type (golden text), so it cannot drift.

### M10.2 A hash rate that means what other miners' numbers mean (size M)

**Status 2026-10-02: the reporting half is built and tested without a GPU; NOTHING about GPU speed is measured yet.** Done: a rate meter
(`tenero-miner/src/rate.rs`) giving attempts a second over 10 s, 60 s, 15 min and the run, in *searching time only* (a pause, a dataset
build, or waiting for a job is left out, and so are the attempts counted in it; a window shows `-` until the samples cover all of it; a
clock set back restarts the windows and does not inflate anything); the backends say when they build a dataset (CPU and GPU) and the
miner thread when it is in a job; both screens show a `hashrate` row (compact numbers such as `34.7k`, the unit `attempts/s` written out, `(idle)` in front when not searching); the miner program's log line
uses the same figures. 44 screen tests, 16 meter tests; 50 injected faults, 14 got through the first time (one was a real bug, the clock
set back), the gaps are closed, two are equivalent (how old samples are dropped). **NVML: done** (owner approved NVML; the crate is `nvml-wrapper` 0.13, MIT OR Apache-2.0, which loads NVIDIA's library at run time; it adds
13 packages to the build: nvml-wrapper-sys, libloading (ISC), thiserror 1.0.69 and its macro, static_assertions, wrapcenum-derive and
darling, darling_core, darling_macro, fnv, ident_case, strsim, all permissive; none audited as far as known): when mining on a GPU both
screens show the card's temperature, power, fan, clocks, memory used, how busy the memory controller is, and when the driver is holding
the clocks down; a missing NVML or an unreported figure leaves rows out and never affects mining. Read once on the owner's real card
(idle). **NVML does not report memory bandwidth**, so the `reads` row is the attempt rate times 16 MiB, labelled as an estimate. The
CUDA and NVML card numbers are assumed to be the same (true with one GPU; not handled for several). **Measured on the owner's card
(2026-10-02, details and caveats in `BENCHMARKS.md`):** three runs of the backend test: 31,758 to 35,566 attempts/s (at batch 128 to 256: 33,348 to 35,566; the first run was a low outlier that did not repeat;
run-to-run variation is about 7 %; **not faster than the earlier measurement**), 34,417 average for the in-process miner over 3 minutes, the card at about 50 C and 256 to 264 W; the new
rows and the rate figures showed on a real node under load. **Also done:** a `luck` row (blocks found against the blocks the attempts should have found, and the effective rate: the work of the
blocks in the chain over the elapsed time, waiting included; the ratio only once 5 blocks are expected) and `gpu_batch = auto` / `--gpu-batch auto` (opt-in; measures
128, 256 and 512 for 4 s each at start-up; on the owner's card 512 won by 1 to 2 %, inside the noise, so **it removes bad choices such as 32 and 64 and
does not find a real winner among the larger sizes**; the default stays 128). **Not done:** several GPUs (one card here; not built or tested), the
(`tenero-miner` itself was run under GPU load once, 44 blocks, all accepted; see `BENCHMARKS.md`, which also records the `luck` accounting fault that run found and its fix).

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

**Decisions, owner, 2026-10-03:** a local desktop program (not a browser); the seed is written as **24 BIP-39 words** (the `bip39` crate, CC0, with 6 small dependencies; BIP-39 is used only as the spelling of our 32-byte seed, not its PBKDF2 step, so the words are not valid in a Bitcoin wallet and vice versa); the **password and the words are separate** (the words are the wallet, the password only encrypts the file on this computer); **several accounts per wallet**, each a separate one-account wallet derived from the master seed (account 0 is the master seed, so older wallet files still open), a payment spends from one account only.
**Step 1 DONE 2026-10-03 (library, no window): `tenero-wallet` `mnemonic.rs`, `purse.rs`, file format `TWL2`; 12 tests** (the BIP-39 reference vectors; a wrong word is caught 7,387 times of 7,413 = it slips through about 1 in 285, which is the 8-bit checksum; restoring from the words finds used accounts, stopping after 3 unused in a row; an account after a longer gap is missed and must be added by hand; the file refuses every changed byte tested). **Step 2 DONE 2026-10-03:** three fee levels (the owner chose Low 1.25x, Normal 2x, High 5x the minimum; the fee is now exactly the level's share, where it used to be "at least the minimum plus 25%, up to twice that"), build-then-send (nothing is reserved until the person confirms), and a record of sent payments in the wallet file (the interim scheme has no outgoing view key, so a restored wallet cannot know whom it paid), with a history that does not list the change as received.
**Step 3 DONE 2026-10-03:** the app's logic without a window (`tenero-gui`: `core.rs`, `procs.rs`, `settings.rs`), tested against real `tenerod` and `tenero-miner` processes on the SHA-256 chain (start, find an already running node, stop cleanly, a node that cannot start, mining, balances, a payment at each level, history, quitting); `tenero-miner --status-file` for the rate. **Step 4 DONE 2026-10-03:** the window (egui), drawn headlessly in tests (every tab in 14 states: the banner, no balance without a node, "not final" while catching up, the confirmation screen, the three fee levels, no clipboard writes). **Not checked by any test and so by hand only:** how it looks, whether it feels right to use, window sizing, and the real GPU mining screen. **Dependencies added with the owner's approval:** `eframe`/`egui` 0.36 (OpenGL, X11; no wgpu, no Wayland, no screen-reader bridge) and `qrcode` 0.14: Cargo.lock went from 131 to 388 crates (the window library is most of that); three new licences allowed (BSL-1.0 for two small Windows clipboard crates, OFL-1.1 and Ubuntu-font-1.0 for the fonts egui embeds: **a release must carry those font notices**, an M11.3 item).

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
* **Sign and verify messages, and prove and check payments, as Monero's wallet does (the owner's request, 2026-10-03; size M; BUILT 2026-10-03: the owner chose to keep each payment's secret key in the wallet file behind a click to reveal, and the hand-written option with no new dependency; the definition is `docs/WALLET_PROOFS.md`, the independent reference `tools/make_vectors_proofs.py`, 4 signature and 9 proof vectors the Rust matches bit for bit, a fuzz target `wallet_proofs`; the window has a Prove tab and History buttons; still unaudited and the review is not done).** The plan as written before building:
  What each does, and what it needs:
  * **Sign a message / verify a signature:** the wallet signs a text with an account's spend key (the signature says "whoever
    holds this address's spend key wrote this", nothing about when or where); anyone checks it against the address alone. The
    signature is bound to a fixed label ("tenero message v1") so it cannot be replayed as anything else (a transaction, a
    block). One account signs; the app shows which. *Needs:* a signature scheme on the Ed25519 curve the keys already live on.
  * **Prove a payment you SENT ("outgoing proof"):** shows that a given transaction paid at least this amount to this address.
    Monero does it with the transaction's secret key `r`. **Our wallet does not keep `r` today** (it is drawn at random while the
    output is made and thrown away), so this can work only **for payments sent after the wallet starts keeping it** (a new field
    in the sent-payment record in the wallet file); **payments already sent cannot be proved by the sender, ever** (the key is
    gone). *Needs:* `r` stored per sent payment; a proof string of the transaction id, the address and the proof.
  * **Prove a payment you RECEIVED ("incoming proof"):** the receiver shows that an output in a given transaction is theirs and
    holds this amount, without giving away the view key, by a zero-knowledge proof about the shared secret. Works for any past
    receipt, with nothing stored. *Needs:* a proof that two points share the same secret scalar (a DLEQ / Chaum-Pedersen proof).
  * **Check a proof:** anyone with the transaction id (looked up through a node) and the proof gets "valid: this transaction paid
    at least X to this address" or "not valid", and why. The check reads the chain through the node; it needs no wallet.
  * **Optional, later: a reserve proof** (prove the wallet holds at least X without moving it, Monero's `get_reserve_proof`):
    harder, because it must show the coins are unspent without linking them; leave out of the first version unless the owner
    asks.
  * **What these proofs do NOT show, and the screen says so:** who sent a payment (the interim scheme has no sender identity);
    that a payment is "final" (only that it is in the chain, and how deep); anything about other outputs of the transaction. With
    the interim scheme's missing Janus protection (`interim.rs`), a proof says the OUTPUT is addressed to this address, not that
    the sender meant it for this wallet. A proof reveals the amount and the link between the transaction and the address to
    whoever is given it, **so the app warns before it makes one and never puts one on the clipboard without a click.**
  * **Cryptography and rule 3:** a message signature and a DLEQ proof are small, standard constructions, but **composing them from
    `curve25519-dalek` is home-made cryptography in the sense of rule 3.** Options, for the owner to choose before any code:
    (a) an audited signature crate (the `ed25519-dalek` family has had an audit; it would be a new dependency and its hazmat
    interface is needed to sign with a bare scalar) for messages, and a hand-written, heavily tested DLEQ proof for payments,
    labelled unaudited like the rest of the interim scheme; (b) hand-write both, with known-answer vectors from an independent
    implementation and tamper tests, labelled unaudited. **Either way these proofs are unaudited, and nothing may call them proof
    in a legal or financial sense.** Recommendation: (b) without a new dependency, because the scheme is already ours, plus
    vectors made by a separate Python reference (as the other vectors are) so a second implementation agrees.
  * **Tests:** a reference in `tools/` and golden vectors; a signature fails if one bit of the message, the address or the label
    changes; a proof fails for another transaction, another address, a lower amount claimed as higher, and for an output that is
    not the receiver's; the sender's proof is refused when `r` was not kept (with the reason); every proof is rejected when
    truncated, extended or of the wrong length; fuzz the proof and signature parsers. In the window: a **Prove** area under
    History (a button on each sent and received row) and a **Sign / Verify** screen, drawn headlessly in tests as the others are.
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

## Decisions for the owner (answered 2026-10-02)

1. **GUI toolkit: egui/eframe, approved**, with a QR-code crate (its licence to be listed when it is added).
2. **An NVML crate (GPU temperature and power) and a terminal-colour crate: approved** (rule 3 satisfied; each one's licence and
   audit state to be noted in the commit that adds it).
3. **Fuzzing before M10 (my recommendation): not answered; I proceed on the recommendation** (proof checks, then fuzzing of the
   decoders and network handlers, then M10) unless the owner says otherwise.
4. **All the suggestions in M10.4 and M11.4 are kept.**
5. **The project licence: the repository already carries `LICENSE` (BSD-3-Clause, copyright 2026 zad112) and the Cargo files say so.** (The first
   draft of this plan said none was chosen; that was wrong.) The owner asked to find a *good* licence, so it is open to change before the
   first release. Considerations for the owner (not legal
   advice): the dependencies are MIT, Apache-2.0 and BSD-style, which all allow either choice; Monero itself is BSD-3-Clause;
   Rust projects commonly use "MIT OR Apache-2.0" (the Apache part carries a patent grant); a copyleft licence (GPL-3.0)
   would require anyone who ships a modified version to publish their source. A `THIRD-PARTY-LICENCES` file is needed
   whichever is picked.
6. **A code-signing certificate: maybe, depends on cost.** Decided when M11.3 starts, with real prices; until then the release
   notes carry the checksum and the SmartScreen warning.
7. **Seed nodes, the way Monero does them:** a list of seed addresses built into the program (several, run by *different*
   people, not only the owner) plus a `--seed` option and a peers file, so one dead seed does not strand a new node. Monero's
   additional DNS seeds are a later option (DNS names can be hijacked, so they add a trust point). Where the first seeds
   run is still open: the owner's machine is the weakest choice (see M11.4).
