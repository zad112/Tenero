# Running a node and a wallet (0.3.0)

**Experimental and unaudited. Every Tenero network is a test network, and nothing on the networks below has any value.**
Do not put anything you care about in a wallet made by these programs. The wallet uses **Carrot and FCMP++**, through
Monero's FCMP++ libraries (only partly audited) and a Carrot written in Rust by this project (not audited); its message
signatures and payment proofs are **our own construction, reviewed by nobody** (`docs/WALLET_PROOFS.md`).

Build (Windows, PowerShell; `cargo` is in `$HOME\.cargo\bin`):

```powershell
cargo build --release -p tenero-app
```

This makes `target\release\tenerod.exe` (the node) and `target\release\tenero-wallet.exe` (the wallet).

## The three networks

All three use the version 3 rules from block 0: FCMP++ spends, Carrot outputs, block weight (`docs/CONSENSUS_V2.md` section 15). **`beta` and `alpha` are not run by the
0.3.0 programs**: keep the 0.2.0 programs for them; nothing moves between networks.

| name | proof of work | what it is for |
|---|---|---|
| `gamma` | the real matmulhash (the gathered attempt from block 0), **a real starting difficulty (a target of 2^237: about 524,000 attempts a block)**, epochs of 100 blocks | **the release network of 0.3.0-gamma.1**: a fresh genesis ("tenero gamma network 1", chain id `bd366b37...28c4`) with **no premine**. Addresses start `TENg`. **Still a test network: unaudited, nothing on it has value, and it can restart.** The default control port is 38352 and the peer port 38353. **Two seeds are built in**, the author's servers `195.26.244.245:38353` and `194.238.27.60:38353`: two machines, one operator. Private peer addresses are refused by default. |
| `dev` | the real matmulhash (4.3 GiB dataset per epoch), starting difficulty a placeholder (one attempt in eight meets the target) | the real proof of work end to end, with `gamma`'s rules and maturities (10 and 60 blocks). Addresses start `TENd`. **Not a launched network:** its genesis is made from a label. |
| `test` | SHA-256, mined by a CPU in an instant, at a fixed difficulty | trying the programs, with `gamma`'s rules (a payment works once a reward has matured, 60 blocks: about 5 minutes at the default pace). Addresses start `TENt`. Nodes on one machine need different loopback addresses (`127.0.0.1`, `127.0.0.2`, ...). |

You must name a network; there is no default, so nobody runs the wrong one by forgetting a setting. A node and a peer on different networks refuse each other at the handshake (the chain ids differ).

**Timestamps (all networks, since M11.2):** a block's timestamp must be **later than its parent's**. A chain that finds blocks faster than one a second therefore runs ahead of the clock (at most 120 s ahead is accepted; beyond that a block is held until the clock catches up), which a network aiming at 60 s per block never reaches; the SHA-256 `test` network mined at `mine_pace = 0` can, in a long run.

## The node: `tenerod`

A try-it-out test network on one machine, mining a block every 5 seconds to a wallet you made:

