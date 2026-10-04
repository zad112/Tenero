# CLAUDE.md

**Tenero**: an experimental proof-of-work coin with a GPU proof of work ("matmulhash v2"), written in Rust (rewritten from a Python prototype; the Python
was retired at M11.1 and survives as the frozen `reference/` and on the `legacy-python` branch), with Monero-style privacy as the design target. It is a learning
project: unaudited, one node, not for real value. Never describe it otherwise.

## Commands (owner's setup: Windows, PowerShell, an RTX 5070 Ti; cargo is in `$HOME\.cargo\bin`)

```powershell
cargo test --workspace                       # the Rust code against the golden vectors, and the proptest fuzz-style properties (`tests/fuzz_*.rs`); no GPU needed
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
$env:TENERO_SLOW_VECTORS = "1"; cargo test --test pow_vectors      # the deep and full (4.3 GiB) vectors
cargo test --release -p tenero-gpu -- --ignored --test-threads=1  # the REAL GPU checks: only on the owner's machine, CUDA 13.4 bin\x64 on PATH
cargo test --release -p tenero-chain --test real_pow_sync -- --ignored --nocapture   # real-PoW sync cost (owner's machine, ~4.3 GiB RAM)
cargo test --release -p tenero-miner --test gpu_mining -- --ignored --nocapture --test-threads=1   # GPU end-to-end check and speed test
cargo run --release -p tenero-app --bin tenerod -- --data $HOME\scratch --network test   # a node on the SHA-256 test chain (see docs/RUNNING.md)

# The Python REFERENCE (reference/: frozen; the second implementation the Rust is checked against; not a program)
cd reference; ..\.venv\Scripts\activate; pip install -r requirements-dev.txt
python -m pytest -q                          # about 340 tests, no GPU needed; 5 expected failures (KNOWN_ISSUES)
cd ..; python reference/tools/make_vectors.py --check        # do the golden vectors still match the reference?
python reference/tools/make_vectors_v2.py --check            # the version 2 data-model vectors (a DRAFT design)
python reference/tools/make_vectors_wire.py --check          # the peer-to-peer wire protocol vectors (a DRAFT)
python reference/tools/make_vectors_control.py --check       # the control protocol vectors
python reference/tools/make_vectors_interim.py --check       # the interim wallet scheme vectors
python reference/tools/make_vectors_proofs.py --check        # the message-signature and payment-proof vectors
$env:TENERO_SLOW_VECTORS = "1"; python -m pytest reference/tests/test_vectors.py -q     # also the deep vectors
```

The old Python miner, wallet and command line (`miner.py`, `cli.py`, `gpu_test.bat` ...) are NOT in this tree: they are on the `legacy-python` branch / `python-final` tag.

## Where things are

- `docs/CONSENSUS.md` the rules, in enough detail to build another implementation from
- `docs/KNOWN_ISSUES.md` verified flaws not to carry over; `docs/ARCHITECTURE.md` the parts;
  `docs/REWRITE_PLAN.md` the proposed plan and the open decisions; `docs/M8_PLAN.md` the proposed plan for
  the node, network, miner and wallet; `docs/THREAT_MODEL.md` what can go wrong and what defends it (M9, first version, by the author, not an audit); `docs/EMERGENCY_PLAN.md` what happens when a rule is wrong (a draft; the owner's decisions are marked); `docs/SEED_POLICY.md` how a new node picks its first peers and how many seed operators are needed (measured in a simulation, with its limits); `docs/M10_M11_PLAN.md` the proposed plan for the usable program (CLI, miner
  reporting, wallet GUI) and the first test release on a fresh chain
