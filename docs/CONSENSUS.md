# Consensus rules

This is the specification of what makes a block and a chain **valid** in the current reference
implementation (the Python code in `tenero/`). It is written so that another implementation can be
built from it and checked against `tests/vectors/`.

How to read it:

- **Normative** means "another implementation must do exactly this". If this document and the
  Python code disagree, that is a bug in one of them; the vectors decide which.
- **LEGACY** marks parts of the current *account model* (addresses with balances, ECDSA signatures,
  JSON serialization). A rewrite to an output-based privacy model replaces these. They are described
  because the reference still does them, not because they should be copied.
- **Policy** marks things that are not consensus (the miner and mempool choose freely).
- A flaw known in the current rules is flagged with **[issue N]**: see `KNOWN_ISSUES.md`.

Every number below is checked by a vector file, named in the last section.

---

## 1. Numbers

- Amounts are **whole units**. `DECIMALS = 4`, `UNIT = 10_000`: one coin is 10,000 units and the
  smallest amount is 0.0001 coins. All consensus arithmetic is exact integer arithmetic.
- Text to units (`to_units`): parse as a decimal number, multiply by 10,000, and reject anything that
  is not a whole number of units (`0.00001` is an error). The reference also accepts a leading/trailing
  space, a sign, and exponent notation (`1e2`); a rewrite may reject those. Units to text (`fmt`):
  `"{units // 10000}.{units % 10000:04d}"`, with a leading `-` for negative values.
- The Python code uses unbounded integers. **A rewrite must choose widths.** Every amount that can
  occur fits in a signed 64-bit integer (the main emission is at most 2 * 10^11 units), but two
  computations need wider intermediates: the oversize penalty (`base * over * over`) and the
  difficulty adjustment (256-bit targets multiplied by solve times).
- **Widths chosen by the Rust implementation** (`crates/tenero-core`; the vectors pass with them, and an
  overflow is an explicit error, never a wrap): amounts and rewards are `i64`/`u64` units; block sizes
  are `u64` bytes; timestamps are `i64` seconds; the oversize penalty's `base * over^2` is computed in
  `u128`; the difficulty adjustment averages targets and multiplies by the weighted solve times in a
  320-bit integer, and refuses a `block_time`/`window` whose weighted times or divisor exceed 64 bits.
  Text amounts are parsed strictly (digits, an optional `-`, at most 4 decimals): no spaces, `+`,
  exponents, `1.` or `.5`.
- A **target** is a 256-bit unsigned integer. A hash is **valid** when it is *strictly below* the
  target, comparing the hash as a big-endian integer.

## 2. Chain parameters

Saved in `chain.json` (`params`, plus `target` at the top level) when a chain is created; an existing
chain keeps them. Defaults are in `reference/tenero/config.py`.

| parameter | default | meaning |
|---|---|---|
| `initial_reward` | 20 coins (200,000 units) | reward of block 1 |
| `halving_interval` | 525,600 blocks | one year of 60-second blocks |
| `max_supply` | 20,000,000 coins | cap on the MAIN emission |
| `tail_reward` | 0.5 coins (5,000 units) | reward once the main emission no longer pays more |
| `block_time` | 60 s | the difficulty adjustment aims at this |
| `difficulty_window` | 30 blocks | 0 turns the adjustment off (fixed difficulty) |
| `target` | SHA-256 chains `2^256 // 32,719,438`; matmul chains `2^256 // 1,400,000` | the STARTING target |
| `pow` | matmul v2, or SHA-256 | see section 8; matmul carries its own parameters |
| `decimals` | 4 | |

Consensus constants that are **not** saved in the chain file (they are constants of the software):

| constant | value |
|---|---|
| minimum fee rate | 0.01 coins per 1000 bytes = 100 units per 1000 bytes |
| maximum memo | 100 bytes (UTF-8) |
| `MIN_BLOCK_MEDIAN` | 300,000 bytes |
| `MEDIAN_WINDOW` | 10 blocks |
| `MAX_TARGET_STEP` | 4 |
| `FUTURE_TIME_LIMIT` | 120 seconds |

## 3. Blocks  (LEGACY serialization)

A block has: `index`, `timestamp`, `transactions` (the first is the reward), `previous_hash`,
`nonce`, `hash`, and for matmul chains `mix` (64 bytes as 128 hex characters).

- **Genesis** is `Block(index 0, no transactions, previous_hash = 64 zeros, timestamp 0)`. Its hash
  is `310d91e47584e2c931967e372ab65e1a3e414e50b0792c86dd1989fc0ecbf207`. The reference does **not**
  check the genesis block **[issue 3]**; a rewrite must define it and check it.
