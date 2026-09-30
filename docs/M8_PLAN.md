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

### M8.0 The node core, on one machine (size M) **DONE 2026-09-30** (`crates/tenero-node`)
`tenero-node`: opens the store, builds `Chain` with `RingCtProofs`, and adds a **mempool**: a transaction is
accepted if it passes the transaction-level checks against the tip's state; conflicts (same key image) are
refused; ordered by fee per byte; bounded in size; evicted when a block confirms or a conflicting one arrives;
re-added after a reorganisation. Gaps 1, 2 and 4 are closed here.
*Done when:* a test feeds transactions and blocks (including a reorganisation) to a node in one process and the
mempool always equals "valid, unconfirmed, non-conflicting", checked by a model.
*Result:* 14 tests, including a randomised run of 400 operations (offers, blocks on two competing branches,
reorganisations) that checks after every step that the pool is sound (everything in it valid at the tip, no shared
key image, nothing confirmed, size bounded, key-image index consistent) and complete (anything valid, offered and
missing conflicts with something in it). 20 deliberate faults injected into the mempool and node: 20 caught (one
first survived, a stale key-image index, and the checks were tightened). Gaps 1, 2 and 4 are closed:
`Validator::check_pool_tx`, `Chain::take_reorg_report`, `RINGCT` as the default, and `Node::new` refuses a proof
check that verifies nothing. Limits: the pool is in memory, has no replace-by-fee, and holds no chains of
unconfirmed spends (an output cannot be spent before it is mature).

### M8.1 Simulated network first (size M) **DONE 2026-09-30** (`crates/tenero-net`)
A `Transport` trait with two implementations: an in-memory deterministic one for tests, and TCP later. The
protocol logic (handshake, sync, relay, banning) is written against the trait, so it can be tested with many
nodes, delays, drops, partitions and hostile peers **without sockets or timing luck**.
*Done when:* N simulated nodes mining on the SHA-256 test chain converge to one chain after partitions heal, a
peer that sends an invalid block is banned, and one that sends junk is disconnected.
*Result:* `Engine` (the protocol as a state machine: events in, actions out, no I/O, no clock of its own) and `Sim`
(a deterministic simulator: real engines, real stores, really mined and validated blocks, latency, message loss,
partitions, in-order links, scripted hostile peers). 38 tests, including: handshake (wrong chain: dropped and
banned; wrong version: dropped only); every limit and every scoring rule, each broken on its own; a flood is rate
limited and an ordinary burst is not; silent and slow peers are pinged, then dropped after five unanswered pings
in a row, and never banned for it; an invalid block bans its sender at once and the ban holds until it expires;
a new node syncs a 1,100-block chain in batches (ids 500 at a time, blocks 32 at a time) from the peer with the
most work; a partition of 10 nodes heals onto the heavier chain; a 12-node network losing 10% of all messages
still converges and keeps its peers; and **60 nodes each connected to 59 peers converge, and every block body is
sent exactly once per node (`blocks` messages = blocks x 59), whatever the peer count**, with no honest peer
banned. 49 deliberate faults injected into the engine, all caught; seven survived at first and led to five new
tests and the removal of one redundant guard. Bugs the simulator found in my own first draft: a request answered
by a second peer looked like an unsolicited block (false bans), a full sync raced a single-block fetch (blocks
downloaded twice), a lost ping dropped a healthy connection, and a node kept re-announcing its tip to a peer that
had already fetched it.
*Limits, stated plainly:* messages are typed values, not bytes (M8.2); there is no peer discovery, so nothing
reconnects a lost link (M8.3a); the simulator delivers in order and loses whole messages, which is not how TCP
fails; the timing and score numbers (ban at 100, ping after 60 s, five unanswered requests, 50 messages a second)
are proposals that nothing has yet tuned against real traffic; bandwidth and memory per connection are not
measured; and the engine has not been reviewed (M9).

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
**What this means for a live network** (decision 6): outputs made under the interim scheme stay valid and
spendable when Carrot arrives, and new wallets support both schemes. What users lose is not funds but privacy
properties the interim scheme lacks (Carrot's Janus protection among them) until they move coins to
Carrot-derived addresses.
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
   interface (M8.6). **Correction, 2026-09-30, after the owner said the network is meant to be a real one, not
   only a test network:** a live chain does not restart at Carrot's arrival. Carrot is mostly wallet-side (how
   one-time keys are derived, view tags, how amounts are encrypted); what consensus sees is only the 123-byte
   enote and the commitment of a public amount, and the Carrot specification's own text matches both of what we
   built (`C_a = G + a*H` for a coinbase output; the field sizes). So, if the interim scheme keeps every
   consensus-visible thing Carrot-shaped, adding Carrot is a **wallet update, not a hard fork**: new wallets
   derive and scan both ways, outputs made under the interim scheme stay spendable (spending needs only the
   output's secret key and mask), and no genesis change is needed. Two caveats: this rests on my reading of a
   specification that has changed before, and is checked only when Carrot's own test vectors are imported; and
   any consensus-visible difference found then would need an activation-height rules change
   (`CONSENSUS_V2.md` section 10).
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

## 7. Before this holds real value (the owner intends a real network, 2026-09-30)

CLAUDE.md still says: unaudited, one node, not for real value. **That stays true of everything built so far and I
will keep the labels until the gates below are met; changing the project's wording is the owner's decision.**
The reasons are concrete, not ceremonial:

- **A consensus bug on a live chain cannot be quietly fixed.** Two nodes that disagree about one rule split the
  chain, and the repair is a coordinated hard fork. Every rule so far has tests and independent vectors, but
  no one else has reviewed them.
- **The cryptography around the libraries is ours** and unaudited: the signed message, the proof layout, the
  checks on points and the balance equation. The libraries under them were audited in an earlier state.
- **Privacy is not delivered yet** (no Carrot, an interim scheme, a small anonymity set), and stating otherwise to
  users would be a false claim.
- **A young chain with little hash power can be rewritten by anyone with modest hardware.** Reorganisations are
  handled correctly, but they can be deep.

Proposed gates, in this order: M9 (fuzzing, a written threat model, a review plan); an independent review of the
validator, the store and `tenero-crypto` by someone other than me; a long public test network with real failures
observed and fixed; an emergency-fork plan written down before launch; and only then a network that could carry
value, with the risks stated to its users. None of this stops M8 or Carrot.