```powershell
# a wallet first (see below), then:
.\target\release\tenerod.exe --data $HOME\tenero-test\n1 --network test --listen 127.0.0.1:18331 `
    --mine sha256 --mine_to <your TENt... address>
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
| `network` | `gamma`, `dev` or `test` | required |
| `listen` | the peer-to-peer address to accept connections on | none (dial out only) |
| `advertise` | the `ip:port` OTHER nodes can reach this one at; it is told to each peer once (a peer takes it only if the IP is the one it connected from, except on a private network). **`0.0.0.0:PORT` means "the address you see me at, on this port"**: no IP to know or keep up to date (the wallet app's "Let other nodes connect to me" uses it). Needs `listen`. This is how a seed learns where its peers are, so what to tell the next node. **Set it only if the port is reachable from outside** (a server, or a forwarded port); behind a router that does not forward it, leave it unset | none (nobody is told) |
| `seed` | an `ip:port` to start from (repeat it); on the command line, `--seed` replaces the file's seeds | none |
| `no_builtin_seeds` | `yes` ignores the seed addresses built into the program for this network (`gamma` has two, the author's servers; `test` and `dev` never do). A node starts from the built-in seeds **and** every `seed` you give; use this for a private network of your own | no |
| `trusted_peer` | an `ip:port` you got **out of band** (from someone you trust, not from the network) to always connect to: dialled first, and again whenever it is not connected (at most every 30 s), exempt from the per-network-group limit, never passed on to other nodes; repeat it (up to 16); on the command line, `--trusted_peer` replaces the file's. It is still checked like any peer and still banned if it misbehaves. This is the one defence against an eclipse that the attacker cannot influence | none |
| `peers` | how many peers to aim for | 50 |
| `max_inbound` | the most inbound peers; **0 means no limit set by the program** (your machine's own limits and the per-connection budgets still apply: on Linux raise the open-files limit if you expect many, `LimitNOFILE` in a systemd unit). Set a number if you want a limit. A full node (one with a limit that is reached) drops an inbound peer that has done nothing for 10 minutes to make room for a newcomer | 0 (no limit; it was 64 until 2026-10-05) |
| `allow_private_peers` | dial and accept addresses such as 127.0.0.2 and 10.x.x.x | yes on `test`, no on `dev` and `gamma` |
| `miner_listen`, `miner_key`, `miner_max`, `miner_rate` | the **miner service** (`docs/REMOTE_MINING_PLAN.md`): `miner_listen` is an `ip:port` (the port the plan proposes is 38334) where **miners on other computers** may ask this node for a block and hand one back; it answers only `info`, `block_template`, `submit_block` and `submit_header`, over an encrypted channel (a template is compact: the header, the reward output and the transactions' ids, so a remote miner mines blocks up to the ceiling with small messages). `miner_key` is 64 hexadecimal digits (32 random bytes, which you make and give to the miners): **required** with `miner_listen` in this first version, so only miners you give the key to can connect. `miner_max` is the most miners at once and `miner_rate` the most `info` and `block_template` requests one address may make in a minute (handing in a found block has a limit of its own, 30 a minute, so a miner told to slow down can still deliver a block). A miner that is told to slow down keeps its connection and its job, waits five seconds and asks again. Off unless `miner_listen` is set. **Do not run it on a node that matters until the limits have been measured: they are not.** | off; 8; 120 |
| `control` | where the control interface listens (**must be a loopback address**) | `127.0.0.1:38352` (`gamma`), `127.0.0.1:28332` (`dev`), `127.0.0.1:18332` (`test`) |
| `prune_keep` | `0` keeps every block in full (an **archive** node: seeds and a block explorer's node should be one); `N` (at least 1000) keeps the proofs of the last `N` blocks only (a **pruned** node). Older blocks are kept in pruned form; a node syncing from scratch will not use a pruned peer that has already thrown away what it needs | 5500 (**pruned**: since 0.3.0) |
| `assume_valid` | `height:blockid` (64 hexadecimal digits): trust that blocks up to there have valid proofs and skip checking them while syncing (`docs/M8_PLAN.md`, M8.3: what this trusts is written there). **Off unless you set it.** | off |
| `mine` | `off`, `sha256` (the test network), `cpu` or `gpu` (the `gamma` and `dev` networks). Mining runs **inside the node's process** and pays `mine_to` | off |
| `mine_to` | the address block rewards are paid to (a main address of the node's network: `TENg...`, `TENd...` or `TENt...`); required when mining | |
| `mine_pace` | seconds to wait after a block is found before starting the next | 5 on `test`, 0 on `dev` |
| `mine_cores` | CPU threads for `mine = cpu` (1 to 6) | 6 |
| `gpu_device`, `gpu_batch` | which GPU, and attempts per batch (`docs/BENCHMARKS.md`); `gpu_batch = auto` measures 128, 256 and 512 for 4 s each at start-up (about 20 s) and uses the fastest (on the one card measured the three were within 1 to 2 %, which is inside the run-to-run noise) | 0, 128 |
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
driver or card does not say; mining does not depend on them. **The long name of the backend is on a row of its own (`backend  GPU: NVIDIA GeForce RTX 5070 Ti, batch 128`) so that it cannot push the counts off the `mining` row.** **The `luck` row** says how many blocks this miner found and how many its attempts should have found (each attempt has a fixed chance of finding one, so the two should be close over a long run, and **over a short run they can differ a lot by chance alone**: the ratio is only shown once 5 blocks are expected), and the **effective** rate: the work of the blocks that are in the chain divided by the time, waiting and pauses included. It depends on luck and on the node's round trips, which is why it is not the hash rate. (On an easy target a GPU batch can hold several solutions and only the first becomes a block; the attempts after it are left out of the expectation, so the two numbers still agree. The CPU backend does not do this: with an easy target on many CPU threads the expectation reads a little high.) **An attempt is one matmulhash evaluation, which is a different amount of work from a hash of any
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
`tenerod stop --data DIR` (and `tenerod status --data DIR`); **on `gamma` and `dev` add `--control 127.0.0.1:PORT` (38352 and 28332), because these commands assume the `test` network's port 18332** (measured 2026-10-04: without it they say "cannot reach the node at 127.0.0.1:18332"). If the node is killed instead, what it has saved is at most
five minutes old (the chain itself is written as each block arrives).

**Which build is this:** `tenerod --version` prints the version and the source commit (`-dirty` if the tree had changes); a bug report or an emergency starts with it. The node's log begins with the same line.

**Emergency rewind (`docs/EMERGENCY_PLAN.md`):** with the node **stopped**, `tenerod rewind --data DIR --network gamma|dev|test --to HEIGHT` takes the newest blocks
off its chain, down to HEIGHT. Without `--yes` it only says what it would remove and changes nothing; with `--yes` it first writes the removed blocks'
ids to `rewind-<time>.txt` in the data directory, sets the side-branch pool aside (`pool.dat.before-rewind`), and then removes the blocks one at a time
(each one a whole database transaction, so a stop in the middle leaves a shorter chain that is intact). **Make a copy of the data directory first** (the
plan's evidence step); the command refuses while a node is using the directory. It does not stop the node taking the same blocks again from a peer that still
has them: the plan is a build that refuses the bad block, then every node rewinds. It removes blocks only; it has no undo.

**Files in the data directory:** `chain.redb` and `chain.redb.segments\` (the chain), `node.key` (the node's long-term
network key; not a wallet), `peers.dat` (the address book, the ban list and the **anchor peers**: up to two long-standing outbound peers that the node dials first after a restart), `pool.dat`, `control.cookie` (new at each start). The wallet's keys are **not**
here; they are in the wallet file you choose.

**Memory and disk:** on `gamma` and `dev` a node's proof-of-work check holds **one dataset at a time, 4 GiB** (4.01 GiB measured as the whole check process, 2026-10-04, across three epoch
boundaries; a node also has the chain database and the operating system, so plan on about 4.3 GiB free and stay well inside the owner's limit of 8 GB). When the first block of a new epoch arrives the old
dataset is freed and the new one built, which makes the node **wait about 3 seconds** (measured: 2.8 to 3.0 s with 6 threads) once every 100 blocks, and again if a reorganisation goes back across an epoch boundary;
a node syncing from scratch pays this at every boundary it crosses. **What it replaced:** the node used to keep the last two epochs (and build the next one ahead of time), which is **8.0 GiB** of dataset for most of
every epoch after the first (measured; the earlier text here said "briefly", which was wrong). **The CPU miner may still hold two** (and prefetch); **the GPU miner holds one 4 GiB dataset, in video memory** (about 5.0 GiB committed for the process on Windows, measured, with 0.36 GiB of it in use as RAM). `mine = cpu` or `mine = gpu` inside the node adds the miner's own
memory on top of the node's: the 8 GB limit is for the node alone. The `test` network needs almost nothing. **Disk:** a pruned node (the default) keeps
every block's prefix and the proofs of the last 5,500 blocks; an archive node keeps everything (FCMP++ proofs make a full block up to about 3.5 times bigger than on
`beta`; estimated from the proof sizes, not measured on a network). **Checking a proof** (FCMP++, measured on one Ryzen 9 5900X thread): about 20 ms an input, and the
node checks the proofs of a block's transactions together.

**Moving the node's data (the chain) to another folder or drive.** The chain grows with every block, so you may want it on a bigger drive. **In the wallet app:** Settings, "Move the node's data": stop the node and the miner, type a new or empty folder (a full
path such as `D:\TeneroData`), press the button. It **copies** the data, then reads every file back and compares it with the original, and only then uses the new place; a progress bar shows how far it is and "Cancel the move" stops it (what it
copied is removed). **The old folder is never touched or deleted**: delete it yourself once the node has run from the new place. The new folder is made private first (only you and the system, as the node does for a new data folder), `control.cookie` is not
copied (the node makes a new one), and links inside the data folder are refused. It does not check free space (a full disk is an error and everything the move made is removed again). Do not use a network drive or a USB stick you may unplug. **By hand,
with the node stopped:** copy the folder, make the copy private (`icacls "NEW" /inheritance:r /grant:r "%USERDOMAIN%\%USERNAME%:(OI)(CI)F"` on Windows, `chmod 700` on Linux), and start the node with `--data NEW`. The wallet file is separate (it is not in the data folder).

## The wallet: `tenero-wallet`

```powershell
.\target\release\tenero-wallet.exe create  --wallet $HOME\me.wallet --network test --data $HOME\tenero-test\n1
.\target\release\tenero-wallet.exe address --wallet $HOME\me.wallet
.\target\release\tenero-wallet.exe balance --wallet $HOME\me.wallet --data $HOME\tenero-test\n1
.\target\release\tenero-wallet.exe pay     --wallet $HOME\me.wallet --data $HOME\tenero-test\n1 --to TENt... --amount 1.5
.\target\release\tenero-wallet.exe pay-many --wallet $HOME\me.wallet --data $HOME\tenero-test\n1 --file payments.txt
.\target\release\tenero-wallet.exe sweep   --wallet $HOME\me.wallet --data $HOME\tenero-test\n1 --yes
.\target\release\tenero-wallet.exe combine --wallet $HOME\me.wallet --data $HOME\tenero-test\n1 --pieces 10 --yes
.\target\release\tenero-wallet.exe seed    --wallet $HOME\me.wallet
.\target\release\tenero-wallet.exe restore --wallet $HOME\again.wallet --network test
.\target\release\tenero-wallet.exe integrated-address --wallet $HOME\me.wallet
.\target\release\tenero-wallet.exe view-key --wallet $HOME\me.wallet --tier all
.\target\release\tenero-wallet.exe restore-view --wallet $HOME\watch.wallet --network test
.\target\release\tenero-wallet.exe info    --data $HOME\tenero-test\n1 --network test
```

* **A wallet belongs to one network** (`--network`, `gamma` unless you say otherwise): it takes only that network's addresses (`TENg...`, `TENd...`, `TENt...`) and refuses a
  node of another network. Without `--control`, it talks to its network's default control port.
* **Integrated addresses.** `integrated-address` prints your main address with a payment ID inside (a random one, or `--payment-id` with 16 hexadecimal digits), for someone who must tell your payments apart; the wallet app's History tab shows the ID of each payment made to one. A subaddress is the more private way to tell payers apart.
* **View-only wallets.** `view-key` prints a wallet's view key (`TENview1...`, **a secret**: whoever has it sees what it shows). `--tier all` (the default) sees incoming and
  outgoing payments and the true balance; `--tier received` sees incoming payments only, so its `balance` is what was RECEIVED, not a balance. `restore-view` makes a
  view-only wallet from one (it asks for the key at a hidden prompt). A view-only wallet cannot spend or sign; it can prove a payment it received (unsigned). The wallet app
  shows a view key too (Settings, "Show a view key…") and opens view-only wallets as well (below).

* **Many recipients, many pieces.** Your balance is made of separate **pieces**, one for each payment you received (each block reward is one piece); the amount of a piece is in TNR. A transaction pays at most **15 recipients** (16 outputs, one of them your change) and may be at most **75,000 bytes**, which is about **48 to 126 pieces** spent
  (depending on the depth of the curve tree). `pay` and `pay-many` split what does not fit into **several transactions that spend different coins**, so all of them can be sent at once; a payment
  that needs more pieces than one transaction holds is paid in parts (the person paid receives several amounts that add up). `pay-many` reads `ADDRESS AMOUNT` a line (blank lines and lines
  starting with `#` are skipped; a bad line refuses the whole file, naming the line). **The change of a transaction cannot be spent for 10 blocks**, so when the pieces that were free at the
  start run out before everyone is paid, the wallet sends what it can and says how many payments are left; `--unsent-file FILE` writes them in the same format, to be run again later.
  `sweep` combines every piece worth more than the fee it adds, as many to a transaction as fit, into your own address (or `--to ADDRESS`); `combine --pieces N` makes the N smallest into one.
  **Both only show a preview until `--yes`.** The new piece can be spent after 10 blocks. Combining costs a fee and puts a transaction on the chain; it is not private to do it in a hurry.
  **A payment proof covers one output: one recipient of a transaction.**
* `--data` is **the node's** data directory: the wallet reads the node's cookie from it and talks to the node's control
  interface. `--control IP:PORT` is needed only if the node does not use its network's default port.
* `create` asks for a passphrase twice (at least 8 characters, hidden as you type), makes a seed, writes the encrypted
  wallet file, and **shows the seed once**: write it down on paper. There is no word-list backup yet; it is the raw seed
  (64 hexadecimal digits). `seed` shows it again after the passphrase; `restore` rebuilds a wallet from it (it asks for
  the seed at a hidden prompt, so it never appears in a command line or a history file). A restored wallet scans from
  height 0 unless you give `--birth HEIGHT`.
* A new wallet starts scanning at the node's tip when it is made, so it does not read old blocks that cannot hold its
  coins. `--birth HEIGHT` sets it by hand (use it when restoring and you know the first block that paid you).
* `balance` scans what is new and saves the wallet file; it shows total, spendable, immature (waiting for maturity) and
  reserved (promised to a payment that a block has not yet taken in). `pay` builds the payment with a real FCMP++ membership
  proof (the coin is one of every output on the chain) and range proofs, checks it itself, hands it to the node, and saves the reservation. **A payment counts once a block
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
.\target\release\tenero-miner.exe --data $HOME\tenero-test\n1 --address TENt... --backend sha256              # the test network
.\target\release\tenero-miner.exe --data $HOME\tenero-gamma --control 127.0.0.1:38352 --address TENg... --backend gpu   # the gamma network
```

* `--data` is the node's data directory (the miner reads the node's cookie from it); `--control` is the node's control address
  (the test network's default, 127.0.0.1:18332, otherwise); `--address` is where block rewards go; `--backend` is `sha256`
  (the test network), or `cpu` or `gpu` (the real proof of work of `gamma` and `dev`), and a backend that does not fit the node's
  network is refused with a message. `--cores`, `--gpu-device`, `--gpu-batch`, `--pace`, `--log-level`, `--log-file` and
  `--status-every` work as the node's settings of the same names; `tenero-miner help` lists them.
* **A node on another computer:** `--node HOST:PORT --key HEX` (instead of `--data` and `--control`) connects to that node's miner
  service (`miner_listen` and `miner_key` above) over an encrypted channel. The miner then **checks every block it is given**: it
  refuses one whose reward output does not pay `--address` (it rebuilds the Carrot output from the address and the template's anchor), whose header does not match the
  transactions' ids, or that is for another height or tip, because a node you do not run could otherwise put its own address in the reward. It cannot check that the difficulty is
  right or that the node's chain is the real one: a bad node can waste your hashing (see `docs/REMOTE_MINING_PLAN.md`). It asks
  once a second, so the node's default of 120 requests a minute is enough.
* **The node checks every block completely.** A found block goes to the node as a local block, so the node's own
  proof-of-work and proof checks decide whether it joins the chain; the miner only reports what the node said (in the chain,
  lost a race, or refused).
* **It does not mine while the node is syncing,** replaces its job when the tip moves (or the template is a minute old), and
  **carries on if the node restarts**: it reconnects (waiting for the node's new cookie) and starts again. Start it before or
  after the node.
* **Memory:** with `--backend cpu` the miner builds its own 4.3 GiB dataset, as the node's check does, so the node and the
  miner together need about 9 GiB on `gamma` or `dev`, or more: the miner keeps the last two epochs. `--backend gpu` keeps its datasets in video memory instead.
* Stop it with Ctrl-C (it prints how many blocks it found and what became of them).

### Mining for a pool: `--pool` (no node needed)

```powershell
.\target\release\tenero-miner.exe --pool default --network gamma --address TENg... --backend gpu
.\target\release\tenero-miner.exe --pool pool.example:38335 --pool-key 64HEXDIGITS --network gamma --address TENg... --backend gpu
```

A miner that works for a **pool** needs **no node at all**: it connects to the pool over an encrypted channel, is given block headers to search in its own slice of the
nonces, and hands in every *share* it finds (a solution to an easier target than a block's). **The block rewards go to the pool, which pays `--address` by its own rules;
nothing in the protocol or the chain makes a pool pay.** The program says so every time it connects. A wallet to receive the payments still needs a node to scan the chain
(`docs/REMOTE_MINING_PLAN.md`: a wallet on someone else's node is for later).

* `--pool HOST:PORT` (a name is looked up) replaces `--data`, `--control`, `--node` and `--key`: a miner works for a pool **or** mines on a node, never both, and never switches
  by itself. `--network NET` says which network the pool must serve (a pool of another is refused); `--worker NAME` is a name for this computer (default: its name).
* **The pool's key is pinned.** `--pool-key` is the pool's public key (64 hexadecimal digits, from the pool's operator, by a way an attacker cannot also change). A pool that
  proves another key is refused before anything is sent. `--pool-unpinned` goes without (a person between you and the pool would not be noticed). `--pool default` is the pool
  built into the program for `--network`: **one, the author's test pool on `gamma`** (`195.26.244.245:38335`, its key `4eb53ae8...770a` built in and pinned), none for the other networks (it says so). It is one computer run by one
  person, and it keeps the block rewards.
* A share is checked before it is sent (it must meet the share target); the pool checks every one again. The status shows shares, not blocks: handed in, accepted, too late, refused.
* The app does the same from the **Mining tab**: "On a pool" (no node needed) or "Alone, on my own node".

**The pool itself** is `tenero-pool` (`docs/RUNNING_A_POOL.md`), and `tenero-poolcheck --pool HOST:PORT --network NET --pool-key HEX` checks any pool against the protocol
(`docs/POOL_PROTOCOL.md`).

## The wallet app: `tenero-wallet-gui` (M10.3)

A desktop window (not a web page) for the wallet that also starts and stops the node and the miner. **Experimental, unaudited; nothing on the networks it uses has value.** The banner "TEST NETWORK. NO VALUE. UNAUDITED." is on every screen.

```powershell
cargo build --release -p tenero-gui -p tenero-app --bins      # the app and the programs it starts
.\target\release\tenero-wallet-gui.exe                        # tenerod and tenero-miner are looked for next to it
```

* **The wallet opens first; the node is started from it** (Node tab). No terminal window opens: the node and the miner run hidden and write what they print to `node-output.txt` and `miner-output.txt` in the app folder (the Node and Mining tabs show the last lines).
* **Closing the window stops the miner and a node this window started** (cleanly: it waits up to a minute for the node to finish writing, then ends its own handle to it). A node it only found already running is left running. The app never stops a process it did not start itself and never looks one up by name.
* **First run:** create a wallet (a password, or none after a warning), write down the **24 words**, and type three of them back; or restore from 24 words. The words are the wallet; the password only locks the file on this computer. A password can be changed later; the words are shown again only after the password is typed again, and are never put on the clipboard.
* **Several wallets (the owner's request, 2026-10-03):** each is its own file `NAME.twl` in the wallets folder (`wallets-gamma`, `wallets-dev` or `wallets-test` under the app folder), with its own seed, password and accounts. "Lock / switch wallet" (top right) goes back to the list; pick one, type its password. "Create another wallet" and "Restore another wallet from 24 words" ask for a name (letters, digits, spaces, - and _; no clash with an existing name, case aside). A wallet file of the first versions (`wallet-<network>.twl` in the app folder) is listed too and stays where it is. Nothing ever overwrites another wallet's file. The selected wallet is remembered.
* **Payment requests (Receive tab):** a request is a link `tenero:<address>?amount=1.5&label=Rent&message=...` and a QR code of it, kept in the wallet file so it can be shown again. In Send, "Paste a payment request or an address" fills in the address and the amount and carries the label to the confirmation screen and the history ("for Rent"). A link with anything the wallet does not understand is refused. **A request is not an invoice and is not marked paid.**
* **The desktop shortcut and the icon:** the window and taskbar icon is the circular logo. `tenero-wallet-gui.exe --app-dir FOLDER` opens a particular app folder (what a shortcut uses). `assets/tenero.ico` is the same logo for shortcuts (made by `python tools/make_icons.py`). Since M11.3 the icon (and the version information) is also compiled into `tenero-wallet-gui.exe` itself (`crates/tenero-gui/build.rs`, `tenero.rc`; Windows only).
* **Accounts:** several per wallet, each with its own address and balance, all from the same 24 words. A payment comes from one account. Restoring finds the accounts that were used (it stops after 3 unused ones in a row; add a later one by hand). Account names and whom each payment went to are kept only in the wallet file: a wallet restored from the words (or a view-all wallet) lists its payments out, found on the chain, with the amount and the fee but not the recipient; a view-received wallet sees no payments out.
* **Send:** an address, an amount, and one of three fee levels shown with their price: **Low** (1.25 times the minimum fee), **Normal** (2 times), **High** (5 times). A higher fee only buys a better place when the pool is full. A confirmation screen shows everything before anything is sent.
* **Receive:** the address, a Copy button and a QR code. **History:** what was received, mined and sent, and where each sent payment stands (waiting, taken in, dropped).
* **Sign, verify and prove (Prove tab, and buttons on History):** sign a message with an account; verify a signature (needs only the address, the message and the signature: no wallet, no node); make a payment proof for a payment you sent ("Prove payment") or an output you received ("Prove receipt", signed by the receiving address); check a proof against the node (no wallet needed); or check a **payment key and an address** (the payment's 32-hex-digit anchor: the node's chain is read from the block you give, up to 50,000 blocks, until the output it made is found, so give a block at or before the payment). "Show payment key" shows the secret of a sent payment only when you click it, in its own window, and never copies it by itself. **Our own construction, reviewed by nobody** (Monero and the Carrot specification define none yet), and a proof shows the amount and the address to whoever you give it to and nothing about who sent it (`docs/WALLET_PROOFS.md`).
* **View key (Settings, "Show a view key…"):** after the password again, an account's view key, view-all or view-received, for a view-only wallet.
* **View-only wallets:** "View-only, from a view key" on the welcome screen (or "Add a view-only wallet" when a wallet is locked) makes one from a `TENview1...` key, with a name and a password like any wallet; a file `tenero-wallet restore-view` wrote opens too. It says VIEW-ONLY at the top and has one account. It shows what it receives, and a view-all one the balance and its payments out; **a view-received one shows what was RECEIVED, not a balance** (it cannot see what was spent). It cannot send, sign, show 24 words or add accounts (the window does not offer them, and the wallet refuses them anyway). It can make integrated addresses and prove a payment it received (unsigned).
* **Mining:** off until you press Start; says what it uses (the GPU at full load, or CPU cores); shows the rate over 10 s, 60 s, 15 min and the run, the card's temperature, power and clocks, and the blocks found. It stops when you lock the wallet or stop the node.
* **A balance is never shown as final** while the wallet is reading the chain or no node is running (without a node there is no number at all).
* **Settings** are in `settings-v3.conf` in the app folder (0.2.0's `settings.conf` is left alone for the 0.2.0 app) (`%LOCALAPPDATA%\Tenero`, or `TENERO_APP_DIR`): no secrets in it. One node folder and one wallet file per network; the app runs `gamma` unless told otherwise, with a pruned node.
* **What is not built yet:** a payment *request* with a label, exporting the history, a transaction detail view, and a wallet-file folder permission warning (the block explorer is a program of its own: below). The window has been drawn and read in automated tests (all screens, all states) but **how it looks and feels is checked by hand**.

## The block explorer: `tenero-explorer`

A window onto a node running on this computer. It shows the height, the **difficulty** (the expected number of proof-of-work attempts for the next block, worked out exactly from the target), an **estimated network hash rate** and the mean block time, the coins **emitted** so far against the cap, the block reward, the **transaction pool** (each transaction's hash, when this node received it, its fee, its fee rate and its size, best fee rate first), and the **latest 30 blocks** (height, time, size, transactions, what the coinbase paid, hash). A hash is shown cut short; hover over it to see all of it, click to copy it. It asks the node again every 5 seconds ("Refresh now" asks at once), and it only reads: it cannot send anything or change the node.

```powershell
cargo build --release -p tenero-explorer
.\target\release\tenero-explorer.exe                                   # the node the wallet app runs (its settings say where)
.\target\release\tenero-explorer.exe --data $HOME\scratch --network test # any node: its data folder, and its network or --control IP:PORT
```

* **The hash rate is an estimate.** Nobody can measure it. It is the work the last 30 blocks proved divided by the time their timestamps say they took. Miners write those timestamps, and over a few blocks luck matters, so read it as a rough figure.
* **Emitted** is the schedule's total (the base rewards of every block so far), not what was paid: an oversize penalty, which creates fewer coins, is not subtracted. Past the cap the tail goes on for ever.
* **"Received"** is when *this* node got a transaction (its own clock). It is "unknown" for a transaction taken before the node was updated to record it.
* **Everything shown is what one node says.** The explorer cannot tell whether that node is on the best chain; it shows whether the node says it is catching up.
* It talks to the node through the control interface with the node's cookie (requests `headers`, `mempool` and `chain_stats`, added 2026-10-07: `docs/CONTROL_PROTOCOL.md`), so it runs only on the node's own computer and needs a node of this version or later.

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

## Not done yet
* **A Windows service or an installer** (a server's systemd units are in `docs/RUNNING_A_SEED.md` and `docs/RUNNING_A_POOL.md`). Run it in a window, or under a scheduler you trust.
* **Tor or I2P,** and any encryption of the control interface (it never leaves the machine).
* **A remote wallet** (a wallet on someone else's node) for a computer without the memory for a node.
* **A warning while the crowd is small:** the first spends on `gamma` hide among very few outputs, and the wallet does not say so yet.
