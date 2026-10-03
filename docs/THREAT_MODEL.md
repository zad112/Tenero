# Threat model (M9, first version, 2026-10-02)

**Status of this document.** Written by the author of the code, from the code, its tests and its design notes. It is a
list of what can go wrong and what is done about it, **not a security audit, and not a claim that the software is
safe.** An author is the worst person to find their own blind spots; the independent review that `docs/M8_PLAN.md` section 7
requires is still ahead and will change this document. Tenero remains **an experiment: unaudited, no launched network,
not for real value.** Where a statement below is "from the docs" and I did not re-trace the code today, it says
**to verify**.

Every threat has a status:

* **Mitigated** — a defence exists and a test exercises it (the test is named).
* **Partly** — a defence exists but has a stated gap.
* **Open** — nothing defends against it yet.
* **Accepted** — known, and left for now with a reason.
* **Out of scope** — not something this project can defend.

## 1. What is being protected

| Asset | Why it matters |
|---|---|
| **Chain integrity**: the rules of `CONSENSUS_V2.md` are enforced identically by every node | One bug that accepts an invalid block (inflation, a double spend) or two nodes that disagree about a rule splits or breaks the chain; on a real network this would need a coordinated hard fork. |
| **Node availability**: a node stays up, in sync, and responsive | Nodes are what testers run; a remote crash or a memory blow-up is the most likely first attack. |
| **Wallet secrets**: the seed, the passphrase, the spend and view keys | Loss of the seed is loss of coins (on a test network, of nothing of value; the habits learned here must still be right). |
| **User privacy** | The point of the design. **Only partly delivered** (section 4, N). |
| **The operator's machine**: files, the GPU, the network position | A node and a miner are programs that talk to strangers and run a lot of native code. |
| **Honesty of the project's claims** | Calling a test network "private", "secure" or "money" would harm testers. |

## 2. Who might attack, and from where (trust boundaries)

1. **An anonymous peer on the internet** (the main adversary): can connect, send any bytes, be slow, lie, flood, or run many nodes.
2. **A passive network observer** (an ISP, a Wi-Fi owner): sees traffic, cannot alter it undetected (Noise).
3. **An active network attacker** (a man in the middle): can alter, drop, delay, or sit in the path.
4. **A miner with a large share of the hash power**: can reorganise, withhold, or manipulate timestamps.
5. **A local program or user on the same machine** as the node or wallet: can read files the operator can, and connect to loopback.
6. **A hostile file**: a damaged or crafted data directory, wallet file, peers file or pool file.
7. **A dependency or the build/release chain** (supply chain).
8. **A tester making a mistake**, or being tricked (fake downloads, "send me your seed").
9. **The operator of a seed node**, and the person who chooses the seed list.

**Out of scope for the program itself:** malware on the user's machine (a keylogger sees the passphrase), physical access to an unlocked
machine, a compromised operating system or GPU driver, and attacks on the hardware.

## 3. What is defended, in one place

These come from the tests; none of it is audited.

* **Decoding.** Every message is decoded strictly (wrong length, trailing bytes, out-of-range counts, non-canonical forms refused) and each
  of the three decoders has golden vectors from an independent Python implementation, hundreds of hand-made invalid cases, and (new,
  2026-10-02) proptest properties: random bytes and edited valid objects never panic, hang or allocate wildly, and what decodes
  re-encodes to the same bytes (`fuzz_decode.rs`, `fuzz_wire.rs`, `fuzz_control.rs`; 5 injected faults, all caught).
* **Validation.** Blocks pass every rule in `CONSENSUS_V2.md` section 8 (each tested by breaking exactly it); transaction proofs (CLSAG, Bulletproofs+) are
  verified by default (`Node::new`; a node refuses to start without them; a real `tenerod` refused seven tampered copies of a real transaction).
* **Fork choice** by cumulative work, with reorganisation that restores the old state exactly on failure (16 tests, 18 injected faults).
* **Peers.** A message-rate limit with burst; scores and bans by host; a handshake deadline; limits on pending handshakes (total and per host);
  a bounded write queue in bytes; self-connection and duplicate-connection detection; address-book limits (per source group, per /16);
  one `get_addrs` answer per connection and a repeat-proof sample; a full node still helps newcomers; assume-valid is off unless chosen.
* **Transport.** Noise XX (`snow`, unaudited) with the chain id and version in the prologue: confidentiality and integrity of the stream.
* **Sync.** "Slow is not hostile": late replies are forgiven once (the first day-long run split because this was missing); sync peers
  are chosen by work; orphans and side branches are held in bounded pools; the pool file is checksummed and every block in it is re-validated.
* **Wallet.** Argon2id plus ChaCha20-Poly1305 for the file (every byte is protected and a hostile file cannot make it allocate gigabytes, tested);
  the transaction is verified by the wallet before it is sent; a node that serves wrong blocks is not believed (`pay.rs` test).
