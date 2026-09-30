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
3. **(Closed in M8.3.)** **The side-branch pool is in memory only** and orphans are dropped: fine for tests, poor for a network where
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

### M8.2 The wire protocol (size M) **DONE 2026-09-30** (`docs/WIRE_PROTOCOL.md`, `crates/tenero-net/src/wire.rs`)
One canonical framed format, reusing the version 2 codec (`CONSENSUS_V2.md` section 4): length-prefixed messages
with a hard maximum size, strict decoding, and a version and chain id in the handshake (a peer on another chain
is dropped at once). Proposed messages: `Hello` (protocol version, chain id, tip height and cumulative work),
`GetBlockIds` / `BlockIds`, `GetBlocks` / `Blocks`, `NewBlock` (id and work, then fetch), `NewTx` (id) with
`GetTx` / `Tx`, `Ping`. Pruned nodes serve only what they have and say so.
*Done when:* every message has a golden vector (made by our Python reference, like the other v2 vectors) and
strict-decoding tests for each malformed case.
*Result:* a frame is `length u32 | kind u8 | body`, fixed-width little-endian on the version 2 codec, with a cap on
every kind's length (16 MiB only for block and transaction lists), counts checked before any element is read, and
the checks in a documented order. An independent Python reference (`tools/make_vectors_wire.py`, standard library
only) makes `v2_wire.json`: 22 valid messages (every kind, empty lists, lists exactly at their caps, extreme
numbers), 40 malformed frames each with the error a decoder must give, and 8 stream prefixes that must fail (or
wait) without more bytes. The Rust codec reproduces all of it byte for byte on the first run. It also has a
streaming `FrameDecoder` (any chunking gives the same messages; an oversized or unknown header is refused after 5
bytes, before any body is buffered; after an error it stays failed), and the encoder refuses what a decoder would
refuse (including a frame of exactly 16 MiB accepted and one byte more refused). Properties over random input:
40,000 randomly damaged real frames never panic and never decode to a message that encodes differently (one
message, one encoding), in both languages. 35 deliberate faults injected into the codec, all caught (eight
survived first: missing one-byte-over-the-cap vectors for six kinds, and two that needed an exact 16 MiB test).
Testing the vectors themselves also exposed that "length over maximum" had no case; it does now.
*Limits:* nothing yet feeds the engine through bytes (M8.4 puts sockets and Noise underneath); the caps and the
16 MiB ceiling are proposals, not measured against real traffic; the codec is unreviewed (M9).

### M8.3a Peer discovery and connection management (size M; new since the first draft) **DONE 2026-09-30** (`crates/tenero-net/src/addrbook.rs`, the engine)
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
*Result:* two new messages (`get_addrs`, `addrs`) and a per-run `nonce` in `Hello`, all in the wire format, the
Python reference and the vectors. The address book keeps only routable `ip:port` addresses, bounds how much one
source network group may add (64 new entries), drops never-worked and most-failed entries first (never a seed or a
tried address first), backs off exponentially with a 30 s minimum gap between dials of one address, forgets an
address that never worked after 10 failures, samples a fresh answer for `get_addrs`, and saves and loads with a
checksum. Bans are by host (an inbound peer's port is arbitrary). The engine keeps **at least 8 outbound peers of its
own choosing and 50 peers in all** (both configurable), never dials a host it is connected to, at most 2 outbound per
/16, never itself, never a banned or backed-off address; answers `get_addrs` once per connection and punishes
unsolicited or oversized address messages; accepts one self-announcement (its own host) per connection; and **a full
node still hands out addresses**: up to 16 "visitors" over the limit may say hello, receive addresses and are sent
away, which is what stops a newcomer being locked out of a busy seed. Two links to one node (simultaneous dials, or
a connection to ourselves) are resolved by the nonce, keeping the link dialled by the smaller nonce, the same one at
both ends. The address book and ban list export and import (`Engine::export_state`/`import_state`), refusing any
damage. **Measured in simulation (not on a real network):** 120 nodes started with five seed addresses each reach 50
peers or more (minimum 50, maximum 72) within seconds of simulated time, every ordinary node having chosen at least
22 of them itself, with no honest peer banned and each block body still sent once per node; 30% of a 40-node network
vanishing is refilled and the returning nodes reconnect; a healed partition reconnects through the nodes' own
dialling; 300 fake addresses pushed by three attackers fill at most 64 entries each and the victim still reaches the
honest network; a crowded /16 gets two outbound slots. 93 deliberate faults injected into the address book and the
new engine logic, all caught (13 survived first, each exposing a missing test or one mutant of my own that was a
no-op, and all now have tests). Bugs the simulator and the tests found in my own design along the way: a
seed's 29 inbound peers blocked all its dialling (the host-dedupe rule counted inbound peers), a full seed locked
out newcomers (the visitor mechanism above), a peer that connected and dropped at once was redialled 3,700 times
(the minimum redial gap), and the simulator threw away a reply sent just before a close (fixed: a close now arrives
after what was sent, like TCP).
*Limits, stated plainly:* the addresses, latencies and failures are simulated; there are no DNS seeds (seeds are
configured addresses) and the node program does not yet write the book to disk (M8.4 and M8.7 will use
`export_state`); an eclipse attacker who controls a node's seeds, or many network groups, can still steer it (the
per-source cap and group diversity are mitigations, not guarantees, and no attack has been tried beyond the ones
above); `get_addrs` answers reveal the book to anyone who connects, limited only to one answer per connection;
two links dialled by the same side at once (which the dialling rules avoid creating) are not resolved
consistently and may both be dropped, after which the nodes redial; the timing and size numbers are untuned
defaults; and it is unreviewed (M9).

