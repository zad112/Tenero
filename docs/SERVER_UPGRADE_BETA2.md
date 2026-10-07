# Upgrading the servers for the `beta` hard fork at block 500 (v0.2.0-beta.2), and adding a second seed

**A DRAFT guide, written 2026-10-07 for the author's own servers. Nothing in it has been run: every command is a proposal for the owner to run one at a time, and the names of the first seed's service, folders and settings are ASSUMED (step 1 shows how to find the real ones).** Unaudited, experimental, no value on any network.

## What the fork does, and why the order matters

From height **500** on `beta`, a block must carry the mix of the *gathered* proof-of-work attempt (`docs/CONSENSUS.md` 8.3). Blocks up to 499 are the same for old and new programs.

* **A node of v0.2.0-beta.1 or earlier refuses every `beta` block from 500 on and stops at 499**, on a chain of its own; a miner of beta.1 makes blocks that beta.2 refuses.
* So **every beta node you run (the seed, the pool's node, the second seed, your own PC), the pool, and every miner must be on beta.2 before the chain reaches 500.** The chain only grows when someone mines, and today the author's GPU is the only miner: **stop mining before about height 480, upgrade everything, then mine again.** (The commit says `beta` was at height 280 when the fork was decided; read the real height in step 1.)
* **There is no going back after 500.** Before 500 the old build still works (keep it: step 2). After 500 an old build stalls at 499.
* The pool: **its node judges shares with the height-aware check** (`check_full(header, height)`), so a beta.2 node accepts old-rule shares below 500 and gathered-rule shares from 500. **Pool miners must be beta.2 too** (the miner switches rules at 500), or from 500 their shares are refused. That the pool works across height 500 on the real network has **not been tried**.

The fork changes the miner's speed, too (the release notes say about 125,000 attempts a second below 500 and about 45,000 above, on the author's card: the difficulty adjusts, and blocks will be slower for a while after 500: **expected, not measured**).

## Before you start

