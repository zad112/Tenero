# Running a node and a wallet (M8.7)

**Experimental and unaudited. There is no launched Tenero network, and nothing on the networks below has any value.**
Do not put anything you care about in a wallet made by these programs. The wallet uses the **interim output scheme, which
is not Carrot** (`crates/tenero-wallet/src/interim.rs` lists what it lacks).

Build (Windows, PowerShell; `cargo` is in `$HOME\.cargo\bin`):

```powershell
cargo build --release -p tenero-app
```

This makes `target\release\tenerod.exe` (the node) and `target\release\tenero-wallet.exe` (the wallet).

## The two networks

| name | proof of work | what it is for |
|---|---|---|
| `test` | SHA-256, mined by a CPU in an instant | trying the programs. Rings of 2 and a maturity of 1 block, so a payment works after a few blocks. Nodes on one machine need different loopback addresses (`127.0.0.1`, `127.0.0.2`, ...). |
| `dev` | the real matmulhash (4.3 GiB dataset per epoch), starting difficulty a placeholder (one attempt in eight meets the target) | the real proof of work end to end. The real rings (16) and maturities (10 and 60 blocks). **Not a launched network:** its genesis is made from a label, and `docs/M8_PLAN.md` section 7 lists what must be true before one exists. |

You must name a network; there is no default, so nobody runs the wrong one by forgetting a setting.

## The node: `tenerod`

A try-it-out test network on one machine, mining a block every 5 seconds to a wallet you made:

```powershell
# a wallet first (see below), then:
.\target\release\tenerod.exe --data $HOME\tenero-test\n1 --network test --listen 127.0.0.1:18331 `
    --mine sha256 --mine_to <your tni1... address>
```

A second node that finds the first (a different loopback address, its own data directory and control port):

```powershell
.\target\release\tenerod.exe --data $HOME\tenero-test\n2 --network test --listen 127.0.0.2:18331 `
    --control 127.0.0.2:18332 --seed 127.0.0.1:18331
```

Settings can be on the command line (`--key value`) or in a file (`key = value`, `#` comments, one per line) given with
`--config FILE`; the command line wins. A typo in a setting, a repeated setting, a missing value or a value that does not
parse is an error that names the setting; nothing silently falls back to a default.

| setting | meaning | default |
|---|---|---|
| `data` | the node's directory: the chain, the node key, the saved peers, the side-branch pool, the log (if any) and the cookie | required |
| `network` | `test` or `dev` | required |
| `listen` | the peer-to-peer address to accept connections on | none (dial out only) |
| `seed` | an `ip:port` to start from (repeat it); on the command line, `--seed` replaces the file's seeds | none |
| `trusted_peer` | an `ip:port` you got **out of band** (from someone you trust, not from the network) to always connect to: dialled first, and again whenever it is not connected (at most every 30 s), exempt from the per-network-group limit, never passed on to other nodes; repeat it (up to 16); on the command line, `--trusted_peer` replaces the file's. It is still checked like any peer and still banned if it misbehaves. This is the one defence against an eclipse that the attacker cannot influence | none |
| `peers` | how many peers to aim for | 50 |
| `max_inbound` | the most inbound peers | 64 |
| `allow_private_peers` | dial and accept addresses such as 127.0.0.2 and 10.x.x.x | yes on `test`, no on `dev` |
| `control` | where the control interface listens (**must be a loopback address**) | `127.0.0.1:18332` (`test`), `127.0.0.1:28332` (`dev`) |
| `prune_keep` | `0` keeps every block in full (an **archive** node); `N` (at least 1000; the design proposes 5,500) keeps the proofs of the last `N` blocks only (a **pruned** node). Older blocks are kept in pruned form; a node syncing from scratch will not use a pruned peer that has already thrown away what it needs | 0 |
| `assume_valid` | `height:blockid` (64 hexadecimal digits): trust that blocks up to there have valid proofs and skip checking them while syncing (`docs/M8_PLAN.md`, M8.3: what this trusts is written there). **Off unless you set it.** | off |
| `mine` | `off`, `sha256` (the test network), `cpu` or `gpu` (the dev network). Mining runs **inside the node's process** and pays `mine_to` | off |
| `mine_to` | the address (`tni1...`) block rewards are paid to; required when mining | |
| `mine_pace` | seconds to wait after a block is found before starting the next | 5 on `test`, 0 on `dev` |
| `mine_cores` | CPU threads for `mine = cpu` (1 to 6) | 6 |
| `gpu_device`, `gpu_batch` | which GPU, and attempts per batch (`docs/BENCHMARKS.md`) | 0, 128 |
| `log_level` | `error`, `warn`, `info`, `debug` | info |
| `log_file` | also write the log here (rotated to `<name>.old` at 20 MiB) | standard error only |
| `status_every` | seconds between status lines | 60 |
| `quiet` | show only warnings and errors on the screen (`--quiet` alone means yes) | no |
| `verbose` | also show every line of the log on the screen (`--verbose` alone means yes) | no |
| `color` | `auto`, `always` or `never` (auto: only on a terminal, never with `NO_COLOR`, never on a `dumb` terminal) | auto |
| `allow_open_data_dir` | start even if other accounts on this computer can read the data directory (the node otherwise refuses, and says how to fix it; a new directory is made private) | no |