* **Control interface.** Loopback only, a random cookie per start that must be the first message, a bounded queue, connection limits and timeouts.

## 4. Threats by area

### A. Parsing what a stranger sends

| # | Threat | Defence and test | Status |
|---|---|---|---|
| A1 | A malformed message crashes or hangs a node | strict decoders; vectors; proptest; the 60 invalid wire cases | **Mitigated** for the decoders; **Partly** overall (coverage-guided fuzzing and the engine's handlers are not fuzzed; cargo-fuzz is planned) |
| A2 | A length or count field makes a decoder allocate gigabytes | counts are checked against limits before allocating; `a_huge_declared_length...` property; hostile wallet file test | **Mitigated** for the decoders (**to verify** for the store's segment-file readers: they read our own files, but a damaged file is a hostile file) |
| A3 | A valid message sent at the wrong time or in a hostile order (before `hello`, twice, unsolicited) | `message before hello` 50 points, `second hello` 50, unsolicited address messages punished; scripted-peer tests; **and (2026-10-02) `tests/fuzz_engine.rs`**: random plausible peers (connect, disconnect, hello, every message kind with real or made-up contents, honest answers with one flaw, whole syncs, clock jumps) in three engine settings, checking after every event that the engine sends only encodable messages to connected peers, dials only `ip:port`, respects its peer limits, holds no peer at the ban score, never lowers the chain's work, keeps its state small, and (run twice) is deterministic; plus a damaged `peers.dat` never breaks loading. 150 + 60 + 120 cases; the harness is checked for reach (over 240 cases about half applied blocks, every message kind was exchanged, 149 had a ban) | **Mitigated** in the sense that 330 random sequences found no violation, and 8 injected faults (a panic, no ban, no limit, a past ban time, a clock read, ...) were all caught; **Partly** in the sense that it is random, not coverage-guided (cargo-fuzz is planned), the chain is 14 blocks (so a cap of 500 ids is never reached), the proof checks are off in it, and it does not model many peers at once |
| A4 | **A reply that cannot be sent.** Found by reading the engine while fuzzing it: a request for blocks that together passed the 16 MiB frame (32 blocks of over 512 KiB each) failed to encode, the transport only logged it, and the requester never got an answer: a node could not serve its own chain to a new node | **Fixed 2026-10-02:** replies are split into as many `blocks` messages as it takes (`split_blocks`, 8 MiB each); a block too big for any frame is answered `not_found`. 5 tests (the old failure shown, packing at the exact boundaries, order, a block over the ceiling, and a sync from a node that answers one block per message); 4 injected faults caught | **Mitigated**; but see E10 |

### B. Making a node run out of something (denial of service)

| # | Threat | Defence | Status |
|---|---|---|---|
| B1 | **Memory: frames queued behind a busy loop.** Before 2026-10-02 the channel into the node's loop was bounded in *messages* (1,024), and one message can be 16 MiB: in principle gigabytes queued while the loop checked a block (**found by reading the transport, not measured as a crash**) | `budget.rs`: a byte budget with two pools (small frames up to 128 KiB, large ones up to 16 MiB), the whole declared size of a frame reserved *before* the rest of it is read, a ticket carried by the message and given back when the loop is done, per-connection ceilings (one 16 MiB frame at a time; 2 MiB of small ones), and the pools separate so held-open large frames cannot stop pings and announcements. Tests: 18 in `tests/budget.rs` (accounts balance under random operations and 12 threads; a waiting reader is let go when its connection closes), and a real-socket test where the loop is stalled and a client tries to send 400 MiB: **the node took 16 MiB, the peak held was 14 MiB** and the reader waited (`transport.rs`, `a_flood_of_large_frames...`) | **Mitigated** for the queue; **Partly** overall: real memory can be up to about twice the pools while a frame is being decoded; a peer that declares a large frame and stalls holds a large reservation until the engine's ping timeout (about 90 s), and 4 such peers can delay other peers' large frames (not small ones); the closed-connection wake-up is tested on the gate, not end to end |
| B2 | **No byte-rate limit per peer** | a per-connection token bucket (default 8 MiB/s sustained, 32 MiB burst; `peer_bytes_per_sec`, `peer_burst_bytes`; 0 turns it off): a connection over its rate is read more slowly and TCP slows the sender; nobody is punished, since a peer serving a sync legitimately sends a lot. Tests: 6 for the bucket's arithmetic (burst, refill, cap, zero), and a real-socket test (1 MiB at 256 KiB/s took 3.7 s, no bad bytes, no ban) | **Mitigated** against one fast peer; **Open** against many peers each at their own rate (the total is not limited; 50 peers at 8 MiB/s is 400 MiB/s, more than a home connection carries anyway) and the CPU a peer can cause with small messages inside the rate (the message limit of 50/s with a burst of 200 covers that) |
| B3 | **Threads**: about two per peer | peer caps (`max_peers`, `max_inbound`, `max_addr_only`) | **Measured 2026-10-02** (`tenero-app/tests/hostile_load.rs`, a real `tenerod` process, 152 s, one run, one source address): idle, 7.3 MiB working set and 6 to 7 threads. **64 inbound peers held open (92 sessions completed the handshake, 64 stay): 16.8 MiB working set, 15.2 MiB private, 167 threads at the peak** (about 2 threads and 0.15 MiB a peer, which fits "about two per peer"). The node answered a control request in 12 to 26 ms in every stage, and 12 s after each stage was back to 4 threads and 0.1 to 1.4 MiB above idle. **Accepted** at the caps (64 inbound peers is about 130 threads: fine for a computer, and the cap is what bounds it). *Limits:* one machine and one address over loopback, an empty chain, no blocks sent: the cost of the proof-of-work check is not in these numbers |
| B4 | **Handshake CPU**: Curve25519 work an attacker can ask for repeatedly | deadline, pending caps (total and per host), bans refused before cryptography | **Partly**: a botnet of many addresses defeats per-host limits. **Measured 2026-10-02 from one address:** 400 connections that say nothing, and 400 that send 1 KiB of junk, moved a real node by 0.1 to 0.3 MiB and 0 to 4 threads (at most 4 handshakes run at once from one host, and a connection that does not finish is closed at the deadline); of 200 clients that tried to complete a handshake, 92 got through in 14 s; **a flood of valid pings was cut off after about 8,000 messages from 18 sessions and the address banned (the other 82 could not connect), and sealed junk after 3.8 MiB from 11 of 50 sessions**: memory did not move (7.9 to 8.0 MiB against 7.3 idle). *Not measured:* many addresses at once (the thing the per-host limit does not stop; it needs more than one machine or source address), file-descriptor limits with thousands of sockets, and Linux |
| B5 | **Proof-of-work cost**: each unknown block costs the CPU check (about 0.2 s for the real PoW) and a dataset of 4.3 GiB per epoch (about 2.6 s to build) | cheap pre-check first (**to verify** the exact order in `Validator`), lookahead of 10 blocks, assume-valid (opt-in) | **Partly**: a stranger cannot make a node build a dataset for a far epoch without a block that passes the cheap check at the claimed height (**to verify**); the first block of each epoch stalls validation about 3 s |
| B6 | **Orphan and side-branch pools** filled with junk that costs the sender nothing | bounded (128 orphans, 32 MiB; oldest dropped); side pool bounded | **Accepted**: honest orphans can be pushed out (a re-download) |
| B7 | **Headers lead nowhere** until the checkpoint height (assume-valid) | opt-in, caught at the checkpoint | **Accepted** (bounded by the height gap) |
| B8 | **Mempool flooding** | 32 MiB cap, lowest fee rate evicted only for a strictly higher rate, proofs verified before admission (**the proof check is itself CPU an attacker can request**: a transaction needs to be well-formed before it is checked) | **Partly**: no per-peer transaction rate beyond the message limit; **to verify** that cheap structural checks precede proof verification |
| B9 | **Disk**: an archive node grows with the chain; logs have no rotation | pruning (`prune_keep`) | **Open** for logs (M10.4); **Accepted** for the chain on a young network |
| B10 | **Control interface flooding** by a local program | 8 connections, queue 64, timeouts | **Mitigated** against accidents; a malicious same-user process can do worse (see H) |

### C. Who the peers are (eclipse, Sybil, address poisoning)

| # | Threat | Defence | Status |
|---|---|---|---|
| C1 | **Eclipse**: all of a node's peers are the attacker's, who then shows it a false chain | at least 8 outbound peers chosen by the node, at most 2 outbound per /16, an address book with per-source limits, seeds that are never forgotten; **and (2026-10-02) anchor peers: a node saves up to 2 of its long-standing (10 minutes or more) outbound peers, from different network groups, with its state, and dials them first when it restarts, before the address book or the seeds** (`anchors.rs`, `engine.rs`). **Stale-tip detection (added 2026-10-02):** if the tip has not moved for 10 minutes the node dials 2 extra outbound peers from network groups it has no outbound peer in, again every 5 minutes while stale, never past `max_peers`, and logs the start and end of the episode; and **pinned peers** (`trusted_peer`): always dialled, exempt from the group limit, never put in the address book or passed on, still validated and still banned if they misbehave (`stale_and_pinned.rs`, 10 tests; the config and a real two-process test; 17 injected faults, all caught after one test was strengthened). Anchor tests: 10 engine-level (which peers are chosen, the order, dialled first and once, skipped when banned or already connected, the old state format still loads, damage refused, the cap), the key contrast (with an attacker's address book and seeds, a restart without anchors dials only the attacker; with anchors it dials the honest peers first), and a real-socket test that a node saves its long-lived peer when it stops; 18 injected faults, all caught | **Partly**: this closes the restart route (an attacker who can make a node restart, or who waits for one, no longer gets to pick all its peers), but **a node that was already eclipsed when it saved has anchors that belong to the attacker**, an attacker who controls the seeds or many network groups can still steer a node that has never run, and a young network with few honest nodes is especially exposed. The eclipse a node cannot see from inside still cannot be *proved* absent: stale-tip detection catches an attacker who withholds blocks, not one who feeds a valid chain slowly. **Bootstrap rules for seeds (added 2026-10-02, `SEED_POLICY.md`):** a first-start node dials only its seeds until every seed network group has answered (or 20 s pass). A simulation of the real engine (`eclipse_sim.rs`) found that **before this, one fast hostile seed took 100% of a new node's first dials**, and that with the wait the attacker's share is about his share of the seed list (27% for 1 of 4 seeds, 51% for 3 of 6), whatever his speed; a quota of seeds does not work, and preferring addresses told by two sources helps the attacker, so it is off. **It does not help when most of the seed list is hostile.** (A per-seed fair-share limit was built and removed: no measured gain, and a measured cost.) **Network health (added 2026-10-02, `RUNNING.md`):** the node logs how many network groups its outbound peers are in and how long since the last block, and raises five alarms (stale tip; a peer reporting more work than the node has for 5 minutes; two *feeler* samples reporting the same; too few outbound peers; too few network groups), each logged once when it begins and once when it ends. A *feeler* is one connection every 2 minutes to an address never connected to, to read its tip and work and hang up; it holds no peer slot. **These show an eclipse; they do not prove or prevent one**, and a node cut off by an attacker who feeds it a valid chain slowly sees nothing wrong. **Seed health-check tool (added 2026-10-02, `tenero-seedcheck`, `RUNNING.md`):** checks each seed and the list (see `SEED_POLICY.md`); it cannot tell whether a seed is honest. Still to do: several independent seed operators in different network groups (planned, M11.4) |
| C2 | **Sybil**: many cheap identities | no identity at all by design (bans are by host); outbound diversity | **Accepted** — nothing prevents one host from running many nodes on many addresses |
| C3 | **Address poisoning** with fake or unreachable addresses | per-source cap (64 entries), never-worked entries dropped first, routable addresses only, 10 failures forget an entry | **Mitigated** in simulation (300 fake addresses, victim still reached the honest network); **not tried on a real network** |
| C4 | **Reading the whole address book** (a map of the network) | 23 percent per answer, the same answer within 24 h per group | **Partly**: many groups each get a sample; the cache is lost on restart |
| C5 | **Ban evasion** by changing address, or **collateral bans** (a shared IP) | bans by host | **Accepted**: a NAT with many users shares a ban; an attacker with many addresses is not slowed |
| C6 | **Self-connection and duplicate links** | `nonce` in `Hello`, the smaller-nonce link is kept | **Mitigated**; a rare same-side double dial may drop both links (documented) |

### D. The channel

| # | Threat | Defence | Status |
|---|---|---|---|
| D1 | A passive observer reads the traffic | Noise XX, ChaCha20-Poly1305 | **Mitigated** (library unaudited) |
| D2 | A man in the middle | encryption alone does not authenticate peers: nothing pins a key to a node, so a MITM running both handshakes is **not detected** | **Accepted for peer-to-peer** (a public network has no way to know who an honest peer is); **Open** for a wallet using a *remote* node over the network (not supported yet) and for any future seed-node authentication |
| D3 | Replay, reorder, or drop within a stream | counters per chunk | **Mitigated** |
| D4 | Traffic analysis; who talks to whom; which node first announced a transaction | none | **Out of scope for now** (P4: Dandelion++, Tor, I2P are not built) |
| D5 | A node on another chain or version | the prologue fails the handshake; a version and network id in the handshake planned (M10.4) | **Mitigated** (chain id and version); the human-readable refusal message is M10 |

### E. The chain as a system (consensus-level attacks)

| # | Threat | Defence | Status |
|---|---|---|---|
| E1 | **51 percent / majority hash power**: rewrite history, double-spend | none possible in software; cumulative-work fork choice makes the heaviest chain win | **Accepted and certain on a young chain.** On the first test network *one miner can be the whole network*, including the owner; any tester with a GPU can out-mine others. This must be said in every tester document. |
| E2 | **Deep reorganisations** | handled correctly and tested; no reorg limit | **Accepted**; the wallet remembers 100 block ids and rescans beyond that (tested) |
| E3 | **Timestamp manipulation against the difficulty adjustment** (LWMA window 30, 6x clamp, median of 11) | standard ingredients, a future-time limit (held, not rejected, so honest disagreement is not permanent) | **Analysed 2026-10-02 (`tenero-core/tests/difficulty_sim.rs`, a simulation of the real rules): the current rules are weak, and a different timestamp rule fixes it. A decision for the owner, not made.** A miner with a **minority** of the hash rate that chooses its own timestamps pulls the difficulty down: backdating its blocks to the median, 10 % of the hash rate gives **0.69x** the honest difficulty, 30 % **0.40x** (blocks every 25 s instead of 60), 45 % **0.29x**; stamping them 120 s ahead, 10 % gives 0.88x and 30 % 0.70x; alternating, 45 % gives 0.27x. Cause: a solve time is floored at 1 s, so a backdated block looks fast and the next honest block, measured from the backdated stamp, looks slow: a pair adds more to the window than the real time. **Effect:** blocks and so coins come faster than aimed for, and each block is less work. **Fix measured:** require a block's timestamp to be *later than its parent's* (instead of not below the median of 11): the same attackers then leave the difficulty at **1.00x** (forward, any share), 1.00x to 1.06x (backward, up to 45 %), 1.20x only at 60 % (slower, bounded, and a 60 % miner controls the chain anyway). **Two other fixes were tried and are WORSE or only partly better:** allowing negative solve times clamped at minus the future limit still gives 0.67x at 30 % (backward); clamped at minus six block times it lets the attacker push the difficulty UP without limit (2.6x at 30 %, no blocks at all at 60 %). **Not a code change yet:** the timestamp rule is consensus (rule 1): the reference, `CONSENSUS.md` section 7, `CONSENSUS_V2.md` 5.5, the validator and the vectors all change together, for the fresh chain (M11.2). *Caveats of the simulation:* no network delay or orphans, perfect clocks, fixed hash-rate shares, exponential block finding; it shows what the algorithm does with these inputs, not what a real network will see. `KNOWN_ISSUES.md` item 12 |
| E4 | **Difficulty swings** when miners join and leave (a few GPUs, then none): the chain can stall or race | adjusts every block over 30 blocks | **Simulated 2026-10-02 (same file); the starting difficulty is still to be chosen at M11.2.** Steady and changing hash rate (20 runs each, the real LWMA): the mean gap stays at 60 s for a hash rate of x2, x10, x100 and /2 (mean 59.6 to 60.9 s) but **a loss of hash rate is slow to recover from**: down to a tenth, the median is 18 blocks and 1.3 h (worst 2.2 h) until the mean gap is back within 25 % of a minute, with single gaps of up to 34 min; **down to a hundredth, 29 blocks, a median of 9.1 h (worst 16.6 h) and single gaps of up to 5.7 h** (the target can move at most 4x a block and a solve time counts as at most 6 block times). **From the start:** a chain started 1000x too easy mines its first 100 blocks at a mean gap of 34 s (the first blocks are nearly free, then it settles); started 100x too hard, the first 100 blocks have a mean gap of 228 s and a first gap of up to 7 h in the worst of 20 runs. Neither start leaves a mark after block 100 (mean 60.5 s). **Meaning for the first test network:** a handful of miners, one of whom leaves, can stall the chain for hours; a starting difficulty set for the owner's one card is slow for the first tester with a weaker one (and for a CPU) and fast for one with a stronger. **Open options (not built):** a minimum-difficulty or emergency rule for a collapsed hash rate, a smaller solve-time cap, or accepting it for a test network and saying so. *Same caveats as above* |
| E5 | **Selfish mining and block withholding** | none beyond proof of work | **Accepted** (research-level on any PoW chain; stronger on a tiny network) |
| E6 | **Inflation or double-spend bug in the validator** | independent Python vectors, tests that break each rule, key-image uniqueness, balance equation, reviewed upstream proof libraries | **Partly**: this is the highest-consequence risk and exactly what an independent review is for; the signed message and the balance check are *ours* (not the libraries') and unaudited |
| E7 | **Two implementations disagreeing**, causing a split | the Python reference and vectors | **Partly**: there is only one node implementation; the vectors make a second one possible |
| E8 | **A consensus change forced by an exploit on a live chain** | none yet | **Open**: the emergency-fork plan is a launch gate (`M8_PLAN.md` section 7) and is not written |
| E10 | **No absolute maximum block size.** The consensus rule was "at most twice the recent median", and the median can grow (about double every 5 blocks for a miner producing half the blocks), but a frame on the wire is at most 16 MiB: a block over that could be mined but **never relayed**, and one would stop every new node from syncing. Found with A4 | **Fixed 2026-10-02 (decided by the owner): a block may carry at most `min(2 * median, 4 MiB)` of transactions** (`CONSENSUS_V2.md` 8.4; vectors in `v2_fees.json`; the validator, the mempool and the block template use it; a compile-time check that a largest block fits a reply and a reply fits a frame). Tests: the vectors in both languages, the validator at exactly the ceiling and one byte over with twice the median above it, the template, a largest block sent in one reply | **Mitigated**. Still open: how fast the median can rise toward the ceiling (a longer window or a growth limit are options), and the ceiling is a guess about demand (a change after launch is a hard fork) |
| E9 | **Premine or hidden allocation** | the genesis has no coinbase output (a test), and the new genesis keeps that (M11.2); every coin is mined | **Mitigated by construction**; says nothing about safety or value |

### F. Cryptography

| # | Threat | Defence | Status |
|---|---|---|---|
| F1 | A flaw in a library (`monero-oxide` CLSAG/Bulletproofs+, `curve25519-dalek`, `snow`, `argon2`, `chacha20poly1305`, `sha2`) | audited upstream state for the proof libraries (the audit's chapters were read; one medium finding is covered by our signed message); `snow` has had **no formal audit** (an owner-approved exception) | **Partly / Accepted**; the git pin of `monero-oxide` (a commit, not a crates.io release) means updates are manual |
| F2 | A flaw in *our* use: the signed message, proof layout, point checks, composition | tests with real proofs and 19 injected faults (18 caught; one redundant guard kept) | **Partly**: unreviewed |
| F3 | **The interim output scheme**: no Janus protection (the anchor in an output is not verified by the receiver; with one address per wallet this limits what it exposes, but it is not the protection Carrot gives) and a key derivation of our own composition | labelled everywhere; one address per wallet; replaced by Carrot later | **Accepted**: *not private in Monero's sense*; funds on it are not Carrot-safe |
| F4 | **The proof of work's memory-hardness is simulated, not proven**, and the fill/fold construction has had no cryptanalysis (`KNOWN_ISSUES` 11) | bit-exact vectors; measured on one GPU | **Open / Accepted**: a shortcut (a faster miner) would centralise mining, not break funds; a PoW *collision or preimage* weakness would be worse |
| F5 | **Randomness**: a bad RNG weakens keys and proofs | `OsRng` (the operating system's) | **Mitigated** |
| F6 | Side channels (timing, memory) in key handling | the libraries' constant-time code; `zeroize` on secrets | **Open**: not examined for our code paths |

### G. Local files and the control interface

| # | Threat | Defence | Status |
|---|---|---|---|
| G1 | A local process connects to the control port | loopback only; the random cookie is required first; **and (2026-10-02) the data directory that holds the cookie is checked: a new one is made private, an existing one that other accounts can read makes the node refuse to start** (`private_dir.rs`; 24 tests including real directories on the owner's machine; 11 injected faults, all caught) | **Partly**: against *other accounts* on the computer this is now **Mitigated** (measured on the owner's Windows: a folder under the user profile is private by default, but one directly under `C:\` is readable by every user and writable by every signed-in user, which would have let any local account read the cookie or replace the node's files); against **any program running as the same user, or as an Administrator or root, nothing here helps** (it can read the cookie and drive the node; it still cannot read the wallet's keys through the node). The Unix side (`chmod 700` and a mode check) is written and unit-tested for its mode arithmetic but **has not been run on Linux**. The HTTP-from-a-web-page case is G2 |
| G2 | A web page reaching the control port (DNS rebinding / cross-site requests) | a raw binary protocol on a TCP port, not HTTP; the first message must carry the cookie; and **every HTTP method, read as the 4-byte length that starts a frame, is over 540 MB, far past the 16 MiB limit**, so a browser's request is refused on its first four bytes, with no reply of any kind, and the port is not held open. Test (2026-10-02): nine methods, a POST with a hostile frame body, a WebSocket upgrade and a bare newline, each closed at once with zero bytes sent back, no request reaching the node, the node not stopped, and a client with the cookie served afterwards; 3 injected faults caught. **Also tried with real clients on the running run 3 node: PowerShell's `Invoke-WebRequest` (GET and POST) and `curl` (exit 56, nothing received); the node's status afterwards was unchanged.** | **Mitigated** (nothing is answered to HTTP, and a page cannot know the cookie). Not covered: a page cannot reach the port at all in browsers that block access to loopback from public sites; that is the browser's rule, not ours, and we do not rely on it |
| G3 | Tampered `chain.redb`, segment files, `peers.dat`, `pool.dat`, `node.key` | `pool.dat` and `peers.dat` are checksummed and re-validated; the store is crash-safe and tested against damaged files; a chain from another network is refused | **Partly**: **the node trusts its own chain database**; someone who can edit it can feed the node an invalid chain. Accepted: whoever can write the data directory owns the node. |
| G4 | Two nodes on one data directory | an exclusive open (`only_one_node_may_use_a_data_directory`) | **Mitigated** |
| G5 | `node.key` and the data directory readable by others | the same check covers everything inside the data directory by inheritance (new files take the directory's permissions); `node.key`, `chain.redb`, `peers.dat`, `pool.dat` and the cookie live there | **Mitigated** for new and checked directories; **Accepted** gaps: a file copied into an already-private directory with its own open permissions is not noticed; a wallet file kept elsewhere is not covered (it is encrypted, but the location is the user's choice) |
| G6 | The log leaking secrets | logs carry addresses, ids and IPs, never seeds or keys. **Two tests (2026-10-02):** `tenero-app/tests/log_secrets.rs` reads every call that writes a line or prints in the ten crates and refuses one whose arguments name a secret (cookie, passphrase, node key, spend or view key, mnemonic, master, wallet seed; the scanner is itself tested to fail); `daemon.rs`, `no_secret_reaches_the_log_at_the_most_verbose_level`, runs two real nodes at debug level through a wallet, a payment, a wrong cookie, an HTTP request and junk on both ports and searches both logs for the cookies, the node keys, both wallets' seeds and the passphrase (whole, in upper case, and the first 16 digits). 4 faults injected (the cookie logged by name, under an innocent name, its first 16 digits, the node key under an innocent name): all caught | **Mitigated** for what these two see. **Limits:** at "debug" the code writes few lines beyond "info" (there are almost no debug lines), so the end-to-end check is as strong as the lines that exist; the scan looks at names, so a secret passed under an unrelated name is for the end-to-end run to find; `tenero-wallet` prints the seed once on purpose (to the terminal, never to a log) |

### H. The wallet

| # | Threat | Defence | Status |
|---|---|---|---|
| H1 | Offline guessing of the passphrase from the file | Argon2id (default parameters not the test ones, tested), authenticated encryption, every byte protected | **Mitigated**; strength depends on the passphrase |
| H2 | `--passphrase-file` leaves a passphrase on disk | documented as weaker | **Accepted** (for automation and tests) |
| H3 | The seed shown once on the terminal stays in scrollback and logs | a banner; shown once | **Accepted** now; the GUI (M10.3) must show it with care and ask for confirmation |
| H4 | Secrets staying in memory, swap or hibernation files | `zeroize` for the secret types | **Open**: no memory locking; not examined |
| H5 | A **lying node** feeds the wallet false chain data | the wallet checks what it can (the chain id, block links, its own proofs are self-verified before sending); a node that sends wrong blocks is not believed (test) | **Partly**: against a *remote* node a wallet cannot know a chain is the real one (no proof of work check in the wallet); today the node is local |
| H6 | The node a wallet uses learns what the wallet cares about | the wallet scans whole blocks (no per-address queries); **it asks for specific outputs when it builds rings, and the real output is among them** | **Accepted for a local node**; **Open** before any remote-node use |
| H7 | A fingerprintable transaction: coin selection that always spends the same way, decoys from a log-uniform guess, a fixed ring size | randomised decoys; the docs say the selection is not randomised | **Accepted**: this is the weakest part of the privacy design (section N) |
| H8 | Spending the same coins twice from two copies of a wallet | reservations in the wallet file; the key image makes the second spend fail on chain | **Mitigated** against self-conflict; **Accepted** across two copies |
| H9 | An address typo or a malicious address substitution (clipboard malware) | a checksum on the text address | **Partly**: catches typos, not substitution |
| H10 | Fee set too low, a stuck payment; or too high | the minimum is dynamic; the wallet pays 25 percent over; a stale reservation expires in 20 blocks | **Mitigated** |

### I. The miner

| # | Threat | Defence | Status |
|---|---|---|---|
| I1 | A wrong block template wastes work or pays the wrong address | the template is for the next height; the coinbase's key exchange binds the height and the miner discards a mismatch; the node validates every submitted block fully | **Mitigated** (tests with a fake node: wrong height, wrong target) |
| I2 | A malicious node feeding a remote miner bad work | the miner trusts its node's templates; loopback only today | **Accepted** (a remote miner is not supported yet) |
| I3 | GPU driver or CUDA bugs | `unsafe` is confined to one crate (`tenero-gpu`); every GPU result is checked by the CPU node | **Mitigated** for correctness; availability is the driver's |
| I4 | Mining overheating a machine | `--pace`, batch size; no thermal control | **Accepted**; M10.2 reports temperature and power |

### J. Supply chain and release

| # | Threat | Defence | Status |
|---|---|---|---|
| J1 | A malicious or compromised dependency | a small, reviewed set; licences recorded; the lock file committed; owner approval per dependency (rule 3) | **Partly**: no `cargo audit` or `cargo deny` yet; the `monero-oxide` pin is a git commit; `snow` is unaudited |
| J2 | A tampered download | none yet | **Open**: M11.3 plans checksums, tagged builds, a recorded build; **no code signing** unless a certificate is bought, so Windows will warn and antivirus may flag it (this already happened to a launch of the miner on the owner's machine) |
| J3 | A fake "Tenero" site, a scam coin, an impersonating download | none possible in software | **Open**: the README and every release say what is official; the experimental labels are the defence against anyone treating the test coins as money |
| J4 | The build machine or the CI secrets compromised | none yet | **Open** (to be addressed when CI is set up: minimal permissions, no secrets in forks, pinned actions) |
| J5 | A tester runs an old, vulnerable build | the handshake version and network id (M10.4); announcements | **Partly** |

### K. Operating a seed node (planned, M11.4)

A seed is an internet-facing node that strangers connect to. It makes B1, B2, B4, C1 and C4 real. **It should not run on the owner's own
computer**, should run with a firewall that allows only the p2p port (never the control port), as an unprivileged user, with a
restart policy, and **there should be several, run by different people** (Monero's way). A hard-coded seed list is itself a trust
point: whoever changes it in a release controls where new nodes start.

### L. Privacy (what is *not* private)

This section exists so that nobody, including the author, overstates the design. **A user of this software today has none of the
privacy of Monero**, for these reasons:

* **The output scheme is an interim one**, with no Janus protection and a different (unreviewed) key derivation; it is not Carrot.
* **The anonymity set is a ring of 16 and the chain is tiny**: with few outputs on a young chain, decoys are easy to rule out by age and amount patterns.
* **Network privacy is absent**: a transaction is relayed from where it was made; a node can see the IP that first sent it (no Dandelion++, no Tor, no I2P).
* **Timing and fingerprinting**: coin selection, ring construction and fees follow patterns an observer could learn (H7).
* **A node knows its wallet**: the wallet and node today trust each other on one machine; a remote node would learn the wallet's real outputs (H6).
* **Logs and the diagnostics bundle** can contain addresses and IPs unless scrubbed (M10.4 requires scrubbing).

## 5. The gaps that matter most, in the order I would work on them

1. **B1 and B2 (memory and bandwidth under attack): DONE 2026-10-02 for the queue and the per-peer rate** (see the table; the per-kind frame caps already existed for everything but `blocks` and `txs`, so the real hole was the message-count queue, not the frame caps). *Still to do:* a measurement of a node's real memory under many hostile connections (the tests bound the gate's accounting, not the process's working set), and a total byte-rate limit across peers.
2. **Fuzz the engine: first version DONE 2026-10-02 (proptest, A3, A4);** still to do: cargo-fuzz (coverage-guided) on the engine's handlers, a longer chain in the harness (so the 500-id and 32-block caps are reached), the proof checks switched on, many peers at once, and the store's file readers (A2, G3). *Medium.*
3. **E3 and E4: difficulty and timestamps on a small network.** Simulate a handful of miners joining and leaving, with a timestamp-manipulating miner, before choosing the fresh chain's parameters (M11.2). This decides whether the first test network survives its first hours. *Medium.*
4. **E6, F2: the independent review** of the validator, the store and `tenero-crypto`, by someone other than the author. Not something I can do; the highest-value item on this list.
5. **G1, G5: file permissions on the data directory: DONE 2026-10-02** (Windows measured and tested; the Unix side written but never run on Linux, to be run in the first Linux CI job, M11.3). *Still to do:* a look at whether the wallet file location should be checked too. (The HTTP request test, G2, was done 2026-10-02.)
6. **C1: anchor peers, stale-tip detection and pinned peers: DONE 2026-10-02** (see the table). *Bootstrap rules and the eclipse simulation: DONE 2026-10-02 (`SEED_POLICY.md`).* *Network-health line, work-comparison alarm and feelers: DONE 2026-10-02.* *Seed health-check tool: DONE 2026-10-02.* *Still to do:* and a measurement on the heavy test network (run 4, restarted 2026-10-02 on the build with all of the above, whose chaos script restarts nodes at random).
7. **J1, J2: `cargo audit` and `cargo deny` in CI; reproducible, checksummed releases.** *Small to medium.*
8. **E8: the emergency-fork plan**, written down: who can decide, how a bad block or rule is handled, how testers are told. *Small, but a decision, not code.*
9. ~~Decide E10~~ **Done 2026-10-02:** 4 MiB ceiling (`CONSENSUS_V2.md` 8.4).
10. **Verify the "to verify" items** in this document, each with a test or a changed sentence.
11. ~~A test that no log line contains a secret (G6), and one that sends an HTTP request to the control port (G2).~~ **Done 2026-10-02** (both).

## 6. How this document is kept honest

* It is updated in the same commit as any change that adds or removes a defence, or finds a new threat.
* A status moves to **Mitigated** only with a named test. "I believe so" stays **to verify**.
* The independent review's findings are added here whether or not they are flattering; items found by someone else are marked as such.
* The labels on tester-facing text (`unaudited`, `no value`, `may be reset`) are part of the defence, and removing them is a decision for the owner, not a side effect of any change.
