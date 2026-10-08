# FCMP++ and Carrot: the plan for 0.3.0-gamma.1 and the `gamma` network

**A PLAN, written 2026-10-08. Nothing here is built yet.** The owner's decisions are marked **DECIDED**; everything else is
**PROPOSED** and open to change. Tenero is an experimental learning project: unaudited, one developer and an assistant, not for
real value. This release puts cryptography into consensus that **Monero itself has not switched on yet** and that is only
partly audited, and adds a Carrot implementation in Rust that **nobody has audited at all**. Every release note, the README and the
wallet will say so.

## 1. What this release is

A **new network, `gamma`**, with a new genesis block. From block 0 it has:

- **FCMP++** (full-chain membership proofs): a spend proves "the coin I spend is one of *all* the outputs on the chain", not one of
  16. There are no ring signatures on `gamma`, ever. This is Monero's design, implemented by Monero's Rust code (monero-oxide), pinned.
- **Carrot** (Monero's new addressing protocol): how outputs are made for a receiver and found by the receiver, with Janus
  protection, subaddresses, integrated addresses, view-only wallet tiers and forward secrecy against a future quantum adversary
  (conditional on the address being unknown to it). **We write it in Rust ourselves from the specification**, because no Rust
  implementation has been published; it must reproduce Monero's C++ implementation's results bit for bit.
- **The gathered proof-of-work attempt from block 0** (`docs/CONSENSUS.md` 8.3), with no first-design phase.

**Why a new network and not a fork of `beta`:** a fork at a height would leave transition code in consensus forever (two
key-image generators, ring signatures before the height, interim outputs in the tree). On `gamma` none of that exists, so a future
main net can be built on the `gamma` rules with no dead fork logic. The price: `beta` coins do not carry over (they are test
coins), and `gamma` starts with an anonymity set of zero (section 10).

What "better than Monero" can honestly mean here is structural: a chain with FCMP++ and Carrot from its first block and no
legacy at all, so the whole chain is the anonymity set. It does **not** mean the cryptography is more trustworthy than
Monero's: we run their code before they do, and add code of our own around it.

## 2. Decisions

| # | decision | outcome |
|---|---|---|
| F1 | which chain | **DECIDED (owner, 2026-10-08): a new network `gamma`**, FCMP++ and Carrot from genesis. (Replaces a first decision, the same day, to hard-fork `beta` at height 5000.) |
| F2 | the FCMP++ code | **DECIDED (owner, 2026-10-08): monero-oxide's crates, pinned to an exact commit**, as an owner-approved exception to rule 3 (partly audited), labelled unaudited wherever the program shows privacy. |
| F3 | Carrot | **DECIDED (owner, 2026-10-08): written in Rust here, from the specification; no C++ in the build.** Checked against Monero's C++ `carrot_core` results (section 8). |
| F4 | wallet scope | **DECIDED (owner, 2026-10-08): all of it in 0.3.0-gamma.1, nothing deferred:** core Carrot, subaddresses, view-only wallets (including outgoing), integrated addresses, payment proofs redone. |
| F5 | order | **DECIDED (owner, 2026-10-08): this first**; the remote wallet and Tor plan waits, and the remote wallet is designed afterwards around curve-tree paths. |
| F6 | address text | **DECIDED (owner, 2026-10-08): Monero-style base58** with Tenero's own network prefixes (a varint before the keys, a 4-byte Keccak checksum after): 99 characters, 110 for an integrated address. **The first four characters show the NETWORK** (owner's choice of "option 1"): `TENg…` on `gamma`, `TENd…` on `dev`, `TENt…` on `test`, for every kind of address (main, subaddress, integrated: three different prefix numbers that all give the same four characters; the wallet names the kind). `TENm…`, `TENs…` and `TENi…` are kept for a future main net. Computed, not guessed: `Ten…` (lower case) is impossible in this encoding, four fixed characters is the most every key allows. The prefix numbers are fixed in G5, after checking them against known CryptoNote coins. |
| F7 | `x25519-dalek` | **DECIDED (owner, 2026-10-08): approved** (BSD-3-Clause, same authors as `curve25519-dalek`, already in the tree), for Carrot's key exchange. |
| F8 | interim addresses | **DECIDED (owner, 2026-10-08): the new wallet refuses to pay a `tni1` address.** On `gamma` they cannot exist anyway. |
| F9 | `dev` and `test` | **DECIDED (owner, 2026-10-08)**, as amended by F1: with no fork, `dev` and `test` simply use the `gamma` rules from block 0 (the "height 100" answer was for the fork). |
| F10 | transaction shape | **DECIDED (owner, 2026-10-08): strict, in consensus**: outputs sorted by one-time address; a 2-output transaction has one ephemeral key, a larger one a distinct key per output; `extra` is exactly the 8-byte encrypted payment ID. Every transaction from every wallet looks alike (Monero allows a free-form `extra`, which can identify a wallet). |
| F11 | size and fees | **DECIDED (owner, 2026-10-08)**: fees stay **per real byte** (a typical transaction costs about 3x what it did on `beta`: FCMP++ proofs are bigger, and Monero's grow too, see 4.2); and, so that FCMP++ does not lower transactions per block nor make ordinary nodes' disks grow faster: **(a)** block limits and the block-size median count the prunable bytes (the proofs) at **one quarter** ("weight" = prefix + prunable / 4), so a typical v3 transaction weighs less than a v2 one; **(b)** nodes are **pruned by default** (proofs kept for recent blocks only; seeds and the explorer stay archive), so an ordinary node's disk grows about as on `beta`; **(c)** a 2-output transaction stores its one ephemeral key **once** (32 bytes saved; no privacy effect: the two outputs carry the same key anyway, by Carrot's design). Archive nodes store up to about 3.5x more per full block: the honest cost. |
| F12 | inputs | **DECIDED (owner, 2026-10-08): no count limit**, only today's size limits (75,000-byte transaction, 64 KiB proof): about 48 to 126 inputs depending on the tree's depth, at most about 1.4 s to check one transaction. |
| P1 | the pin | PROPOSED: every monero-oxide crate at **`31c26d96eaadbba910ffe3613ad8b4cf9c598a93`** (branch `fcmp++`, 2026-08-18, "Incorporate zkSecurity's audit of generalized-bulletproofs"): the exact commit Monero's own stressnet release `v0.19.0.0-beta.3.0` pins. |
| P2 | `beta` and `alpha` in 0.3.0 | **DECIDED (owner, 2026-10-08): the 0.3.0 programs run `gamma`, `dev` and `test` only.** `beta` carries on with the 0.2.0-beta.4 programs for as long as its seeds run, and `alpha` likewise. Then CLSAG, the ring rules, the interim scheme's scanner and the first-design proof of work leave the 0.3.0 consensus path entirely. |
| P3 | `gamma`'s identity | **DECIDED (owner, 2026-10-08):** genesis label `"tenero gamma network 1"`, port **38352** (peer) and the control port next to it as for the others, the version 2 emission and difficulty unchanged, and **`version = 3`** for blocks and transactions from block 0, so that a `gamma` object can never be mistaken for a `beta` one. |

