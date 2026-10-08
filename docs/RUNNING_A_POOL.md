# Running a mining pool (a guide for an operator; updated for `gamma`, 2026-10-08)

**Experimental and unaudited; nothing on `gamma` or any other Tenero network has any value, and a pool is the part of this project that has run least.** The pool was built and tested on one computer (the SHA-256 test chain, real sockets, real programs) and ran on one server on `beta` (2026-10-07) for one GPU miner; it has **not** run on `gamma` yet, or with more than a handful of miners. Every number below that is not marked "measured" is an estimate or a default.

## What the pool is, and what its miners must be told

`tenero-pool` sits in front of a node of its own. It gives each connected miner a **job** (a block header to search, with the pool's wallet as the block's reward), takes the miner's **shares** (solutions to an easier target than a block's), checks every one by recomputing the proof of work, counts them, hands a block to the node when a share is one, and **pays the miners from its own wallet**.

**The block rewards go to the pool.** The pool pays the miners by the rules below; nothing in the protocol or the chain makes it. A pool operator should say this to every miner, together with the fee, the scheme and the smallest payout (a page, the pool's name, the README). The miner programs say it every time they connect.

## The rules this pool follows

| | |
|---|---|
| **Scheme** | **PPLNS**, pay per last N shares (the owner's choice, 2026-10-07): when a block is found, its reward is shared among the newest shares whose work adds up to **`window` times the block's work** (default 2), in proportion to each share's work, whatever rounds they fell in. A miner who joins late or leaves early is paid for the shares they have in the window. |
| **A share's worth** | the attempts its target stands for: `floor(2^256 / share target)`. A share at a target 16 times easier than the block's is worth a sixteenth of a block of work, whatever the miner's speed. |
| **Difficulty** | each miner has its own share target, the block target times a ratio that starts at 16 and is looked at every 30 seconds: it moves by the factor by which the shares came faster or slower than one every 15 seconds, at most 4 times either way. (So a CPU and a GPU both find their level in a few minutes.) |
| **Fee** | `fee` percent of every block, **default 0** (a test pool). It comes off first. Publish it. A percentage with **up to four decimals** (`0.5` is a half of one percent, `0.05` a twentieth of one percent; the pool keeps it in parts per million of the reward, so the smallest step is 0.0001 %); anything else is refused at start. It is **rounded down**: the odd unit goes to the miners. It applies to blocks found after it is set; the credits of blocks already found are not changed. (Before this change only whole percents could be set.) |
| **When a reward counts** | a block is credited to the miners only when it is **as deep as the coinbase maturity** (60 blocks on `gamma`) **and is still the node's block at its height**. A block another one replaced pays nobody, because the pool never received the coins. |
| **Payouts** | once every `payout-every` seconds (**default 3600: an hour**; the next time is kept in the state file, so a restart does not bring a payout forward), every miner owed at least `min-payout` (**default 0.1 coins**) is paid, largest first, at most 500 a round, in as many transactions as it takes (15 recipients to a transaction; `Wallet::build_batch`). Coins that come back as change are spendable after 10 blocks, so a round that runs out of separate coins pays what it can and the rest waits for the next round (10 minutes later). |
| **Network fees** | **paid by the pool**, out of its wallet, not taken from the miners. |
| **What a miner needs** | an address to be paid at. Anyone may use anyone's address: the pool does not know who a miner is. |
| **The record** | `payments.log` in the pool's data folder: one line for every payment, `time transaction-id address amount-in-units`, so a miner can check what it was paid against the chain. |

## What fee to set (an estimate, to be replaced by a measurement)

**Recommended for a small pool: `fee = 0.05`** (a twentieth of one percent). **It is an estimate** built from the two payouts the author's test pool made on `beta` (2026-10-07: 0.031 and 0.029 coins of network fee on 21.97 and 18.03 coins paid, about 0.015 %, each transaction about 9 KB to one recipient) and the fee rule, **not a measurement of a busy pool**. On `gamma` a transaction is about three times bigger and its fee rate a third (`FEE_REFERENCE_WEIGHT` 1000 against 3000), so the estimate is about the same; **not yet measured on `gamma`**:

* **Why a fee at all:** the pool pays the network fee of every payout out of its own wallet (a miner is paid the full amount), and at a fee of 0 it credits miners the whole reward, so its wallet ends up short by exactly the fees it has paid. A small fee keeps a reserve in the wallet.
* **Why the fee never has to be a guess about a rise:** the minimum fee falls when the block-size median rises and when the reward halves, and the median never goes below 150,000, so **under today's rules the price of a unit of weight is already the highest it can be** (`docs/CONSENSUS_V2.md` 15).
* **What it costs, assuming 0.03 to 0.04 coins a transaction of 15 recipients (not measured for 15 recipients) and 1,200 coins of rewards an hour:** one miner paid each hour about 0.0025 % of the rewards; 100 miners about 0.02 %; **500 miners about 0.09 to 0.12 %**. So `0.05` covers a pool of up to about 250 miners paid every hour, and a pool of 500 should start at `0.1` and re-measure. Nothing here is a reason to charge more than the pool needs: publish the fee.
* **Many small payments cost the most:** a payment at the minimum payout (0.1 coins) costs the pool about as much as a large one, so a pool with many miners near the minimum should raise `min-payout` or `payout-every` before it raises its fee.
* **Measure it:** every payout round logs `payout: N payments in N transactions, X units paid, fees F`. This prints what the fees were as a share of what was paid, over the whole log:

      grep "payout:" /var/lib/tenero-pool/pool.log | awk '{for(i=1;i<=NF;i++){if($i=="units"){p+=$(i-1)} if($i=="fees"){f+=$(i+1)+0}}} END{printf "paid %d units, fees %d units, fees are %.4f%% of paid\n",p,f,100*f/p}'

  The fee must be at least that share, with room to spare. A fee is **kept by the pool in its wallet**; it is not a reserve anyone else can draw on.

## What it needs

* **A node of its own**, the same version, on the same network, that mines nothing: `tenerod` with `network = gamma`. It builds the blocks and takes the pool's. It must be a node **of its own**, not the seed (see "Beside a seed" below).
* **4 GiB of memory for its node** (a node holds one dataset at a time, 4.01 GiB measured). **The pool itself holds no dataset**: it checks every share's mix by asking its node (`check_pow` on the control interface), so it needs little memory of its own. (The first version held a dataset of its own: a seed, a pool node and a pool were then about 12 GiB, more than the 11 GiB the owner's server has. That was found before any miner connected, and is why the pool asks its node.) **Estimated, not yet measured on a server:** a machine with a seed and a pool's node needs about **8.2 GiB** of dataset and a little more.
* **A wallet of its own** (`tenero-wallet create`, a `gamma` wallet), used for nothing else. Every block pays it; the miners are paid from it. **Its passphrase is in a file on the server**: a person who gets onto the server gets the coins. That is acceptable for coins that have no value, and it is the reason this is not a design for a pool of real money.
* **One open port**, TCP, for miners: **38335** by default.

## Setting it up (the commands are what the programs' documentation says; run on one server in 2026-10-07 only as far as the owner's report says)

All as `root` on an Ubuntu 22.04 or newer server. The names are a choice; change them everywhere if you change one.

1. **A user with no login and no privileges** for the pool and its node: `useradd --system --create-home --home-dir /var/lib/tenero-pool --shell /usr/sbin/nologin tenero-pool`.
2. **The programs**, from a release or a build: `tenerod`, `tenero-pool`, `tenero-wallet` and `tenero-poolcheck` into `/opt/tenero-pool/` (`install -o root -g root -m 755 FILE /opt/tenero-pool/`). Nothing there may be writable by the pool's user.
3. **The node's settings**, `/var/lib/tenero-pool/node.conf` (the control port is **different from the seed's**; the node **dials out** and, if you want other nodes to find it, also listens: see the last three lines below and "Beside a seed"):

       data = /var/lib/tenero-pool/node
       network = gamma
       control = 127.0.0.1:38362
       log_file = /var/lib/tenero-pool/node.log
       listen = 0.0.0.0:38356
       advertise = 0.0.0.0:38356
       max_inbound = 16