- **Header bytes** are the UTF-8 encoding of Python's
  `json.dumps({"index", "timestamp", "transactions": [each transaction as a dict], "previous_hash"}, sort_keys=True)`
  with the default separators (`", "` and `": "`) and default ASCII escaping. **The block hash depends on
  Python's JSON formatting, including how a float timestamp prints (`1700000000.0` differs from
  `1700000000`) [issue 5].** `tests/vectors/legacy_account_model.json` shows the exact bytes. A rewrite
  should define its own canonical binary serialization instead of imitating this.
- **SHA-256 chains:** `hash = sha256(header_bytes + decimal(nonce))` as lower-case hex.
- **Matmul chains:** `hash` is the matmulhash digest (section 8) as lower-case hex, and the block must
  carry the `mix` it was computed from.
- Timestamps are seconds. The reference treats them as `int(timestamp)` in every rule.

## 4. Transactions  (LEGACY: an account model)

Fields: `sender` (an address, or the text `COINBASE`), `recipient`, `amount`, `fee` (units, paid to the
miner on top of the amount), `memo` (text), `public_key` (hex), `signature` (hex).

- **Address** = the first 40 hex characters of `sha256(public_key bytes)`, where the public key is the
  raw 64-byte `x || y` of a secp256k1 point.
- **Payload** (what is signed) = `json.dumps({sender, recipient, amount, fee, memo, public_key}, sort_keys=True)`
  as UTF-8 (default separators).
- **Signature** = ECDSA on secp256k1 over **SHA-1** of the payload, as the raw 64 bytes `r || s`
  **[issue 2]**. It is randomized, and malleable: `(r, n - s)` is a second valid signature **[issue 1]**.
- **Size** (used for fees and block size) = the byte length of
  `json.dumps(tx_as_dict, sort_keys=True, separators=(",", ":"))`, measured with 128 zeros standing in
  for an empty public key or signature. A plain payment is 427 to 428 bytes.
- **The reward transaction** has sender `COINBASE`, no signature, and pays the miner.

A non-reward transaction is **acceptable on its own** when, in this order:

1. its sender is not `COINBASE`;
2. `amount` is an integer greater than 0;
3. `fee` is an integer;
4. `memo` is text of at most 100 bytes (UTF-8);
5. `fee >= min_fee(size)`, where `min_fee(size) = max(1, ceil(100 * size / 1000))` units;
6. it has a public key and signature, the public key hashes to the sender address, and the signature
   verifies.

Nothing binds a signature to a chain **[issue 4]** or limits an amount other than the sender's balance.

## 5. Emission and rewards

Let `H = halving_interval`. For block height `h >= 1`:

```
scheduled(h)     = initial_reward >> ((h - 1) // H)                 (an integer right shift)
issued_before(h) = min(max_supply, sum of scheduled(x) for 1 <= x < h)      (stops when scheduled = 0)
main_reward(h)   = max(0, min(scheduled(h), max_supply - issued_before(h)))
reward(h)        = max(main_reward(h), tail_reward)                 the BASE reward of block h
in_tail(h)       = tail_reward > 0 and main_reward(h) < tail_reward
```

So the last main-emission block is trimmed to hit `max_supply` exactly, and the tail takes over as soon
as the main reward would pay LESS than the tail, **even if the cap has not been reached**. Consequences
(all checked in `emission.json`):

- The default schedule pays 20, 10, 5, 2.5 and then 1.25 coins; the cap is reached exactly at block
  2,334,400 (the last main block pays a full 1.25), and the tail (0.5 coins) starts at block 2,334,401.
  The supply is exactly 20,000,000 coins when the tail starts.
- A schedule that halves below the tail too early hands over to the tail before reaching the cap
  (20 coins halving every 500,000 blocks: block 3,000,001 is the first paid the tail, with 19,687,500
  coins issued). `main_reward` keeps shrinking after that but is never paid, because
  `reward = max(main_reward, tail_reward)`, and `issued_before` counts the schedule, not what was paid.
- A halving interval that is too long never halves before the cap (20 coins every 1,051,200 blocks pays
  20 coins until the cap).
- If the trimmed final reward is below the tail, the tail pays more, and the supply ends slightly above the cap.

## 6. Block size, penalty and fees

- **Block size** = the sum of the sizes of all transactions except the first (the reward).
- **Median** for the block at position `pos`: take the sizes of blocks `max(1, pos - 10) .. pos - 1`
  (the genesis block is excluded), sort them, take the element at index `len // 2` (the upper median),
  and use `max(300000, that)`. With no blocks yet the median is 300,000.
