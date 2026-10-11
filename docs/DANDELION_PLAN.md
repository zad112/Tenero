# Dandelion++ for transactions (protocol version 3)

Status: **built, 2026-10-11, for the release after 0.3.0-gamma.1; unreviewed.** Owner's decision (2026-10-11): a clean
switch to protocol version 3 in that release. Nodes of 0.3.0-gamma.1 (version 2) can no longer connect to it: the seeds,
the pool and every node must upgrade together. Not consensus: blocks and transactions are unchanged.

## What it is for

Until now a node announced a new transaction to every peer at once (`new_tx`), its own transactions included. A peer that
is connected to many nodes (a "spy") sees which node announced a transaction FIRST, and that is very often the node it
came from: the IP address of the wallet that made the payment. That is threat `D4` in `THREAT_MODEL.md`.

Dandelion++ (Fanti, Venkatakrishnan, Bakshi, Denby, Bhargava, Miller, Viswanath, *Dandelion++: Lightweight
Cryptocurrency Networking with Formal Anonymity Guarantees*, 2018; Monero has used it since 2020) sends a new transaction
along a random path first, one peer at a time (the **stem**), and only then announces it as before (the **fluff**). The
first announcement then comes from a node some hops away, not from the one that made it.

## The rules (`crates/tenero-net/src/dandelion.rs`, used by `engine.rs`)

* **Epochs.** Every 10 minutes (`epoch_ms`) a node chooses again, at random:
  * its **role**: a *diffuser* (20 %, `fluff_percent`), which fluffs every stem transaction it is given, or a *relayer*
    (80 %), which passes it on;
  * its **stem peers**: 2 of its OUTBOUND peers (`stems`), peers it dialled, so an attacker cannot make itself one by
    connecting to us.
* **Routes.** A relayer sends every stem transaction that came from one peer to the SAME stem peer for the whole epoch
  (each incoming peer is mapped to one stem peer, Dandelion++'s "pseudorandom forwarding"), and its OWN transactions all
  to one stem peer. A transaction is never sent back to the peer it came from.
* **A node's own transaction** is always stemmed (never fluffed by the node that made it), whatever its role, if it has a
  stem peer. With no outbound peer at all it is fluffed: there is no one to hide among.
* **Embargo.** Every node that holds a stem transaction sets a timer, random, exponential with a mean of 39 s
  (`embargo_mean_ms`, at most 4 minutes). If the transaction has not been announced to it by then (a stem peer dropped it,
  or went away), the node fluffs it itself. So a transaction is never lost to a black hole, only delayed.
* **The stem pool.** A stem transaction is checked as a pool transaction would be (`Node::check_tx`), but it is kept apart
  from the pool: it is not announced, not mined, not listed by the control interface's `mempool`, and **not served** to
  `get_txs` (or a spy could ask every node for it and find who holds it). When it is announced to the node, it moves into
  the pool at once (no need to fetch it). At most 1,024 are held; past that a new one is fluffed.
* **Abuse.** A stem transaction that is invalid costs its sender 20 points, as an invalid transaction does today; one that
  spends what the chain has spent costs nothing (it may be valid on the sender's chain). A transaction that conflicts with
  one already in the stem pool or the pool is ignored.
* **Randomness.** Every choice is drawn from SHA-256 of a secret made at start (from the operating system in `tenerod`;
  from the seed in the simulator) with the epoch number and what is being chosen, so a peer cannot predict a node's
  role, its stem peers or its timers from what it sees.

## The wire (`docs/WIRE_PROTOCOL.md`)

* `hello`'s `version` is **3**; a peer of any other version is dropped ("protocol version mismatch"), as before.
* A new message, **`stem_tx` (kind 21)**: one whole transaction, the version 3 encoding, nothing else. It is the only way a
  transaction is pushed unasked; `txs` stays the answer to `get_txs`.

## What it does not do, said plainly

* **A small network hides little.** `gamma` has about ten nodes, and most outbound peers are the two seeds. A stem often
  reaches a seed in one hop, so whoever runs the seeds still learns a lot. Dandelion++ stops an ordinary peer (and a spy
  connected to every node) from simply reading the origin off the first announcement; it is not strong anonymity until
  there are many independent nodes. The simulator measures it (`tests/sim.rs`, `dandelion_*`).
* **It hides the IP address, not the payment.** Amounts and addresses are Carrot's and FCMP++'s business, unaudited.
* **Timing.** A fluff is announced at once, with no random delay. Monero adds one; that is future work.
* **A peer that sees a stem transaction knows it is new.** That is the design: the anonymity comes from not knowing how
  far along the stem it is.

## With Tor (planned for the same release)

Dandelion++ does not care how a peer is reached: a stem peer is a peer by its connection, not its address, so an outbound
peer reached through Tor is a stem peer like any other, and an inbound one (which arrives from 127.0.0.1) is mapped by its
connection, not its IP. Two things are left open for the Tor work, so the two fit:

* A node's own transactions are routed in ONE place (`Engine::route_own`). With Tor on, it can send them over a Tor peer
  instead (as Monero does: a node's own transactions go over the anonymity network, and Dandelion++ covers the relaying on
  the open network).
* Stem peers are chosen from the outbound peers in one place (`Engine::stem_candidates`), so a rule such as "prefer Tor
  peers" or "only clear-net peers relay stems" is one change.

## Parameters

| | value | where it comes from |
|---|---|---|
| epoch | 10 minutes | Monero (`CRYPTONOTE_DANDELIONPP_EPOCH_TIME`, as far as I know it) |
| diffuser probability | 20 % | Monero; the paper studies 10 to 20 % |
| stem peers | 2 outbound | Monero and the paper's "4-regular" graph |
| embargo | exponential, mean 39 s, at most 4 min | Monero's average |
| stem pool | 1,024 transactions | ours |

These are to check against Monero's source before a main network; they were taken from memory of it.
