# M8 plan: the node, the network, the miner and the wallet (a PROPOSAL, for the owner to approve)

Status: **owner's decisions recorded 2026-09-30 (section 3); nothing here is built.** One item still needs the
owner's explicit confirmation: the `snow` exception (section 3, decision 3). Sizes are relative guesses (S/M/L), **not measured**.

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
8. **Carrot is not in.** The wallet runs on an interim, labelled output scheme meanwhile (M8.6, decision 6).

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

### M8.3a Peer discovery and connection management (size M; new since the first draft)
The owner wants **at least 50 peers** per node (decision 1), so finding and keeping peers is a component, not a
detail: an address book (persisted, with when each address was last seen and last worked), address exchange
between peers (`GetAddrs` / `Addrs`, with a cap on how many a peer may send and how fast), a list of seed
addresses shipped in the release, diversity rules (no more than a few peers from one network range), a target of
outbound connections and a cap on inbound ones, reconnecting when peers drop, and a persisted ban list.
**An honest limit:** a private test network of the owner's machines has three nodes, not 50. The 50-peer
behaviour is tested in the simulated network (M8.1, which can run hundreds of nodes) and only exercised for real
once other people run nodes.
*Done when:* in simulation a node started with only a seed list reaches its outbound target, replaces dropped
peers, refuses to be filled by one attacker's addresses, and survives 50+ peers sending at once.

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
TCP transport, the Noise channel (decision 3), inbound and outbound limits, per-peer rate limits and a ban score,
message timeouts. With 50+ connections the relay rules matter for bandwidth: a new block or transaction is
announced by its id and fetched from ONE peer, never pushed in full to all of them.
*Done when:* three nodes on the owner's machine (three data directories, three ports) run a private test network
for a day without diverging, the logs show refusals of deliberately bad input, and a stress test holds 50+
real loopback connections on one node while it keeps syncing.

### M8.5 The miner (size M to L)
A block template from the node (mempool, correct coinbase, Merkle root, `Validator::next_block` for the target),
the GPU engine feeding attempts, epoch switching with the next dataset prefetched (gap 6), and submission through
`Chain`. A CPU path for the SHA-256 test chain. Respects `--max-cores` (CLAUDE.md rule 8).
*Done when:* the GPU miner mines blocks accepted by a node that verifies them with the CPU check (the real
end-to-end test of "bit for bit"), **and the owner measures the attempts per second** (CLAUDE.md rule 5: no GPU
number is claimed without one).

### M8.6 The wallet, without waiting for Carrot (size L)
Keys (encrypted at rest), scanning the chain for one's outputs, building transactions (the prover exists), decoy
selection (wallet policy, `CONSENSUS_V2.md` 6.3), fee choice from `dynamic_min_fee`, and the connection to the
node. **The project does not stop for Carrot** (decision 6): everything runs on an **interim output scheme**
behind a small interface (create an output for a recipient, recognise one's own outputs, recover the amount and
the one-time secret), so Carrot slots in later by replacing one implementation. The interim scheme uses the same
123-byte output format and the same proofs; only how keys are derived and how a wallet recognises its outputs
differs, and it is **not Carrot and not private in the Monero sense** (for example, it will not have Carrot's
Janus protection). It lives in its own module marked interim, and the docs and the program's own banner say so.
**A consequence to accept now:** outputs made under the interim scheme cannot be scanned by a Carrot wallet, so
when Carrot is added the private test network gets a new genesis (a new chain id). That is fine for a test
network and not something to hide.
*Done when:* two wallets pay each other through a running node, with real CLSAG and range proofs, and the docs
say exactly what is interim.

### M8.7 Interfaces and operation (size S to M)
A local control interface for the wallet and miner to reach the node (DECIDE 4), a config file, logging,
graceful shutdown, and the node kinds of `CONSENSUS_V2.md` 14 (archive, pruned) selectable by flag.

## 3. Decisions (owner, 2026-09-30)

1. **Peers: at least 50 per node.** Recorded as a requirement. It changes three things from the first draft:
   peer discovery becomes its own step (M8.3a); relay announces ids and fetches from one peer; and the design must
   hold 50 to about 128 connections. **Threads still work at that scale**: two threads per peer (a reader and a
   writer) is about 100 to 250 threads, which an operating system handles easily, and it keeps the code simple
   and free of an async runtime. The design puts the connection behind a small trait, so if the count grows to
   many hundreds, switching to `tokio` (MIT) is contained. Proposed defaults, all configurable: outbound target
   50, inbound cap 64, hard cap 128, of which at least 16 outbound so that inbound peers alone cannot fill a node
   (a home connection behind a router usually gets few inbound peers, so reaching 50 relies on outbound). Bandwidth
   and memory per connection are **not yet measured**; M8.4 measures them.
2. **Protocol: our own framed protocol**, tested with golden vectors, strict decoding and a simulated network.
   It is unreviewed until M9, and the plan says so.
3. **Transport security: the Noise protocol via `snow`. NEEDS THE OWNER'S CONFIRMATION of an exception.**
   `snow` 0.10.0 (released 2025-07-19), licence Apache-2.0 OR MIT, about 27 million downloads, pure Rust by
   default. **Its own README says it "has not received any formal audit."** CLAUDE.md rule 3 says to use audited
   libraries, so using it is an exception to a rule the owner wrote. What softens it (from my recollection, not
   re-checked today): its underlying primitives come from widely used crates such as `curve25519-dalek`,
   ChaCha20-Poly1305 and BLAKE2, which have had reviews; what is unaudited is `snow`'s handshake state machine.
   The alternative that avoids the exception, plaintext and unauthenticated in v1, is worse for a node that talks
   to 50 strangers; writing our own Noise implementation would be worse still (home-made cryptography). I
   recommend the exception, recorded in `Cargo.toml` next to the dependency, and a mention in the threat model
   (M9). Noise here gives an encrypted, authenticated channel; it is not anonymity.
4. **Wallet and miner reach the node over the same protocol on `127.0.0.1`,** with a few extra request messages,
   refused from any other address.
5. **Assume-valid: opt-in, a checkpoint shipped in the release, default off** until a real chain exists; the
   docs say what it trusts.
6. **No waiting for Carrot.** Everything is built and run on the interim output scheme behind a swappable
   interface (M8.6). When Carrot arrives, its implementation replaces the interim one and the private test network
   restarts from a new genesis.
7. **Order as listed in section 2** (with M8.3a added before sync).

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
- **The interim output scheme** (decision 6) is a hazard if it is mistaken for Carrot, or if it outlives it. It is
  labelled everywhere, sits behind one interface, and the test network restarts from a new genesis when Carrot
  replaces it.
- **Scope.** This is the largest milestone so far. If it must shrink, the order in section 2 is also the order
  of what to cut last: the node and the simulated network are the core; the wallet is the part most likely to
  wait.
- **GPU work can only be tested on the owner's machine** (CLAUDE.md rule 5).