- **Hard limit**: a block whose size exceeds `2 * median` is invalid.
- **Penalty** for `size > median` (otherwise 0): `ceil(base * over^2 / median^2)` with
  `over = size - median` and `base = reward(index)`. It is exactly `base` at `size = 2 * median`.
- **The reward transaction must pay exactly** `reward(index) - penalty + sum of the fees in the block`.
  One unit more or less makes the block invalid.
- **Fees**: only the minimum in section 4 is consensus. Which transactions a miner includes, and in what
  order, is policy. (The reference miner sorts by fee per byte and only goes over the median when the fee
  exceeds the extra penalty.)

## 7. Difficulty and timestamps

If `difficulty_window == 0` every block must meet the starting `target`, and timestamps are not checked.

Otherwise, with `T = block_time`, `W = difficulty_window`, position `pos` (block number), and lists
`ts[i]` and `targets[i]` (the timestamp and the required target of block `i`; `ts[0] = 0` and
`targets[0] = starting target` for the genesis block):

```
n = min(W, pos - 2)
if n < 1: required(pos) = starting target
weighted = 0
for k in 1 .. n:                                    # k = 1 is the oldest block in the window
    i = pos - n - 1 + k
    solve = max(1, min(6 * T, ts[i] - ts[i-1]))
    weighted += solve * k
avg_target = sum(targets[pos-n .. pos-1]) // n      # integer division
new = avg_target * weighted // (T * n * (n + 1) // 2)
prev = targets[pos-1]
new = max(prev // 4, min(prev * 4, new))
required(pos) = max(1, min(2^256 - 1, new))
```

Notes: block 1 follows the genesis block, which has no real timestamp, so blocks 1 and 2 use the starting
target and the first measured solve time is block 2's. A backwards timestamp counts as a 1-second solve.

**Timestamp rules** (only when `W > 0`): block `pos` is invalid if `int(timestamp)` is

