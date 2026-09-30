# M8 plan: the node, the network, the miner and the wallet (a PROPOSAL, for the owner to approve)

Status: **proposal, 2026-09-30. Nothing here is built.** Decisions marked **DECIDE** are open, each with a
recommendation that is only a recommendation. Sizes are relative guesses (S/M/L), **not measured**.

M8 turns the libraries of M0 to M7 (rules, storage, validation, fork choice, proofs, the GPU engine) into
programs you can run. It is the first milestone where a second computer could join, so it is also where the
"experimental, unaudited, not for real value" label matters most.

## 1. What exists, and what M8 must add to it

| Exists | Where |
|---|---|
| Rules, data model, ids, genesis | `tenero-core` |
| Chain storage, pruning, crash safety | `tenero-store` |
| Block validation, fork choice, reorganisation | `tenero-chain` (`Validator`, `Chain`) |
| CLSAG, Bulletproofs+, balance, the prover | `tenero-crypto` (`RingCtProofs`, `prove`) |
| GPU proof of work (the engine only) | `tenero-gpu` |

Gaps found by reading the code, all needed before M8 is honest:

1. **No transaction-level validation.** The per-transaction checks (fee, key images, rings, maturity, proofs) are
   private inside the block validator. A mempool needs them alone, against the chain's current state.
2. **`Chain::submit_block` does not say which transactions a reorganisation undid.** A mempool must re-add them.
3. **The side-branch pool is in memory only** and orphans are dropped: fine for tests, poor for a network where
   blocks arrive out of order.
4. **`RingCtProofs` is not the default** (the plan says M8). The node must refuse to start without it.
5. **No block template.** Nothing yet builds a block from the mempool with the right coinbase and Merkle root.
6. **The GPU engine mines one dataset.** No epoch switching, no prefetch of the next epoch, no overlap of CPU and
   GPU work (`docs/REWRITE_PLAN.md`, M4).
7. **Assume-valid and state snapshots** (the pruning layers of `CONSENSUS_V2.md` 14) are designed, not built.
8. **Carrot is not in.** Without it a wallet cannot create or scan real outputs (see 4).

## 2. The pieces, in the order I propose to build them

Each step ends in something checkable. Order matters: a node that runs alone comes before a network, and a
network of simulated peers comes before a real socket.

### M8.0 The node core, on one machine (size M)
`tenero-node`: opens the store, builds `Chain` with `RingCtProofs`, and adds a **mempool**: a transaction is
accepted if it passes the transaction-level checks against the tip's state; conflicts (same key image) are
refused; ordered by fee per byte; bounded in size; evicted when a block confirms or a conflicting one arrives;
re-added after a reorganisation. Gaps 1, 2 and 4 are closed here.
*Done when:* a test feeds transactions and blocks (including a reorganisation) to a node in one process and the
mempool always equals "valid, unconfirmed, non-conflicting", checked by a model.

### M8.1 Simulated network first (size M)
A `Transport` trait with two implementations: an in-memory deterministic one for tests, and TCP later. The
protocol logic (handshake, sync, relay, banning) is written against the trait, so it can be tested with many
nodes, delays, drops, partitions and hostile peers **without sockets or timing luck**.
*Done when:* N simulated nodes mining on the SHA-256 test chain converge to one chain after partitions heal, a
peer that sends an invalid block is banned, and one that sends junk is disconnected.

### M8.2 The wire protocol (size M)
One canonical framed format, reusing the version 2 codec (`CONSENSUS_V2.md` section 4): length-prefixed messages
with a hard maximum size, strict decoding, and a version and chain id in the handshake (a peer on another chain
is dropped at once). Proposed messages: `Hello` (protocol version, chain id, tip height and cumulative work),
`GetBlockIds` / `BlockIds`, `GetBlocks` / `Blocks`, `NewBlock` (id and work, then fetch), `NewTx` (id) with
`GetTx` / `Tx`, `Ping`. Pruned nodes serve only what they have and say so.
*Done when:* every message has a golden vector (made by our Python reference, like the other v2 vectors) and
strict-decoding tests for each malformed case.

### M8.3 Sync (size L, the hard part)
A new node learns the chain: ask a peer for its tip and work, fetch the block ids, then the blocks, validate and
add them in order through `Chain`. Out-of-order blocks and orphans are held (gap 3 closed by a bounded persisted
pool). **The cost is real:** the full proof of work is checked per block on the CPU (measured in
`docs/BENCHMARKS.md`: about 175 ms an attempt, 35 ms with `target-cpu=native`, plus 2.7 to 3.9 s once per epoch to
build the 4 GiB dataset), so syncing a long chain is slow without **assume-valid**: below a shipped checkpoint,
skip the signature and full-PoW checks and verify only the cheap ones. That is a trust decision (see 6), so it is
opt-in and visible.
*Done when:* a fresh node syncs a 10,000-block test chain from another, on the SHA-256 chain quickly and on the
real proof of work with the measured time reported, both an archive and a pruned node.

### M8.4 Real sockets (size M)
TCP transport, peer address book, inbound and outbound limits, per-peer rate limits and a ban score, message
timeouts, an encrypted and authenticated channel if approved (DECIDE 3).
*Done when:* three nodes on the owner's machine (three data directories, three ports) run a private test network
for a day without diverging, and the logs show refusals of deliberately bad input.

