# Running a private test network on one machine (M8.4)

**A test, not a coin.** These nodes run the SHA-256 test chain, which a CPU mines in an instant, with a
placeholder wallet-less coinbase and no real proof of work. They exercise the peer-to-peer layer (real sockets, the
Noise channel, sync, relay, forks, bans, restarts) and nothing else. Experimental and unaudited, and nothing on this
chain has any value.

## Build once

```powershell
cargo build --release -p tenero-net --examples
```

This makes `target\release\examples\p2p_testnode.exe` (a node) and `p2p_clients.exe` (a tool that opens many
connections to a node).

## The day-long run: three nodes, three data directories, three ports

The engine does not connect to two peers on one host, so the three nodes use three loopback addresses
(`127.0.0.1`, `127.0.0.2`, `127.0.0.3`, all of which Windows treats as this machine). Open three PowerShell
windows, one per node, from the repository folder:

Every node is given the other two as seeds, so each connects to both (and the two links that two nodes open to each
other at the same moment are settled into one, which is part of what is tested):

```powershell
# window 1
.\target\release\examples\p2p_testnode.exe --data $HOME\tenero-testnet\n1 --listen 127.0.0.1:18331 --seed 127.0.0.2:18331 --seed 127.0.0.3:18331 --mine-every 60
# window 2
.\target\release\examples\p2p_testnode.exe --data $HOME\tenero-testnet\n2 --listen 127.0.0.2:18331 --seed 127.0.0.1:18331 --seed 127.0.0.3:18331 --mine-every 60
# window 3
.\target\release\examples\p2p_testnode.exe --data $HOME\tenero-testnet\n3 --listen 127.0.0.3:18331 --seed 127.0.0.1:18331 --seed 127.0.0.2:18331 --mine-every 60
```

Each node mines a block about once a minute (with random jitter, so two nodes sometimes mine at the same time and
the network has to settle a fork, which is part of the test), prints a **status line** every 30 seconds, and prints a
**log line** for every connection, disconnection, refusal and ban.

A status line looks like:

```text
status: tip 512 (3a1b1cbb) | peers 2 (in 1, out 1) | book 3 | blocks applied 388, mined 124 | bans 0 | bytes in 1204511, out 1187302 | threads 4
```

### While it runs: what to look for

* **The tips agree.** `tip HEIGHT (first 4 bytes of the block id)`. Two windows showing the same height and
  *different* ids for a minute or two is a fork being settled (normal); the same pair of different ids still there
  ten minutes later, or heights drifting apart for good, is a failure.
* **`peers` stays at 2** on each node (three nodes, each connected to the other two; one link may be shown as
  inbound and one as outbound, or both the same way).
* **`bans 0`.** Nothing here should ever be banned.
* **`threads`** is 2 per peer plus a few: a count that keeps growing is a leak.

### To stop it, and the check that matters

1. In each window type `nomine` and Enter (they stop mining and keep syncing).
2. Wait two minutes, then compare the last `status:` lines: the three tips must be **identical**.
3. In each window type `quit` and Enter (each saves its peers and stops). Each prints
   `stopped at height H, tip <the full block id>`: the three must match.
4. Start one again with the same `--data` directory: it continues from its chain and finds its peers again from
   `peers.dat` (you can leave out `--seed`).

**Please paste back:** the last two `status:` lines from each window, and the three `stopped at ...` lines; plus any
line that mentions `banned`, `not reading`, `handshake ... failed` or `sent bytes that are not a message` and what
you were doing at the time (there should be none, unless you tried to break it).

### Breaking it on purpose

* Kill one node's window abruptly (close it) and start it again with the same command: it must rejoin and catch up.
* Stop node 1 (the seed) for ten minutes, then start it: nodes 2 and 3 keep each other and the chain going, and
  node 1 catches up.
* Start a fourth node with a new data directory, `--seed 127.0.0.1:18331` and its own loopback address
  (`127.0.0.4`): it must
  sync the whole chain.

### A short version, to check your setup first

Add `--mine-for 50 --duration 75` to each command: the nodes mine for 50 seconds, listen for 25 more, and stop by
themselves. The three `stopped at height ... tip ...` lines should be identical. (This run, on the machine it was
written on, ended with all three at height 32 and the same block id.)

## The first day-long run (2026-09-30 21:19 to 2026-10-01 15:50, 18.5 hours): what it showed

Three `p2p_testnode` processes on loopback, each mining about a block a minute. **It did not stay one network.** Read
from the three chain databases after the nodes were stopped (measured, not estimated):