- **not later than its parent's**: less than `ts[pos - 1] + 1`, where the genesis block counts as time 0 (so block 1 may carry any timestamp from 1
  on). **Changed at M11.2 (2026-10-04, the owner's decision of 2026-10-03):** this replaced "less than the median of the last 11", under which a miner
  with 30 % of the hash rate could backdate its blocks to the median and pull the difficulty to 0.40x the honest value (`THREAT_MODEL.md` E3); under this
  rule the same miner leaves it at 1.00x. It is a consensus change, so it starts a new chain: there is no activation height (`EMERGENCY_PLAN.md` section 8); or
- greater than the validator's clock plus 120 seconds. **This makes validity depend on the wall clock,
  so two honest nodes can disagree about a block near the boundary [issue 6].**

## 8. Proof of work

### 8.1 SHA-256 (test chains)
`hash = sha256(header_bytes + decimal(nonce))` (section 3); valid if below the target.

### 8.2 matmulhash v2 (the default)

An int8 matrix multiplication against a large dataset built from ChaCha20. Words are **little-endian
uint32**; "ChaCha20 core" means the 20-round permutation followed by adding the original state back
(RFC 8439), applied to any 16-word state. Checked in `chacha20.json`, `matmulhash_*.json`, `pow_misc.json`.

**Parameters** (default `m=64, k=8192, nb=2048, num_blocks=256`, saved in the chain). A slice is
`k * nb` bytes (16 MiB), the dataset is `num_blocks` slices (4 GiB), a dataset **block** is 64 bytes
(16 words), `blocks_per_slice = k * nb / 64`. Required: `k` and `nb` multiples of 8, `m*k` a multiple of
64, `m*nb` a multiple of 16, `k * 128 * 128 < 2^31`, counters below 2^32.

**Epochs.** Block `index` belongs to epoch `(index - 1) // epoch_blocks` (default `epoch_blocks = 100`).
`epoch_seed(0) = sha256("tenero matmulhash epoch 0")` and `epoch_seed(e+1) = sha256(epoch_seed(e))`.
`dataset_key = sha256("tenero matmulhash v2 dataset" + epoch_seed)`.

**The dataset** (for one epoch):

- Slice 0, block `u` = the ChaCha20 block with key `dataset_key` (8 words), counter `u`, nonce `(0, 0, 0)`.
- Slice `j >= 1`, block `u`: let `prev` be block `u` of slice `j - 1`. For each of `i = 0, 1, 2`, pick
  block `prev[2i+1] mod blocks_per_slice` of slice `prev[2i] mod j`, and XOR the three picked blocks
  into `ref`. Then `state = [c0 c1 c2 c3] ++ (prev[0..7] XOR ref[0..7]) ++ [u, j, prev[8] XOR ref[8], prev[9] XOR ref[9]]`
  (c0..c3 = `expand 32-byte k`), `out = ChaCha20core(state)`, and **the new block = out XOR prev XOR ref**.
  Slices must be built in order: each depends on all earlier ones (the picks are data-dependent).
- **Memory layout**: a slice's raw bytes are the transposed matrix. The `k x nb` matrix is
  `W[t][n] = raw[n * k + t]` (the bytes viewed as `nb` rows of `k`).

**One attempt** for `header_hash` and `nonce`:

1. `header_hash = sha256(header_bytes)` where the header is the block's (section 3), **without** nonce, hash and mix.
2. `seed = sha256(header_hash + nonce as 8 little-endian bytes)`; `0 <= nonce < 2^64`.
3. `X` = the ChaCha20 keystream with key `seed`, counter starting at 0, nonce `(0,0,0)`, `m*k/64` blocks,
   read as **signed int8**, row-major `m x k`.
4. `b = little-endian uint64 of sha256(seed + 0x01)[0:8], mod num_blocks`: the slice this attempt uses.
5. `C = X @ W_b` with **exact** integer arithmetic (int8 x int8 accumulated in int32; the values fit),
   an `m x nb` array of int32.
6. **Fold.** View `C` row-major as consecutive chunks of 16 words (the int32 values reinterpreted as uint32).
   For chunk number `c` (from 0): XOR `c` into word 0, apply the ChaCha20 core, and add each output word into
   eight uint64 sums: `sums[i] += out[i] + out[i + 8]` for `i = 0..7`.
7. `mix` = the 8 sums as little-endian uint64 (64 bytes). `digest = sha256(seed + mix)`.
8. The block `hash` is `digest` as lower-case hex, and the attempt succeeds when the digest, as a big-endian
   integer, is **strictly below** the required target.

**Verification.** *Full*: recompute steps 1 to 8 from the dataset; needs the epoch's dataset in memory
(4 GiB; about 4.3 GiB of RAM), after which one check takes about 0.1 s. *Cheap pre-check* (no dataset,
microseconds): the block carries a `mix` of exactly 64 bytes, `nonce` is an integer in `[0, 2^64)`,
`sha256(seed + mix)` equals the block's hash (compared as lower-case hex), and the hash is below the target.
A block that fails the cheap check is invalid. A block that passes it is **not** known to be valid: a forger
can grind a made-up `mix` to the target for the cost of about `difficulty` SHA-256 hashes, and only the full
check rejects it. Validation therefore runs the cheap check first and the full one afterwards.

Two details that the vectors pin down and that a second implementation must copy (clarifications found
while writing the Rust version; neither changes a rule or a vector):

- The reference also calls the cheap check with a target of exactly **2^256**, meaning "no limit": it then
  checks only that the block is self-consistent (`sha256(seed + mix)` equals the hash), not that it meets a
  target. 2^256 needs 257 bits, so it is not a valid *chain* target (section 7 clamps targets to at most
  `2^256 - 1`); an implementation needs a separate "no target" case. `pow_misc.json` uses it.
- The ChaCha20 block counter of the keystream **wraps modulo 2^32** (`chacha20.json` has a case that starts
  three blocks below 2^32). The parameter rules keep every counter the proof of work uses below 2^32, so
  consensus never depends on the wrap.

### 8.3 The gather fork (beta and dev, from height 500)

**Decided by the owner on 2026-10-07** (beta was then at height 280). A network has a **gather fork height** `G`:
**500 on `beta` and `dev`**, none on `alpha` (it keeps 8.2 for ever), and none on the SHA-256 `test` network. A block at
height `>= G` must carry the mix of the **gathered attempt** below; a block below `G` must carry the mix of 8.2. Each side
refuses the other's mix (`tenero-chain/tests/validate.rs`, `across_the_gather_fork_each_side_accepts_only_its_own_design`).
The height is the block's own (its parent's height + 1), the dataset is the epoch's dataset of 8.2, unchanged, and the
cheap pre-check is unchanged (it never depended on the slice). **A node without this rule refuses every beta block from
height 500 on and is left on a chain of its own.** There is no activation mechanism: the height is fixed in the program
(`GATHER_FORK_HEIGHT`).