**What the screen shows (M10.1).** A banner (what this is, the network, "EXPERIMENTAL and UNAUDITED"), then events in plain words
(`block 1,204 mined in 0.4 s, reward 20 TNR`, `synced: the chain is up to date at height 5,000`, an error with a `what to do:` line
under it) and a **status block** that is redrawn in place when the output is a terminal:

```
-- status ------------------------------------------------------------
  chain    height 1,204 (a1b2c3d4) | last block 41s ago
  sync     syncing 1,204 of 5,000 (24%) | 41 blocks/s | 1m 33s left
  peers    5 (in 2, out 3) | outbound in 3 network groups
  node     mempool 7 | up 2h 05m | disk 12.3 MiB
  mining   mining (cpu, 2 threads) | 3 found, 3 in the chain
  alarms   none
```

When the output is a file, a pipe or a service, there is no redrawing: events are lines with the time (UTC) in front, and a status line
every `status_every` seconds. **The log file is not changed by any of this:** it keeps every line, with ids and nonces, as before. `--quiet`
shows only warnings and errors; `--verbose` also shows each line of the log as it is written to the file. The screen is ASCII only, lines are
at most 78 columns, colour is off with `NO_COLOR`, and `tenero-miner` has the same options and the same kind of screen (the node it
mines for, its attempts a second, the blocks it has found). **The `TNR` after a reward is the coin's ticker (the owner chose to keep it).**
**The `hashrate` row** (`hashrate 10s 34.7k | 60s 30.3k | 15m - | avg 30.2k attempts/s`, k meaning thousands) shows attempts a second over the last 10 seconds, 60 seconds, 15 minutes and the whole run, counting only the
time the miner was searching (not while paused for a sync or building a dataset; `(idle)` means it is not searching at this moment; `-`
means that window is not yet full). **When mining on a GPU there are also rows for the card** (`gpu`: temperature, power, fan, core and memory clocks; `memory`: used and how
busy the memory controller is; `limited`: only if the driver is holding the clocks down; `reads`: **the memory reads the attempt rate
implies, worked out from the rate and not measured**: NVML does not report bandwidth). They come from NVML and are left out where the
driver or card does not say; mining does not depend on them. **The long name of the backend is on a row of its own (`backend  GPU: NVIDIA GeForce RTX 5070 Ti, batch 128`) so that it cannot push the counts off the `mining` row.** **An attempt is one matmulhash evaluation, which is a different amount of work from a hash of any
other coin, so these numbers cannot be compared with another coin's miner.** `tenerod status` and `tenerod stop` print the same one-liners as before (scripts read them).

