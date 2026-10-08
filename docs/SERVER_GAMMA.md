# The servers for `gamma` (0.3.0-gamma.1): two seeds and the pool

What the owner runs on the two servers when `gamma` launches (decision S1, 2026-10-08). **Nothing here has been run yet**:
it follows the programs' documentation and the steps that worked for `beta` (`docs/SERVER_UPGRADE_BETA2.md`,
`docs/RUNNING_A_POOL.md`). Every command is run by the owner (Claude has no access to the servers). The names are choices:
change one, change it everywhere.

| | 195.26.244.245 (the second server, 11 GiB) | 194.238.27.60 (the Contabo server) |
|---|---|---|
| before | `beta` seed (`tenero-beta`), `beta` pool's node and pool | `beta` seed |
| after | **`gamma` seed**, **`gamma` pool's node and pool**; its `beta` seed and pool stopped | `beta` seed **and `gamma` seed** (if the memory allows: step 3.1) |

The ports (`gamma` follows `beta`'s pattern; **every control port is on loopback only**):

| what | port | open to the internet |
|---|---|---|
| `gamma` seed, peers | 38353 | yes |
| `gamma` seed, control | 38352 (`gamma`'s default) | no |
| `gamma` pool's node, peers | 38356 | yes |
| `gamma` pool's node, control | **38362** (not 38352: the seed on the same machine has that one) | no |
| `gamma` pool, miners | 38335 (where `beta`'s pool was) | yes |

## 1. Before launch day

1. **The package**: `tenero-0.3.0-gamma.1-linux-x64.tar.gz` and `SHA256SUMS` from the release page. Check the hash on your PC,
   then copy it to both servers and check it there again (as for `beta`: `scp`, then `sha256sum` and `tar xzf` in `/root`).
2. **The pool's key.** It was made on your PC before the release so that the program can carry it (2026-10-08): the file
   `C:\Users\12143\tenero-gamma-pool-key\pool.key` (32 bytes, **a secret**: whoever has it can pose as the pool). Its public
   half, `4eb53ae8fee5bf1596b3c652896ebf5190c0415d92542c9c536842bd5cfa770a`, is built into 0.3.0 as the `gamma` pool. **Keep a
   copy somewhere safe and offline** (losing it means a new key, and miners' pins break until a new release); never put it in
   the repository. Copy it to the second server:

   ```
   scp C:\Users\12143\tenero-gamma-pool-key\pool.key root@195.26.244.245:/root/gamma-pool.key
   ```

3. **Tell the testers** (the pinned issue https://github.com/zad112/Tenero/issues/1): the `beta` pool closes on launch day
   (its last payout runs first), `gamma` is a new network with new wallets (`TENg...` addresses), and the 0.2.0 programs keep
   working for `beta` on its remaining seed (194.238.27.60) for as long as that runs.

## 2. The second server (195.26.244.245)

### 2.1 Look first, and close `beta` here

```
systemctl list-units | grep -i tenero
free -h
```

The `beta` pool: let its last payout run (it pays every hour; balances under its minimum stay owed: `beta` has no value),
note the totals, then stop it and its node, and the `beta` seed. **Their data stays on disk**; only the services stop:

```
grep "status:" /var/lib/tenero-pool/pool.log | tail -1
systemctl disable --now tenero-pool tenero-pool-node tenero-beta
ufw delete allow 38336/tcp && ufw delete allow 38343/tcp
free -h
```

(The pool's port 38335 stays open: the `gamma` pool takes it.)

### 2.2 The `gamma` seed

```
useradd --system --create-home --home-dir /var/lib/tenero-gamma --shell /usr/sbin/nologin tenero-gamma
mkdir -p /opt/tenero-gamma
install -o root -g root -m 755 /root/tenero-0.3.0-gamma.1-linux-x64/tenerod /opt/tenero-gamma/
```

`/var/lib/tenero-gamma/node.conf` (a seed is an **archive** node: a new node syncing from scratch needs every block's proofs,
and nodes are pruned by default from 0.3.0):

```
data = /var/lib/tenero-gamma/data
network = gamma
listen = 0.0.0.0:38353
advertise = 195.26.244.245:38353
control = 127.0.0.1:38352
prune_keep = 0
log_file = /var/lib/tenero-gamma/node.log
```

`/etc/systemd/system/tenero-gamma.service`:

```
[Unit]
Description=Tenero gamma seed (experimental)
After=network-online.target
Wants=network-online.target

[Service]
User=tenero-gamma
ExecStart=/opt/tenero-gamma/tenerod --config /var/lib/tenero-gamma/node.conf
Restart=on-failure
RestartSec=10
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=/var/lib/tenero-gamma
PrivateTmp=true
ProtectHome=true

[Install]
WantedBy=multi-user.target
```

```
chown -R tenero-gamma:tenero-gamma /var/lib/tenero-gamma && chmod 700 /var/lib/tenero-gamma
ufw allow 38353/tcp
systemctl daemon-reload && systemctl enable --now tenero-gamma
/opt/tenero-gamma/tenerod --version
/opt/tenero-gamma/tenerod status --data /var/lib/tenero-gamma/data --control 127.0.0.1:38352
```

The version line must say `v0.3.0-gamma.1`; `status` says height 0 and the chain id `bd366b37...` until someone mines.

### 2.3 The `gamma` pool's node and the pool

```
useradd --system --create-home --home-dir /var/lib/tenero-gamma-pool --shell /usr/sbin/nologin tenero-gamma-pool
mkdir -p /opt/tenero-gamma-pool
for f in tenerod tenero-pool tenero-wallet tenero-poolcheck; do install -o root -g root -m 755 /root/tenero-0.3.0-gamma.1-linux-x64/$f /opt/tenero-gamma-pool/; done
```

`/var/lib/tenero-gamma-pool/node.conf` (control **38362**; the node dials the other server's seed too, an IP of its own):

```
data = /var/lib/tenero-gamma-pool/node
network = gamma
control = 127.0.0.1:38362
log_file = /var/lib/tenero-gamma-pool/node.log
listen = 0.0.0.0:38356
advertise = 0.0.0.0:38356
max_inbound = 16
seed = 194.238.27.60:38353
```

The pool's wallet (a `gamma` wallet; `create` prints the seed once: write it down if the coins matter to you), and the key
from your PC, **private to the pool's user** (check that the key printed is the one built into the program):

```
mkdir -p /var/lib/tenero-gamma-pool/pool && cd /var/lib/tenero-gamma-pool/pool
head -c 24 /dev/urandom | base64 > pass.txt
/opt/tenero-gamma-pool/tenero-wallet create --network gamma --wallet pool.wallet --birth 0 --passphrase-file pass.txt
install -m 600 /root/gamma-pool.key /var/lib/tenero-gamma-pool/pool/pool.key && shred -u /root/gamma-pool.key
/opt/tenero-gamma-pool/tenero-pool key --data /var/lib/tenero-gamma-pool/pool
chown -R tenero-gamma-pool:tenero-gamma-pool /var/lib/tenero-gamma-pool
chmod 700 /var/lib/tenero-gamma-pool /var/lib/tenero-gamma-pool/pool && chmod 600 pass.txt pool.wallet pool.key
```

The `key` line must print `4eb53ae8fee5bf1596b3c652896ebf5190c0415d92542c9c536842bd5cfa770a`. If it prints anything else,
stop: the file did not arrive whole.

`/var/lib/tenero-gamma-pool/pool.conf`:

```
data = /var/lib/tenero-gamma-pool/pool
network = gamma
node-data = /var/lib/tenero-gamma-pool/node
control = 127.0.0.1:38362
wallet = /var/lib/tenero-gamma-pool/pool/pool.wallet
passphrase-file = /var/lib/tenero-gamma-pool/pool/pass.txt
listen = 0.0.0.0:38335
name = Tenero gamma test pool
fee = 0
min-payout = 0.1
payout-every = 3600
log-file = /var/lib/tenero-gamma-pool/pool.log
```

The two services are `beta`'s (`docs/RUNNING_A_POOL.md` step 7) with the names changed: `tenero-gamma-pool-node.service`
(`User=tenero-gamma-pool`, `ExecStart=/opt/tenero-gamma-pool/tenerod --config /var/lib/tenero-gamma-pool/node.conf`,
`ReadWritePaths=/var/lib/tenero-gamma-pool`, `MemoryMax=6G`, `CPUWeight=50`, `Nice=5`) and `tenero-gamma-pool.service`
(`ExecStart=/opt/tenero-gamma-pool/tenero-pool --config /var/lib/tenero-gamma-pool/pool.conf`,
`After=` and `Requires=tenero-gamma-pool-node.service`). The drop-in that hides the pool's folder from the node
(`InaccessiblePaths=/var/lib/tenero-gamma-pool/pool`, `RUNNING_A_POOL.md` "Beside a seed") applies the same way.

```
ufw allow 38356/tcp
systemctl daemon-reload && systemctl enable --now tenero-gamma-pool-node
```

Wait for the node's log to say `in sync` (at launch that is at once: the chain is empty), then:

```
systemctl enable --now tenero-gamma-pool
grep -E "listening for miners|key" /var/lib/tenero-gamma-pool/pool.log | tail -2
free -h
```

**The memory:** two nodes and a pool; on `beta` the same three measured 8.5 GiB of 11 GiB (2026-10-07). Look once both nodes
have built their 4 GiB dataset (after the first mined block).

## 3. The Contabo server (194.238.27.60): a `gamma` seed beside the `beta` seed

### 3.1 The memory first

```
free -h
```

Two nodes need about 9 GiB (a 4 GiB dataset and about half a GiB more each). **If the server has less, do not start the
second node**: decide first between keeping `beta`'s last seed and adding `gamma`'s second (with only one `gamma` seed, a
new `gamma` node still starts from 195.26.244.245).

### 3.2 The seed

As 2.2, with this server's address: `advertise = 194.238.27.60:38353`, and its own user and folders
(`tenero-gamma`, `/opt/tenero-gamma`, `/var/lib/tenero-gamma`). Its control port 38352 is free here (the `beta` seed uses
38342). `ufw allow 38353/tcp`.

## 4. Checks, from your PC

```
tenero-seedcheck --network gamma --seed 195.26.244.245:38353 --seed 194.238.27.60:38353
tenero-poolcheck --pool 195.26.244.245:38335 --network gamma --pool-key 4eb53ae8fee5bf1596b3c652896ebf5190c0415d92542c9c536842bd5cfa770a
```

The seed check warns that two seeds of one operator are below the policy's three (`docs/SEED_POLICY.md`): that is the state of
an experiment, as on `beta`. Two of the pool checks are skipped on a network with the real proof of work (they need the 4 GiB
dataset); a real miner covers them.

## 5. The first blocks

`gamma` starts empty: nothing is mined until someone mines. A block reward can be spent **60 blocks** after its block, and the
first spends hide among very few outputs (`docs/FCMP_CARROT_PLAN.md` section 10): say so to the testers.

## What this guide has not checked

That the services start as written (no `gamma` build has run on these servers), the memory of two `gamma` nodes and a pool, and
whether the Contabo server has room for a second node.