### M8.3 Sync (size L, the hard part) **DONE 2026-10** (`crates/tenero-net` sync, `tenero-chain` assume-valid and orphan pool, `tenero-node` pool file)
A new node learns the chain: ask a peer for its tip and work, fetch the block ids, then the blocks, validate and
add them in order through `Chain`. Out-of-order blocks and orphans are held (gap 3 closed by a bounded persisted
pool). **The cost is real:** the full proof of work is checked per block on the CPU (measured in
`docs/BENCHMARKS.md`: about 175 ms an attempt, 35 ms with `target-cpu=native`, plus 2.7 to 3.9 s once per epoch to
build the 4 GiB dataset), so syncing a long chain is slow without **assume-valid**: below a shipped checkpoint,
skip the signature and full-PoW checks and verify only the cheap ones. That is a trust decision (see 6), so it is
opt-in and visible.
*Done when:* a fresh node syncs a 10,000-block test chain from another, on the SHA-256 chain quickly and on the
real proof of work with the measured time reported, both an archive and a pruned node.
*Progress (M8.3, first slice):* the sync loop itself came with M8.1. **Done:** (1) a fresh node syncs a 10,000-block
SHA-256 test chain from an archive node in the simulator (**measured: about 23 s of real time** on the owner's machine,
release build, coinbase-only blocks, simulated network, so it says nothing about a real network or real proof of
work); (2) **pruned peers**: a peer whose `pruned_below` is past the next block we need is never chosen as a sync
peer, on any path (before this, an orphan block started a sync with it, which got `NotFound` and punished an
honest pruned node; found by the test, which needed real transactions in the old blocks because pruning a
coinbase-only block deletes nothing); a node that is only a few blocks behind still syncs from a pruned peer.
**Headers on the wire (done, second slice):** `get_headers` (kind 15) and `headers` (kind 16, 146 bytes per header, 500 at most) are in the wire format, the independent Python reference, the regenerated vectors (now about 510 KiB) and `docs/WIRE_PROTOCOL.md`; the engine serves them (a pruned node too, since it keeps every header) and punishes headers nobody asked for. 8 of 9 injected faults in that code are caught; the survivor (a `headers` reply should reset the unanswered-request count) only matters once the engine asks for headers, so its test comes with that slice. Protocol version stays 1: nothing is deployed and the format is a draft.
**Assume-valid (done, third slice; off by default, no checkpoint is shipped because there is no real chain yet):**
`EngineConfig::assume_valid = Some(AssumeValid { height, id })`. A node behind that height asks the peer it syncs
from for **headers** first (500 a reply). Every header must link to the one before, the first to a block of ours, and
the header at the checkpoint height must hash to the checkpoint id; only then are the ids on that path handed to the
validator, and **only for those blocks** the full proof of work and the transaction proofs are skipped. Everything
else still runs on them: the cheap proof-of-work check, the Merkle root, the coinbase amount, fees, ring shape and
membership, key-image uniqueness and the state update. Blocks past the checkpoint are checked in full, and the set is
dropped as soon as the tip reaches it. A peer whose chain has another block at that height, or whose headers do not
link or continue, gets 50 points (two such replies ban it); a peer that simply has a shorter chain, or no headers,
is not blamed and is synced from in full. Why headers first: the ids in `block_ids` cannot be shown to link, so a
dishonest peer could have had us apply blocks without proof-of-work checks that do not lead to the checkpoint, each
counting toward our cumulative work; the linked headers make every assumed block a real ancestor of the checkpoint.
Injected faults: 27 in the engine and validator, all caught (2 of my own mutants did not compile or were redundant,
and a guard that turned out to be redundant was removed). Tests: 4 chain-level (an assumed block skips exactly the
proofs and the full proof of work, and nothing else; a pool transaction is always proof-checked) and 17 sync-level
(real nodes at 120 and 1,100 blocks; scripted peers for every refusal).
*Limits, stated plainly:* **it is a trust decision**: if the shipped checkpoint is wrong, or a chain is built to
reach it, a node that turns this on believes it. The checkpoint's cumulative work is not checked. The headers are
not proof-of-work checked while they are fetched (they cannot be: the target needs the blocks before them), so a
peer can make a node fetch up to `checkpoint height - our height` headers (146 bytes each, 32 bytes each held) that
lead nowhere before it is caught at the checkpoint height. Nothing here measures what it saves with the real
proof of work: the saving is the attempt cost in `docs/BENCHMARKS.md` per block plus a dataset per epoch, unmeasured
end to end. It is unreviewed (M9).
**Out-of-order blocks and the pool file (done, fourth slice; closes gap 3):** a block whose parent is unknown is
held, **unvalidated** (it cannot be checked without its ancestors), in a bounded orphan pool (128 blocks, 32 MiB,
oldest dropped first); when its parent arrives it is applied, and so on down a run of them, and if that makes a
side branch heavier the reorganisation is announced at once. The engine no longer asks for a block it already
holds. The side and orphan pools can be saved (`Node::save_pool`: written aside, then renamed) and loaded
(`Node::load_pool`): the file is checksummed, refuses any malformation, and **every block in it is submitted again
like a block from a peer**, so the file is not trusted and the mempool follows what it causes. Fault injection
over this code: 40 faults, all caught in the end (a hang counts as caught: one mutant, which removes the purge of orphans
built on an invalid block, loops forever; the real code cannot). The injection found real holes in my tests that
are now filled (the pool file's checksum and size limits, an oversized orphan flushing the pool, loading not
reaching the mempool, a missing announcement). *Limits:* an orphan costs its sender nothing to make, so a flood
of them can push honest ones out (costing only a re-download); nothing writes the file yet, so a node program
must call `save_pool` (M8.7); orphans that were invalid are only found out when their parent arrives, and then
nobody is blamed for them (the sender is no longer known).
**The `get_addrs` answer (done):** an answer is at most 23 percent of the address book (and never over 100), but a
book of 20 addresses or fewer may be given out whole, because a young network needs newcomers to learn enough
addresses to start; and a network group (an IPv4 /16) that asks again within 24 hours gets the **same** answer, so
reconnecting does not draw a fresh sample each time. Up to 1,024 groups' answers are remembered. 14 injected
faults, all caught (one redundant guard found that way was removed). This stops one
host reading the whole book by reconnecting; it does **not** stop an attacker with many addresses in many groups
(each gets its own sample), nor one who connects for a long time, and the remembered answers are lost on restart.
What Monero adds beyond this, as far as I know it (separate white and gray lists, transport encryption,
Dandelion++ for transactions, Tor and I2P): white and gray are the tried and new entries of the address book here;
encryption is M8.4; Dandelion++ and Tor/I2P are later work, not started.
**Real-proof-of-work sync time (DONE, measured on the owner's machine):**
`crates/tenero-chain/tests/real_pow_sync.rs`, an `#[ignore]`d test that mines a short chain with the real matmulhash
on the CPU at an easy target, then feeds it to fresh nodes, once checked in full and once with assume-valid (all
but the last 5 blocks), timing every `submit_block`. **Measured (30 blocks, generic x86-64 build):** a full check
costs 179 ms a block plus about 2.6 s for each epoch's dataset (8.0 s for the chain with one dataset, 13.6 s with
three); an assumed block costs about 2 ms; assume-valid took the run from 8.0 s to 3.7 s (the last 5 blocks and one
dataset build being most of what is left). Full tables and the straight-line estimates for a year of blocks
(about 30 hours in full against about 18 minutes assumed, **estimates**) are in `docs/BENCHMARKS.md`. Proof-of-work
cost only: no network, no transaction proofs, coinbase-only blocks, no `target-cpu=native`.
**M8.3 is done** apart from what its notes list as limits; the next step is M8.4 (real sockets and Noise).

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
