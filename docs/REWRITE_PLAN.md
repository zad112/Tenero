# Rewrite plan (a proposal, to settle at the start of the rewrite session)

Goal: a fast native node, miner and wallet for this coin with Monero-class privacy. The proof of work
(matmulhash v2), the emission, the difficulty adjustment and the fee and block-size rules carry over.
The account model is replaced by an output model, because privacy needs one: outputs with one-time keys
and hidden amounts, spent at most once, in place of addresses with balances.

Decisions marked **DECIDE** are open. Each has a recommendation, which is only a recommendation.

## What is already fixed

- **matmulhash v2**, bit for bit as in `CONSENSUS.md` section 8, and the CUDA source in
  `tenero/gpubackend.py` (it is already C++).
- **Emission** (20 coins, halving every 525,600 blocks, 20,000,000 coin cap, 0.5 tail), **60 s blocks**,
  **LWMA** difficulty, the **block-size floor with the quadratic penalty and the 2x hard limit** (300 kB in
  v1; **150 kB in version 2**, `CONSENSUS_V2.md` 8.2).
- **`tests/vectors/`** define conformance for all of it.

## Decisions to make first

**Outcome (2026-09-29), details in `CONSENSUS_V2.md` section 13:** 1 Rust only. 2 the cryptography comes from
the audited Rust crates (`curve25519-dalek`, and the `monero-oxide` CLSAG and Bulletproofs+ crates once their
audit report is read and a version pinned), not from Monero's C++. 3 CLSAG rings as a stand-in behind an
FCMP++-ready output format. 4 one canonical fixed-width little-endian serialization, with vectors. 5 Carrot.
6 8 decimals. 7 `redb`, laid out for pruning. 8 Cargo and MSVC (no CMake). New requirement, decision 11: the
chain must be able to run pruned (`CONSENSUS_V2.md` section 14). The original options follow as the record.

1. **Language split. DECIDED (2026-09-29): Rust only, option (b).** The owner does not need C++. The
   CUDA kernels stay C++ (they are CUDA source), built with nvcc; everything else is Rust. CMake and Ninja
   are not needed for the Rust parts. The reasoning below is kept as the record of the options. The FCMP++ cryptography is written in Rust, and Monero itself is a C++ core
   that links that Rust as a static library through a C interface. So realistic options are (a) a C++ core
   plus Rust for the proof systems, as Monero does; (b) a Rust core; (c) all C++ with the proof systems
   reimplemented (not recommended: it is the riskiest way to spend the effort). Recommendation: (a), if C++
   is the goal, and expect a Rust toolchain in the build either way.
2. **Where the cryptography comes from. DECIDE.** Reuse audited implementations (Monero's own code for
   Ed25519 outputs, key images, CLSAG and Bulletproofs+; libsodium for basics; the FCMP++ Rust crates when
   they are stable). Check each one's licence (Monero's code is BSD-3-Clause; confirm its `LICENSE`) and
   maintenance status at the time. **Never write curve arithmetic or zero-knowledge proofs from scratch.**
3. **Privacy staging. DECIDE.** As of late September 2026, Monero's own repository shows FCMP++ still being
   integrated and audited, and some websites' claim that it is already live on mainnet is contradicted by
   that activity; check getmonero.org before relying on either. Options: (i) ring signatures (CLSAG) first,
   behind a small interface, then FCMP++ later (some throwaway work, a private testnet sooner); (ii) make
   the output format FCMP++-ready and wait for stable libraries. Recommendation: define the output and
   transaction format so it is FCMP++-ready (an output key, a key-image generator and a commitment per
   output; a place for a membership proof), and start with a stand-in membership proof behind that
   interface. Be honest that a small chain has a small anonymity set, whatever the maths.
4. **Serialization. DECIDE.** A canonical binary format defined once (fixed-width little-endian or
   varints), with vectors. Not JSON (`KNOWN_ISSUES` 5).
5. **Address and key format. DECIDE.** Follow Monero's current specification (its newer Carrot scheme or the
   classic one), read at the time; do not invent one.
6. **Units. DECIDE.** Four decimals leaves plenty of room (2 * 10^11 units). Monero uses twelve so that
   its whole supply fits in 64 bits. Amounts are hidden in commitments either way.
7. **Storage. DECIDE.** An indexed store (LMDB as Monero does, or RocksDB or SQLite) with the block index,
   the output set, the spent key images and, for FCMP++, the curve tree.
8. **Build and CI. DECIDE.** CMake with a package manager (vcpkg or Conan), MSVC on Windows (the owner's
   platform) and gcc/clang on Linux, cargo for Rust, sanitizers and fuzzing in CI. GitHub Actions on Windows
   as well as Linux. GPU code cannot run in CI: keep the emulator tests and the owner's `gpu_test.bat`.
9. **What happens to the Python. Recommendation:** keep it as the executable reference, the vector
   generator and the tooling, but do not ship it. Retire a Python module only when the native code passes
   its vectors, and say so in the commit.
11. **Pruning. DECIDED as a requirement.** A node can run with most of the history discarded, and a new node
    need not download all of it: the transaction prefix/prunable split, the id over a hash of the prunable
    part, pruned wire forms, deterministic output indexes, and a table layout whose proof table can be
    emptied. See `CONSENSUS_V2.md` section 14 for the layers, the trade-offs and the (unmeasured) sizes.