* All three chains were identical **up to height 379**, then split into **three separate chains** of 1,328, 1,332 and
  1,335 blocks, each about 950 blocks past the split. They never rejoined.
* The processes were alive to the end, with flat memory (about 8.6 MB each) and **no established connections**.
* The saved ban lists held entries: node 2 had banned the hosts of nodes 1 and 3, node 3 the host of node 1, each for the
  engine's 24 hours. (Which message earned each ban was not logged: the nodes print to their windows only.)
* The difficulty had climbed from the starting target to about 2^28 hashes a block, so that a single CPU thread needs
  tens of seconds per block, and one node was using about 87% of a core when stopped.

**What I think happened (a hypothesis, not proven):** the test node mines *inside* the network loop: it searches for the
nonce in a plain loop until it finds one, and nothing else runs meanwhile. The difficulty retargets to a block a minute for
whatever speed the CPU has, so after a few hours each search blocks the loop for tens of seconds. A stalled node answers no
pings and no requests, its peers' requests time out, and when it wakes the replies it sent late (or the blocks it asked for)
reach peers that have already given up on them: *"unsolicited pong"* (10 points), *"a block nobody asked for"* (20) and
similar are scored, and 100 points is a 24-hour ban. Two such bans on one host and the network is split for a day.

**What it means:**
* **The test tool is wrong, not necessarily the engine.** The real miner runs on its own thread (`tenero-miner`, `tenerod`)
  and never blocks the loop; `p2p_testnode` is the only thing that mines in the loop. The repeat run should use `tenerod`
  with `mine = sha256` and a `mine_pace`.
* **But the engine has a real weakness this exposed:** a reply that arrives *after* its request timed out is scored as if
  it were unsolicited, so a node that merely stalls (for example while a real node builds a 4.3 GiB proof-of-work dataset
  at an epoch change, which takes seconds inside the loop) can be banned by honest peers. The engine's own principle is
  "slow is not hostile" (a timed-out request is not scored); it should extend to the late answer. **Fixed afterwards (see below).**
* The first 379 blocks, about six hours, did stay in sync across three nodes, with forks settled as designed.

### The fix (made after that run)

Two changes, each with tests that fail without it (`crates/tenero-net/tests/late_replies.rs`, and two tests in
`tests/transport.rs`):

1. **A late reply is forgiven once** (`engine.rs`). When our own timeout gives up on a request for block ids, headers or
   transactions, or on a ping, the engine remembers that a reply of that kind from that peer would now be *late*, not
   unsolicited, for four request timeouts. The first such reply is ignored without a penalty and counted
   (`Stats::late_replies_forgiven`); a second reply to the same request, a reply from another peer, a reply of another kind,
   and a reply after the grace has passed are punished exactly as before. (Late *blocks* were already welcome: the engine
   remembers what it asked for for four timeouts.)
2. **The loop reads what is already queued before it looks at the clock** (`transport.rs`). After a stall the answers
   to our own requests are waiting in the queue; ticking first would declare those requests timed out and then find their
   answers unwelcome. It reads up to 4,096 events first.

Fault injection: 20 faults in the fix, one by one; all caught (two of the expiry checks are redundant with each other,
so removing either alone changes nothing; the third survivor of the first pass was dead code and was removed). **Not
tested:** a stall on a real network with real proof-of-work blocks, and what a hostile peer can do with the grace (it can
cost us at most one ignored reply per request that timed out, which is no more than being slow costs it).

## Many connections

To see what a node costs with many peers, in one window run a node (`--max-inbound 300`) and in another:

```powershell
.\target\release\examples\p2p_clients.exe --connect 127.0.0.1:18331 --count 120 --hold 60
```

and look at the node's memory and thread count in Task Manager (or `Get-Process p2p_testnode`). The clients connect
40 ms apart on purpose: a node allows only a few handshakes at once from one address.

## A local network of the REAL programs (`tools/localnet.ps1` and `tools/soak.ps1`)

The sections above run the protocol engine's own test nodes. These two PowerShell scripts run the **actual release programs** (`tenerod`, `tenero-miner`, `tenero-wallet`) as separate processes on this
machine, on the SHA-256 `test` network (a CPU finds a block in an instant), each node on its own loopback address (`localnet.ps1`: `127.0.0.1`, `127.0.0.2`, ...; `soak.ps1`: `127.1.0.1`, `127.2.0.1`, ..., one network group each). **They check the programs and the peer-to-peer behaviour. They
do not test the real proof of work, its memory (the 4 GiB dataset) or its speed, and they do not test Linux**: that needs the `dev` or `alpha` network and a GPU. Build first: `cargo build --release -p tenero-app`.

