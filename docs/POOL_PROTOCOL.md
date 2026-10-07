# The pool protocol: how a miner talks to a mining pool (version 1, a DRAFT standard; a pool, a pool miner and a conformance tool are built, for the test network)

**Status (2026-10-07): a draft by the author; the owner has decided its open points (see "Decisions"). **Only the messages exist in the code** (`crates/tenero-app/src/pool.rs`, checked against golden vectors from an independent Python reference); no pool and no pool miner exists. Tenero is unaudited and experimental; no pool exists and nobody has said they will run one.** The point of writing it now, before any pool exists, is that every pool and every miner that follows this page can work together: a miner written to it connects to any pool written to it. The owner's rule (2026-10-06): *set the standard first.*

This is **not** the miner service of `docs/REMOTE_MINING_PLAN.md`. That service is solo mining on someone else's node: the node builds the whole block and the reward goes to **the miner**. In a pool the reward goes to **the pool**, the miner is paid by the pool later, and the miner works on a header the pool built. The two must never be mistaken for each other (see "Two modes that cannot be confused").

**Built 2026-10-07:** `tenero-pool` (`crates/tenero-app/src/pool_server.rs`, `pool_core.rs`, `bin/tenero_pool.rs`: `docs/RUNNING_A_POOL.md`), the miner's `--pool` mode (`pool_miner.rs`) and the app's pool option, and **`tenero-poolcheck`** (`pool_check.rs`), the conformance tool of "A conformance kit" below. **Not built:** job declaration (this pool says so: its `hello_ok` has no capability bits, and a miner that sends `declare_job` is disconnected), the optional tip witness, and the reference pool for other implementers (this pool is the reference for now). What a share is worth, the PPLNS window, the payout rules and the difficulty adjustment are the pool's own rules, not the protocol's: they are in `docs/RUNNING_A_POOL.md`.

## What it is modelled on, and what it is not

Binary frames, an encrypted channel, jobs, a share target and per-miner nonce ranges are the shape of Stratum V2, as far as I know it (this page is written from memory of that design, not from its text; a pool author who knows it better may correct me). It is **not** compatible with Stratum V1 (JSON text, a 32-bit nonce and no encryption) or with V2: Tenero's proof of work (matmulhash) searches a 64-bit nonce and the miner's answer includes a 64-byte *mix*, which those protocols have no place for. A bridge for those protocols is possible and is somebody else's work.

## The channel

* The same **Noise** channel as the peer-to-peer code and the miner service (`Noise_XX_25519_ChaChaPoly_BLAKE2s`, `crates/tenero-net/src/noise.rs`), with its own prologue, `tenero pool v1`, so a pool port cannot be mistaken for any other Tenero port and the handshake fails at once if it is.
* **No pre-shared key**: a pool is public. The encryption hides the traffic from someone on the path and stops them changing it. It does **not** tell the miner who the pool is: nothing pins the pool's key to anyone (`noise.rs` says so), so a person who sits between a miner and a pool can take the miner's work. A miner may be given the pool's public key out of band (`--pool-key`) to refuse any other; a pool should publish its key.
* Frames are those of the control protocol (`docs/CONTROL_PROTOCOL.md`): `length u32 little-endian | kind u8 | payload`, integers little-endian, text as UTF-8 with a `u32` length, flags 0 or 1, strict decoding (an unknown kind, a short or long payload, a count over its limit or a bad flag closes the connection). A frame is at most 4 MiB in this protocol.

## Messages

**The exact encodings are in `tests/vectors/pool.json` (35 valid messages and 126 malformed ones, made by the independent Python reference `reference/tools/make_vectors_pool.py`) and in `crates/tenero-app/src/pool.rs` (the messages only: no pool and no pool miner exists).** This table says what they are. Integers are little-endian; "text" is a `u32` length then UTF-8; a "flag" is one byte, 0 or 1; "(32)" is a fixed byte string; a coinbase or transaction is the consensus encoding of `docs/CONSENSUS_V2.md`.

Miner to pool: kinds 1 to 5. Pool to miner: 0x80 and up. A pool message is never a valid miner message and the reverse (an unknown kind). The answer to request `k` is `k | 0x80` where there is one.