## 3. Launch

There is no fork deadline. 0.3.0-gamma.1 launches `gamma` when sections 4 to 8 are done and tested, and not before. The release
needs:

- **Seeds for `gamma`** (`docs/SEED_POLICY.md`): **DECIDED (owner, 2026-10-08): the two `beta` seed hosts**, `195.26.244.245` and
  `194.238.27.60`, on port **38353**. The Contabo server is upgraded by the owner.
- **Miners and pool** on the new network: genesis is a new chain, so mining starts from difficulty's starting target.
- The `beta` network keeps running on 0.2.0-beta.4 (P2) for anyone still using it; its coins are not moved to `gamma`.

## 4. Consensus: version 3 on `gamma` (rule 1: the reference, `docs/CONSENSUS_V2.md`, the vectors)

### 4.1 Blocks and the coinbase

- Every block carries `version = 3`, and needs the gathered proof-of-work attempt from height 0.
- **Every coinbase `onetime_address` must be a canonical point in the prime-order subgroup, not the identity.** (On `beta`, coinbase
  keys were never checked; ordinary outputs were, `ringct.rs` `strict_point`.) The same check applies to every output key and
  commitment, so **no torsion clearing** is needed in the tree.
- **The coinbase's public-amount commitment** is `1*G + amount*H`, now confirmed by the Carrot specification (4.1 there: "implied to
  be `C_a = G + a H`"), so its "provisional" label in `CONSENSUS_V2.md` 7.1 goes.