### M8.5 The miner (size M to L)
A block template from the node (mempool, correct coinbase, Merkle root, `Validator::next_block` for the target),
the GPU engine feeding attempts, epoch switching with the next dataset prefetched (gap 6), and submission through
`Chain`. A CPU path for the SHA-256 test chain. Respects `--max-cores` (CLAUDE.md rule 8).
*Done when:* the GPU miner mines blocks accepted by a node that verifies them with the CPU check (the real
end-to-end test of "bit for bit"), **and the owner measures the attempts per second** (CLAUDE.md rule 5: no GPU
number is claimed without one).

### M8.6 The wallet (size L, and partly BLOCKED)
Keys, scanning the chain for one's outputs, building transactions (the prover exists; decoy selection is wallet
policy, `CONSENSUS_V2.md` 6.3), fee choice from `dynamic_min_fee`, and a way to talk to the node.
**Blocked by Carrot** for real outputs: the wallet cannot recognise its own outputs or derive one-time addresses
without it, and waiting for Monero's `carrot_core` was the decision. Until then I can build only what does not
need it: key storage (encrypted at rest), transaction building, decoy selection, fee logic, and an **interim,
clearly labelled test-only scheme** where a wallet knows the secrets of outputs it created. That scheme is not
private and must never be described as if it were.
*Done when:* two wallets pay each other through a running node, in the interim scheme, with real CLSAG and
range proofs, and the docs say what is missing.

### M8.7 Interfaces and operation (size S to M)
A local control interface for the wallet and miner to reach the node (DECIDE 4), a config file, logging,
graceful shutdown, and the node kinds of `CONSENSUS_V2.md` 14 (archive, pruned) selectable by flag.

## 3. Decisions for the owner

1. **Concurrency model. Recommendation: plain threads and channels, no async runtime.** A handful of peers does
   not need an async runtime, and it avoids a large dependency tree (rule 3). Revisit if a node must handle
   hundreds of peers. *Alternative:* `tokio` (MIT, widely used), which would be needed for many peers.
2. **Protocol. Recommendation: our own small framed protocol** on the v2 codec, as in M8.2. *Alternatives:*
   libp2p (large, many dependencies, a big surface to trust), or Monero's own "levin" protocol (complex, built
   around Monero's assumptions). Ours is small enough to read and to fuzz (M9), but it is ours, so it is
   unreviewed.
3. **Transport security. Recommendation: the Noise protocol (the `snow` crate)** for an encrypted, authenticated
   channel, **if you approve that dependency** (I have not checked its current licence, version or audit status;
   I would before asking). *Alternative:* plaintext in v1 and add it before any non-local use. This is not the
   same as network privacy (Dandelion++, Tor, I2P), which stays out of M8 (P4 in the plan).
4. **How the wallet and miner reach the node. Recommendation: the same framed protocol over `127.0.0.1` with a
   few extra request messages**, one codec to test. *Alternatives:* HTTP and JSON-RPC (needs a server crate and
   is more code to trust), or the wallet embeds the node (simple, but one process holds everything).
5. **Sync trust. Recommendation: opt-in assume-valid with a checkpoint shipped in the release**, default off
   until a real chain exists. Say in the docs what it trusts.
6. **The wallet before Carrot. Recommendation: build the parts that do not need it, and the labelled test-only
   interim scheme,** so the node, network and miner can be tested end to end. *Alternative:* build the wallet
   last, after Carrot. The risk of the first is that "interim" code tends to stay; I would put it in its own
   crate that the release does not include.
7. **Order.** Recommendation: as in section 2. *Alternative:* the miner first (you can see hashes sooner), but
   it needs the node and the template anyway.

## 4. Testing, and what "done" means for the milestone

- Every step above has a stated check; none is "it seems to work".
- **Simulation before sockets** (M8.1): partitions, delays, reordering, hostile peers, all deterministic and
  repeatable.
- **Model tests** for the mempool, as for the store.
- **Golden vectors** for every wire message, from an independent Python reference, as for the data model.
- **Mutation checks** on the protocol and sync logic, as done for every module so far.
- **A three-node private network on the owner's machine** before anything is called working.
- Fuzzing the decoders and the network handlers is **M9**, not M8; M8 only keeps them small and strict enough
  to fuzz.

## 5. What M8 will not do

No Dandelion++, Tor or I2P (network privacy, P4). No FCMP++ (P2). No subaddresses, integrated addresses or
multisig (P3). No public network: the labels stay "experimental", and the first network is private. No claim
that the network resists a determined attacker: a small chain has little proof-of-work security, an eclipse
attack on a node with few peers is easy, and an unreviewed protocol has bugs nobody has found.

## 6. Risks

- **Networking is where a chain gets attacked.** Every message is input from an adversary. Strict decoding,
  hard size limits, per-peer budgets and banning are in the plan, but only a review (M9) can say they are enough.
- **Sync cost with the real proof of work** may be the practical limit for a new node. It is measured in M8.3
  rather than assumed; assume-valid trades trust for speed.
- **Assume-valid and pruning are trust decisions** and must be visible to the user, not silent defaults.
- **The Carrot dependency** blocks a real wallet; the interim scheme is a hazard if it is mistaken for the real
  one.
- **Scope.** This is the largest milestone so far. If it must shrink, the order in section 2 is also the order
  of what to cut last: the node and the simulated network are the core; the wallet is the part most likely to
  wait.
- **GPU work can only be tested on the owner's machine** (CLAUDE.md rule 5).
