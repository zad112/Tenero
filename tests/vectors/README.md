# Golden test vectors

Fixed inputs with the outputs the Python reference produces for them. They are how another
implementation of this coin (a C++ or Rust rewrite, a GPU kernel) is checked against the reference
**bit for bit**. Nothing here is random and nothing depends on the clock: regenerating gives
identical bytes.

Read `docs/CONSENSUS.md` for what each rule means. Every file is JSON with `schema`, `name` and
`description` fields; big numbers are hex strings (or ints that fit in 64 bits).

| file | what it pins down | size |
|---|---|---|
| `chacha20.json` | ChaCha20 (RFC 8439): the block function, the bare permutation on arbitrary states, keystream digests | small |
| `matmulhash_small.json` | the proof of work at small sizes: dataset hashes, every step of full attempts (seed, slice, X, C, fold sums, mix, digest), and the fold on its own | small |
| `matmulhash_real.json` | the same at the real parameters, for the first 8 of the 256 slices | small |
| `matmulhash_deep.json` | real parameters, slices up to 77 (each depends on all earlier ones) | slow to check |
| `matmulhash_full.json` | a hash of every one of the 256 slices (committed; regenerate with `--full`, see below) | slow, 4.3 GiB RAM |
| `matmulhash_gather.json` | the GATHERED attempt (`docs/CONSENSUS.md` 8.3: beta and dev from height 500) at small sizes: every step (seed, pick key, the column numbers, X, C, fold sums, mix, digest) | small |
| `matmulhash_gather_real.json` | the gathered attempt at the real parameters on the epoch-0 dataset (committed; regenerate with `--full`) | slow, 4.3 GiB RAM |
| `pow_misc.json` | epoch seeds and numbering, `bits_to_target`, the cheap pre-check | small |
| `emission.json` | rewards by height for several schedules (including the edge cases) | small |
| `difficulty.json` | the difficulty adjustment and the median-time rule over 20 block-time scenarios | small |
| `fees_and_size.json` | minimum fee, oversize penalty, the block-size median and its window | small |
| `units.json` | parsing and formatting coin amounts | small |
| `chains.json` | 36 whole chains and whether they are valid; each `rule:` case breaks exactly one rule | small |
| `v2_serialization.json`, `v2_ids.json`, `v2_merkle.json`, `v2_genesis.json`, `v2_fees.json`, `v2_emission.json`, `v2_work.json` | the DATA MODEL and ARITHMETIC of the version 2 rewrite (`docs/CONSENSUS_V2.md`, a draft): canonical serialization with its invalid cases, header hash and block id, transaction ids, the Merkle root, the genesis block and chain id, the dynamic minimum fee and the 150 kB median floor, the 8-decimal emission, and the work of a target. Made by `reference/tools/make_vectors_v2.py` (`--check` / `--write`), which has its own reference and is separate from `make_vectors.py` | small |
| `v2_wire.json` | the PEER-TO-PEER WIRE PROTOCOL (`docs/WIRE_PROTOCOL.md`, a draft): every message of `tenero-net` as a whole frame, malformed frames with the error a decoder must give, and the prefixes on which a stream decoder must already fail. Made by `reference/tools/make_vectors_wire.py` (`--check` / `--write`), an independent Python implementation | medium |
| `interim_scheme.json` | the wallet's **INTERIM output scheme** (`crates/tenero-wallet`, NOT Carrot): addresses (valid and invalid), and for six outputs the keys, the sender's randomness, every field of the output, and what the receiver recovers (amount, mask, one-time secret). Made by `reference/tools/make_vectors_interim.py` (`--check` / `--write`), an independent Python implementation with its own Ed25519 arithmetic | small |
| `wallet_proofs.json` | the wallet's **message signatures and payment proofs** (`crates/tenero-wallet/src/proofs.rs`, `docs/WALLET_PROOFS.md`; **unaudited**): four signed messages, and nine proofs (received, sent and key, for an ordinary output, a second output and a block reward) with the randomness fixed so the bytes are exact. Made by `reference/tools/make_vectors_proofs.py` (`--check` / `--write`), an independent Python implementation | small |
| `control.json` | the CONTROL PROTOCOL between a wallet and a node on one machine (`docs/CONTROL_PROTOCOL.md`, a draft): every request and answer as a body and a frame, malformed bodies with the error a decoder must give, and the frame length rule. Made by `reference/tools/make_vectors_control.py` (`--check` / `--write`), an independent Python implementation | medium |
| `v3_serialization.json`, `v3_ids.json`, `v3_genesis.json`, `v3_weight.json`, `v3_shape.json`, `v3_tree_schedule.json` | the **version 3 data model of the `gamma` network** (`docs/CONSENSUS_V2.md` section 15): the 91-byte output, the transaction with its implied ephemeral-key count and fixed payment ID, the prunable part with its reference height; the v3 ids, tags and proof message; the gamma, dev and test genesis; weight (prefix + prunable / 4); the shape rules and reference heights; which outputs enter the curve tree when. Made by `reference/tools/make_vectors_v3.py` (`--check` / `--write`), checked by `crates/tenero-core/tests/v3_vectors.rs` | v3_serialization 495 KiB, the others small |
| `upstream_monero_bpp.json` | **third-party data, not made by our reference:** two real Monero mainnet Bulletproofs+ range proofs, verbatim from monero-oxide's tests (source, commit and licence are in the file). Imported by `reference/tools/import_upstream_vectors.py`, not regenerated by `make_vectors.py`. They show the pinned proof library verifies real Monero proofs | small |
| `upstream_monero_clsag.json` | **third-party data, as above:** one real Monero mainnet transaction with two CLSAG ring signatures, with each input's ring of 16, key image and pseudo-output. Its signatures verify against Monero's own signature hash (computed by monero-oxide, a test-only dependency) | small |
| `upstream_monero_fcmp_pp.json` | **third-party data, as above:** Monero's own FCMP++ test proofs (1, 2, 4, 8 and 128 inputs, 7 tree layers) with the signable transaction hash, tree root, pseudo-outputs and key images each was made for, verbatim from Monero's FCMP++ stressnet release `v0.19.0.0-beta.3.0` (BSD-3-Clause; source and commit in the file). Imported by `reference/tools/import_upstream_fcmp_pp.py`. They show the pinned `monero-fcmp-plus-plus` verifies proofs made by Monero's code (`docs/FCMP_CARROT_PLAN.md`, G1) | medium (430 KB) |
| `upstream_monero_carrot_convergence.json` | **third-party data, as above:** Monero's Carrot "convergence" values, verbatim from `tests/unit_tests/carrot_convergence.cpp` of its stressnet release `v0.19.0.0-beta.3.0` (BSD-3-Clause): one master secret and the expected result of each Carrot derivation from it. Imported by `reference/tools/import_upstream_carrot.py`; `crates/tenero-carrot/tests/upstream_convergence.rs` reproduces all 36 | small |
| `carrot_monero.json` | **made by Monero's own C++ `carrot_core`** (same tag), not by our code: `reference/tools/carrot_harness` is built inside a Monero checkout in WSL (`run.sh`) and runs Monero's functions on fixed pseudo-random inputs (a seeded generator, so a rerun gives the same bytes). Six accounts with every secret and address; twelve output sets (2, 4 and 16 outputs; subaddresses, integrated addresses, internal and special self-sends) with every output and what each account finds in it, spend keys and key images included; coinbase outputs; unclamped X25519 edge cases; the unbiased hash to point. `crates/tenero-carrot/tests/upstream_monero_carrot.rs` must reproduce every byte | medium (120 KB) |
| `curve_tree_monero.json` | **made by Monero's own C++ curve-tree code** (same tag): `reference/tools/carrot_harness/tree_vectors.cpp` grows an FCMP++ curve tree block by block with Monero's production `get_tree_extension`, re-hashes every layer with Monero's own test audit after each block, and records the root and the layer count. 19 blocks from 1 to 26,498 outputs, landing exactly on every layer boundary (38, 684, 25,992). The outputs are derived from their index (Keccak), so only the roots and four sample leaves are stored. `crates/tenero-crypto/tests/curve_tree_monero.rs` must reproduce every root, growing and trimming | small |
| `legacy_account_model.json` | the CURRENT account-model formats (Python-JSON serialization, ECDSA over SHA-1). **Legacy: documents what the reference does, not what to copy** | small |

