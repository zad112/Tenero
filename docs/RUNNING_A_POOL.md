# Running a mining pool (a guide for an operator, 2026-10-07)

**Experimental and unaudited; nothing on `beta` or any other Tenero network has any value, and a pool is the part of this project that has run least.** The pool was built and tested on one computer (the SHA-256 test chain, real sockets, real programs); it has **not** been run on a server, with the real proof of work, or with more than a handful of miners. Every number below that is not marked "measured" is an estimate or a default.

## What the pool is, and what its miners must be told

`tenero-pool` sits in front of a node of its own. It gives each connected miner a **job** (a block header to search, with the pool's wallet as the block's reward), takes the miner's **shares** (solutions to an easier target than a block's), checks every one by recomputing the proof of work, counts them, hands a block to the node when a share is one, and **pays the miners from its own wallet**.

**The block rewards go to the pool.** The pool pays the miners by the rules below; nothing in the protocol or the chain makes it. A pool operator should say this to every miner, together with the fee, the scheme and the smallest payout (a page, the pool's name, the README). The miner programs say it every time they connect.

## The rules this pool follows

| | |
|---|---|
| **Scheme** | **PPLNS**, pay per last N shares (the owner's choice, 2026-10-07): when a block is found, its reward is shared among the newest shares whose work adds up to **`window` times the block's work** (default 2), in proportion to each share's work, whatever rounds they fell in. A miner who joins late or leaves early is paid for the shares he has in the window. |
| **A share's worth** | the attempts its target stands for: `floor(2^256 / share target)`. A share at a target 16 times easier than the block's is worth a sixteenth of a block of work, whatever the miner's speed. |
| **Difficulty** | each miner has its own share target, the block target times a ratio that starts at 16 and is looked at every 30 seconds: it moves by the factor by which the shares came faster or slower than one every 15 seconds, at most 4 times either way. (So a CPU and a GPU both find their level in a few minutes.) |
| **Fee** | `fee` percent of every block, **default 0** (a test pool). It comes off first. Publish it. |
| **When a reward counts** | a block is credited to the miners only when it is **as deep as the coinbase maturity** (60 blocks on `beta`) **and is still the node's block at its height**. A block another one replaced pays nobody, because the pool never received the coins. |
| **Payouts** | once every `payout-every` seconds (**default 3600: an hour**; the next time is kept in the state file, so a restart does not bring a payout forward), every miner owed at least `min-payout` (**default 0.1 coins**) is paid, largest first, at most 500 a round, in as many transactions as it takes (15 recipients to a transaction; `Wallet::build_batch`). Coins that come back as change are spendable after 10 blocks, so a round that runs out of separate coins pays what it can and the rest waits for the next round (10 minutes later). |
| **Network fees** | **paid by the pool**, out of its wallet, not taken from the miners. |
| **What a miner needs** | an address to be paid at. Anyone may use anyone's address: the pool does not know who a miner is. |
| **The record** | `payments.log` in the pool's data folder: one line for every payment, `time transaction-id address amount-in-units`, so a miner can check what it was paid against the chain. |

## What it needs

* **A node of its own**, the same version, on the same network, that mines nothing: `tenerod` with `network = beta`. It builds the blocks and takes the pool's. It must be a node **of its own**, not the seed (see "Beside a seed" below).
* **4 GiB of memory for its node** (a node holds one dataset at a time, 4.01 GiB measured). **The pool itself holds no dataset**: it checks every share's mix by asking its node (`check_pow` on the control interface), so it needs little memory of its own. (The first version held a dataset of its own: a seed, a pool node and a pool were then about 12 GiB, more than the 11 GiB the owner's server has. That was found before any miner connected, and is why the pool asks its node.) **Estimated, not yet measured on a server:** a machine with a seed and a pool's node needs about **8.2 GiB** of dataset and a little more.
* **A wallet of its own** (`tenero-wallet create`), used for nothing else. Every block pays it; the miners are paid from it. **Its passphrase is in a file on the server**: a person who gets onto the server gets the coins. That is acceptable for coins that have no value, and it is the reason this is not a design for a pool of real money.
* **One open port**, TCP, for miners: **38335** by default.

## Setting it up (the commands are what the programs' documentation says; run on one server in 2026-10-07 only as far as the owner's report says)

All as `root` on an Ubuntu 22.04 or newer server. The names are a choice; change them everywhere if you change one.

1. **A user with no login and no privileges** for the pool and its node: `useradd --system --create-home --home-dir /var/lib/tenero-pool --shell /usr/sbin/nologin tenero-pool`.
2. **The programs**, from a release or a build: `tenerod`, `tenero-pool`, `tenero-wallet` and `tenero-poolcheck` into `/opt/tenero-pool/` (`install -o root -g root -m 755 FILE /opt/tenero-pool/`). Nothing there may be writable by the pool's user.
3. **The node's settings**, `/var/lib/tenero-pool/node.conf` (the control port is **different from the seed's** and the node does **not listen**: it only dials out):

       data = /var/lib/tenero-pool/node
       network = beta
       control = 127.0.0.1:38352
       prune_keep = 0
       log_file = /var/lib/tenero-pool/node.log