**Why.** In 8.2 the slice an attempt reads (step 4) is two SHA-256 hashes of the nonce, known before any matrix work, and a
miner chooses its nonces. It can try them 16 to a slice and multiply all 16 X matrices by ONE read of the slice: the
proof of work is then limited by int8 multiply speed and not by memory (measured on an RTX 5070 Ti: about 35,000
attempts a second without that, about 127,000 with it), which is where data-centre GPUs and special chips lead most
(`THREAT_MODEL.md` E11 and E12, `BENCHMARKS.md`). The same freedom lets a miner keep only the nonces whose slice is 0
and mine with that one 16 MiB slice, which depends on no other: the 4 GiB is not needed at all (reported privately by a
tester). The gathered attempt picks its columns one by one from the whole dataset,
so attempts share almost no reads, and the same card does about 45,000 a second, at its memory bandwidth.

**The gathered attempt** for `header_hash` and `nonce` (steps 1 to 3 and 6 to 8 are those of 8.2):

1 to 3. `seed`, `X` as in 8.2.

4. `pick_key = sha256(seed + 0x02)`. Take the ChaCha20 keystream of key `pick_key`, counter starting at 0, nonce
   `(0,0,0)`, `ceil(nb / 16)` blocks; for `n = 0 .. nb-1`, `idx[n]` = **little-endian uint32 word `n`** of it,
   **mod `num_blocks * nb`**. (At the real parameters: 2,048 numbers below 524,288.)
5. **Dataset column `j`** is the `k` bytes at offset `j * k` of the whole dataset (all slices one after the other), read
   as signed int8; so column `n` of slice `b` is column `b * nb + n`. `W` is the `k x nb` matrix whose column `n` is
   dataset column `idx[n]` (a column may be picked more than once), and `C = X @ W` with exact integer arithmetic, an
   `m x nb` array of int32.

6 to 8. The fold, `mix`, `digest` and the target test of 8.2, on this `C`.

**Verification** needs the **whole** epoch dataset (any column can be picked), which the node already holds; one check
costs about what an 8.2 check costs. Checked in `matmulhash_gather.json` (small sizes: every step, including the column
numbers, X and C) and `matmulhash_gather_real.json` (the real parameters, the epoch-0 dataset; slow).

## 9. Validating a chain

The reference `is_valid()` walks blocks `1..tip` in order, keeping `balances`, the set of signatures seen,
the list of block sizes, and the `timestamps` and `targets` so far. For each block, **in this order**:

1. `index == previous.index + 1`, and `previous_hash == previous.hash`;
2. compute `required` (section 7); if `W > 0`, the timestamp rules (section 7);
3. **cheap proof-of-work check** (SHA-256: hash matches the contents and is below `required`; matmul:
   section 8.2's cheap check with `required`);
4. **full proof-of-work check** (matmul only; skipped only when the caller asks for a structure-only check);
5. the block has at least one transaction; the first is from `COINBASE`; every other transaction has integer
   `amount` and `fee`;
6. block size and median (section 6): `body_size <= 2 * median`, and the reward transaction pays exactly
   `reward(index) - penalty + fees`; the reward is added to the miner's balance;
7. for each other transaction in order: acceptable on its own (section 4); its signature has not been seen
   before (then remembered); the sender's balance covers `amount + fee`; move the amount to the recipient and
   the fee out of the sender's balance (the fee is already counted in the reward).

Any failure makes the chain invalid. There is **no fork choice** (one chain only) and no networking yet.

## 10. Not consensus

Mempool contents and ordering, fee-tier estimates, which transactions a miner includes, the miner's
threading and CPU/GPU split, how the GPU computes attempts (any implementation is fine if the results are
bit-identical), CLI text, the `chain.json` file layout, and all storage.

## 11. Where each rule is tested

| section | vectors |
|---|---|
| 1 numbers | `units.json` |
| 3 to 4 formats (legacy) | `legacy_account_model.json` |
| 5 emission | `emission.json` |
| 6 size, penalty, fees | `fees_and_size.json`, the `size:` cases in `chains.json` |
| 7 difficulty and timestamps | `difficulty.json`, the timestamp cases in `chains.json` |
| 8 proof of work | `chacha20.json`, `matmulhash_small.json`, `matmulhash_real.json`, `matmulhash_deep.json`, `matmulhash_full.json`, `pow_misc.json`, the `matmul:` cases in `chains.json`; 8.3 (the gather fork): `matmulhash_gather.json`, `matmulhash_gather_real.json` |
| 9 validation | `chains.json` (every `rule:` case is a properly mined block that breaks exactly one rule) |
