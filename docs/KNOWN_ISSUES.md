# Known issues in the current design

Things wrong or missing in the current Python implementation, found while preparing for a rewrite.
A rewrite should **not** carry them over. Each says how it was established:

- **verified** = reproduced by a test in `reference/tests/test_known_issues.py` (a strict expected failure:
  the test asserts the correct behaviour and fails today; when the flaw is fixed it flips and its
  marker must be removed);
- **from the code** = read in the source, not reproduced by a test.

Most of 1 to 5 disappear by themselves in the output-based privacy model that the rewrite is meant
to introduce, but they are worth knowing because they show what to test for.

**Status at M11.1 (2026-10-04): the Python program was retired; nothing below was fixed by that.** Items 1 to 10 and 14 are flaws of the Python account-model chain, wallet and
JSON storage. That code is **still present, frozen, in `reference/tenero/`** (`chain.py`, `block.py`, `transaction.py`, `wallet.py`, `storage.py`), kept only because the
vector generators call it for the emission, difficulty and fee arithmetic; nothing shipped uses it. So the flaws are still there, their strict-xfail tests are still in
`reference/tests/test_known_issues.py` and still fail as expected, and nothing here is called "fixed". The Rust program was designed not to carry them (an output model with
key images, a canonical binary serialization, a checked genesis and chain id: `CONSENSUS_V2.md`), which is a design claim tested by its own tests, not by these. **Item 15 is
retired because it no longer describes anything shipped** (the Rust program has fork choice by cumulative work, networking, relay rules and reorganisation handling; it
described the Python prototype). Items 11, 12 and 13 apply to the Rust program too. No item is retired merely because its code was deleted: the code that remains
carries its items with it.

## Correctness and security

**1. A confirmed payment can be replayed once (verified).** A transaction's identity is its
signature string, ECDSA signatures are malleable (`(r, s)` and `(r, n - s)` both verify), and the
account model has no per-account sequence number. Anyone can flip `s` in a confirmed transaction and
have it included again: the recipient is paid twice from one signed payment. Reproduced: bob's
balance goes from 10 to 20 coins and the chain still validates. Only one replay is possible per
payment (flipping again gives back the original). *Rewrite:* a transaction identity that does not
depend on a malleable signature (a hash of the signed content), low-s or an equivalent canonical form,
and spend-once outputs / key images instead of balances.

**2. Signatures use SHA-1 (verified).** `Wallet.sign` calls python-ecdsa's `sign()` without naming a
hash, and its default is SHA-1. Verification with SHA-256 fails. *Rewrite:* a modern signature scheme
with a stated hash.

**3. The genesis block is not checked (verified).** `is_valid` starts at block 1 and only compares
`previous_hash` with whatever block 0 the file contains, so a chain with a different genesis is
accepted and there is no chain identity. *Rewrite:* a fixed, checked genesis, and its hash as the chain id.

**4. Signed data names no chain (verified).** The payload holds no chain id or genesis hash, so a signed
transaction is valid on any chain with the same history. *Rewrite:* bind signatures to the chain.

**5. The block hash depends on Python's JSON formatting (verified).** Header bytes are Python's
`json.dumps(..., sort_keys=True)`; a timestamp written as `1700000000.0` hashes differently from
`1700000000`, and non-ASCII memos are escaped as `\uXXXX`. A second implementation would have to imitate
Python exactly. *Rewrite:* a canonical binary serialization, defined once, with test vectors.

**6. Validity depends on the wall clock (from the code).** A block more than 120 s ahead of the
validator's clock is invalid, so two honest nodes can disagree about a block near the boundary, and a
chain that was valid yesterday can be judged differently today. *Rewrite:* treat far-future blocks as
"not yet acceptable" rather than permanently invalid, as most chains do.

**7. Sizes and integer widths are implicit (from the code).** Transaction size is the length of a JSON
string, and Python integers are unbounded, so amounts and fees have no defined range. *Rewrite:*
define widths (64-bit amounts; 128-bit intermediates for the penalty; 256-bit targets) and check
overflow explicitly.

**8. The genesis block's transactions are counted by `balance_of` but ignored by `is_valid` (from the
code).** Harmless while the genesis is empty; a real inconsistency if it were not.

**9. Consensus constants are not stored with the chain (from the code).** The fee rate, the memo limit,
the block-size floor and window, the timestamp windows and the step limit are software constants. Changing
one silently changes which old blocks are valid (raising the size floor would invalidate an old block that
paid an oversize penalty). *Rewrite:* version the rules, with activation heights.

**10. The minimum fee is a consensus rule (from the code).** A block containing a transaction below the
minimum is invalid, not merely unrelayed. That is a choice worth making deliberately: many chains keep a
minimum relay fee as policy instead.

## The proof of work

**11. Memory-hardness is simulated, not proven (from the code and the simulation).** The dependency
structure was measured on a 256-slice stack of small slices (`reference/tenero/analysis.py`), and the slowdown for
keeping only part of the dataset is an estimate built from that and from one GPU's measured times. The fill
and fold construction (a ChaCha20 core with feed-forward and XORs, used as a compression function) is
original to this project and has had no cryptanalysis or independent review.

**12. A minority miner can lower the difficulty with its timestamps (verified by simulation of the real rules, 2026-10-02; FIXED in the code at M11.2, 2026-10-04, for the new chain: "a block's timestamp must be later than its parent's", the owner's decision of 2026-10-03; measured after the fix: a 30 % backdating miner leaves the difficulty at 1.02x, was 0.40x).**
LWMA with a window of 30, a solve time floored at 1 s and capped at 6 block times, and a median-of-11 timestamp rule. A miner with 30 %
of the hash rate that backdates its blocks to the median gets 0.40x the honest difficulty (blocks every 25 s instead of 60); with 10 %, 0.69x.
The same attackers leave it at 1.00x if a block's timestamp must be later than its parent's. `tenero-core/tests/difficulty_sim.rs` keeps
both numbers as tests and `docs/THREAT_MODEL.md` (E3) has the table and the fixes that were tried and rejected. A consensus change, so
for the fresh chain (M11.2), by decision. **Both the Python reference and the Rust validator now have the new rule** (`CONSENSUS.md` section 7; vectors and tests per edge). It starts a new chain: data from before is not valid under it. The analysis above is as it was written.

**13. Speed numbers come from one machine (from the code).** Everything measured about the GPU (about
22,000 attempts per second, a 0.10 s dataset build) is from one RTX 5070 Ti.

## Scale

**14. Storage does not scale (from the code, measured).** The whole chain is rewritten to one JSON file on
every save, `balance_of` scans the whole chain, and `is_valid` re-walks it. Measured with 300 kB blocks:
about 370 kB of file per full block, so a day of full blocks is about 530 MB and about 8 s per save. Mining
a completely full block also costs about 5 s of CPU before the GPU search starts. *Rewrite:* an indexed
database, incremental validation, and a UTXO or output set.

## Gaps (not bugs)

**15.** There is no fork-choice rule (the chain with the most cumulative work: the sum of `2^256 // target`
over its blocks), no networking, no transaction relay rules, and no chain reorganisation handling.