4. **The wallet.** Make a passphrase and the wallet (`create` prints the seed once: write it down if the coins matter to you):

       mkdir -p /var/lib/tenero-pool/pool && cd /var/lib/tenero-pool/pool
       head -c 24 /dev/urandom | base64 > pass.txt
       /opt/tenero-pool/tenero-wallet create --wallet pool.wallet --birth 0 --passphrase-file pass.txt
       chown -R tenero-pool:tenero-pool /var/lib/tenero-pool && chmod 700 /var/lib/tenero-pool /var/lib/tenero-pool/pool && chmod 600 pass.txt pool.wallet

5. **The pool's settings**, `/var/lib/tenero-pool/pool.conf` (the names are those of the command line):

       data = /var/lib/tenero-pool/pool
       network = beta
       node-data = /var/lib/tenero-pool/node
       control = 127.0.0.1:38352
       wallet = /var/lib/tenero-pool/pool/pool.wallet
       passphrase-file = /var/lib/tenero-pool/pool/pass.txt
       listen = 0.0.0.0:38335
       name = Tenero beta test pool
       fee = 0
       min-payout = 0.1
       payout-every = 3600
       log-file = /var/lib/tenero-pool/pool.log

6. **The firewall**: allow the pool's port and nothing new besides it: `ufw allow 38335/tcp`. The node's control port is on loopback and is not opened.
7. **Two services** (these files have **not** been run; `MemoryMax`, `CPUWeight` and `Nice` are what keep the pool from taking a seed's room: see "Beside a seed"). `/etc/systemd/system/tenero-pool-node.service`:

       [Unit]
       Description=Tenero pool's node (beta; experimental)
       After=network-online.target
       Wants=network-online.target

       [Service]
       User=tenero-pool
       ExecStart=/opt/tenero-pool/tenerod --config /var/lib/tenero-pool/node.conf
       Restart=on-failure
       RestartSec=10
       MemoryMax=6G
       CPUWeight=50
       Nice=5
       NoNewPrivileges=true
       ProtectSystem=strict
       ReadWritePaths=/var/lib/tenero-pool
       PrivateTmp=true
       ProtectHome=true

       [Install]
       WantedBy=multi-user.target

   and `/etc/systemd/system/tenero-pool.service`:

       [Unit]
       Description=Tenero mining pool (beta; experimental)
       After=tenero-pool-node.service
       Requires=tenero-pool-node.service

       [Service]
       User=tenero-pool
       ExecStart=/opt/tenero-pool/tenero-pool --config /var/lib/tenero-pool/pool.conf
       Restart=on-failure
       RestartSec=10
       MemoryMax=6G
       CPUWeight=50
       Nice=5
       NoNewPrivileges=true
       ProtectSystem=strict
       ReadWritePaths=/var/lib/tenero-pool
       PrivateTmp=true
       ProtectHome=true

       [Install]
       WantedBy=multi-user.target

8. **Start the node first and let it catch up** (`systemctl enable --now tenero-pool-node`; the log says `syncing` until it has the chain; the pool hands out no job while the node is syncing). Then `systemctl enable --now tenero-pool`.
9. **The pool's key.** The pool makes it at its first start; `tenero-pool key --data /var/lib/tenero-pool/pool` prints it again (64 hexadecimal digits). **Miners pin it**: give it to them by a way an attacker cannot also change (the pool's page, the README, the project's issue), not only on the pool's own screen.
10. **Check it from outside**: `tenero-poolcheck --pool IP:38335 --network beta --pool-key KEY`. Every check should pass (two are skipped on a network with the real proof of work: they need a valid share and so the 4 GiB dataset; try those with a real miner).

## Beside a seed on one machine

A seed is a public node that must stay up. A pool beside it must not take its room. What this guide does, and **what nobody has measured**:

* **Separate users, folders, services, ports.** The seed's data, key and ports (38343 and, on loopback, 38342) are never touched; the pool node uses control 38352 and does not listen; the pool listens on 38335. The pool never talks to the seed's control interface (it has its own node).
* **Memory.** Each service has `MemoryMax=6G`: if the pool or its node grew without bound, the system kills **it**, not the seed. The seed has the room it had.
* **CPU.** `CPUWeight=50` and `Nice=5`: under load the seed is served first. The proof-of-work check of a share is one attempt, done **on the pool node's own thread** (about 36 ms of one CPU core on the real proof of work: from the CPU speed measured in `docs/BENCHMARKS.md`, an estimate for this use). **At the default difficulty each miner sends a share about every 15 seconds**, so 256 miners are about 17 shares a second, which would keep the pool's node thread about 60 % busy: **an estimate to check, and the limit of one pool node**. The seed's node is not touched: only the pool's own node does the checking.
* **The pool's node dials the seed** like any other node (the seed's address is built in). Look at the pool node's log for `peers 1`. If a server cannot dial its own public address, give the node `seed = ` another node's address.
* If memory is short, **stop the pool first**: `systemctl stop tenero-pool tenero-pool-node`. The seed does not depend on it.

