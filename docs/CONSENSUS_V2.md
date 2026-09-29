# Consensus rules, version 2 (a DRAFT PROPOSAL for the native rewrite)

Status: **a design on paper. Nothing here is implemented, audited or final.** It is milestone M5 of
`REWRITE_PLAN.md`. Where the owner has not decided, the text says **PROPOSED** and gives the reasoning; the
list of open decisions is in section 13. `CONSENSUS.md` (v1) still describes what the Python reference does
and is not changed by this document.

This is an experimental learning project. Its privacy claims are limited by everything in section 12.

How to read it: **Carried over** means the v1 rule is kept as it is (same vectors). **New** means a v2 rule.
**Not consensus** means software may choose freely. Rules that fix a flaw from `KNOWN_ISSUES.md` say **[issue N]**.

---

## 1. What carries over from v1, unchanged

- **Proof of work: matmulhash v2**, bit for bit (`CONSENSUS.md` section 8), including epochs, the cheap
  pre-check and the full check. Its input is a 32-byte `header_hash`; how that is computed changes (section 5).
- **Emission**: schedule, halving, cap and tail (section 5 of v1), **60 s blocks**, **LWMA difficulty** with its
  clamps, the **median-time rule**, and the **block-size median, penalty and hard limit** (sections 6 and 7 of
  v1). Their Rust implementations already pass the v1 vectors.
- **The reward rule**: the coinbase pays exactly `reward(height) - penalty + fees`.

What v1 did with accounts, ECDSA, JSON and balances is **replaced** (sections 3 to 7).

## 2. Building blocks (audited libraries only)