4. **The wallet.** Make a passphrase and the wallet (`create` prints the seed once: write it down if the coins matter to you):

       mkdir -p /var/lib/tenero-pool/pool && cd /var/lib/tenero-pool/pool
       head -c 24 /dev/urandom | base64 > pass.txt
       /opt/tenero-pool/tenero-wallet create --wallet pool.wallet --birth 0 --passphrase-file pass.txt
       chown -R tenero-pool:tenero-pool /var/lib/tenero-pool && chmod 700 /var/lib/tenero-pool /var/lib/tenero-pool/pool && chmod 600 pass.txt pool.wallet

5. **The pool's settings**, `/var/lib/tenero-pool/pool.conf` (the names are those of the command line):

       data = /var/lib/tenero-pool/pool
       network = gamma
       node-data = /var/lib/tenero-pool/node
       control = 127.0.0.1:38362
       wallet = /var/lib/tenero-pool/pool/pool.wallet
       passphrase-file = /var/lib/tenero-pool/pool/pass.txt
       listen = 0.0.0.0:38335
       name = Tenero gamma test pool
       fee = 0
       min-payout = 0.1
       payout-every = 3600
       log-file = /var/lib/tenero-pool/pool.log

6. **The firewall**: allow the pool's port and the node's: `ufw allow 38335/tcp` and `ufw allow 38356/tcp`. The node's control port is on loopback and is not opened.
7. **Two services** (these files have **not** been run; `MemoryMax`, `CPUWeight` and `Nice` are what keep the pool from taking a seed's room: see "Beside a seed"). `/etc/systemd/system/tenero-pool-node.service`:

       [Unit]
       Description=Tenero pool's node (gamma; experimental)
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
       Description=Tenero mining pool (gamma; experimental)
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
9. **The pool's key.** The pool makes it at its first start, **with the default file mode (readable by every user of the machine: found on the author's server, 2026-10-07)**, so run `chmod 600 /var/lib/tenero-pool/pool/pool.key /var/lib/tenero-pool/pool/pool-state.dat` after it; the folder's mode 700 (step 4) is what protects it until then, and `node.key` and `control.cookie` in the node's folder are the same. `tenero-pool key --data /var/lib/tenero-pool/pool` prints it again (64 hexadecimal digits). **Miners pin it**: give it to them by a way an attacker cannot also change (the pool's page, the README, the project's issue), not only on the pool's own screen.
10. **Check it from outside**: `tenero-poolcheck --pool IP:38335 --network gamma --pool-key KEY`. Every check should pass (two are skipped on a network with the real proof of work: they need a valid share and so the 4 GiB dataset; try those with a real miner).

