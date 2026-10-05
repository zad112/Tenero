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
  card. Each is a consensus-neutral change that must still pass `reference/tests/test_fused_kernels.py` and the owner's GPU check.

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

**Owner's pick, 2026-10-03: from this list only the payment requests (built 2026-10-03: `tenero-wallet/src/request.rs`, the Receive and Send screens, the label in history), plus the signing and proof tools added above (built). The rest (the block explorer, the diagnostics bundle, config and requirement checks, log rotation, the handshake's version, a faucet question, exporting the history) is NOT selected and not planned; it stays listed below as ideas. M10.3 is finished with the several-wallets switch and the icon (both the owner's requests).**

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
* **Sign and verify messages, and prove and check payments, as Monero's wallet does (the owner's request, 2026-10-03; size M; BUILT 2026-10-03: the owner chose to keep each payment's secret key in the wallet file behind a click to reveal, and the hand-written option with no new dependency; the definition is `docs/WALLET_PROOFS.md`, the independent reference `reference/tools/make_vectors_proofs.py`, 4 signature and 9 proof vectors the Rust matches bit for bit, a fuzz target `wallet_proofs`; the window has a Prove tab and History buttons; still unaudited and the review is not done).** The plan as written before building:
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

**The order (the owner's, 2026-10-03):** M11.0 (the stored-block checksum and the emergency plan), M11.1 (retire the Python), M11.2 (the fresh chain,
with the timestamp rule), **M11.25 (open `main`)**, M11.3 (packaging, with the icon inside the program), M11.4 (what testers need, with the README
rewrite). Nothing is pushed to `main` before M11.25, and then only when the owner says so. **CHANGED by the owner on 2026-10-04: `main` became the Rust program at M11.1, ahead of M11.2** (see M11.1 and M11.25 below).

### M11.0 Two things to settle before anything else (the owner's pick, 2026-10-03; BOTH DONE 2026-10-04: the checksum, and the emergency plan's decisions, `tenerod rewind` and a first drill)

* **A checksum on every stored record (`THREAT_MODEL.md` G3, A2; a storage-format change, NOT a consensus change). DONE 2026-10-04: format version 4, 16 bytes after each record covering the record and its place; measured: 600 damaged copies, 3,600 reads, 3,581 refused, 19 right, 0 a different block (was 3 of 900); with the check removed the test fails (mutation check); a format-3 store is refused (test); no migration. The text below is the plan as written.** Measured on 2026-10-03: of 900
  damaged reads of a segment file, 3 returned a *different, valid-looking block* with no error, because the damage landed where a block still
  decodes. The fix: each record in a segment file carries a checksum (a SHA-256 prefix of the record, from a hash the project already uses; no
  new dependency) checked on every read, so a damaged record is an error that names the segment and the record, never a wrong block. **To do:**
  the format and its version marker; the reader refuses (with a clear message) a store written without checksums rather than guessing, and the
  development networks' old data is throwaway, so no migration is written unless the owner wants one; the existing damage test
  (`tenero-store/tests/store.rs`, "a damaged segment file never panics and the store says how often it cannot tell") becomes "0 of 900 returned the
  wrong block, and every damaged read is an error"; a mutation check (remove the check, the test must fail); a note in `KNOWN_ISSUES.md`/`THREAT_MODEL.md`
  that G3 is closed, with the numbers. Cost: a few bytes and one hash per record read.
* **The emergency plan, finished (`EMERGENCY_PLAN.md`; the owner's decisions are marked DECIDE in it). 2026-10-04: DECIDED, the owner alone may say "stop mining" (no second person, stated as a known weakness); the channel is a pinned issue (to be created before the first tester); a rewind command is built (`tenerod rewind`, offline, dry run by default, one integration test on real data); a rule change is a reset (no activation mechanism). The drill was run the same day (`EMERGENCY_PLAN.md` section 9): one scenario, by the author, on one machine; it found three faults (no `--version`, the log did not name the bad block, the rewind's wording), all fixed. The text below is the plan as written.** **The owner decides and writes in:** who
  besides the owner may say "stop mining" (a named second person, and how they are reached), and where testers are told (the channel, which must
  exist before the first tester does). **Then:** the plan is changed from DRAFT; the two gaps that are code are decided one way or the other and
  written down: a `rewind` command (the plan's default for a recent bad block depends on it) and an activation mechanism for planned rule changes
  (or the plan says that a rule change is a reset). **A drill, which is the plan's gap 4:** a deliberately broken build on a scratch network, run through
  sections 3 to 6 of the plan by the people named in it, timed, and written up with what did not work. The plan is not called finished until the drill has
  been done once.

### M11.1 Retire the old Python (size M, deliberate, in steps): DONE 2026-10-04

**What was done, in order (each step with the tests green):** (0) the Python state of `main` was kept as the `python-final` tag and the `legacy-python` branch (commit
`797f07b`); (1) the old program that only it used was deleted: `miner.py`, `cli.py`, `view.py`, `demo.py`, `calibrate.py`, `gpu_pow_test.py`, the `.bat` files, and the
modules `checker.py` and `mempool.py`, with their tests (commit `d462211`); (2) the independent reference was moved to `reference/` with `git mv` (commit `31de034`); (3) `main`
was fast-forwarded to it (`797f07b..31de034`, no force, no history lost), after CI was green on `rewrite` (rust on Ubuntu and Windows, supply-chain, and the Python reference
suite on Python 3.12, 3.13, 3.14). **Two differences from the plan as written, and why:** the order was delete-then-move (less to move, same safety), and **more stayed than the
plan listed**: `chain.py` itself imports `paths.py` and `storage.py`, so those stayed; `gpubackend.py` stayed with its emulator test (the only check of the CUDA kernel logic that
runs without a GPU, and the Rust copy of the kernels is held identical to it); `analysis.py` stayed (the memory-hardness simulation the README's argument rests on; its tests were
split into `test_analysis.py`). **Measured:** Python 343 passed, 64 skipped, 5 expected failures (was 468 + 115 skipped before; the difference is the deleted program's tests); all
six `make_vectors --check` ok; Rust 893 passed, 0 failed, 24 ignored. The four vector files that embed their generator's path were regenerated and differ by that one line each.
`KNOWN_ISSUES.md` now says that items 1 to 10 and 14 still live in the frozen reference code (not fixed), and that item 15 no longer describes anything shipped. `CLAUDE.md` and
the README are rewritten for the Rust-first project (the README is an interim one; the full rewrite is M11.4). **The plan as it was written:**

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
  Rust GPU check), and `main` is replaced by the `rewrite` branch only when the owner says so (the owner said so on 2026-10-04).

### M11.2 A fresh chain, with no premine (size S to M): DONE 2026-10-04

**What was done (commits `3ba3001`, `4bb652a`, and the one that records this):**
* **The owner's three choices, 2026-10-04:** the network is called **`alpha`** (label "tenero alpha network 1"; a restart for Carrot becomes "alpha network 2");
  the **starting target is 2^237** (about 524,288 attempts a block); the **epoch is 100 blocks** (the original value; the one every measurement used; a choice, not
  an optimisation). The difficulty window (LWMA 30), the 60 s block time, the 4x step, the emission, the rings (16) and the maturities (60 and 10) are unchanged.
* **The timestamp rule (a CONSENSUS CHANGE, in the order of rule 1):** a block's timestamp must be later than its parent's. Python reference, `CONSENSUS.md` section 7,
  `CONSENSUS_V2.md` 5.5, the vectors regenerated (`difficulty.json`, `chains.json`), then the Rust validator and `difficulty::earliest_time*`. A test per edge: equal to the
  parent (refused), one second later (accepted), earlier (refused), where the old median line was (refused now), block 1 against the genesis time 0, the future limit still held.
* **The simulation rerun against the real rule** (the crate's own function, 20 runs of 3,000 blocks): a miner that backdates holds the difficulty at **1.00x with 10 %** of the hash
  rate, **1.02x with 30 %**, **1.06x with 45 %** and 1.20x with 60 % (a 60 % miner controls the chain anyway); stamping ahead 0.99x to 1.00x and alternating 1.00x to 1.01x at every
  share. The old rule on the same code: 0.69x, 0.40x, 0.29x (kept as the stand-in `Rule::OldMedian` so that the finding stays reproducible).
* **The network:** `Network::Alpha` through the node, the miner, the wallet app, the seed check and `tenerod rewind`; `v2_genesis.json` has the alpha label, and **every case says 0
  transactions and 0 coinbase outputs**; chain id `430ca70081d3e52c618fd9af46fecdf6d6fc8f7965dc8ed2aa53c92ecfe069d3`, the same in the Python reference and in Rust (a test reads the
  vector). **No premine is checked on the real store of every network** (the genesis creates no output; no coin exists before block 1). 5 new tests; Rust 898 passed, 0 failed, 24 ignored;
  Python 345 passed.
* **Measured on the real network, one run, 2026-10-04 (the owner's RTX 5070 Ti, one node, mining into a scratch folder, 16 blocks in 12 minutes):** the node's genesis tip was the alpha chain
  id; block 1 was found 4.5 s after the start and block 2 after 17.4 s (the plan's "never under a second" held); then a 3-minute wait for block 3 (the difficulty had just gone up about 3.5x,
  a long draw); over the 15 gaps between blocks the shortest was 11 s, the longest 183 s and the mean 48 s (block times are random, so gaps of a few times the mean are normal; the LWMA
  was still moving from its start toward 60 s: **it had not settled in 16 blocks, which this run does not show it doing**).
  Every block passed the node's full CPU proof-of-work check; no invalid block, no ban, one warning (no peers: one node). **What this does NOT show:** a settled difficulty, a
  second miner, a CPU miner, a network of more than one node, an epoch boundary (100 blocks) on the new chain, or that the 2^237 start suits other people's hardware.
* **Release-notes line (to be carried into M11.4):** "The timestamp rule changed: a block must be later than its parent. This starts a new chain; chains from before are not valid under it."
* **Still true:** the old `test` and `dev` networks are kept as development networks (the timestamp rule applies to them too: one validator, so old data mined under the old rule can
  contain blocks the new rule refuses; it is throwaway), and a peer on another chain is refused at the handshake (the chain ids differ).

**The plan as it was written:**

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
* **The timestamp rule (DECIDED by the owner, 2026-10-03): a block's timestamp must be later than its parent's** (in place of "not below the
  median of the last 11"). Why: the simulation (`tenero-core/tests/difficulty_sim.rs`, `THREAT_MODEL.md` E3) showed that with the median rule a
  miner holding only 30 % of the hash rate can pull the difficulty to 0.40x by backdating its blocks, and that this rule leaves it at 1.00x.
  **To do, together, for the fresh chain (rule 1):** the reference (`reference/tools/make_vectors.py` and `reference/tools/make_vectors_v2.py`), `CONSENSUS.md`
  section 7 and `CONSENSUS_V2.md` 5.5, the validator in `tenero-chain`, new vectors, a test per edge (equal to the parent, one second later, earlier,
  and the future limit still held), the simulation rerun against the real rule instead of a stand-in, and a line in the release notes. It
  changes what a block must satisfy, so it is a new chain (there is no activation mechanism, `EMERGENCY_PLAN.md` section 8). Not changed yet.
* **What "no premine" does and does not mean here:** it is about how the chain starts, and it is true by construction
  and checkable by anyone. It says **nothing about value or safety**: the network has no value, a small chain can be rewritten
  by anyone with a GPU, and the chain can restart again (the testnet will be reset when Carrot replaces the interim scheme).
* The old test and dev networks are *kept* as development networks; the release network gets a new, distinct name, and
  a peer on another chain is refused at the handshake (M10.4).

### M11.25 Open `main` (the owner's, 2026-10-03; only when the owner says so)

**2026-10-04: the first half is DONE early, at the owner's word** (the `python-final` tag, `legacy-python`, and `main` = the Rust program as a fast-forward; the Python
reference job runs in CI on `main`). **2026-10-04, later: CI on `main` is green on both runners; the fuzz targets are done (a sixth, `noise_handshake`, and the engine target 2.1x faster on Linux; `THREAT_MODEL.md` A3/A4 has the numbers); branch protection is DEFERRED by the owner (GitHub does not offer it on a private repository without GitHub Pro, and the owner will make the repo public later: see the security notes below).** **2026-10-04, evening: the repository is PUBLIC and its settings are done as follows (read back from GitHub):** Actions limited to GitHub's own actions plus `dtolnay/rust-toolchain`, every action pinned to a commit hash and required to be; a fork's pull request needs the owner's approval and runs on GitHub's hosted runners, never the owner's machines (`rust`, `tests`, `supply-chain`); the workflow token is read-only and cannot approve pull requests; a ruleset protects `main` and `legacy-python` (no deletion, no force-push, linear history) and the tag `python-final` (no deletion, no update; **configured but not tested live**); secret scanning, push protection, Dependabot alerts and private vulnerability reporting are on (`SECURITY.md`). **Required status checks were chosen NOT to be added** (the owner, 2026-10-04): GitHub would then refuse direct pushes to `main`, and nobody else has write access, so they would guard only against the owner's own mistakes. **Later the same evening the owner moved ALL CI to GitHub's hosted runners (Linux as well as Windows: the Linux runner was not much faster, and a public repository gets hosted minutes free); the self-hosted runners and the work to wall them in (the WSL lockdown stays as hygiene for the hand-test box; the firewall script drafted for the Linux runner is not needed) are retired.** The Windows job on a hosted runner (about 22 minutes, one test, `two_nodes_sync_and_a_wallet_pays_through_the_control_interface`, failed once there and could not be reproduced; its failure message now carries the evidence); the owner's WSL Ubuntu, now only a box for hand tests, has no Windows drives, no Windows interop and no sudo for its user, and can still reach the local network (measured: the Cumulus MX web port and the router were reachable from it; harmless while it runs no one else's code). **Fork pull requests were designed to run on hosted runners; now every job does, so that route needs no separate test, but the approval requirement for outside contributors is still untested (it needs a second GitHub account).** **Still to do here:** the first long fuzz run through the workflow (see below for what happened), and (done 2026-10-04: **the history was rewritten once, before the repository went public, to replace the author's private email with GitHub's noreply address (every file is identical commit by commit; every commit hash changed; a hash quoted in an OLD commit message is the pre-rewrite one, the hashes in these docs are the new ones)**; the `rewrite` branch is deleted, it had no commit that `main` lacked, and the workflows now trigger on `main` only). **Before the repository is made public (the owner's plan; a walkthrough is owed): the workflows run on the owner's own machines (self-hosted runners), and a public repository lets anyone open a pull request whose code could run there; `rust.yml`, `tests.yml` and `supply-chain.yml` all trigger on `pull_request`. That must be closed first (approval for outside contributors, no self-hosted runner for pull requests from forks, or a separate machine), and it is the first thing to settle.** The branch `rewrite` equals `main` now and
can be deleted when the owner wants. **The text below is the plan as written on 2026-10-03.** `main` still holds the Python prototype and `rewrite` holds the Rust program; nothing has been pushed to `main`. When M11.0 to M11.2 are done and the
owner gives the word: tag the last Python state (`python-final`, M11.1), make `main` the Rust program (a merge or a replacement, whichever keeps the
history the owner wants; **a replacement of `main` is the owner's explicit decision, not mine**), check that CI is green on `main` on both systems, and
move the things that only work from the default branch: the **long fuzz run** (`fuzz.yml`'s manual start and its monthly schedule), the supply-chain
schedule, and any branch protection. Then the first long fuzz runs start (hours, not the two-minute smoke run), with the engine target made faster and
the noise handshake and the proofs targets included. The old Python tests and tools that are kept (the vector references) move with it.

### M11.3 Packaging for Windows and Linux (size L): BUILT 2026-10-04, first dry run green; the owner's steps remain (see `docs/RELEASING.md`)

**What exists (commits `4ab29f6`, `ba9a0b5`, and the one that records this):** version `0.1.0-alpha.1`; `--version` with the source commit on `tenerod`, `tenero-miner`, `tenero-wallet`, `tenero-seedcheck`
(the wallet app shows it in its About tab: it has no console on Windows); the circular icon and the version information compiled into `tenero-wallet-gui.exe` (`embed-resource`, owner-approved
2026-10-04, build-time and Windows-only; the build FAILS if it cannot embed); `THIRD-PARTY-LICENCES.txt` made by `cargo-about` 0.9.2 (owner-approved 2026-10-04, a CI tool, not linked into the
programs) plus the two font notices the tool misses; `.github/workflows/release.yml` (a tag builds a Windows zip and a Linux tar.gz with SHA-256 files and creates a DRAFT pre-release; a manual run is a dry
run that keeps the packages for a week; only the draft job can write); notes `docs/releases/v0.1.0-alpha.1.md`; the owner's checklist `docs/RELEASING.md`.
**Decisions (owner, 2026-10-04):** GPU users install the CUDA Toolkit 13.x themselves (the miner needs NVRTC and cuBLASLt, which come with the Toolkit, not the driver: `cublasLt64_13.dll` is 470 MB and NVRTC
101 MB on the owner's machine; nothing from NVIDIA is bundled; bundling after a licence check, precompiling the kernels, or writing our own int8 multiply are later options, none done); first release `v0.1.0-alpha.1`.
**Measured in the first dry run (run 37233836092, commit `ba9a0b5`):** both packages built and the packed programs ran and reported the right version and commit; Windows zip 7.35 MB, Linux tar.gz 9.63 MB;
build 5 min 15 s (Windows) and 2 min 44 s (Linux), the licence tool 4 min and 2 min on a cold cache; **minimum glibc 2.34 (programs) and 2.35 (wallet app)**; I downloaded the Windows zip, checked its SHA-256 against the
workflow's, ran the four command-line programs from it, and checked the app's icon and version information and the font notices in the licences file.
**NOT done or NOT verified:** the wallet app has never been opened from the package (CI has no screen); the GPU tests for this build (the owner's, before a tag); the Linux package was run by hand only as in the next paragraph (nothing else on Linux)
(only `--version` in CI); the handshake does not carry the version (the plan asked for it: that is a protocol change, left out); the `.exe` files are not code-signed and the builds are not shown to be bit-for-bit reproducible;
no release has been made (no tag exists, no draft).

**The Linux build, run by hand for the first time (2026-10-04, in WSL2 Ubuntu 26.04, glibc 2.43, on the owner's PC; the CI package, hash checked after copying in):** all four programs reported the right version and commit;
a node loaded a copy of the 16-block `alpha` chain (tip 16, `00000cc6`) and served it; **a fresh node from the Windows package synced all 16 blocks from it in under 5 s with the real proof-of-work check (blocks applied 16, bans 0)**;
**an empty Linux node synced the same 16 blocks, checking every proof of work itself on Linux, using about 4.1 GiB resident**; `tenero-seedcheck` (Windows) reached the Linux node, found the right chain id and tip and warned only that it
had no addresses to give and that one seed is below the policy's three. **Found:** (1) SIGTERM (what a service manager's stop sends) killed the node on the spot, exit status 143 and no shutdown line: **fixed** by switching on
`ctrlc`'s empty `termination` option (no package added; `cargo build --locked` on Linux confirms), with `crates/tenero-app/tests/sigterm.rs` (SIGTERM and SIGHUP; it fails without the option, checked); the dry-run package
`ba9a0b5` predates the fix. (2) `tenerod status` and `stop` assume the `test` network's port and need `--control` on `alpha` (documented; not changed). **Not run on Linux:** the miner, the wallet, the wallet app, a GPU, a native Linux
machine or a server, and a node under strangers' traffic.

**The node's memory (2026-10-04, the owner's limit: the node must stay under 8 GB at all times; the miner is not limited):** measured first, because the documents said "8.6 GiB briefly": with the old rule (keep the last two epochs, build the next
ahead of time) a process checking a real-proof-of-work chain went from **4.0 to 8.0 GiB of dataset at the first epoch boundary and stayed there**, i.e. two datasets for most of every epoch, over the limit once the rest of the node is added.
**Fixed without any consensus change:** `MatmulPow::low_memory` (used by the node on `dev` and `alpha`; the miner keeps the old behaviour): one dataset at a time, the old one freed (once no check is using it) before the new one is allocated, builds one at a
time, no build ahead of time. **Measured after:** 4.01 GiB for the whole process across three epoch boundaries and every sync variant (harness `real_pow_sync` with `TENERO_POW_LOW_MEMORY=1`, 70 blocks, epochs of 20); the first block of an epoch took
2.9 to 3.0 s (the build, 2.76 s), the median block 179 ms. Six tests (`pow_low_memory.rs`; three guards broken on purpose, three tests failed as they should) and one that pins the node's setting (`alpha_network.rs`). **The price is a wait of about 3 s once per
epoch, and a rebuild after a reorganisation across a boundary** (`THREAT_MODEL.md` has the note). **Not done:** a dataset kept on disk (only one 16 MiB slice is read per check, so steady memory could be tiny, but a build still needs the 4 GiB and the code
would be consensus-critical); a smaller dataset or a light-verification design (both change the rules and the memory-hardness argument: a decision for the owner with a re-run of the simulation); the seed server can now be an 8 GB one.

**The plan as it was written:**

* **The program's icon inside the `.exe` (the owner's, 2026-10-03):** the circular logo (`assets/tenero.ico`, made by `tools/make_icons.py`) is today only
  the window icon and a shortcut's icon, so a copied `tenero-wallet-gui.exe` shows the default one. Embedding it needs a Windows resource compiled into the
  program at build time: **a build dependency (the `embed-resource` crate, MIT, or the older `winres`) or the platform's resource compiler. Rule 3: ask the
  owner before adding it, with its licence and tree listed.** The Linux build gets its icon through the `.desktop` file instead. The release notes say that
  the file is still not code-signed.

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

### M11.4 What testers need (my suggestions; the owner picks): README, TESTING.md and the facts check WRITTEN 2026-10-04; the rest is open

**Done (2026-10-04):** `README.md` rewritten (banner first and unaltered; what it is and is not in the first screen; how it works; the proof of work with measured, simulated and argued kept apart; how to get it; the `alpha` network;
what is next with no dates; links); `docs/TESTING.md` (the testers' guide: what to expect to break, requirements, the app way and the command-line way, finding peers, what to attach and not to attach, resets);
`docs/README_FACTS.md` (every number traced to code, a document, a measurement or arithmetic; links checked; the README searched for words it must never use); small fixes in `docs/RUNNING.md` (alpha in the tables, the icon).
**Seeds (2026-10-04, the owner's decisions: the first seed on a rented Linux server; a built-in list per network plus `--seed`):** the mechanism is built and tested (`ALPHA_SEEDS`, now one entry: the author's server, 2026-10-05; `no_builtin_seeds`; `check_seed_list`), and `docs/RUNNING_A_SEED.md` was followed on a real Contabo server on 2026-10-05 (installed from the verified package, started as a service, reached and checked from outside; node memory 4.0 GiB at height 2). **Earlier open item, now done:** renting the server (a node on `alpha` needs about 8 GB of RAM at least, which excludes the cheapest sizes; unpriced), a hand test of the Linux build, putting the address in `ALPHA_SEEDS`, and more operators than one. **Still open:** more than one seed operator (one exists, built in: the author's server), a status page, the "small group first" decision, the licence is BSD-3-Clause as it stands, a diagnostics bundle (planned, not built; the guide says so),
the README was updated when `v0.1.0-alpha.1` was published (2026-10-05, a pre-release, from commit 642b08b; the owner pressed Publish).

**The plan as it was written:**

* **A full rewrite of `README.md` (the owner's request, 2026-10-03; not started).** More professional, and **the banner image stays at the
  top, unaltered** (`assets/banner.webp`; the circular logo `assets/logo-circle.webp` may sit beside it). Today's README describes the
  Python prototype (one node, a secp256k1 wallet, "no networking yet") and is out of date. It should read as a project front page, and
  it must cover:
  1. **What Tenero is, in a paragraph, and what it is not:** an experimental proof-of-work coin, a learning project, **unaudited, no
     value, a test network that will be reset**: the labels in `CLAUDE.md` stay in the first screen, not at the bottom.
  2. **How it works,** at the level of someone who knows what a blockchain is: the chain and its rules (60-second blocks, the difficulty
     adjustment, emission, the flexible block size), the privacy design (ring signatures, hidden amounts, and **what the interim output scheme
     does not give**: it is not Carrot and not private in Monero's sense), the programs (`tenerod`, the miner, the wallet app, the command-line
     wallet), and how the pieces talk (peer network, the loopback control interface).
  3. **How the proof of work resists ASICs, explained honestly.** The design argument: each attempt multiplies a ChaCha20-generated matrix by a
     16 MiB slice of a dataset of about 4.3 GiB that is rebuilt every epoch (each slice depends on earlier ones, so keeping part of it does not
     help), so an attempt is limited by how fast memory can be read, which is what a GPU's memory system is built for and a dedicated chip would
     need to match with the same memory. **Say what is measured and what is argued, as `CLAUDE.md` rule 5 requires:** measured = the rate on the
     owner's card (33 to 36 thousand attempts per second, the miner's own "reads" estimate of memory traffic, the CPU numbers in
     `docs/BENCHMARKS.md`); argued = that no ASIC advantage is possible, which **nobody has shown and the README must not claim**. It is "designed
     to be ASIC-resistant, by making memory bandwidth the bottleneck; unreviewed; resistance is an economic argument, not a proof, and the
     design has had no independent cryptographic or hardware review." Also say what resistance does NOT protect against (a large GPU
     owner or rental, a small network's 51 % risk, the timestamp issue and its fix). Every number in this section is checked against
     `docs/CONSENSUS.md` and `docs/BENCHMARKS.md` before it is published (the facts check below).
  4. **How to set it up:** requirements (OS, RAM for the 4.3 GiB dataset, GPU and driver, disk), building from source and running the packaged
     build (M11.3), first run of the wallet app, starting the node, mining, a payment, what the labels mean, where things are stored.
  5. **The test network plan:** what the first test release is for and not for, who it is for (the small group first), how to connect (seed
     nodes, adding a peer by address), the fresh chain with no premine, the parameters (starting difficulty, epoch length, the timestamp
     rule), the reset policy and `docs/EMERGENCY_PLAN.md`, how to report a problem, and the roadmap (Carrot, the independent review, the long
     public test). Honest about dates: none is promised.
  6. **Links** to `docs/` for the details (consensus, wire protocol, threat model, known issues, running, benchmarks), the licence (once the
     owner chooses one), and the security contact.
  * **Written last, after M11.2 and M11.3**, because the setup and test-network sections state the final parameters and the real way to install;
    sections 1 to 3 can be drafted before. **A facts check before it is merged:** each number and claim in the README is traced to a document, a
    test or a measurement (the test counts, the emission schedule, the block time, the dataset size, the speeds); anything that cannot be traced
    is removed or marked as an estimate. The text must pass the same rules as the app's screens: never describe it as money, as private in
    Monero's sense, as audited, or as ASIC-proof. **The Python prototype's quick-start moves to `docs/PYTHON_LEGACY.md`** until M11.1 retires it,
    so nothing is lost.

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

The stored-record checksum is in and the emergency plan is finished and drilled (M11.0); `main` is the Rust program with CI green and the long fuzz runs started (M11.25). The repository contains only the Rust code, the independent references, the vectors, the docs and the packaging; the build
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