**The status line and the alarms (in the log).** Every `status_every` seconds the node logs a line such as
`status: tip 782 (00000000) | peers 4 (in 2, out 2) | out groups 2 | last block 40s ago | samples 3 | alarms none | book 8 | ...`.
`out groups` is how many different network ranges (IPv4 /16) the node's own outbound peers are in; `last block` is how long since the chain moved;
`samples` is how many other nodes it has briefly connected to in the last half hour (a *feeler*: once every two minutes it dials one address it has
never connected to, reads that node's tip and work, and hangs up). `alarms` lists what the node thinks you should look at, and each is also logged once
as a warning when it begins and once when it ends: `stale-tip` (no new block for 10 minutes), `behind-peers` (a peer has reported more work than the node
has for 5 minutes and the node has not caught up), `network-ahead` (two sampled nodes report more work than the node has, for 5 minutes), `few-outbound`
(fewer than 2 outbound peers for 5 minutes), `few-groups` (outbound peers in fewer than 2 network ranges; not on a private network).
**None of these is proof of an attack:** each is what an eclipse, a partition or a fork looks like from inside, and also what a bad network day looks like.
A node cut off by an attacker who feeds it a valid chain slowly cannot tell from inside; see `THREAT_MODEL.md` C1.

**Stopping:** Ctrl-C (or closing the window) shuts the node down cleanly: it closes its connections, saves the peers and
the side-branch pool, and prints `stopped at height H, tip ID`. A second Ctrl-C ends it at once. From another window:
`tenerod stop --data DIR` (and `tenerod status --data DIR`). If the node is killed instead, what it has saved is at most
five minutes old (the chain itself is written as each block arrives).

**Files in the data directory:** `chain.redb` and `chain.redb.segments\` (the chain), `node.key` (the node's long-term
network key; not a wallet), `peers.dat` (the address book, the ban list and the **anchor peers**: up to two long-standing outbound peers that the node dials first after a restart), `pool.dat`, `control.cookie` (new at each start). The wallet's keys are **not**
here; they are in the wallet file you choose.

**Memory and disk:** the `dev` network's proof-of-work check needs about 4.3 GiB of memory per epoch (about 8.6 GiB while the
next epoch's dataset is built in the background); the `test` network needs almost nothing. These are the figures
`CLAUDE.md` rule 8 states, measured in earlier milestones, not measured again here.

## The wallet: `tenero-wallet`

```powershell
.\target\release\tenero-wallet.exe create  --wallet $HOME\me.wallet --data $HOME\tenero-test\n1
.\target\release\tenero-wallet.exe address --wallet $HOME\me.wallet
.\target\release\tenero-wallet.exe balance --wallet $HOME\me.wallet --data $HOME\tenero-test\n1
.\target\release\tenero-wallet.exe pay     --wallet $HOME\me.wallet --data $HOME\tenero-test\n1 --to tni1... --amount 1.5
.\target\release\tenero-wallet.exe seed    --wallet $HOME\me.wallet
.\target\release\tenero-wallet.exe restore --wallet $HOME\again.wallet
.\target\release\tenero-wallet.exe info    --data $HOME\tenero-test\n1
```

* `--data` is **the node's** data directory: the wallet reads the node's cookie from it and talks to the node's control
  interface. `--control IP:PORT` is needed only if the node does not use the default port of the test network.
* `create` asks for a passphrase twice (at least 8 characters, hidden as you type), makes a seed, writes the encrypted
  wallet file, and **shows the seed once**: write it down on paper. There is no word-list backup yet; it is the raw seed
  (64 hexadecimal digits). `seed` shows it again after the passphrase; `restore` rebuilds a wallet from it (it asks for
  the seed at a hidden prompt, so it never appears in a command line or a history file). A restored wallet scans from
  height 0 unless you give `--birth HEIGHT`.
* A new wallet starts scanning at the node's tip when it is made, so it does not read old blocks that cannot hold its
  coins. `--birth HEIGHT` sets it by hand (use it when restoring and you know the first block that paid you).
* `balance` scans what is new and saves the wallet file; it shows total, spendable, immature (waiting for maturity) and
  reserved (promised to a payment that a block has not yet taken in). `pay` builds the payment with real ring signatures
  and range proofs, checks it itself, hands it to the node, and saves the reservation. **A payment counts once a block
  takes it in**; until then the node holds it in its pool.
* Amounts are coins with up to 8 decimals (`1.5`, `0.00000001`); anything else (a sign, an exponent, a ninth decimal, a
  number too large) is an error, never a different amount.
* `--passphrase-file FILE` reads the passphrase from a file instead of asking. It is weaker (the passphrase sits in a
  file); it exists for scripts and tests.

**What is not protected:** the wallet file's encryption slows down guessing a weak passphrase and does not stop it; malware on
your computer can read the passphrase as you type it; the wallet is only as private as the node it is pointed at (the node
learns which blocks it reads); and nothing locks the wallet file, so two programs saving to it at once lose one's changes.

## The miner in its own process: `tenero-miner`

The node can mine by itself (`mine = ...`, above); this is the other way: a separate program that asks a running node for a
block over the control interface, searches for it, and hands a found block back. The GPU then has a process of its own: it
can be started, stopped and restarted without touching the node, a crash of one does not take the other down, and it needs
no node settings.

```powershell
.\target\release\tenero-miner.exe --data $HOME\tenero-test\n1 --address tni1... --backend sha256              # the test network
.\target\release\tenero-miner.exe --data $HOME\tenero-dev\n1 --control 127.0.0.1:28332 --address tni1... --backend gpu   # the dev network
```

* `--data` is the node's data directory (the miner reads the node's cookie from it); `--control` is the node's control address
  (the test network's default, 127.0.0.1:18332, otherwise); `--address` is where block rewards go; `--backend` is `sha256`
  (the test network), or `cpu` or `gpu` (the dev network's real proof of work), and a backend that does not fit the node's
  network is refused with a message. `--cores`, `--gpu-device`, `--gpu-batch`, `--pace`, `--log-level`, `--log-file` and
  `--status-every` work as the node's settings of the same names; `tenero-miner help` lists them.
* **The node checks every block completely.** A found block goes to the node as a local block, so the node's own
  proof-of-work and proof checks decide whether it joins the chain; the miner only reports what the node said (in the chain,
  lost a race, or refused).
* **It does not mine while the node is syncing,** replaces its job when the tip moves (or the template is a minute old), and
  **carries on if the node restarts**: it reconnects (waiting for the node's new cookie) and starts again. Start it before or
  after the node.
* **Memory:** with `--backend cpu` the miner builds its own 4.3 GiB dataset, as the node's check does, so the node and the
  miner together need about 9 GiB on the dev network. `--backend gpu` keeps its datasets in video memory instead.
* Stop it with Ctrl-C (it prints how many blocks it found and what became of them).

## The seed check: `tenero-seedcheck`

Connects to each seed the way a new node would and says which are fit to be on a list (`SEED_POLICY.md` says why that matters). It reads nothing
from a data directory and changes nothing on the seeds: it connects, says hello, asks for addresses and leaves.

```
tenero-seedcheck --network test --seed 203.0.113.5:18331 --seed 198.51.100.7:18331 --seed 192.0.2.9:18331 [--history seeds.tsv]
tenero-seedcheck --network dev --seeds-file seeds.txt        (one seed per line, # comments)
```

For each seed it checks that the address resolves and the connection opens; that the encrypted handshake works **for this network's chain** (a seed of
another chain or protocol version fails here); that it says hello in the same protocol version; that it answers an address request with at least 5
addresses (`--min-addrs`), at least half of them routable and in more than one network group; that it is not slow (3 s to its hello); that it is not
pruned (a seed should be able to serve the whole chain); and that it is not more than 3 blocks (`--max-lag`) behind the middle of the answering seeds
(a failure; more than 3 ahead is a warning, and so is a different tip at the same height as most). For the list: at least 3 seeds listed and
answering (`--min-seeds`), none listed twice, and **no two in one network group**, because a new node counts a group once.

The exit code is 0 when all is well, 1 for warnings and 2 for a failure, so it can be run from a schedule. `--history FILE` adds a line per seed
to a file (time, seed, up or down, severity, milliseconds, tip) and shows how many of the last 50 checks found each seed up; run it every few
minutes from a scheduler to build that record. On the test network (`--private yes` is its default) addresses need not be routable and groups are not compared.

**What it cannot tell you:** whether a seed is *honest*. A seed that answers promptly, with plausible addresses and the right tip, passes; the policy's
protection is a mostly-honest list of independent operators, and a program cannot check who runs a seed. It is one moment's look at a seed unless
you keep a history. Measured on 2026-10-02 against the 8 nodes of the heavy test network (run 4): all 8 up, one tip, 6 to 9 ms each.

## Not done in M8.7
* **A Windows service, a systemd unit, an installer.** Run it in a window, or under a scheduler you trust.
* **Tor or I2P,** and any encryption of the control interface (it never leaves the machine).
* **Anything on a launched network:** there is none.