Nothing below is home-made. Every choice is **PROPOSED** and needs a check of the crate's licence and status
on the day it is added (the plan's rule), because these libraries move.

| need | proposed source | licence (verify) | note |
|---|---|---|---|
| Ed25519 group and scalar arithmetic | `curve25519-dalek` | BSD-3-Clause | audited (Quarkslab, 2019); parts formally verified |
| Ristretto / hashing to points | `curve25519-dalek` | BSD-3-Clause | only if a scheme needs it |
| BLAKE2b, SHA-256 | RustCrypto `blake2`, `sha2` | MIT OR Apache-2.0 | `sha2` is already in use |
| X25519 (Carrot's view key exchange) | `x25519-dalek` or Monero's `mx25519` | BSD-3-Clause / check | must match Carrot's encoding rules |
| CLSAG ring signatures, Bulletproofs+ | the `monero-oxide` crates (`monero-clsag`, `monero-bulletproofs`) | MIT | **audit status unclear from what I could find: verify on crates.io and the audit reports before depending on them** |
| FCMP++ (later) | Monero's own Rust implementation | check | **not final**: see section 12 |

Monero's own C++ code is no longer an option for the reason that we chose Rust only (`REWRITE_PLAN.md` decision 1).

## 3. Numbers and units  (decision 6, PROPOSED)

- Amounts are `u64` **atomic units**. All amounts inside a transaction are hidden in commitments, but every
  amount is proven to lie in `[0, 2^64)`, so a `u64` is the natural width.
- **PROPOSED: 8 decimals** (`1 coin = 100,000,000 units`). The supply cap is 20,000,000 coins = `2 * 10^15`
  units, far below `2^64`. **Monero's 12 decimals do not fit here**: 20,000,000 coins at 12 decimals is
  `2 * 10^19`, which is more than `2^64 = 1.8 * 10^19`. Four decimals (v1) fit but are coarse for fees on
  small transactions. Eight leaves 9 orders of magnitude of headroom and matches Bitcoin's habit.
- **Consequence:** the emission constants in v2 are in the new units (initial reward 20 coins =
  `2,000,000,000` units, tail 0.5 coins = `50,000,000`, the minimum fee rate likewise). The *rules* are v1's;
  only the unit changes, and v2 gets its own emission vectors so that a mistake in the scaling is caught.
- A target is a 256-bit unsigned integer, as in v1. Timestamps are `u64` seconds (a negative timestamp is
  never valid, so v1's signed type is not needed).

## 4. Serialization  (decision 4, PROPOSED) [issue 5]

One canonical binary format, defined here once, with vectors (section 11). The consensus hash of any object
is over its **serialized bytes**, never over a re-encoding of parsed fields.

- **Integers**: unsigned, **fixed width, little-endian** (`u8`, `u16`, `u32`, `u64`). **No varints**: a varint
  has several encodings of one value, which is exactly how malleability creeps in.
- **Fixed-size byte arrays** (hashes, keys, commitments): written as is, no length.
- **Variable byte strings**: a `u32` length, then the bytes. Each field has a **maximum length** in
  consensus; a longer one is invalid and must be refused **before** allocating memory for it.
- **Lists**: a `u32` count, then the elements. Each list has a consensus maximum count, checked first.
- **No booleans, no strings, no optional fields, no floating point.** A field that may be absent is a list of
  0 or 1 elements or a versioned variant.
- **Decoding is strict**: any trailing byte, a count or length over its maximum, or a short read makes the
  object invalid. **One value has exactly one encoding**, so `decode` then `encode` reproduces the input.
- Curve points and scalars are 32 bytes on the wire; whether they are canonical *and on the correct subgroup*
  is checked by the cryptographic layer (section 7), and a non-canonical encoding is invalid.

## 5. Blocks, ids and the chain id  (partly decision 4, PROPOSED)

### 5.1 The header

```
BlockHeader = version    u16                 the rules version (section 10)
              prev_id    [32]
              timestamp  u64                 seconds
              tx_root    [32]                Merkle root of the block's transaction ids (5.3)
              nonce      u64
              mix        [64]                the matmulhash mix; 64 zero bytes on a SHA-256 test chain
```

The block body is `coinbase` then the other transactions (section 6). The header commits to them through
`tx_root`, whose first leaf is the coinbase transaction id.

### 5.2 The proof-of-work hash and the block id

- `header_hash = SHA-256( "tenero block header v2" ‖ version ‖ prev_id ‖ timestamp ‖ tx_root )`: the header
  **without** `nonce` and `mix`, with a domain tag (the tag is a fixed 22-byte ASCII string). This is the
  32-byte `header_hash` the proof of work already takes.
- **matmulhash** (unchanged): `seed = SHA-256(header_hash ‖ nonce as 8 LE bytes)`; the attempt produces
  `mix`; `digest = SHA-256(seed ‖ mix)`. **The block id is this `digest`.** It commits to the header (through
  `header_hash`), the nonce and the mix, so nothing in the header can change without changing the id.
- **SHA-256 test chain**: `digest = SHA-256(header_hash ‖ nonce as 8 LE bytes)`, and `mix` is 64 zero bytes.
- A block is valid only if `digest` (as a big-endian integer) is **strictly below** the required target.
  Validation runs the cheap check, then the full check, as in v1.

### 5.3 The transaction Merkle root [issue 5]

RFC 6962 style, so that a second-preimage attack and the duplicate-last-leaf mistake (Bitcoin's CVE-2012-2459)
are impossible by construction:

```
leaf(id)      = SHA-256( 0x00 ‖ id )
node(l, r)    = SHA-256( 0x01 ‖ l ‖ r )
root([])      = SHA-256( "" )                                   (the empty tree)
root([x])     = leaf(x)
root(n items) = node( root(first k), root(rest) ),  k = the largest power of two below n
```

An odd number of leaves is never padded by repeating the last one.

### 5.4 Genesis and the chain id [issues 3, 4]

- **The genesis block is fixed in the software and checked** (v1 never checked it). It has no transactions and
  is exempt from the proof of work. Its header is `version = 2`, `prev_id = 32 zero bytes`, `timestamp = 0`,
  `nonce = 0`, `mix = 64 zero bytes`, and `tx_root = SHA-256("tenero genesis" ‖ network_label)`, where
  `network_label` is a fixed ASCII name such as `"tenero experimental network 1"`. **PROPOSED**: the label is what
  makes two Tenero networks different.
- `genesis_id = SHA-256( "tenero genesis id v2" ‖ serialized genesis header )`.
- **`chain_id = genesis_id`.** Every signature and proof in a transaction is bound to it (section 7), so a
  transaction cannot be replayed on another chain **[issue 4]**. A node refuses a chain whose genesis
  differs.

### 5.5 Time [issue 6]

- A block whose `timestamp` is not above the **median time** rule (v1 section 7) is invalid.
- A block more than 120 seconds ahead of the node's clock is **not yet acceptable**: the node holds it and
  reconsiders later. It is **never permanently invalid** for being early, so two honest nodes converge and a
  chain that was valid yesterday is valid today.

## 6. Transactions and outputs (the output model)  (decision 3, PROPOSED)

Coins are **outputs**: a one-time key, a hidden amount and the data the receiver needs to find it. An output
is created once and spent at most once. There are no accounts and no balances in consensus.

### 6.1 An output (a Carrot "enote")

```
Output = onetime_address   [32]   Ko: the one-time public key (Ed25519)
         amount_commitment [32]   Ca: a Pedersen commitment to the amount
         amount_enc        [8]    the amount, encrypted for the receiver
         view_tag          [3]    lets the receiver skip 255 in 256 outputs quickly
         ephemeral_pubkey  [32]   De: for the receiver's key exchange
         anchor_enc        [16]   the encrypted "Janus anchor" (protects against a malicious sender)
```

123 bytes, fixed. The field list and sizes are from the **Carrot** addressing specification as summarised
when this draft was written; **they must be checked against the specification's own tables before
implementation.** A **coinbase output** has no commitment: its amount is a plaintext `u64` and its commitment
is the fixed value defined by the specification for a public amount.

### 6.2 A transaction

```
Transaction = version        u16
              inputs         list<Input>     1 ..= MAX_INPUTS
              outputs        list<Output>    2 ..= MAX_OUTPUTS        (at least 2: see below)
              fee            u64             units, public
              extra          bytes           <= MAX_EXTRA
              proof_data     bytes           <= MAX_PROOF: the range proof, the pseudo-output commitments and
                                                            the membership proofs, in the format of `version`
Input       = key_image      [32]            the spend-once tag
              membership     variant by version: 6.3
```

- **At least two outputs** in every spend (the second is the sender's own "change", possibly worth zero), as
  the Carrot design requires, so that every spend has a self-send output.
- **PROPOSED constants**: `MAX_INPUTS = 32`, `MAX_OUTPUTS = 16`, `MAX_EXTRA = 128`, `MAX_PROOF = 32 KiB`.
  These bound memory, must be chosen from measured proof sizes, and are consensus once set.
- **Payment ids** and the transaction's ephemeral data live in `extra`, encrypted per the Carrot rules.
- The **coinbase transaction** is different: `version`, `height u64`, a list of outputs with plaintext
  amounts, and `extra`. Its outputs cannot be spent until `COINBASE_MATURITY = 60` blocks pass; ordinary
  outputs need `SPEND_MATURITY = 10` confirmations (**PROPOSED**, Monero's values).

### 6.3 Membership: the stand-in now, FCMP++ later (decision 3)

An input proves "the spent output is one of many, without saying which", and reveals its key image so that
spending it twice is detected.

- **P1 (the stand-in)**: a **ring**: `ring: list<u64>` of exactly `RING_SIZE = 16` distinct **global output
  indexes** in ascending order, plus a **CLSAG** signature inside `proof_data`. Every ring member must exist
  and be mature. **PROPOSED**, Monero's current size.
- **P2 (FCMP++, when a stable and audited implementation exists)**: the membership variant becomes a
  reference to a curve-tree root (a block height) plus a proof. **The output format above does not change**:
  the leaf for an output is `(Ko, its key-image generator, Ca)`, all derivable from the fields it already has.
- A `version` change with an **activation height** (section 10) switches from P1 to P2. Old transactions stay
  valid under their version.
- **Decoy selection** (which 15 other outputs a wallet puts in a ring) is wallet policy, not consensus.

### 6.4 Identity, replay and malleability [issue 1]

- **The transaction id** is `SHA-256( "tenero tx v2" ‖ serialized transaction )`, over every byte, so it
  commits to the proofs as well as the prefix. The **coinbase id** is `SHA-256( "tenero coinbase v2" ‖
  serialized coinbase )`: a different tag, so bytes that happened to parse as both never share an id.
- **What stops a payment being replayed or doubled is the key image, not the id.** A key image can appear
  once in the whole chain. Changing a signature's bytes gives a different id but the same key image, so the
  copy is rejected. v1's "flip `s` and pay twice" attack has nothing to work on.
- Every signature and proof covers the `chain_id` (5.4) and the whole transaction prefix.

## 7. Cryptographic rules (P1)

The exact algorithms are those of the audited libraries and the Carrot and RingCT specifications; this
document does not restate them, and **must not invent them**. What consensus adds:

1. **Points and scalars** must be canonical encodings; points must be in the prime-order subgroup and not the
   identity where a scheme requires it; a scalar must be reduced below the group order.
2. **Balance**: for each input a **pseudo-output commitment** `Cp_i` is given; the CLSAG proves it commits to
   the same amount as the ring member spent. Then `sum(Cp_i) - sum(Ca_j) - fee * H = 0`, where `H` is the
   commitment generator for amounts. **A coinbase adds no inputs**, and its amounts are public.
3. **Range**: one aggregated **Bulletproofs+** proof that every output amount is in `[0, 2^64)`.
4. **Key images**: for every input, the key image is not in the spent set, and no two inputs of one
   transaction share one; **inputs are sorted by key image ascending**, with no duplicates (one canonical
   order).
5. **Chain binding**: the message every signature covers includes `chain_id`.

## 8. Validating a block (the order of the checks)

For a block at height `h` on top of a known parent:

1. `version` is the rules version active at `h` (section 10); `prev_id` is the parent; `timestamp` passes
   5.5 (median time; too early is invalid, too far ahead is deferred).
2. The **required target** for `h` (v1 section 7) and the **cheap** proof-of-work check.
3. The **full** proof-of-work check (needs the epoch's dataset).
4. The block **decodes strictly** (section 4), its size is within `2 * median` (v1 section 6), and `tx_root`
   matches its transactions.
5. The first transaction is the coinbase for height `h`, no other transaction is a coinbase, and its
   plaintext outputs sum to **exactly** `reward(h) - penalty + sum(fees)`.
6. For every other transaction, in order: it decodes strictly; its counts and lengths are within their
   maximums; **no input's key image is already spent** (in the chain or earlier in this block); every ring
   member exists and is mature; the proofs of section 7 verify.
7. Then the block's outputs are added to the output set (each gets the next global index) and its key images
   to the spent set.

**Fork choice** (M6): the valid chain with the largest **cumulative work**, `sum(2^256 // target)` over its
blocks. A reorganisation removes the outputs and key images of the blocks it undoes.

**Fees**: the consensus rule is only `fee >= 0` and the balance equation. **PROPOSED**: a *minimum* fee is
**relay policy**, not consensus (v1's minimum was consensus, `KNOWN_ISSUES` 10), because the oversize penalty
and the tail emission already govern spam, and a consensus minimum invalidates old blocks when it is retuned.

## 9. Addresses and keys  (decision 5, PROPOSED)

**PROPOSED: follow Monero's Carrot specification**, read at implementation time, not invent one. Carrot is
the addressing protocol designed for FCMP++, is maintained by Monero's developers, and fixes weaknesses of
classic CryptoNote addresses (the "Janus" attack and burning bugs). It provides the key hierarchy (spend,
view, generate-address), subaddresses, integrated addresses with an 8-byte payment id, and outgoing view keys.

- The text encoding of an address (checksum, prefix, human-readable form) is **left to the specification and
  to the wallet milestone**. A network prefix for Tenero is chosen there so that a Tenero address cannot be
  mistaken for a Monero one.
- Multisig is **not** part of P1 (the plan puts it in P3).

## 10. Rule versions and activation heights [issue 9]

Every consensus constant and every rule set is tied to a **rules version**, and a table of
`(version, first height)` is part of the software. A block's `version` must equal the version active at its
height. **Retuning a constant is a new version with a future activation height**, never an edit, so old blocks
keep their meaning.

| version | from height | what it is |
|---|---|---|
| 2 | 0 (genesis) | P1: ring signatures (CLSAG), Bulletproofs+, Carrot outputs |
| 3 | to be decided | P2: FCMP++ membership proofs (only when a stable, audited implementation exists) |

## 11. What M5 delivers as vectors

Vectors for the parts that need no cryptography are produced by a small standard-library Python reference,
`tools/make_vectors_v2.py`, so that the Rust code in M6 is checked against an independent implementation:

| file | what it pins down |
|---|---|
| `v2_serialization.json` | the primitive encodings; 14 valid objects with their exact bytes (an output, an input, a transaction at its limits, a coinbase, a header, a block); 22 invalid encodings covering all four failure kinds (short read, trailing bytes, a count out of range, a length over its maximum), including counts of `2^32 - 1` that must be refused without allocating |
| `v2_ids.json` | the header hash, the proof-of-work seed, the block id (matmul and SHA-256 test chain), transaction and coinbase ids, and domain separation |
| `v2_merkle.json` | the Merkle root for 0 to 17, 31, 32, 33 and 100 leaves, and the no-padding and leaf-versus-node properties |
| `v2_genesis.json` | the genesis header and the chain id for three network labels |

`python tools/make_vectors_v2.py --check` compares them with the reference, and `tests/test_vectors_v2.py`
also checks them against the RFC 6962 definition and a from-scratch computation of the hashes, so the
reference is not only agreeing with itself. The limits in the serialization vector are the PROPOSED constants
of section 6; changing one is a consensus change and regenerates the file.

**Not vectors we generate ourselves:** the Carrot derivations, CLSAG, Bulletproofs+ and FCMP++. We import the
**upstream test vectors** of the specifications and libraries when their code is added, because a vector we
made with our own reference would only prove that we agree with ourselves.

## 12. Risks and limits (read before believing any privacy claim)

- **A small anonymity set.** A ring of 16 hides the sender among 16 outputs, and only outputs that exist and
  are mature can be chosen. On a small, young chain with few users, chain analysis (timing, ring reuse, the
  fact that few people transact) can shrink that a lot. The mathematics does not fix this.
- **The bootstrap problem.** Nobody can spend privately until 16 mature outputs exist (at least 76 blocks with
  one coinbase output each). Early transactions have very small rings in practice.
- **A moving target.** As of late September 2026 FCMP++ and Carrot are in a public stressnet and audit
  (a Trail of Bits review of the integration in May 2026 is reported by several sources, which I did not
  check against the report itself), and mainnet activation is not fixed. This draft follows their public design but will need updating to whatever ships.
  Verify at getmonero.org before relying on any of it.
- **Unaudited dependencies of ours.** Whatever crate we depend on for CLSAG or Bulletproofs+ carries its own
  audit status; that must be checked when it is added, and a crate with no audit is a reason to stop.
- **Implementation risk we add.** Constant-time behaviour, side channels, serialization and validation bugs
  are not visible to vectors. The plan's hardening milestone (M9: fuzzing, a written threat model, a review)
  is not optional.
- **No formal proofs of our own protocol.** This is an assembly of published components. Combining them is
  where mistakes hide.

## 13. Decisions this document asks the owner to make

1. **Units** (section 3): 8 decimals (PROPOSED), 4 (v1, coarse) or another.
2. **Address scheme** (section 9): follow Carrot (PROPOSED) or the older classic Monero scheme.
3. **Ring size and maturity** (6.2, 6.3): 16, 60 and 10 (PROPOSED).
4. **Minimum fee** (section 8): relay policy (PROPOSED) or consensus.
5. **Storage** (`REWRITE_PLAN.md` decision 7): PROPOSED `redb`, a pure-Rust embedded database with ACID
   copy-on-write transactions (MIT OR Apache-2.0; verify at the time), in preference to LMDB bindings or
   RocksDB, which need a C or C++ build on Windows. The layout: blocks by id and by height, transactions by id,
   outputs by global index, the spent key-image set, cumulative work, and later the curve tree.
6. **The genesis label** (5.4) and the transaction limits (6.2).
7. **CLSAG and Bulletproofs+ source**: which crate, once its audit status is checked (section 2).