- Coinbase outputs pay **main addresses only** (Carrot 7.9), a wallet and pool rule; consensus cannot see addresses.

### 4.2 The version 3 transaction

The prefix is the version 2 prefix (key images, the 123-byte outputs, the fee, `extra`) with `version = 3`. The prunable part
changes. PROPOSED:

```
PrunableV3 = reference_height  u64       the block whose tree root the proof is made against
             proof_data        var       pseudo-outputs, Bulletproofs+, and the FCMP++ proof (all inputs together)
```

There are no rings. The number of tree layers is not sent: the verifier knows it from the tree at `reference_height`.
The transaction id keeps its definition (the prefix, then the hash of the prunable bytes).

- **The reference block** PROPOSED: `reference_height` must be at most the current tip and no older than
  `MAX_REFERENCE_AGE = 1440` blocks (a day at the target). Too old is refused so that a node keeps a bounded number of past roots;
  a transaction that waited too long in a mempool is rebuilt by its wallet.
- **Limits:** `MAX_PROOF` is 64 KiB today and a transaction at most 75,000 bytes (decision 10). An FCMP++ proof grows with the
  number of inputs and the tree's depth; `FcmpPlusPlus::proof_size(inputs, layers)` gives it exactly, and milestone G1 tabulates it
  to see how many inputs fit. (G1: the FCMP++ proof alone fits 64 KiB with at most 126 inputs at 1 layer, 69 at 4, 48 at 7 and
  31 at 12; `docs/BENCHMARKS.md`.) Monero's own stressnet limits are 128 inputs, 16 outputs and 12 layers; we keep 16 outputs and
  propose **at most 12 layers**. The minimum fee's reference weight is re-measured (it was set for CLSAG).
- **Key images** are Carrot's: `L = x * Hp²(K_o)`, the **unbiased** hash-to-point, for every output. There is only one kind,
  because `gamma` has no older outputs.

### 4.3 The curve tree (the new state every node keeps)