| kind | message | payload | notes |
|---|---|---|---|
| 1 | `hello` | lowest version u16, highest version u16, capabilities u32, network (text, at most 32), payout address (text, at most 256), worker (text, at most 32), agent (text, at most 64) | the first message; nothing else is accepted before it. The versions are 1 <= lowest <= highest. The **payout address** is the miner's own wallet address: the pool uses it to credit the miner. The network is `alpha`, `dev` or `test`; a pool for another network refuses. Unknown capability bits are ignored |
| 0x81 | `hello_ok` | version u16 (at least 1), capabilities u32, session u64, nonce prefix bits u8 (0 to 32), nonce prefix u64 (must fit in the bits), share target (32, big-endian; **not zero and not all ones**), pool name (text, at most 64), **pays the pool** flag | the version is the highest both support. The flag is always 1 and is there so a miner can refuse a pool that does not say it plainly |
| 2 | `submit_share` | job id u64, nonce u64, mix (64) | a solution to the share target of a job |
| 0x82 | `share_result` | job id u64, flag accepted, reason u8 (0 to 6), text (at most 128) | **accepted is 1 exactly when the reason is 0.** Every share gets an answer |
| 3 | `ping` | token u64 | the miner may check the pool is alive; the pool answers `pong` (0x83) with the same token |
| 0x90 | `job` | job id u64, height u64, flag **clean**, header (146 bytes, **nonce 0 and mix all zero**), block target (32, big-endian), seconds to live u32 (1 to 3600) | work. `clean = 1` means the tip has moved or the pool has changed its mind: **every earlier job is dead and the miner must drop it at once** |
| 0x91 | `set_share_target` | share target (32, big-endian; not zero, not all ones) | the pool changes how hard a share is for this miner, from the next job on (see "Share difficulty") |
| 0x92 | `set_payout` | height u64, one-time address (32), view tag (3), ephemeral key (32), anchor (16) | **decided 2026-10-07:** where the reward of a block at `height` goes (the pool's); a declared job must pay exactly this. Sent for each new height to miners that declared the capability |
| 4 | `declare_job` | declaration id u64, height u64, previous block id (32), timestamp u64, a coinbase, then a count u32 (0 to 8192) and that many transaction ids (32 each) | optional (capability bit 0 on both sides); see "Job declaration" |
| 0x84 | `declare_result` | declaration id u64, status u8, then: **0 accepted**: job id u64 and a header (146, nonce and mix empty); **1 refused**: reason u8 (1 to 7) and text (at most 128); **2 missing**: a count u32 (1 to 8192) and that many ids (32 each) | the answer to `declare_job` |
| 5 | `provide_txs` | declaration id u64, a count u32 (1 to 64) and that many whole transactions | the answer to "missing". The pool also limits the total to 2 MB, which is the pool's rule, not the encoding's |
| 0xFF | `error` | text (at most 128) | the pool then closes the connection |

A frame is `length u32 | body` with a length of 1 to **4 MiB** (a `declare_job` of 8192 ids is 262 KB and a `provide_txs` of 64 transactions can be 2 MB).

**Reasons in `share_result`:** 0 accepted, 1 stale (the job is dead: the tip moved or the job expired), 2 duplicate (the same nonce for the same job), 3 above the share target (also: outside the miner's nonce prefix), 4 the mix is wrong (the proof of work does not reproduce it), 5 unknown job, 6 not allowed (for example, too many bad shares in a row: the pool then closes the connection).

**Reasons in a refused `declare_result`:** 1 not on the pool's tip, 2 the coinbase does not pay the pool's payout, 3 too many transactions, 4 against the pool's policy, 5 too many declarations in a minute, 6 a transaction is not valid, 7 anything else.

### What a share is

A share is a header (the job's, with the miner's `nonce` and `mix`) whose **block id**, computed exactly as the chain computes it (`ids::block_id`, the matmulhash proof of work), read as a big-endian 256-bit number, is **at most the share target**. A share that is also at most the job's **block target** is a block: the pool submits it to its node. The miner does not need to tell the difference; the block target in the job is so the miner can show "this was a block". (The target relation is the chain's: a lower target is harder; difficulty is 2^256 over the target.)

### The search space: a nonce prefix for each miner

The pool gives each session a **nonce prefix**: the top `nonce prefix bits` bits of the 64-bit nonce are fixed to `nonce prefix`, and the miner searches only the lower bits. Two miners of one pool therefore never search the same nonces (no work is duplicated and no share is a duplicate of another miner's). A pool with up to 2^16 miners at once uses 16 bits and leaves 48 for each (2^48 nonces per job; whether that is enough for a fast GPU on a long job has not been worked out, and a pool replaces the job at every new block anyway). The miner must keep its nonces inside its prefix; a share outside it is refused (reason 3).

### Share difficulty

A share target easier than the block target is the whole point: it lets a pool count a miner's work in a few seconds instead of waiting for a block. **The pool chooses it, per miner, to aim at about one share every 10 to 30 seconds** (at the alpha network's starting difficulty a block takes about 524,000 attempts; the pool's cost is one proof-of-work check per share, which has not been measured). A pool may change it at any time with `set_share_target`. A miner **must** accept any share target the pool sets **except** zero (it can never be met) and all ones (every attempt would be a share, which floods the pool: the usual sign of a broken or hostile one); it refuses those as an error and disconnects.

### The pool checks every share

The pool recomputes the proof of work for every share (it needs the epoch's dataset, about 4 GiB, as a node does) and compares it with the target. A pool that trusts shares without checking them can be cheated by a miner who sends junk; a miner who sends many bad shares may be disconnected (reason 6).

## Job declaration (optional, version 1)

**Why:** with a plain job the pool alone chooses the transactions of every block, so a pool can censor. With *job declaration* a miner that runs its own node builds the block itself and the pool only counts its shares and checks the block pays the pool. It is **optional**: a pool may refuse it (it does not set the capability bit) and a miner without a node never needs it. It is not for the miners the pool exists for (those with no node); it is for those who want the pool's payouts without handing it the choice of transactions. (The idea is Stratum V2's job declaration; this is a design for Tenero's data model, from my memory of that one, and a pool author who knows it should correct me.)

**Capability:** bit 0 of `capabilities` (0x01) means "can do job declaration". A miner declares only if the pool's `hello_ok` has the bit.

**What the pool tells the miner first:** in a `set_payout` message the **pool payout** for a height (a one-time address, view tag, ephemeral key and anchor: the fields of a coinbase output, `crates/tenero-node`'s `Payout`). A declared job's coinbase must pay **exactly this**, the whole reward, as one output with no extra data. This is what lets the pool pay its miners: the reward is the pool's, as in a plain job. (A coinbase's key exchange binds the height, so the pool sends a payout for each height in a `set_payout` message.)

**The flow:**
1. The miner builds a block template on its own node with the pool's payout (the node's `block_template`, as the miner service does today) and sends **`declare_job`**: a declaration id u64, the height u64, the previous block id (32), the timestamp u64, the whole coinbase (the consensus encoding), and the **transaction ids** (a count of 0 to 8192, then 32 bytes each). The block's `tx_root` is the Merkle root of the coinbase id and these ids (`ids::block_tx_root`), so the pool computes it itself; the miner does not send it.
2. The pool checks, cheaply: the previous block is its tip, the height is right, the coinbase pays exactly its payout for that height, and the timestamp fits. It checks which of the transactions its own node already has (its mempool). It answers **`declare_result`**: accepted (with the job id to mine, and the header the miner must use, so both sides hash the same bytes), or refused with a reason (not on the tip, wrong payout, too many transactions, a policy of the pool, too many declarations a minute), or **missing**: the ids it does not have.
3. For missing ids the miner sends **`provide_txs`** (the whole transactions, at most 64 in a message and 2 MB in all); the pool validates each one against its tip as a mempool would, and only then accepts the declaration. A declaration containing a transaction that does not validate is refused as a whole (and counts against the miner).
4. The miner mines the header it was given and sends shares as usual. A share that meets the block target is a block: **the pool, which now has every transaction, builds the block and submits it to its own node.** The miner does not need to.

**What the pool must still do:** validate every transaction it did not already have (a declaration is a way to make a pool do work: so a cap on declarations per minute, on transactions per declaration and on bytes). **What it does not do:** choose the transactions. **What a miner gains and risks:** it chooses its transactions, and it spends more (a node, and one declaration for every new tip). It does **not** gain any say in where the reward goes: that is the pool's, so nothing here helps a miner who wants a payout to itself (that is solo mining).

## Two modes that cannot be confused

| | solo service (`docs/REMOTE_MINING_PLAN.md`) | pool (this page) |
|---|---|---|
| Who builds the block | the node | the pool, which keeps the transactions |
| Where the reward goes | **the miner's address** (the miner checks the coinbase) | **the pool**; it pays the miner later, by its own rules |
| What the miner receives | a whole block | a header only |
| What the miner can check | the coinbase, the body, the root, the clock | only the header's version, clock, nonce and mix fields; it **cannot** see the body or where the reward goes |
| The miner program's flag | `--node HOST:PORT --key HEX` | `--pool HOST:PORT [--pool-key HEX]` |
| The handshake prologue | `tenero miner service v1` | `tenero pool v1` |

Because the prologues differ, a miner pointed at the wrong kind of server fails the handshake instead of mining for the wrong party. **A miner never switches mode by itself:** `--node` and `--pool` are different paths through the program, and the pool one says on the screen, every time it starts, that the reward goes to the pool and not to the miner's address.

## What the miner checks, and what it cannot

A pool miner can and must check (cheap, local): the job's header has version 1 and the nonce and mix empty; its timestamp is within an hour of this computer's clock; the height is not lower than the last job's; the share target is not zero or all ones. It **cannot** check that the job is built on the real tip, that the transactions are valid or that the reward goes to the pool and not to someone else: those need the chain or the body. The recommended defence is a **tip witness**: the miner also asks a node it trusts (its own, or a public one, over the miner service's `info`) what the tip is, and **stops mining a pool's job whose `prev_id` is not that tip for more than a minute**: that catches a pool mining on a stale or private fork, which wastes the miner's work and pays nothing. This is optional in version 1 and the miner program should offer it (`--witness HOST:PORT`).

## What the protocol cannot protect a miner from

* **A pool that does not pay.** Nothing in the protocol or the chain makes a pool pay. The share count is the pool's own record; the miner keeps its own count of accepted shares (the protocol requires an answer to every share so it can) and can see if the pool's payouts do not match. A miner chooses a pool by its reputation, as with any pool anywhere.
* **A pool that keeps blocks.** A pool can find a block and not credit the miner's shares. The miner cannot see blocks that the pool does not announce; the chain shows who the pool is.
* **A pool that is a single point of failure and of control**: it chooses the transactions. Pools make the network less decentralised; that is a cost the owner has accepted for the benefit of miners without a node, and the README should say so when a pool exists.
* **Privacy**: the pool knows the miner's address and its address on the internet.

## What a pool must do (the rules a developer follows)

1. Speak this protocol exactly; refuse anything that does not decode.
2. Answer every share. Say `clean` when the tip moves, before any share for the old tip is accepted again.
3. Keep jobs for the **current tip only**; a share for an older tip is stale (reason 1).
4. Pay the miners according to published rules and **publish them** (the fee, the scheme, the smallest payment). The protocol does not carry them: a pool's page does.
5. Run its own node, fully validating (a pool built on someone else's node trusts that node as much as a miner trusts a pool).
6. Not require anything of the miner beyond the address and a worker name.

## A conformance kit (so "works with every miner" is checkable)

Before this is called a standard it needs, in this repository:

* golden vectors for every message (valid and malformed), made by an independent Python reference, as every other protocol here has (CLAUDE.md rules 1 and 6);
* **`tenero-poolcheck`**, a program a pool developer runs against their pool, like `tenero-seedcheck`: it connects as a miner, checks the handshake, that a job arrives, that a share that meets the target is accepted and one that does not is refused, that a duplicate is refused, that `clean` is respected and that a bad frame closes the connection. A pool that passes it is *compatible with version 1*; it says nothing about its honesty;
* a reference pool for the tests (a small one, not for use) and the miner's `--pool` mode checked against it, on the SHA-256 test chain first.

## Decisions (the owner, 2026-10-07)

1. **Nonce partition by prefix** (as written above): decided.
2. **Tip witness optional** (`--witness HOST:PORT`): decided. A pool miner works without it.
3. **The pool's public key is pinned by default**: decided. A miner is given the pool's key (`--pool-key`, or a field in the app) and refuses any other. The program's own default pool (see `docs/REMOTE_MINING_PLAN.md`, stage 3) carries its key inside it, as the built-in seed carries its address.
4. **Default port 38335** for a pool; a pool may use any port.
5. **Job declaration is part of version 1**, as an optional capability (above): decided.

**Decided 2026-10-07:** the pool hands the miner the payout for each new height in a **message of its own** (`set_payout`, 0x92), so ordinary jobs stay small. **Still open inside job declaration:** whether the miner may also send the block's transactions in `declare_job` when the pool has none of them (a size and abuse trade-off); and whether a pool must support it to be called compatible (the author suggests not: a pool may refuse it, and `tenero-poolcheck` tests it only if the bit is set).

## Threat model

A pool is a public listener and a trust relationship: it needs its own entries in `docs/THREAT_MODEL.md` (a hostile pool; a hostile miner flooding a pool; a person between them) before any release. It has none yet.