* **`tools/localnet.ps1`** (about 3 minutes): four nodes and one miner, a payment, a late node, a clean restart, a crash. `-BreakOnPurpose` is a self-test of the checks (a node that is never told where to connect;
  the "syncs" checks must fail).
* **`tools/soak.ps1 -Minutes 60 [-Visible]`**: four nodes, **each with its own miner**, a fifth node that joins late (no miner), node 2 stopped cleanly and restarted, node 3 killed and restarted, payments from the miners'
  wallets to two payee wallets that never mine (their balances must equal exactly what was paid), convergence checkpoints, then everything stopped and every data folder opened offline (`tenerod rewind` as a dry run)
  and compared. `-Visible` gives every program a console window (on Windows 11 they are tabs or windows of Windows Terminal); without it they are hidden and write to files. Exit code 0 = all checks passed.

Both stop only what they started (by process id or `tenerod stop`) and **delete nothing**: the folder they used (printed at the start) keeps the logs, `samples.csv` and the data. **What they found while being
written:** the status parser read the peer count as the height (a check that could not fail), the miner's log file says "found a block at height" (the screen says "mined in"), and the test chain's difficulty
adjusts toward one block a minute within a few minutes, so a miner finds a block only every few minutes (a "finds blocks soon" check was wrong; "reconnects" is the right one).

**How the soak's nodes find each other (2026-10-05), and what that found in the program.** Node 1 is the ONLY seed; nodes 2 to 5 are given only its address, and every node starts with `--advertise` (its own `ip:port`), as a node on the real
network would. Each node has its own network group (`127.1.0.1`, `127.2.0.1`, ... one /16 each). The first tries gave 3/1/1/1 peers for an hour of nothing better, and showed three things, now fixed or known:

1. **`tenerod` never told anyone its own address** (the engine could, `tenerod` never turned it on), so a seed's address book stayed empty and it had nothing to tell the next node. Fixed: the `advertise` setting (`docs/RUNNING.md`).
2. **A peer may only announce the host it connected from** (so it cannot send us to a third party). Windows dials out from `127.0.0.1` whatever address a node listens on, so every local announcement was refused. On a private network (`allow_private_peers`, on by
   default only for `test`) the announced address is now taken; on a public network the strict rule stands (a test pins both).
3. **A seed repeats one answer to a network group for 24 hours** (so one group cannot harvest the address book), and all local nodes look like one group (`127.0.0.1`), so the first, empty answer went to everyone. On a private network the answer is no longer repeated
   (a test pins both). **What still differs from the real network:** every local node appears to dial from `127.0.0.1`, so the engine cannot tell which host it is connected to and sometimes dials a peer it already has (it keeps one link, but it shows as reconnecting
   every 30 s in the logs). The fix would be to dial out from the node's own `listen` address, which needs a socket library the project does not have (an owner decision under rule 3); a test on real separate machines (the seed server) is the faithful one.

**Result of the first full hour (run 5, 2026-10-04/05, `target\release` of commit 261a129 plus the changes above, nodes in Windows Terminal tabs):** the three nodes found each other within 30 s of the first node (exactly 3 peers each), then exactly 4 each when node 5 joined
late, and again after node 2's clean stop/restart and node 3's kill/restart; all nodes were always on one tip (apart from a one-block difference for a moment when two miners found a block at once); **18 payments, and the two payees held exactly what was paid at
every checkpoint and at the end**; every data folder opened offline and held the same chain (178 blocks); no panic, corruption or ban in any log; every node's largest working set was 11 MB (this chain has no 4 GiB dataset, so this says nothing about the real
network's memory). **One check failed, and it was the check that was wrong:** "miner 3 found a block again after its node was killed" (it found none in the 16 minutes before the end; the other three found 19 in that time, which is about a 0.4% chance if all are equally strong).
Its log showed it connected, hashing at full speed and taking a new job for every block; **a replay of the same miner on a copy of node 3's data, killed and restarted, found a block after 12 minutes** (one run, so luck is the likely, not a proven, cause). The check now asks whether the
miner is still hashing and taking jobs, and only prints how many blocks it found.

**A limit the soak cannot show and that matters for the first public test:** a node learns where its peers are only from peers that announce an address, and a node behind a home router that does not forward the port cannot be dialled, so it should not announce one. **The testers' nodes
will mostly be outbound-only, so with one seed the network will look like a star around the seed** (every tester connected to the seed, few to each other) unless some testers forward a port and set `advertise`. Blocks still reach everyone through the seed, but the seed is then a single
point of failure and the seed policy's "several independent seeds" matters more.