## Using them

```
python reference/tools/make_vectors.py --check          # do the committed files match the reference?
python -m pytest reference/tests/test_vectors.py -q     # Python checks itself against them
TENERO_SLOW_VECTORS=1 python -m pytest reference/tests/test_vectors.py -q     # also deep and full
```

A new implementation should load these files and reproduce every value. Start with `chacha20.json`,
then `matmulhash_small.json`, then `chains.json`.

## The full vector

`matmulhash_full.json` is committed, but `--check` does not verify it, because that needs the whole
4 GiB dataset in memory. It is checked only by the slow test
(`TENERO_SLOW_VECTORS=1 python -m pytest reference/tests/test_vectors.py`, about 4.3 GiB of RAM). Its slices
0-3, 7, 15, 31, 63 and 77 also agree with `matmulhash_real.json` and `matmulhash_deep.json`. To
regenerate it (only after a proof-of-work change), on a machine with about 4.3 GiB of free RAM:

```
python reference/tools/make_vectors.py --full --threads 4
```

## Changing a vector

A changed vector is a **consensus change**. Do not edit the files by hand. Change the reference,
update `docs/CONSENSUS.md`, run `python reference/tools/make_vectors.py --write` (add `--deep` if the
proof of work changed), and say in the commit message what changed and why.