- `docs/WIRE_PROTOCOL.md` the DRAFT byte encoding of the peer-to-peer messages;
  `docs/CONSENSUS_V2.md` the DRAFT design of the rewrite's data model (outputs, serialization, genesis,
  privacy staging); `docs/BENCHMARKS.md` measured CPU and GPU numbers; `crates/` the Rust code
  (`tenero-core` rules and data model, `tenero-store` storage, `tenero-chain` block validation and fork choice, `tenero-node` the node core (mempool), `tenero-net` the protocol engine and network simulator,
  `tenero-gpu` the GPU engine). `tenero-net` also has the encrypted channel (`noise.rs`, via `snow`, which has had
  no formal audit: an owner-approved exception to rule 3) and the real-socket transport (`transport.rs`);
  `docs/TESTNET.md` runs a private test network on one machine. `tenero-miner` is the miner (a hook of the node's
  loop; GPU, CPU and SHA-256 test backends): `cargo test --release -p tenero-miner --test gpu_mining -- --ignored
  --nocapture --test-threads=1` is the GPU end-to-end check and speed test (the owner's machine). `tenero-crypto` holds the CLSAG and Bulletproofs+ verification (`RingCtProofs`; `Node::new`, and so `tenerod`, uses it by default and refuses to run without a real proof check; only the bare `Validator` and the test simulator can run unchecked) and the prover; Carrot is not built. `tenero-app` is the programs: `tenerod` (the node: settings file, logging, archive or pruned, clean shutdown, the in-process miner), `tenero-wallet` (the wallet command line) and `tenero-seedcheck` (checks a list of seeds; it cannot tell if one is honest), joined by the loopback control interface; what the node and the miner show on the screen is decided in one place, `ui.rs` (golden tests; the log file keeps the detail) (`docs/CONTROL_PROTOCOL.md`; `python tools/make_vectors_control.py --check`); `docs/RUNNING.md` says how to run them, and ctrlc and rpassword are owner-approved dependencies. `tenero-miner` also reads the GPU's health through `nvml-wrapper` (an owner-approved dependency; for the screen only). `tenero-gui` is the wallet app (`tenero-wallet-gui`: an egui window that starts and stops the node and the miner as processes of their own; its logic is in `core.rs` and is tested without a window; `eframe` and `qrcode` are owner-approved dependencies). `tenero-wallet` is the wallet library: keys, scanning, building payments, the encrypted file, the 24-word seed phrase and several accounts per seed (`mnemonic.rs`, `purse.rs`; `bip39` is an owner-approved dependency), payment requests (`request.rs`), message signatures and payment proofs (`proofs.rs`, `docs/WALLET_PROOFS.md`; **unaudited**, `python tools/make_vectors_proofs.py --check`), on an **INTERIM output scheme that is not Carrot** (its limits are at the top of `interim.rs`; `python tools/make_vectors_interim.py --check`; argon2 and chacha20poly1305 are owner-approved dependencies). Nothing cryptographic is audited as used.
- `tests/vectors/` golden vectors (see its README); `reference/tools/make_vectors.py` generates them, and
  `reference/tools/make_vectors_v2.py` the version 2 data-model ones

## Rules

1. **A consensus change is deliberate.** Change the reference, update `docs/CONSENSUS.md`, regenerate the
   vectors with `python reference/tools/make_vectors.py --write` (add `--deep` if the proof of work changed), and say
   what changed and why in the commit. Never edit a vector file by hand. `test_the_committed_vectors_are_current`
   fails if the reference and the vectors disagree.
2. **Never commit secrets or data.** `wallets/`, `chain.json` and `mempool.json` are git-ignored and hold
   private keys and coins. Check `git status` before every commit, and never print or log a private key.
3. **No home-made cryptography.** Use audited libraries. Ask before adding a dependency, and note its licence.
4. **Bit for bit, or it is wrong.** Any implementation of ChaCha20, the dataset fill, the fold or the attempt
   must reproduce `tests/vectors/` exactly. CUDA changes must pass `reference/tests/test_fused_kernels.py` (which
   includes mutation tests), keep `crates/tenero-gpu/kernels/matmulhash.cu` identical to the Python copy (`reference/tests/test_kernel_source_copy.py`), and, before they are called done, pass the Rust GPU checks on the owner's machine (`cargo test --release -p tenero-gpu -- --ignored --test-threads=1`).
5. **Say what is measured and what is estimated.** There is no GPU in CI or in a sandbox, so never claim GPU
   speed without a number from the owner's machine.
6. **Every rule gets a test.** A rule with no vector or test is not finished.
7. **Known issues stay visible.** Do not fix an item in `docs/KNOWN_ISSUES.md` silently: its strict-xfail test in
   `reference/tests/test_known_issues.py` will start passing, which fails the suite until the marker is removed.
8. **Resource limits are design constraints.** The miner stays within 6 CPU cores (`--max-cores`), and the CPU
   check needs about 4.3 GiB of RAM per epoch dataset (about 8.6 GiB briefly during the prefetch).
9. Explain plainly, with the reasoning, and mention risks and uncertainty rather than hiding them.
