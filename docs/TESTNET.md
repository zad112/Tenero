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

## Many connections

To see what a node costs with many peers, in one window run a node (`--max-inbound 300`) and in another:

```powershell
.\target\release\examples\p2p_clients.exe --connect 127.0.0.1:18331 --count 120 --hold 60
```

and look at the node's memory and thread count in Task Manager (or `Get-Process p2p_testnode`). The clients connect
40 ms apart on purpose: a node allows only a few handshakes at once from one address.