## Running it, and what it logs

`journalctl -u tenero-pool -f`, or the `log-file`. A status line every minute: miners connected, shares (and how many were stale or refused), blocks found (in the chain, lost to another block, refused by the node), what is owed, how many blocks are maturing, what has been paid. A payout round logs how many payments, transactions and units it paid and what it cost in network fees.

* **The books** are in `pool-state.dat` (written atomically, with a checksum, after every block and payout and every few seconds). **If the file is damaged the pool will not start over it** (it says so): move it away to start with empty books, and know that what the miners were owed is gone.
* **The wallet file** is saved after every payout that sent anything (the reservations are in it).
* **If the node is down** the pool says so and tries again; no jobs are handed out and shares for the old jobs are stale.

## Limits and defaults

256 miners at once, 4 connections from one address, 10 s to finish the handshake, 5 minutes of silence closes a connection, 600 shares a minute for one connection, and 20 bad shares in a row ban the address for 10 minutes. All are settings or constants in `pool_server.rs`; **none has been measured**.

## What this pool does not do

* **Job declaration** (a miner building its own blocks): not supported; the pool offers no capability beyond version 1.
* **Payment proofs for a miner**: a miner sees its payments as ordinary received coins (the sender is the pool's wallet).
* **Anything about keeping the miners' addresses private**: the pool knows each miner's address and its internet address.
* **A web page, an API or a payout history for miners**: only the log and `payments.log`.
