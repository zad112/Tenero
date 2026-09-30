# Consensus rules, version 2 (a DRAFT PROPOSAL for the native rewrite)

Status: **a design on paper. Nothing here is implemented, audited or final.** It is milestone M5 of
`REWRITE_PLAN.md`. The owner decided the main questions on 2026-09-29 (section 13 lists them); text marked
**DECIDED** records one, **PROVISIONAL** means approved for now and to be re-measured, and **PROPOSED** is
still open. `CONSENSUS.md` (v1) still describes what the Python reference does and is not changed by this
document.

This is an experimental learning project. Its privacy claims are limited by everything in section 12.

How to read it: **Carried over** means the v1 rule is kept as it is (same vectors). **New** means a v2 rule.
**Not consensus** means software may choose freely. Rules that fix a flaw from `KNOWN_ISSUES.md` say **[issue N]**.

---

## 1. What carries over from v1, unchanged

- **Proof of work: matmulhash v2**, bit for bit (`CONSENSUS.md` section 8), including epochs, the cheap
  pre-check and the full check. Its input is a 32-byte `header_hash`; how that is computed changes (section 5).
- **Emission**: schedule, halving, cap and tail (section 5 of v1), **60 s blocks**, **LWMA difficulty** with its
  clamps, the **median-time rule**, and the **block-size median, penalty and hard limit** (sections 6 and 7 of
  v1), except that the median's floor is **150,000 bytes instead of 300,000** (section 8.2). Their Rust
  implementations already pass the v1 vectors.
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
| CLSAG ring signatures, Bulletproofs+ | the `monero-oxide` crates (`monero-clsag`, `monero-bulletproofs`) **DECIDED** | MIT | both 0.1.0, published 2026-07-31 (an earlier 0.0.1 from December 2024), few downloads. A Cypher Stack audit of the code they came from (Serai's `/networks/monero`) was finished in May 2025 and published in August 2025 in the `monero-oxide` repository. **Its scope and findings have not been read by us, and 0.1.0 may include later changes: reading it, pinning an exact version and running the upstream tests come before the crate is added (M7).** |
| FCMP++ (later) | Monero's own Rust implementation | check | **not final**: see section 12 |

Monero's own C++ code is no longer an option for the reason that we chose Rust only (`REWRITE_PLAN.md` decision 1).

## 3. Numbers and units  (decision 6, **DECIDED: 8 decimals**)

- Amounts are `u64` **atomic units**. All amounts inside a transaction are hidden in commitments, but every
  amount is proven to lie in `[0, 2^64)`, so a `u64` is the natural width.
- **`1 coin = 100,000,000 units`.** The supply cap is 20,000,000 coins = `2 * 10^15` units, far below
  `2^64`. **Monero's 12 decimals do not fit here**: 20,000,000 coins at 12 decimals is `2 * 10^19`, which is
  more than `2^64 = 1.8 * 10^19`. Four decimals (v1) fit but are coarse for fees on small transactions.
- **The v2 constants, in units:** initial reward 20 coins = `2,000,000,000`; tail 0.5 coins = `50,000,000`;
  cap `2,000,000,000,000,000`; halving every 525,600 blocks. The *rules* are v1's; only the unit changes, and
  `v2_emission.json` guards the scaling (the schedule is v1's default times 10^4, the cap is reached exactly
  at block 2,334,400, and the tail starts at 2,334,401).
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
Transaction = prefix  ‖  prunable

prefix   = version        u16
           inputs         list<Input>     1 ..= MAX_INPUTS
           outputs        list<Output>    2 ..= MAX_OUTPUTS        (at least 2: see below)
           fee            u64             units, public
           extra          bytes           <= MAX_EXTRA
prunable = rings          list<list<u64>> exactly one ring per input, in the order of the inputs; each ring is
                                          at most MAX_RING global output indexes (P1, 6.3)
           proof_data     bytes           <= MAX_PROOF: the range proof, the pseudo-output commitments and
                                          the membership proofs, in the format of `version`
Input    = key_image      [32]            the spend-once tag: the ONLY thing an input holds in the prefix
```

- **The split matters** (section 14): the prefix is what a node needs after it has verified a transaction (the
  key images and the outputs), and the prunable part, which is most of the bytes, is only needed to verify it
  once. The id (6.4) commits to the prefix and to a hash of the prunable part, so the rings and proofs can be
  discarded later without losing the ability to recompute any id or Merkle root.
- **The rings are in the prunable part, not in the prefix** (decided 2026-09-29): nothing needs a ring after the
  transaction has been verified, and the id still covers every ring index through the prunable hash, so a ring
  cannot be changed, swapped between inputs or shortened under an existing id. This takes a typical prefix
  from about 620 to **356 bytes**.
- **At least two outputs** in every spend (the second is the sender's own "change", possibly worth zero), as
  the Carrot design requires, so that every spend has a self-send output.
- **PROVISIONAL constants** (approved by the owner for now, to be re-measured once real proofs exist in M7):
  `MAX_INPUTS = 32`, `MAX_OUTPUTS = 16`, `MAX_EXTRA = 128`, `MAX_PROOF = 32 KiB`, `MAX_RING = 16`,
  `MAX_BLOCK_TXS = 8192`, `MAX_COINBASE_OUTPUTS = 16`. They bound memory and verification cost and are
  consensus once set (a later change is a new rules version, section 10).
- **Payment ids** and the transaction's ephemeral data live in `extra`, encrypted per the Carrot rules.
- The **coinbase transaction** is different: `version`, `height u64`, a list of outputs with plaintext
  amounts, and `extra`. It has no prunable part. **DECIDED maturity**: a coinbase output cannot be spent, or
  used as a ring member, until **60** blocks have passed (`COINBASE_MATURITY = 60`); any other output needs
  **10** (`SPEND_MATURITY = 10`). Precisely: an output created in the block at height `h` may be used in a
  block at height `h + 60` (coinbase) or `h + 10` (ordinary) or later.

### 6.3 Membership: the stand-in now, FCMP++ later (decision 3)

An input proves "the spent output is one of many, without saying which", and reveals its key image so that
spending it twice is detected.

- **P1 (the stand-in)**: a **ring** per input, in the prunable part (6.2): exactly `RING_SIZE = 16` distinct
  **global output indexes** in ascending order (the serialization only bounds a ring to `MAX_RING = 16` and
  requires one ring per input; the validator requires exactly 16, distinct and ascending), plus a **CLSAG**
  signature inside `proof_data`. Every ring member must exist and be mature. **DECIDED: 16, fixed** (Monero's
  current size, and every transaction looks alike).
- **P2 (FCMP++, when a stable and audited implementation exists)**: the membership variant becomes a
  reference to a curve-tree root (a block height) plus a proof. **The output format above does not change**:
  the leaf for an output is `(Ko, its key-image generator, Ca)`, all derivable from the fields it already has.
- A `version` change with an **activation height** (section 10) switches from P1 to P2. Old transactions stay
  valid under their version.
- **Decoy selection** (which 15 other outputs a wallet puts in a ring) is wallet policy, not consensus.

### 6.4 Identity, replay and malleability [issue 1]

- **The transaction id** is `SHA-256( "tenero tx v2" ‖ serialized prefix ‖ prunable_hash )`, where
  `prunable_hash = SHA-256( "tenero tx prunable v2" ‖ serialized prunable part )` (the rings, then the `u32`
  proof length and the proof bytes). So it commits to every byte, rings and proofs included, and is
  **identical for the full and the pruned form** of a transaction. The **coinbase id** is `SHA-256( "tenero coinbase v2" ‖
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
   maximums; **`fee >= min_fee`** (8.1); **no input's key image is already spent** (in the chain or earlier in
   this block); every ring member exists and is mature; the proofs of section 7 verify.
7. Then the block's outputs are added to the output set and its key images to the spent set. Global output
   indexes are assigned in order: the coinbase outputs first, then each transaction's outputs in transaction
   order and output order, continuing from the previous block's last index (section 14.6).

The checks above are implemented in `crates/tenero-chain` (`Validator`), each with a test that breaks exactly
that rule, **except the cryptographic proofs of section 7, which are behind a hook and not checked yet**
(M7); a block accepted meanwhile is reported with `proofs_checked = false`. The work of a block,
`floor(2^256 / target)`, is `v2_work.json`; a target of 1 has work 2^256 and is refused.

**Fork choice** (M6): the valid chain with the largest **cumulative work**, `sum(2^256 // target)` over its
blocks. A reorganisation removes the outputs and key images of the blocks it undoes.

### 8.1 The minimum fee: a dynamic consensus rule  (decision 4, **DECIDED**)

Every non-coinbase transaction must pay at least

```
min_fee(size, base_reward, median) = max( 1,  ceil( base_reward * FEE_REFERENCE_WEIGHT * size / median^2 ) )
```

in units, with `FEE_REFERENCE_WEIGHT = 3000` bytes (**PROVISIONAL**; Monero's reference weight), where `size`
is the transaction's full serialized size in bytes, `base_reward = reward(height)` of the block it is in
(before any penalty) and `median` is the block-size median that block is judged against (section 6 of v1).
All three are known before the block, so every node computes the same number, and a block containing a
cheaper transaction is invalid. The computation is exact integer arithmetic (a 128-bit intermediate; a result
that does not fit in a `u64` is an error). `v2_fees.json` pins it.

What it does, with the default numbers (reward 20 coins, median at its 150 kB floor, section 8.2):

- A 2,500-byte transaction needs 666,667 units (**0.00666667 coins**).
- A block filled up to the median pays **at least `reward * 3000 / median` = 2% of the reward** in fees.
- The fee **scales with the reward**, so it falls as the emission halves (in the tail, 0.5 coins, it is 40
  times lower), and with **1 / median^2**: when blocks have been big for a while, fees fall (room is cheap),
  and when they are small they rise. This is the same shape as Monero's dynamic fee; it is the design of this
  document, not Monero's exact constants.

Consequences to know about:

- **It is a consensus rule and can therefore invalidate a transaction between broadcast and inclusion**,
  because the median can change. Wallets should add a margin (policy) and nodes should accept, relay and
  include a transaction only when it meets the rule for the *next* block. Retuning `FEE_REFERENCE_WEIGHT` is
  a new rules version (section 10), never an edit.
- **A miner can influence the median** by stuffing recent blocks, which lowers the fee for the next ones. The
  oversize penalty (quadratic beyond the median) is what makes that expensive, and the cost is not zero.
- **Nothing here stops a miner filling their own blocks**: they can pay the fee to themselves. What limits
  that is the 150 kB size floor (blocks up to it carry no penalty, section 8.2), so the worst case is about
  216 MB of chain growth a day. That is what pruning (section 14) is for.

### 8.2 The block-size floor: 150,000 bytes  (**DECIDED**)

The block-size median (v1 section 6: the upper median of the last 10 block sizes) never goes below
**`MIN_BLOCK_MEDIAN = 150,000` bytes** in version 2 (v1 used 300,000). A block is penalty-free up to the median,
loses reward quadratically above it, and is invalid above twice the median (v1's rules, unchanged). The floor is
therefore the size a block may be **for free**, so it bounds how fast the chain can grow without anyone paying
for it:

- **`150,000 bytes * 525,600 blocks a year = about 79 GB a year`, at the very most.** With v1's 300 kB it was
  158 GB. (Monero's 2-minute blocks with a 300 kB floor have the same ceiling of about 79 GB, at half the blocks.)
- **What it costs**: at 150 kB a block holds about 60 ordinary transactions, roughly one a second, before any
  penalty. It is not a cap: sustained demand raises the median, but each block above the median pays a
  quadratic penalty out of the miner's reward, which is the price of growth.
- **The fee formula scales with it**: `fee` is proportional to `1 / median^2`, so the floor makes fees 4 times
  higher than they would be at 300 kB (8.1).
- It is a consensus constant of rules version 2; changing it later is a new version (section 10). `v2_fees.json`
  pins the floor and the median rule.

## 9. Addresses and keys  (decision 5, **DECIDED: Carrot**)

**Follow Monero's Carrot specification**, read at implementation time, not invent one. Carrot is
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
| `v2_serialization.json` | the primitive encodings; 18 valid objects with their exact bytes (an output, an input, transactions at their limits and with empty rings, a coinbase, a header, a block, and the **pruned** forms of a transaction and a block); 30 invalid encodings covering all four failure kinds (short read, trailing bytes, a count out of range, a length over its maximum), including counts of `2^32 - 1` that must be refused without allocating, and a wrong number of rings |
| `v2_ids.json` | the header hash, the proof-of-work seed, the block id (matmul and SHA-256 test chain), transaction ids (with the prunable hash, and the **same id from the pruned form**), coinbase ids, and domain separation |
| `v2_fees.json` | the dynamic minimum fee (76 cases, including three that overflow a `u64` and must be refused) and the oversize penalty in the 8-decimal units |
| `v2_emission.json` | the emission in the 8-decimal units: the default schedule up to 2^40, and a small schedule that trims its final reward |
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

## 13. Decisions

Decided by the owner on 2026-09-29:

| # | decision | outcome |
|---|---|---|
| 1 | units (section 3) | **8 decimals** |
| 2 | address scheme (section 9) | **Carrot** |
| 3 | ring size and maturity (6.2, 6.3) | **ring 16, fixed; 60 blocks for mined outputs, 10 for ordinary outputs** |
| 4 | minimum fee (8.1) | **dynamic consensus rule** |
| 5 | storage | **`redb`** (pure Rust, MIT OR Apache-2.0, 4.3.0 at the time of writing), laid out for pruning (14.3) |
| 6 | genesis label (5.4) and limits (6.2) | label **`"tenero experimental network 1"`**; the limits are **PROVISIONAL** |
| 7 | CLSAG and Bulletproofs+ (section 2) | **the `monero-oxide` crates**, after the audit is read and a version is pinned (M7) |
| 8 | pruning (section 14) | **required**: the chain must be able to run pruned so nobody has to download hundreds of gigabytes |
| 9 | block-size floor (8.2) | **150,000 bytes** (v1: 300,000), to bound the free growth of the chain |

Still open, none of which blocks M6:

- `FEE_REFERENCE_WEIGHT` and the limits of 6.2 are to be re-measured when real proofs exist (M7).
- The Carrot field sizes in 6.1 are to be checked against the specification's own tables.
- The `monero-oxide` audit report (Cypher Stack, May 2025) is to be read: its scope, its findings and whether
  0.1.0 includes later changes.
- The text encoding of addresses and the network prefix (the wallet milestone).
- The pruning policy constants of 14.6 (how many blocks to keep whole, the assume-valid checkpoint).
- Whether to add a state commitment to the header in a later rules version (14.5).

## 14. Pruning: running the chain without hundreds of gigabytes  (decision 8)

**The requirement**: a node must be able to run *pruned*, keeping a small fraction of the history, and a new
node must not have to store or (with a snapshot) download all of it. This is designed in now because the
parts that make it possible are data-model decisions that are very costly to add after launch.

### 14.1 What can be dropped, in three layers

1. **Prunable data (the proofs).** Most of a transaction's bytes are its proofs, needed only to verify it
   once. The prefix/prunable split (6.2) and the id that commits to a hash of the prunable part (6.4) mean a
   node can delete the proofs of old transactions and still recompute every transaction id, every Merkle
   root and every block id. It keeps the prefix (the outputs and key images) and the 32-byte `prunable_hash`.
   The `pruned_transaction` and `pruned_block` forms in `v2_serialization.json` are the wire format for this.
2. **Assume-valid sync.** A new node need not verify the proofs, or the full proof of work, of every old block.
   Below a **checkpoint shipped in the software** (a block id and its cumulative work; **not consensus**), it
   checks the chain of ids, the Merkle roots, the cheap proof-of-work check (microseconds), the emission
   rules and key-image uniqueness, and builds the whole state itself from the prefixes. It **trusts** that the
   proofs below the checkpoint were valid. (Full proof-of-work checks cost about one attempt per block plus a
   dataset per epoch: measured 0.04 to 0.18 s per attempt on this machine, so roughly 6 to 26 hours of CPU per
   year of blocks, plus about 4 hours to build the 5,256 datasets of a year at about 3 s each. That is why the
   option matters.)
3. **State snapshot sync.** Later, a new node could download the *state* at a checkpoint (the output set and
   the spent key images) instead of replaying history, and check it against a hash shipped in the software.
   Everything needed is already canonical: the serialization is unique, and global output indexes are assigned
   deterministically (14.6). It adds a trust assumption (the checkpoint), stated openly. Removing it needs a
   **state commitment in the block header**, which is heavy (an accumulator over the key-image set) and is
   deferred; the header's `version` field allows adding it in a later rules version without breaking anything.

### 14.2 Kinds of node

| kind | keeps | can serve |
|---|---|---|
| **archive** | everything | any block, in full |
| **pruned** (the target for most users) | every header; the most recent `PRUNE_KEEP_BLOCKS` blocks in full; older blocks as prefixes plus prunable hashes; the whole state | recent blocks in full; older blocks in pruned form |
| **snapshot** (later) | every header; a state snapshot; recent blocks in full | recent blocks in full |

- Pruning is **never consensus**: no block is valid or invalid depending on who has pruned what.
- A **reorganisation deeper than a pruned node's whole window** needs proofs it no longer has and must fetch
  them from an archive peer; a pruned-only network cannot reorganise that deep. **The network needs archive
  nodes**, and a small chain with few of them is weaker.

### 14.3 Storage layout (`redb` and segment files)

The index and the state are in `redb` tables; the prunable data is in flat segment files, and only its location
is in the database, so pruning never rewrites anything large:

| table | key -> value |
|---|---|
| `headers` | height -> header bytes (146 bytes) and cumulative work |
| `block_ids` | block id -> height |
| `coinbase` | height -> coinbase bytes |
| `block_txs` | height -> the transaction ids in order |
| `tx_prefix` | transaction id -> prefix bytes, `prunable_hash`, height and position |
| `tx_loc` | transaction id -> where its prunable part (the rings and the proof bytes) is: segment, offset, length (16 bytes): **the only table pruning touches**, with a `pruned_below` height in `meta` |
| `seg_len` | segment id -> the committed length of that segment file |
| `outputs` | global index -> one-time address, commitment, height, kind (coinbase or not) |
| `key_images` | key image -> height (the spent set) |
| `meta` | chain id, tip, cumulative work, `pruned_below`, the assume-valid checkpoint |

The prunable data itself is **not in the database**: it is in flat **segment files**, `<database>.segments/seg-<id>.dat`,
one per `segment_blocks` heights (`id = height / segment_blocks`, recorded when the database is created;
default 1,000 blocks, about 17 hours). The rings and proofs are large, written once, never changed and deleted
in whole ranges of blocks, which is what a flat file is good at and a B-tree is not (see the measurements
below). `redb` keeps the index, the state and the 16-byte location of each transaction's bytes.

This layout is implemented in `crates/tenero-store` (`Store::append_block`, `pop_block` for reorganisations,
`prune_below`, `prune_below_in_steps`, `prune_keeping`, `compact`, and a `state_digest` that pruning must never
change). It is tested against an in-memory model through a random history of appends, reorganisations (also
through pruned blocks) and prunings, and every stored block's Merkle root is recomputed from the pruned data.

How the two parts stay consistent, and what pruning does:

- **Writing.** A block's prunable bytes are written to its segment **and synced to disk before** the database
  transaction that refers to them commits, and **at the segment's last committed length**. After a crash the
  file may hold bytes nothing refers to; the next write simply overwrites them and cuts the file to the right
  length. A segment file the database has no committed length for (left by a crash, or by a block that
  never committed) is an orphan, and `open` deletes it. Every check that can refuse a block runs before any
  byte is written, so a refused block writes nothing.
- **Reading.** A transaction's location gives segment, offset and length. A missing or short file is reported
  as corruption, never a panic, and a failed operation changes nothing.
- **Rolling a block back** returns its bytes (they are the tail of its segment) and gives the space back: the
  segment's committed length moves back, and an emptied segment's file is deleted.
- **Pruning** below a height deletes the 16-byte locations of those blocks **exactly**, in one transaction, so
  `pruned_below` keeps its exact meaning. A segment file is deleted as soon as **every** block in it is below
  `pruned_below`; a segment that still holds some unpruned blocks keeps its dead bytes until the rest are
  pruned, so at most one segment (`segment_blocks` blocks of prunable data) is wasted.

**Measured** (`crates/tenero-store/tests/store.rs`, `pruning_gives_back_the_disk_and_never_grows_the_database`;
`redb` 4.3.0 on Windows; 150 blocks of 40 transactions, 8-block segments, fields of the real sizes with
16-member rings, and **random stand-in proofs of 1,900 bytes**, not real ones; a transaction is 356 bytes of
prefix and 2,172 bytes of prunable data):

| | database | segment files | total |
|---|---|---|---|
| the full chain | 6.5 MB (compacted) | 13.03 MB, **exactly the prunable bytes** | **21.5 MB** (about 3.6 kB per transaction) |
| after pruning 149 blocks, and compacting | 6.05 MB | 0.61 MB (18 files deleted, 12.4 MB) | **6.66 MB, 31% of the full chain** (about 1.1 kB per transaction) |

The design **before** segment files kept the same data in the database and measured 30.9 MB for the full chain
(about 5.2 kB per transaction) and 6.2 MB after pruning and compacting. So:

- **An archive node is about 31% smaller** (21.5 MB against 30.9 MB), because the segment files hold exactly the
  prunable bytes and the database no longer wastes space packing 2 kB rows into its 4 KiB pages (the likely
  cause of the earlier growth; not separately verified).
- **A pruned node is about the same size** (6.66 MB against 6.2 MB): it is dominated by the prefixes, the state and
  the database's own overhead, which did not change. The gain there is not size but **how pruning works**: it
  deletes rows and whole files, with nothing to rewrite and no dependence on the database's file growth.
- **Correction of an earlier explanation.** I had said that one big prune transaction doubled the database file
  "because a copy-on-write database needs room for the pages it is replacing". That was wrong. The test now
  shows that **any** write after a compaction, even one that rewrites a single small row, grows the file by
  exactly 2.00x (6.52 MB to 13.05 MB): it is `redb`'s file-growth policy after compaction leaves no free pages,
  not a cost of pruning, and the next compaction returns it. The database file is therefore not a good measure
  of what a node uses between compactions; the honest figure is the compacted one.
- **Extrapolated to the worst-case 31 million transactions a year** (a small test with fake proofs, so an
  indication, not a measurement): **about 110 GB a year for an archive node and about 34 GB for a pruned one**,
  against raw-byte estimates of 79 and 19 GB (14.4). The difference is the database's overhead and the
  state repeating some of what the prefix rows hold, which is the next thing to look at if the size matters.

### 14.4 How big, roughly (my arithmetic, **not measured**; M6 measures)

For a typical 2-input, 2-output transaction: a prefix of **356 bytes** (two 32-byte key images, two outputs of
123, the fee, a little `extra`) and a prunable part of about **2.2 kB** (two rings of 16, and about 1.9 kB of two
CLSAG signatures, two pseudo-output commitments and one aggregated range proof, from the published sizes of
those schemes). That is about 2.55 kB a transaction, **about 86% of it prunable**. At the worst case (every
block full to the 150 kB floor, all such transactions, about 31 million a year), in raw bytes:

| | per year, worst case |
|---|---|
| everything (archive) | about 79 GB |
| prefixes only | about 11 GB |
| state (outputs and key images, about 230 bytes a transaction) | about 7 GB |
| headers (146 bytes a block) | about 77 MB |
| the last 5,500 blocks in full | about 0.8 GB |

So a pruned node (prefixes, state, the recent blocks and the headers: about 19 GB) is about **a quarter** the
size of an archive node, and a snapshot node (state, recent blocks and headers: about 8 GB) about **a tenth**.
A young chain will be nowhere near the worst case; these are ceilings, and the ratios rest on my size
estimates above. (On disk the numbers are higher: see the measurements in 14.3.)

**Moving the ring indexes out of the prefix was done** (6.2): each input had carried 16 ring indexes of 8 bytes,
about 256 of a 620-byte prefix, which nothing needs after verification. The measured effect on a pruned
chain was 23% smaller (14.3).

### 14.5 What this does not solve

- **Initial sync still downloads every prefix** (about a quarter of history) without a snapshot. A snapshot
  brings that down to roughly the state's size, in exchange for trusting a checkpoint.
- **The state only grows.** Every output ever created stays (rings, and later the FCMP++ curve tree, need
  them), and so does every key image. Pruning removes proofs, not the outputs.
- **A pruned or assume-valid node verifies history less than a full node does.** That is a real weakening,
  and the checkpoint is a trust assumption.
- **Old data becomes hard to find** if few archive nodes exist.

### 14.6 What this draft already fixes so that pruning stays possible

- The **prefix/prunable split**, the id over the prefix and the prunable hash, and the pruned wire forms.
- A **coinbase has no prunable part** (nothing to drop, and it is small).
- **Global output indexes are deterministic**: in a block, the coinbase outputs first, in order, then each
  transaction's outputs in transaction order and output order, continuing from the last index of the previous
  block. A reorganisation removes the undone blocks' indexes and reuses them.
- **Fees, sizes and the block-size median use the full serialized size**, pruned or not, so a node's
  pruning never changes a consensus number.
- Software constants that are **policy, not consensus**: `PRUNE_KEEP_BLOCKS` (proposed default 5,500, about
  3.8 days) and the assume-valid checkpoint.