1. **A package.** Either the release (`tenero-0.2.0-beta.2-linux-x64.tar.gz` and `SHA256SUMS` from the release page, once the tag exists), or, before it, the *dry run* of the release workflow (Actions, release, "Run workflow" on the branch with the fork; the package is the run's artifact). Check its sha256 against the one GitHub shows **before** it goes near a server. The package holds `tenerod`, `tenero-miner`, `tenero-wallet`, `tenero-seedcheck`, `tenero-pool`, `tenero-poolcheck` and the wallet app.
   **Without the browser (used on 2026-10-07):** build the Linux programs from the tag in WSL (`git archive --format=tar.gz v0.2.0-beta.2`, piped into the distro, then `TENERO_COMMIT=<12 characters of the commit> cargo build --release --locked -p tenero-app` with the workflow's `CARGO_PROFILE_RELEASE_STRIP=debuginfo` and path remap), copy `tenerod`, `tenero-pool`, `tenero-wallet` and `tenero-seedcheck` out, and `scp` them. **They are NOT the published package's bytes** (builds are not shown to be bit-for-bit reproducible, and the draft's `SHA256SUMS` will not match them); they are the same source at the same commit, and each needs only glibc 2.34 (`objdump -T`). Check the `scp`'d file against the hash printed where it was built, and `--version` must say the tag's commit.
2. **Tell the testers** (the pinned issue https://github.com/zad112/Tenero/issues/1 is the only place a notice is posted; the Discord may repeat it): *"Beta nodes, wallet apps and miners must be upgraded to v0.2.0-beta.2 before block 500. Older versions stop at 499."*
3. **Copy the package to each server** (from your PC; `scp` asks for the key passphrase):

```
scp tenero-0.2.0-beta.2-linux-x64.tar.gz root@195.26.244.245:/root/
```

   and the same for `194.238.27.60`. On each server, unpack it and compare the hash:

```
cd /root && sha256sum tenero-0.2.0-beta.2-linux-x64.tar.gz && tar xzf tenero-0.2.0-beta.2-linux-x64.tar.gz
```

## Part 1: the first server (195.26.244.245: the beta seed, the pool's node and the pool)

### 1. Look before you change anything

```
systemctl list-units | grep -i tenero
```

```
systemctl cat tenero-beta | grep -E "ExecStart|User|WorkingDirectory"
```

   **Read on the author's server, 2026-10-07:** the seed is the service `tenero-beta` (user `tenero-beta`), started as `/opt/tenero-beta/tenerod --config /var/lib/tenero-beta/node.conf`; its settings: `data = /var/lib/tenero-beta/data`, `network = beta`, `listen = 0.0.0.0:38343`, `advertise = 195.26.244.245:38343` (no `control` line: the default for `beta`, `127.0.0.1:38342`). **Use your real names where yours differ.** The height of each node:

```
/opt/tenero-beta/tenerod status --data /var/lib/tenero-beta/data --control 127.0.0.1:38342
```

```
/opt/tenero-pool/tenerod status --data /var/lib/tenero-pool/node --control 127.0.0.1:38352
```

   **If a height is near 480 or more, stop mining first and tell the pool's miners to stop** (they will not be paid for blocks that do not come).

### 2. Keep the old programs (only good until block 500)

```
cp -a /opt/tenero-beta/tenerod /opt/tenero-beta/tenerod.beta1
```

```
cp -a /opt/tenero-pool/tenerod /opt/tenero-pool/tenerod.beta1 && cp -a /opt/tenero-pool/tenero-pool /opt/tenero-pool/tenero-pool.beta1
```

### 3. The seed (a clean stop, then the new program)

```
systemctl stop tenero-beta
```

```
install -o root -g root -m 755 /root/tenero-0.2.0-beta.2-linux-x64/tenerod /opt/tenero-beta/tenerod
```

```
systemctl start tenero-beta
```

```
/opt/tenero-beta/tenerod --version; systemctl is-active tenero-beta
```

   The version line must say `v0.2.0-beta.2` and the commit of the package. The chain stays on disk; the node needs a minute and then holds the 4 GiB dataset again.

### 4. The pool's node, then the pool (in this order, one after the other)

   First give the pool's node a second peer **on another IP**, which the second seed (Part 2) will be: add the line to its settings, if the second seed is already running:

```
echo "seed = 194.238.27.60:38343" >> /var/lib/tenero-pool/node.conf
```

```
systemctl stop tenero-pool tenero-pool-node
```

```
install -o root -g root -m 755 /root/tenero-0.2.0-beta.2-linux-x64/tenerod /opt/tenero-pool/tenerod && install -o root -g root -m 755 /root/tenero-0.2.0-beta.2-linux-x64/tenero-pool /opt/tenero-pool/tenero-pool
```

```
systemctl start tenero-pool-node
```

   Wait until the node's log says `in sync` (`journalctl -u tenero-pool-node --no-pager | grep "status:" | tail -1`), then:

```
systemctl start tenero-pool
```

```
/opt/tenero-pool/tenero-pool --version; systemctl is-active tenero-pool tenero-pool-node; grep "status:" /var/lib/tenero-pool/pool.log | tail -1
```

   The pool's `owed` and `paid` totals must be what they were (they live in `pool-state.dat`; a restart keeps them: seen on 2026-10-07). The fee in `pool.conf` is kept (check the `pays blocks` line).

### 5. The memory

   The seed, the pool's node and the pool were measured at 8.5 GiB used of 11 GiB (2026-10-07); **the gathered rule does not change the dataset's size**, but nobody has measured the new build: look once with `free -h` after both nodes have built their dataset.

## Part 2: the old alpha server (194.238.27.60) becomes the second beta seed

**This ends `alpha`:** its only seed goes away. The release notes of beta.1 told people it would last about a day; post the notice first. **The server has 7.8 GiB and no swap: one node (about 4.2 GiB) fits; the alpha seed and a beta seed together do not.**

1. **Clear it.** Easiest and cleanest: reinstall the operating system from the Contabo panel (it wipes everything; set a key login there yourself; afterwards `ssh-keygen -R 194.238.27.60` on your PC removes the old host key). By hand instead: `systemctl disable --now tenero-seed`, remove `/etc/systemd/system/tenero-seed.service`, `/opt/tenero`, `/var/lib/tenero`, the user `tenero`, and `ufw delete allow 38333/tcp`.
2. **Update the system:** `apt update && apt full-upgrade -y && reboot`.
3. **The user, the folders, the program:**

```
useradd --system --create-home --home-dir /var/lib/tenero-beta --shell /usr/sbin/nologin tenero-beta
```

```
mkdir -p /opt/tenero-beta && install -o root -g root -m 755 /root/tenero-0.2.0-beta.2-linux-x64/tenerod /opt/tenero-beta/tenerod && install -o root -g root -m 755 /root/tenero-0.2.0-beta.2-linux-x64/tenero-seedcheck /opt/tenero-beta/tenero-seedcheck
```

4. **The settings,** `/var/lib/tenero-beta/node.conf` (**`advertise` is this server's own address, never `0.0.0.0:PORT`: an old node punishes that form**). The built-in beta seed (195.26.244.245) is dialled by itself:

       data = /var/lib/tenero-beta/data
       network = beta
       listen = 0.0.0.0:38343
       advertise = 194.238.27.60:38343
       control = 127.0.0.1:38342
       prune_keep = 0
       log_file = /var/lib/tenero-beta/node.log

```
chown -R tenero-beta:tenero-beta /var/lib/tenero-beta && chmod 700 /var/lib/tenero-beta
```

5. **The service,** `/etc/systemd/system/tenero-beta.service` (as the pool's node's, plus an open-files limit, because a seed may hold many connections):

       [Unit]
       Description=Tenero beta seed (experimental)
       After=network-online.target
       Wants=network-online.target

       [Service]
       User=tenero-beta
       ExecStart=/opt/tenero-beta/tenerod --config /var/lib/tenero-beta/node.conf
       Restart=on-failure
       RestartSec=10
       MemoryMax=6G
       LimitNOFILE=65536
       NoNewPrivileges=true
       ProtectSystem=strict
       ReadWritePaths=/var/lib/tenero-beta
       PrivateTmp=true
       ProtectHome=true

6. **The firewall:** SSH and the seed port only.

```
ufw default deny incoming && ufw allow 22/tcp && ufw allow 38343/tcp && ufw enable
```

7. **Start it and let it sync** (it checks every block with the real proof of work and builds the 4 GiB dataset first: the first minutes are slow):

```
systemctl daemon-reload && systemctl enable --now tenero-beta
```

```
journalctl -u tenero-beta --no-pager | grep "status:" | tail -1
```

   It is ready when it says `in sync` at the same height as the first seed. Then, from your PC, check it the way a new node would (it cannot tell if a seed is honest, only that it answers):

```
tenero-seedcheck --network beta --seed 194.238.27.60:38343 --seed 195.26.244.245:38343
```

8. **Make it known.** Nodes learn about it through their peers. For a brand-new node it must be in the program's own list (`BETA_SEEDS` in `crates/tenero-app/src/config.rs`): that is a change for the next release. Until then it can be added by hand (`--seed 194.238.27.60:38343`, or the wallet app's Settings).
9. **Why a different server helps:** it is on another IP and another network group, so a node can hold connections to **both** seeds (a node never dials a host it already holds a connection to: `engine.rs`), and the pool's node gets a peer that is not the first seed's machine.

## Part 3: the owner's own PC

The wallet app, its node and the miner are one download: replace them together, **before the chain reaches 500**, and check `About` shows `0.2.0-beta.2` and the same commit as the servers. Start mining again only after every server shows the same version.

## After the upgrade: what to watch across block 500

* The height of all nodes keeps rising past 499 and they agree (`tenerod status`). A node stuck at 499 is on the old build.
* The pool's `status:` line: `shares` accepted and not mostly `refused`; `blocks found` rising; `in chain` the same as `found`.
* **If the pool's miners are refused from 500 on:** check the miner's version first.
* The pool's first payout after the fork, and the pool's wallet balance (see `RUNNING_A_POOL.md`).
* Record what you see in the release notes under "what was seen", and what was not.

## What this guide has not checked

* The names of the first seed's service, folders and settings (assumed from the pool's setup).
* That a beta.2 node, pool and miner work together across height 500 **on the real network**: the code has tests for each side of the fork and a GPU run across a fork at height 10 (see the commit), not a run of the pool.
* That a beta.2 pool and a beta.1 node (or the reverse) refuse to talk or not: upgrade all of them.
* The memory of the two nodes with the new rule, the sync time of a fresh seed, and how long the slower blocks after 500 last.
