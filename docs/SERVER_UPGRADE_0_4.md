# Upgrading the `gamma` servers to 0.4.0-gamma.1 (two seeds and the pool)

What changes: **only the programs.** The chain, the data folders, the settings files, the users and the systemd services stay
exactly as `docs/SERVER_GAMMA.md` set them up on 2026-10-09. 0.4.0-gamma.1 speaks **network protocol version 3**, which
0.3.0-gamma.1 nodes cannot talk to, so **all three nodes switch in one sitting**, and the release is published right after.
Every command is run by the owner (Claude has no access to the servers).

| server | services | programs in |
|---|---|---|
| 195.26.244.245 (second server) | `tenero-gamma` (seed), `tenero-gamma-pool-node`, `tenero-gamma-pool` | `/opt/tenero-gamma`, `/opt/tenero-gamma-pool` |
| 194.238.27.60 (Contabo) | `tenero-gamma` (seed) | `/opt/tenero-gamma` |

**The pool protocol did not change**: pool miners on 0.3.0-gamma.1 keep mining for the pool through the switch (they need no
node). Solo miners and every node must upgrade.

## 1. Before (the draft release exists, nothing is published yet)

1. **Download the Linux package from the DRAFT** to your PC (the repository's owner can before it is published):

   ```
   gh release download v0.4.0-gamma.1 --repo zad112/Tenero --pattern "*linux-x64.tar.gz" --pattern SHA256SUMS
   ```

   (`*linux-x64.tar.gz` is the ordinary Linux package, not the `-amd` one.) Check it on your PC (in PowerShell,
   `Get-FileHash .\tenero-0.4.0-gamma.1-linux-x64.tar.gz -Algorithm SHA256` must equal its line in `SHA256SUMS`).

2. **Copy it to both servers:**

   ```
   scp tenero-0.4.0-gamma.1-linux-x64.tar.gz SHA256SUMS root@195.26.244.245:/root/
   scp tenero-0.4.0-gamma.1-linux-x64.tar.gz SHA256SUMS root@194.238.27.60:/root/
   ```

3. **On each server**, check it again and unpack it (nothing is stopped yet):

   ```
   cd /root && sha256sum -c SHA256SUMS --ignore-missing && tar xzf tenero-0.4.0-gamma.1-linux-x64.tar.gz
   /root/tenero-0.4.0-gamma.1-linux-x64/tenerod --version
   ```

   The check must say `OK`, and the version line `v0.4.0-gamma.1`.

4. **Tell the testers** in the pinned issue (https://github.com/zad112/Tenero/issues/1), a little ahead: 0.4.0-gamma.1 is a
   forced upgrade (same chain, same coins and wallets); after the switch a 0.3.0-gamma.1 node finds no peers, and a solo miner
   on it mines for nothing; pool miners are not affected.

## 2. The switch (both servers, one after the other, a few minutes)

### On 195.26.244.245 (the second server)

Keep the old programs for a rollback, put the new ones in, restart the three services (the pool last, after its node):

```
cp -a /opt/tenero-gamma /opt/tenero-gamma.0.3 && cp -a /opt/tenero-gamma-pool /opt/tenero-gamma-pool.0.3
N=/root/tenero-0.4.0-gamma.1-linux-x64
install -o root -g root -m 755 $N/tenerod /opt/tenero-gamma/
for f in tenerod tenero-pool tenero-wallet tenero-poolcheck; do install -o root -g root -m 755 $N/$f /opt/tenero-gamma-pool/; done
systemctl restart tenero-gamma tenero-gamma-pool-node
sleep 30 && systemctl restart tenero-gamma-pool
/opt/tenero-gamma/tenerod --version
/opt/tenero-gamma/tenerod status --data /var/lib/tenero-gamma/data --control 127.0.0.1:38352
/opt/tenero-gamma-pool/tenerod status --data /var/lib/tenero-gamma-pool/node --control 127.0.0.1:38362
grep -E "listening for miners|key" /var/lib/tenero-gamma-pool/pool.log | tail -2
```

Both `status` lines must show the chain's current height (not 0) and the chain id `bd366b37...`.

### On 194.238.27.60 (Contabo)

```
cp -a /opt/tenero-gamma /opt/tenero-gamma.0.3
install -o root -g root -m 755 /root/tenero-0.4.0-gamma.1-linux-x64/tenerod /opt/tenero-gamma/
systemctl restart tenero-gamma
/opt/tenero-gamma/tenerod --version
/opt/tenero-gamma/tenerod status --data /var/lib/tenero-gamma/data --control 127.0.0.1:38352
```

Then, a minute later, on both servers: the seeds and the pool's node see each other again (version 3 to version 3):

```
grep -E "connected (to|from)" /var/lib/tenero-gamma/node.log | tail -5
free -h
```

## 3. Checks from your PC (with the 0.4.0-gamma.1 programs)

```
tenero-seedcheck --network gamma --seed 195.26.244.245:38353 --seed 194.238.27.60:38353
tenero-poolcheck --pool 195.26.244.245:38335 --network gamma --pool-key 4eb53ae8fee5bf1596b3c652896ebf5190c0415d92542c9c536842bd5cfa770a
```

Both seeds must answer on the `gamma` chain at the current height (the two-operator warning is expected, as at launch); the pool
must prove its pinned key. Then start your own node and miner on 0.4.0-gamma.1 and see a block arrive.

## 4. Publish

Only now press **Publish** on the draft (after filling in its GPU line). People then download programs that have a network to join.

## If something is wrong

Put the old programs back and restart (the data is untouched, and the old programs read it as before):

```
cp -a /opt/tenero-gamma.0.3/. /opt/tenero-gamma/ && systemctl restart tenero-gamma
cp -a /opt/tenero-gamma-pool.0.3/. /opt/tenero-gamma-pool/ && systemctl restart tenero-gamma-pool-node tenero-gamma-pool
```

Do that on every server you switched, not one only: a version 3 seed and a version 2 seed cannot talk to each other. Do not
publish the draft until it works. Once it has run a day or two, remove the `.0.3` folders.

## Later, not part of the switch: a `.onion` address for the Contabo seed

`docs/RUNNING.md`, "Tor": install Tor, add the onion service, put `onion_listen`, `advertise_onion` and `proxy` in the seed's
`node.conf`, restart it. Then its `.onion` address can go into the program's built-in seeds in a later release.

## What was done (2026-10-11)

Run by the owner as above, with Claude reading each output. The Contabo seed first (it took a minute or two after the
restart before `status` answered: the node builds its 4 GiB dataset first), then the second server's seed, pool node and pool.
**From the owner's PC, with the 0.4.0-gamma.1 checkers:** both seeds answered on the `gamma` chain (`bd366b37`) at tip 2,880,
in 80 and 90 ms, 5 addresses each (5 routable, 4 groups), WARN only for two seeds of one operator; the pool passed 13 checks,
skipped 2 (they need the 4 GiB dataset) and failed none, its pinned key proven in the handshake: as at the `gamma` launch.
