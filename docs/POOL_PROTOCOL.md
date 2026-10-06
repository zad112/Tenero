# The pool protocol: how a miner talks to a mining pool (a DRAFT standard, version 1; nothing is built)

**Status (2026-10-06): a draft by the author, for the owner to decide and for anyone who wants to write a pool to criticise. Nothing here exists in the code. Tenero is unaudited and experimental; no pool exists and nobody has said they will run one.** The point of writing it now, before any pool exists, is that every pool and every miner that follows this page can work together: a miner written to it connects to any pool written to it. The owner's rule (2026-10-06): *set the standard first.*

This is **not** the miner service of `docs/REMOTE_MINING_PLAN.md`. That service is solo mining on someone else's node: the node builds the whole block and the reward goes to **the miner**. In a pool the reward goes to **the pool**, the miner is paid by the pool later, and the miner works on a header the pool built. The two must never be mistaken for each other (see "Two modes that cannot be confused").

## What it is modelled on, and what it is not

Binary frames, an encrypted channel, jobs, a share target and per-miner nonce ranges are the shape of Stratum V2, as far as I know it (this page is written from memory of that design, not from its text; a pool author who knows it better may correct me). It is **not** compatible with Stratum V1 (JSON text, a 32-bit nonce and no encryption) or with V2: Tenero's proof of work (matmulhash) searches a 64-bit nonce and the miner's answer includes a 64-byte *mix*, which those protocols have no place for. A bridge for those protocols is possible and is somebody else's work.

## The channel

* The same **Noise** channel as the peer-to-peer code and the miner service (`Noise_XX_25519_ChaChaPoly_BLAKE2s`, `crates/tenero-net/src/noise.rs`), with its own prologue, `tenero pool v1`, so a pool port cannot be mistaken for any other Tenero port and the handshake fails at once if it is.
* **No pre-shared key**: a pool is public. The encryption hides the traffic from someone on the path and stops them changing it. It does **not** tell the miner who the pool is: nothing pins the pool's key to anyone (`noise.rs` says so), so a person who sits between a miner and a pool can take the miner's work. A miner may be given the pool's public key out of band (`--pool-key`) to refuse any other; a pool should publish its key.
* Frames are those of the control protocol (`docs/CONTROL_PROTOCOL.md`): `length u32 little-endian | kind u8 | payload`, integers little-endian, text as UTF-8 with a `u32` length, flags 0 or 1, strict decoding (an unknown kind, a short or long payload, a count over its limit or a bad flag closes the connection). A frame is at most 64 KiB in this protocol.

## Messages

Miner to pool (kinds 1 to 15), pool to miner (kinds 0x80 and up). The answer to request `k` is `k | 0x80` where there is one.

| kind | message | payload | notes |
|---|---|---|---|
| 1 | `hello` | lowest version u16, highest version u16, network (text, at most 32), payout address (text, at most 256), worker (text, at most 32), agent (text, at most 64) | the first message; nothing else is accepted before it. The **payout address** is the miner's own wallet address: the pool uses it to credit the miner. The network is `alpha`, `dev` or `test`; a pool for another network refuses |
| 0x81 | `hello_ok` | version u16, session u64, nonce prefix bits u8, nonce prefix u64, share target (32, big-endian), pool name (text, at most 64), **pays the pool** flag (always 1) | the version is the highest both support. The flag is there so a miner can refuse a pool that does not say it plainly |
| 2 | `submit_share` | job id u64, nonce u64, mix (64) | a solution to the share target of a job |
| 0x82 | `share_result` | job id u64, flag accepted, reason u8, text (at most 128) | reasons below. **Every share gets an answer** |
| 3 | `ping` | token u64 | the miner may check the pool is alive; the pool answers `pong` (0x83) with the token |
| 0x90 | `job` | job id u64, height u64, flag **clean**, header (146), block target (32, big-endian), seconds to live u32 | work. `clean = 1` means the tip has moved or the pool has changed its mind: **every earlier job is dead and the miner must drop it at once**. The header has its nonce and mix empty (zero) |
| 0x91 | `set_share_target` | share target (32, big-endian) | the pool changes how hard a share is for this miner, from the next job on (the pool decides; see "Share difficulty") |
| 0xFF | `error` | text (at most 128) | followed by the pool closing the connection |

**Reasons in `share_result`:** 0 accepted, 1 stale (the job is dead: the tip moved or the job expired), 2 duplicate (the same nonce for the same job), 3 above the share target, 4 the mix is wrong (the proof of work does not reproduce it), 5 unknown job, 6 not allowed (for example, too many bad shares in a row: the pool then closes the connection).

### What a share is

A share is a header (the job's, with the miner's `nonce` and `mix`) whose **block id**, computed exactly as the chain computes it (`ids::block_id`, the matmulhash proof of work), read as a big-endian 256-bit number, is **at most the share target**. A share that is also at most the job's **block target** is a block: the pool submits it to its node. The miner does not need to tell the difference; the block target in the job is so the miner can show "this was a block". (The target relation is the chain's: a lower target is harder; difficulty is 2^256 over the target.)

### The search space: a nonce prefix for each miner

The pool gives each session a **nonce prefix**: the top `nonce prefix bits` bits of the 64-bit nonce are fixed to `nonce prefix`, and the miner searches only the lower bits. Two miners of one pool therefore never search the same nonces (no work is duplicated and no share is a duplicate of another miner's). A pool with up to 2^16 miners at once uses 16 bits and leaves 48 for each (2^48 nonces per job; whether that is enough for a fast GPU on a long job has not been worked out, and a pool replaces the job at every new block anyway). The miner must keep its nonces inside its prefix; a share outside it is refused (reason 3).

### Share difficulty

A share target easier than the block target is the whole point: it lets a pool count a miner's work in a few seconds instead of waiting for a block. **The pool chooses it, per miner, to aim at about one share every 10 to 30 seconds** (at the alpha network's starting difficulty a block takes about 524,000 attempts; the pool's cost is one proof-of-work check per share, which has not been measured). A pool may change it at any time with `set_share_target`. A miner **must** accept any share target the pool sets **except** zero (it can never be met) and all ones (every attempt would be a share, which floods the pool: the usual sign of a broken or hostile one); it refuses those as an error and disconnects.

### The pool checks every share

The pool recomputes the proof of work for every share (it needs the epoch's dataset, about 4 GiB, as a node does) and compares it with the target. A pool that trusts shares without checking them can be cheated by a miner who sends junk; a miner who sends many bad shares may be disconnected (reason 6).

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

## Open decisions for the owner

1. **Nonce partition by prefix (this draft) or an extra nonce in the coinbase.** The prefix is simpler and keeps every job a bare header. The coinbase way lets a pool give a miner a different body per miner, but needs the miner to rebuild the transaction root, which this draft avoids on purpose.
2. **Tip witness: optional (this draft) or required.** Required protects miners better and costs them a second connection.
3. **Whether the pool's public key must be pinned** (`--pool-key` required). Safer; makes joining a pool a copy-and-paste of a key.
4. **The name and the port**, if the standard is also to be a default for pools to listen on (the author suggests 38335 and does not ask for it to be fixed).
5. **Job declaration** (the miner builds its own block from its own node and the pool only counts shares: a design in Stratum V2 that keeps the choice of transactions with the miner). Not in version 1; the message set leaves room for it by version number.

## Threat model

A pool is a public listener and a trust relationship: it needs its own entries in `docs/THREAT_MODEL.md` (a hostile pool; a hostile miner flooding a pool; a person between them) before any release. It has none yet.
