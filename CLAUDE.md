# CLAUDE.md

**Tenero**: an experimental proof-of-work coin in Python with a
GPU proof of work ("matmulhash v2"), being prepared for a rewrite to native code with Monero-style
privacy. It is a learning project: unaudited, one node, not for real value. Never describe it otherwise.

## Commands (owner's setup: Windows, PowerShell, a `.venv`, Python 3.14, an RTX 5070 Ti)

```powershell
.venv\Scripts\activate                       # use the venv's python, never the system one
pip install -r requirements-dev.txt
python -m pytest -q                          # about 550 tests, no GPU needed
python tools/make_vectors.py --check         # do the golden vectors still match the reference?
$env:TENERO_SLOW_VECTORS = "1"; python -m pytest tests/test_vectors.py -q     # also the deep vectors
.\gpu_test.bat                               # the REAL GPU check: only on the owner's machine
python miner.py <address> [--pow sha256]     # sha256 = a small chain a CPU can mine
$env:TENERO_DATA = "$HOME\scratch"          # do experiments on a scratch chain, not the real one

# The native rewrite (branch `rewrite`; Rust only): cargo is in $HOME\.cargo\bin
cargo test --workspace                       # the Rust code against the golden vectors; no GPU needed
cargo clippy --workspace --all-targets -- -D warnings
$env:TENERO_SLOW_VECTORS = "1"; cargo test --test pow_vectors      # the deep and full (4.3 GiB) vectors
cargo test --release -p tenero-gpu -- --ignored --test-threads=1  # the GPU checks: owner's machine, CUDA 13.4 bin\x64 on PATH
cargo test --release -p tenero-chain --test real_pow_sync -- --ignored --nocapture   # real-PoW sync cost (owner's machine, ~4.3 GiB RAM)
python tools/make_vectors_v2.py --check      # the version 2 data-model vectors (a DRAFT design)
python tools/make_vectors_wire.py --check    # the peer-to-peer wire protocol vectors (a DRAFT)
```

## Where things are

- `docs/CONSENSUS.md` the rules, in enough detail to build another implementation from
- `docs/KNOWN_ISSUES.md` verified flaws not to carry over; `docs/ARCHITECTURE.md` the parts;
  `docs/REWRITE_PLAN.md` the proposed plan and the open decisions; `docs/M8_PLAN.md` the proposed plan for
  the node, network, miner and wallet
- `docs/WIRE_PROTOCOL.md` the DRAFT byte encoding of the peer-to-peer messages;
  `docs/CONSENSUS_V2.md` the DRAFT design of the rewrite's data model (outputs, serialization, genesis,
  privacy staging); `docs/BENCHMARKS.md` measured CPU and GPU numbers; `crates/` the Rust code
  (`tenero-core` rules and data model, `tenero-store` storage, `tenero-chain` block validation and fork choice, `tenero-node` the node core (mempool), `tenero-net` the protocol engine and network simulator,
  `tenero-gpu` the GPU engine). `tenero-net` also has the encrypted channel (`noise.rs`, via `snow`, which has had
  no formal audit: an owner-approved exception to rule 3) and the real-socket transport (`transport.rs`);
  `docs/TESTNET.md` runs a private test network on one machine. `tenero-miner` is the miner (a hook of the node's
  loop; GPU, CPU and SHA-256 test backends): `cargo test --release -p tenero-miner --test gpu_mining -- --ignored
  --nocapture --test-threads=1` is the GPU end-to-end check and speed test (the owner's machine). `tenero-crypto` holds the CLSAG and Bulletproofs+ verification (`RingCtProofs`, opt-in; the default validator still accepts every proof) and the prover; Carrot is not built. `tenero-wallet` is the wallet (library only): keys, scanning, building payments, the encrypted file, on an **INTERIM output scheme that is not Carrot** (its limits are at the top of `interim.rs`; `python tools/make_vectors_interim.py --check`; argon2 and chacha20poly1305 are owner-approved dependencies). Nothing cryptographic is audited as used.
- `tests/vectors/` golden vectors (see its README); `tools/make_vectors.py` generates them, and
  `tools/make_vectors_v2.py` the version 2 data-model ones

## Rules

1. **A consensus change is deliberate.** Change the reference, update `docs/CONSENSUS.md`, regenerate the
   vectors with `python tools/make_vectors.py --write` (add `--deep` if the proof of work changed), and say
   what changed and why in the commit. Never edit a vector file by hand. `test_the_committed_vectors_are_current`
   fails if the reference and the vectors disagree.
2. **Never commit secrets or data.** `wallets/`, `chain.json` and `mempool.json` are git-ignored and hold
   private keys and coins. Check `git status` before every commit, and never print or log a private key.
3. **No home-made cryptography.** Use audited libraries. Ask before adding a dependency, and note its licence.
4. **Bit for bit, or it is wrong.** Any implementation of ChaCha20, the dataset fill, the fold or the attempt
   must reproduce `tests/vectors/` exactly. CUDA changes must pass `tests/test_fused_kernels.py` (which
   includes mutation tests) and, before they are called done, the owner's `gpu_test.bat`.
5. **Say what is measured and what is estimated.** There is no GPU in CI or in a sandbox, so never claim GPU
   speed without a number from the owner's machine.
6. **Every rule gets a test.** A rule with no vector or test is not finished.
7. **Known issues stay visible.** Do not fix an item in `docs/KNOWN_ISSUES.md` silently: its strict-xfail test in
   `tests/test_known_issues.py` will start passing, which fails the suite until the marker is removed.
8. **Resource limits are design constraints.** The miner stays within 6 CPU cores (`--max-cores`), and the CPU
   check needs about 4.3 GiB of RAM per epoch dataset (about 8.6 GiB briefly during the prefetch).
9. Explain plainly, with the reasoning, and mention risks and uncertainty rather than hiding them.