10. **Networking. DECIDE later.** A peer-to-peer protocol, Dandelion++ for transaction propagation, Tor or
    I2P support, and the **cumulative-work fork rule** (the chain with the largest sum of `2^256 // target`).

## A sensible order, each step ending in something checkable

- **M0. Skeleton.** A branch, the CMake project, CI on Linux and Windows, a JSON vector loader.
  *Done when* CI runs a trivial vector test.
- **M1. Primitives.** SHA-256, ChaCha20, little-endian helpers, 256-bit integers for targets.
  *Done when* `chacha20.json` passes.
- **M2. matmulhash on the CPU.** Dataset fill (portable first; then AVX2), the attempt, the fold, the cheap
  pre-check, the epoch seeds. *Done when* `matmulhash_small`, `matmulhash_real`, `matmulhash_deep`,
  `matmulhash_full` and `pow_misc` pass. In the sandbox, a naive C++ fill was 2x and hand-written AVX2 4.3x
  faster than the numpy reference on one thread.
- **M3. Consensus arithmetic.** Units, emission, difficulty, fee, penalty and median. *Done when*
  `units`, `emission`, `difficulty` and `fees_and_size` pass.
- **M4. The GPU miner without Python.** Reuse the CUDA kernels. *Done when* its self-test matches the CPU
  reference and the owner measures at least the current baseline (about 22,000 attempts/s).
  *Status (2026-09-29): the engine is done, the miner is not.* `crates/tenero-gpu` runs the same three
  kernels (NVRTC) and the cuBLASLt int8 matmul, passes 11 GPU tests including all the golden vectors, and
  measures about 30,000 to 35,000 attempts/s (`docs/BENCHMARKS.md`; the 22,000 baseline is an earlier Python
  measurement, not a same-day one). Not done: a miner program (it needs the chain, so M6 and M8), epoch
  switching and prefetching the next dataset, overlapping the CPU work with the GPU, more than one GPU.
  The CUDA source is a byte-identical copy checked by `tests/test_kernel_source_copy.py`.
- **M5. The new data model, on paper first.** The output model, the canonical serialization, a fixed genesis
  and chain id. Write `CONSENSUS.md` v2 and its vectors before writing the validator.
  *Status (2026-09-29): done as `docs/CONSENSUS_V2.md`, a draft, with six vector files
  (`v2_serialization`, `v2_ids`, `v2_merkle`, `v2_genesis`, `v2_fees`, `v2_emission`) made by
  `tools/make_vectors_v2.py`. The dynamic minimum fee and the version 2 emission are already implemented in
  Rust and pass their vectors. Not vectorised, because it needs the upstream libraries: everything
  cryptographic (Carrot, CLSAG, Bulletproofs+).*
- **M6. Chain state and validation** in the new model, storage, fork choice, reorganisations.
  *Status (2026-09-29): started.* Done: `tenero_core::v2`, the canonical wire form of every version 2 object
  (including the pruned forms), strict decoding, ids, the Merkle root and the genesis block, matching all the
  `v2_*` vectors made by the independent Python reference. Also done: `crates/tenero-store`, the `redb`
  storage laid out for pruning (`CONSENSUS_V2.md` 14.3): append, reorganisation (also through pruned blocks),
  pruning in steps, compaction and a state digest, tested against an in-memory model. Next: block validation
  without the cryptographic proofs, then fork choice (which needs cumulative-work arithmetic).
- **M7. Privacy in stages.** (P1) one-time addresses, Pedersen commitments, range proofs, key images, and the
  stand-in membership proof; (P2) FCMP++ when a stable, audited implementation exists; (P3) subaddresses,
  view keys, integrated addresses, multisig; (P4) network privacy.
- **M8. Networking, the node and the wallet.**
- **M9. Hardening.** Fuzzing, a written threat model, a review plan.

**Do not port the account-model validation** (`chains.json` and the legacy vectors): it is thrown away in M5.
Use those chain cases as a checklist of rules the new model must also enforce in its own terms.

## Risks

- **Scope.** "All of Monero's privacy features" took a large community many years and several audits. Work
  in milestones and keep the project labelled experimental.
- **Cryptographic correctness**, including constant-time behaviour and side channels, which tests do not show.
- **A small anonymity set** on a small chain.
- **A moving target** in FCMP++, and a Rust dependency inside a C++ build.
- **GPU work can only be tested on the owner's machine.**
- **Windows build complexity** with C++, Rust and CUDA together.

## The first hour of the rewrite session

1. `git checkout -b rewrite`; run `python -m pytest -q` and `python tools/make_vectors.py --check`. Confirm
   the baseline is green.
2. `tests/vectors/matmulhash_full.json` is already committed. Run the slow vector test once on the owner's
   machine (4.3 GiB of free RAM) to confirm slices 78-255, which no other vector cross-checks:
   `TENERO_SLOW_VECTORS=1 python -m pytest tests/test_vectors.py -q`.
3. Read `docs/CONSENSUS.md` and `docs/KNOWN_ISSUES.md`; settle decisions 1 to 4.
4. Build M0.