## Beside a seed on one machine

A seed is a public node that must stay up. A pool beside it must not take its room. What this guide does, and **what nobody has measured**:

* **Separate users, folders, services, ports.** The seed's data, key and ports (38353 and, on loopback, 38352) are never touched; the pool node uses control 38362 and peer port 38356; the pool listens on 38335. The pool never talks to the seed's control interface (it has its own node).
* **Memory.** Each service has `MemoryMax=6G`: if the pool or its node grew without bound, the system kills **it**, not the seed. The seed has the room it had.
* **CPU.** `CPUWeight=50` and `Nice=5`: under load the seed is served first. The proof-of-work check of a share is one attempt, done **on the pool node's own thread** (about 36 ms of one CPU core on the real proof of work: from the CPU speed measured in `docs/BENCHMARKS.md`, an estimate for this use). **At the default difficulty each miner sends a share about every 15 seconds**, so 256 miners are about 17 shares a second, which would keep the pool's node thread about 60 % busy: **an estimate to check, and the limit of one pool node**. The seed's node is not touched: only the pool's own node does the checking.
* **The pool's node dials the seeds** like any other node (both `gamma` seeds are built in; a `seed = IP:PORT` line in its `node.conf` adds another). Look at the pool node's log for `peers`. If a server cannot dial its own public address, give the node `seed = ` another node's address. **On `beta` the author's pool node listened on 38336** (on `gamma`: 38356, as in the settings above), so another node CAN connect to it (a TCP connection to 38336 from another computer on the internet succeeded, and a fresh node on a Windows PC, given only this node's address as its seed, synced the chain from it, 2026-10-07), **but a node never dials a host it already holds a connection to, whatever the port (`engine.rs`, `maintain_connections` and `dial_trusted`), so a node connected to the seed will not also dial a pool node on the seed's IP**; the pool node is reached only by a node that has no connection to that IP. **Since 2026-10-07 the pool node has a second seed on another IP** (`seed = 194.238.27.60:38343`: a second server of the author's, set up as in `docs/SERVER_UPGRADE_BETA2.md`), and the author read `peers 2` on it afterwards (which two peers they are was not looked at; the two seeds are what is expected). **Both seeds are one person's servers**: if both are down and no other node has connected, the blocks the pool finds reach nobody, and when the node reaches a longer chain the shorter one is dropped. A seed run by someone else is the real fix and does not exist yet. A listening node is also a public target: **keep the pool's wallet and passphrase files out of the node's reach.** On the author's server the node's unit has a drop-in with `InaccessiblePaths=/var/lib/tenero-pool/pool` (and `PrivateDevices`, `ProtectKernelTunables`, `ProtectKernelModules`, `ProtectControlGroups`, `RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX`, `RestrictNamespaces`, `RestrictRealtime`, `LockPersonality`, `CapabilityBoundingSet=`, `SystemCallArchitectures=native`), checked on 2026-10-07 with `nsenter` as the service user (`Permission denied` on the wallet's folder); **it is not in the setup steps above yet**, and nothing but that check has tested it.
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