FCMP++ proves membership in a Merkle-like tree of all spendable outputs. Its leaves and its hashing come from Monero's design
(`src/fcmp_pp/curve_trees.cpp` in Monero's stressnet release); the tree hashing itself (`hash_grow`, `hash_trim`) is in the
pinned crate. **The bookkeeping of the tree (which outputs, in what order, when, and undoing it on a reorganisation) is consensus
code we write.**

- **A leaf** per output is six Selene scalars: the Weierstrass x and y of `O` (the output key), `I` (its key-image generator,
  `Hp²(O)`) and `C` (the commitment; for a coinbase output, `1*G + amount*H`).
- **When an output enters:** when it becomes spendable: a coinbase output 60 blocks after its block, an ordinary one 10 blocks
  after (`COINBASE_MATURITY`, `SPEND_MATURITY`), in global output index order.
- **Widths:** 38 children per Selene layer, 18 per Helios layer (Monero's `SELENE_CHUNK_WIDTH`, `HELIOS_CHUNK_WIDTH`).
- **The root** after every block is kept for `MAX_REFERENCE_AGE` blocks at least, so that a transaction can reference it.
- **Reorganisations:** undoing a block removes the outputs it added to the tree (`hash_trim`). A tree that differs between two
  nodes is a chain split, so the tree vectors and a reorg test with random fork depths (proptest) come first in G3.

### 4.4 Storage

The tree lives in the `redb` store, next to the output set, with a per-block undo record. **A pruned node must still keep the
tree**: it is state, not history. A leaf is stored as the output's index (its points come from the output set) and each layer
above holds about 1/38, then 1/18, of the one below, 32 bytes a node: **measured in G3, about 2,800 points (90 KB) for 100,000
outputs**, so RAM is small next to the 4 GiB proof-of-work dataset (rule 8). Growing costs about 0.3 ms an output, undoing a block
of 100 outputs 8 ms (`docs/BENCHMARKS.md`). As built (G3): a trim recomputes the last chunk of each layer from its children,
rather than subtracting as Monero's `hash_trim` does; the result is the same root by definition, and is tested.

## 5. Checking a proof

`FcmpPlusPlus::verify` queues each transaction's proof into three batch verifiers (Ed25519, Selene, Helios). The node verifies a
block's transactions as one batch, and a lone transaction (the mempool) as a batch of one. If a block's batch fails, the block is
refused; a failed mempool transaction is refused.

**Measured in G1** (`docs/BENCHMARKS.md`, the owner's Ryzen 9 5900X, one thread): checking costs about 25 ms for a 1-input
proof and 16 to 20 ms an input for larger ones, against 1.92 ms an input for CLSAG; a block's proofs checked as one batch cost
about 9.4 ms an input. Making a proof takes 0.5 to 0.9 s for one input (1 to 7 layers), 0.8 to 2.4 s for two and 2 to 5 s for
four. That matters for:
- denial of service: a peer that sends a large invalid proof costs us its verification time, so the per-peer limits in
  `THREAT_MODEL.md` get an entry;
- the wallet on a small machine, and the pool's payout batches.

Monero's own test proofs (`tests/vectors/upstream_monero_fcmp_pp.json`) verify with the pinned crates (G1).

## 6. Carrot in Rust (crate `tenero-carrot`)

A transcription of the specification (`jeffro256/carrot`, `carrot.md`, BSD-3-Clause; audited as a **specification** by Cypher Stack
in November 2024 and September 2025, and Monero's C++ code in February 2026). **Our Rust code has had no audit.** It is built only
from audited primitives: `curve25519-dalek` (Ed25519, the point conversion and the X25519 ladder) and, from monero-oxide, `Blake2bMonero` and the generators. (`x25519-dalek`, approved as F7, turned out to be unusable: it always clamps, and Carrot multiplies unclamped. It is not a dependency.)

What it covers, in the order of the specification:
- **The key hierarchy (its 5.2, the "new" hierarchy only; `gamma` has no legacy wallets):** from a master secret `s_m`: the
  prove-spend key, the view-balance secret, the generate-image key, the view-incoming key and the generate-address secret; the
  account spend key is `K_s = k_gi*G + k_ps*T`.
- **Where `s_m` comes from (Tenero-specific, PROPOSED):** wallets keep the 24-word BIP39 phrase and several accounts per phrase
  (`mnemonic.rs`, `purse.rs`). `s_m` for account `n` is a domain-separated hash of the BIP39 seed and `n`, defined in
  `CONSENSUS_V2.md` section 9 with vectors from the Python reference. The same 24 words give a `beta` (interim) wallet and a
  `gamma` (Carrot) wallet, which never share keys.
- **Addresses (F6):** main addresses, subaddresses (6.1.3), integrated addresses with an 8-byte payment ID; base58 with Tenero's
  network bytes for `gamma`, `dev` and `test`.
- **Making outputs:** the input context, the X25519 ephemeral key, the shared secret, the view tag, the one-time key, the
  commitment's mask, the encrypted amount, the Janus anchor, the self-send ("internal" and "special") outputs, "one payment, one
  change", and the mandatory self-send output.
- **Scanning:** the view tag, then the full check, including the Janus check and the burning-bug defence.
- **Key images:** `x * Hp²(K_o)`.

## 7. The wallet, the node's interface, and the programs

- **On `gamma` a wallet is Carrot only.** The interim scheme stays only in the 0.2 programs for `beta` (P2). The 0.3.0 wallet refuses
  `tni1` addresses (F8).
- **Spending:** the wallet needs, for each output it spends, its **path in the tree** at the reference height. A new control
  request gives it the path. A wallet that asks a node for one path tells that node which output it is spending. That is harmless
  for a node on the same machine; for the remote wallet later, it must fetch whole layers, not one path.
- **Proving** is done by the pinned crate (`Fcmp::prove`, and the spend-authorisation and linkability proof in `sal`). It needs fresh
  randomness and secret keys, so it runs in the wallet, never in the node.
- **The control protocol** (`docs/CONTROL_PROTOCOL.md`) gains the path request, the reference root, and version 3 transactions;
  its vectors are regenerated (`make_vectors_control.py`).
- **View-only wallets:** the "view-received" tier (incoming only) and the "view-all" tier (incoming and outgoing, from the
  view-balance secret). A view-only wallet cannot spend, and the program says so.
- **Payment proofs** (`proofs.rs`, `docs/WALLET_PROOFS.md`): message signatures carry over; proving a payment ("I sent X to this
  address in this transaction") is rebuilt on Carrot's derivations. **Spend proofs and reserve proofs have no published FCMP++
  design yet** (Monero's stressnet lists transaction proofs as unsupported); they are left out rather than invented, and the
  release notes say so.
- **The pool** pays through the wallet's batches, so it gets Carrot and FCMP++ through the wallet; miners give Carrot main
  addresses; the pool protocol vectors change where they carry addresses.
- **The GUI, the CLI and the explorer** get the new address forms, subaddress lists and view-only wallets, and a new banner that says
  what is and is not audited (the interim banner goes).

## 8. Tests and vectors (rules 4 and 6)

**Vectors we did not make ourselves come first**: a vector from our own code only shows that we agree with ourselves.

| what | where the expected results come from |
|---|---|
| FCMP++ verification | Monero's `tests/data/fcmp_pp_verify_inputs_*.bin` (stressnet `v0.19.0.0-beta.3.0`): every one must verify, and every one with a flipped byte must not |
| Carrot derivations, scanning, key images | Monero's C++ `carrot_core` at the same tag, run in the owner's WSL `Ubuntu` by a small harness that prints JSON (`tools/`), committed as `tests/vectors/carrot_*.json`. C++ runs **only to make vectors**, never in our build |
| the curve tree's leaves and roots | the same C++ build (`curve_trees.cpp`) on fixed output sets: growth across many layers, and trimming |
| `gamma`'s genesis, the version 3 serialization, `s_m` from BIP39, base58 addresses | the Python reference (`make_vectors_v2.py`, and a new Carrot-wallet vector file), rule 1 |
| spending once | a key image spent in one block is refused in a later one, in the mempool, and twice in one block |
| reorganisations | proptest: random chains with random fork points; the tree root after a reorg equals the root of building the winning chain from scratch |

`docs/THREAT_MODEL.md` gets new entries: verification cost as denial of service, the tree as a split risk, the unaudited Carrot
transcription, and the reliance on a branch that Monero may still change.

## 9. Milestones

| | what | needs |
|---|---|---|
| G0 | this plan agreed (section 11); `CLAUDE.md` records the rule 3 exception | the owner |
| G1 | the pinned crates in the build (P1), licences listed; Monero's FCMP++ test proofs verify; **verify and prove times measured** on the owner's machine. **Done 2026-10-08** | nothing else |
| G2 | `tenero-carrot`: keys, addresses, outputs, scanning, against the C++ vectors. **Done 2026-10-08**: Monero's 36 convergence values and the harness's 6 accounts, 12 output sets, coinbase outputs, X25519 and hash-to-point cases are reproduced bit for bit. Left for G5: `s_m` from the BIP39 seed and the base58 address text (Tenero's own, with Python reference vectors) | the WSL harness |
| G3 | the curve tree and its reorganisation, against the C++ vectors. **Done 2026-10-08** (`tenero-crypto::curve_tree`): Monero's roots after 19 blocks (1 to 26,498 outputs, 1 to 4 layers) reproduced growing and trimming; a property test of random block and reorganisation histories against from-scratch builds; real FCMP++ proofs from our paths verify against our root (1 to 4 layers, several inputs); timings in `BENCHMARKS.md`. **Storage moved to G4**: which outputs enter the tree and when is part of the `gamma` rules, so the tree is stored where those rules are applied | G1 |
| G4 | the `gamma` network: genesis, version 3 transactions, validation, the mempool; the curve tree in the store (its layers by index in `redb`, a per-block record of the leaves added, roots kept for `MAX_REFERENCE_AGE`), grown and trimmed with the chain; CLSAG and the version 2 rules out of the 0.3.0 path (P2); tested on `test` and `dev` | G1, G3 |
| G5 | the wallet: Carrot receiving and sending, FCMP++ proving, the control requests, the CLI | G2, G4 |
| G6 | subaddresses, integrated addresses, view-only tiers, payment proofs | G5 |
| G7 | the GUI, the explorer, the pool; the docs; release notes; `gamma` seeds; launch (`release.yml` titles every release "(beta, experimental)": it must say gamma) | G6 |

All of G1 to G7 are in 0.3.0-gamma.1 (F4).

## 10. Risks (read before believing any privacy claim)

- **Unaudited code in consensus.** The FCMP++ crates are only partly audited (Veridise, zkSecurity and Cypher Stack reviewed
  parts; the audit notes in the pinned repository say so in their own words). Our tree bookkeeping and our Carrot are not audited at all.
- **A moving target.** Monero may still change FCMP++ or Carrot before its own main net (a community estimate puts that around
  April 2027, not a commitment). If it does, `gamma` keeps what it launched with, and does not get their fixes for free.
- **A split or a double spend** is the worst outcome of a tree or key-image bug. Section 8 starts there.
- **An empty set at launch.** The anonymity set is every spendable output on `gamma`: zero until the first coinbase output matures (60
  blocks), then a handful, all of them coinbase outputs of a few miners. **The first spends on `gamma` hide among very few outputs**,
  and timing still leaks. The wallet should say so while the set is small (PROPOSED: under 1,000 outputs).
  **A guess, not a measurement:** with one coinbase output a block and no other transactions, the set passes 16 outputs at about
  block 76 (60 to mature, then 16 more) and 1,000 at about block 1,060, roughly 1.2 days at `beta`'s rate of about 870 blocks a day.
  But the number of outputs is not the number of *people*: while only a few miners own every output, a spend hides among those
  few people, as a ring of 16 on `beta` did too.
- **Performance unknown** until G1 measures it.

## 11. Questions for the owner

None open on 2026-10-08: P2, P3 and the seeds were decided the same day. New ones are added here as they come up.

## Sources (read 2026-10-08)

- Monero's FCMP++ and Carrot stressnet release `v0.19.0.0-beta.3.0` (seraphis-migration/monero on GitHub): `src/fcmp_pp/`,
  `src/carrot_core/`, `src/cryptonote_config.h`, `tests/`.
- monero-oxide, branch `fcmp++` (`monero-oxide/ringct/fcmp++`, `crypto/fcmps`, `audits/`).
- The Carrot specification, `jeffro256/carrot` (`carrot.md`, `audits/README.md`).
- Monero's FCMP++ milestone (69% in August 2026) and `jeffro256/fcmp-carrot-plan` (a non-binding estimate of week 16 of 2027).
