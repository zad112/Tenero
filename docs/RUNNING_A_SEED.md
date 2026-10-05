# Running a seed node (a guide for an operator, 2026-10-04)

**Experimental and unaudited; nothing on `alpha` has any value. Nobody has followed this guide yet: it was written from the code and the documents, not from a run on a server, and the Linux build has been run by hand only as a node in WSL2 (`docs/TESTING.md`), never on a server.** Read [`SEED_POLICY.md`](SEED_POLICY.md) first: a seed is a convenience for newcomers, and **a list of seeds is only as trustworthy as the number of independent operators in different network
groups behind it.** One seed is one point of failure and one point an attacker would aim at.

## What a seed is, and what this one must do

A seed is an ordinary `tenerod` node at a **fixed public IP address and port** that stays up. A brand-new node dials the seeds it knows, asks them for addresses of other nodes, and then mostly forgets them. So a seed must
serve the **whole chain** (an archive node, never pruned), accept connections from strangers, and keep the same address. It does not mine and holds no wallet.

## What you need (and a cost you should know before you rent anything)

* **A server that is only this.** A small cloud Linux server or a machine on a separate network, running **Ubuntu 22.04 or newer** (the Linux build needs glibc 2.34; 2.35 for the wallet app, which a seed does not need).
  **Not your own computer and not one that holds a wallet or anything else of yours.** A seed accepts connections from strangers, the part of the project that is least hardened against hostile traffic.
* **RAM.** A node on `alpha` checks every block's real proof of work, which needs the epoch's **4 GiB dataset in memory**. **A node holds one dataset at a time** (the owner's limit for the node is
  8 GB, 2026-10-04): **4.01 GiB measured** for a whole check process across three epoch boundaries (a Linux node used 4.1 GiB resident with one epoch built), plus the chain database and the system. **An 8 GB
  server is the target**; I would not go below 6 GB and have not tried (nothing smaller was measured, and building a dataset touches all of it). **The price:** the node waits about 3 seconds (measured) for the first
  block of each 100-block epoch, while it frees the old dataset and builds the next. Not measured on a server. 16 GB would be headroom, not need. (The program used to hold two datasets, 8.0 GiB, which is why
  earlier drafts of this guide asked for 16 GB.) I have not priced servers of this size; the price is yours to check, and in the figures I found (unverified for Hetzner) 8 GB is the cheaper size by a wide margin.
* **A fixed IPv4 address.** `seed` entries are `ip:port` literals (names are not accepted), and a built-in list ships with a release, so an address that changes means a new release.
* **One open port**, TCP, of your choosing (there is no default; `38333` is a reasonable convention next to `alpha`'s local control port 38332, nothing more). Nothing else may be open to the internet except SSH.

## Setting it up (untested; the commands are what the programs' documentation says)

1. **Make a user with no privileges and no login for the node:** `sudo useradd --system --create-home --home-dir /var/lib/tenero --shell /usr/sbin/nologin tenero`.
2. **Install a release** (once one is published): download the Linux tar.gz **and `SHA256SUMS`**, check it (`sha256sum -c SHA256SUMS --ignore-missing`), unpack the programs into `/opt/tenero/`. Do not copy the wallet app or `tenero-wallet`
   there; a seed needs only `tenerod` (and `tenero-seedcheck` if you want to check other seeds from it).
3. **The firewall:** allow SSH and your one P2P port, deny the rest (for example `ufw default deny incoming`, `ufw allow 22/tcp`, `ufw allow 38333/tcp`, `ufw enable`). **Do this before starting the node.** The node's
   **control interface listens only on `127.0.0.1:38332`** and refuses any other address; leave it so.
4. **A settings file**, `/var/lib/tenero/node.conf` (one `key = value` per line; `docs/RUNNING.md` explains each):

       data = /var/lib/tenero/data
       network = alpha
       listen = 0.0.0.0:38333
       advertise = YOUR.SERVER.PUBLIC.IP:38333
       prune_keep = 0
       log_file = /var/lib/tenero/node.log
       # no mining, no mine_to: a seed has no wallet
       # no_builtin_seeds = yes     (only if you want this node to ignore the program's own list)

5. **A service**, so it starts again after a reboot or a crash. `/etc/systemd/system/tenero-seed.service` (this exact file has **not been run**):

       [Unit]
       Description=Tenero seed node (alpha; experimental)
       After=network-online.target
       Wants=network-online.target

       [Service]
       User=tenero
       ExecStart=/opt/tenero/tenerod --config /var/lib/tenero/node.conf
       Restart=on-failure
       RestartSec=10
       NoNewPrivileges=true
       ProtectSystem=strict
       ReadWritePaths=/var/lib/tenero
       PrivateTmp=true
       ProtectHome=true

       [Install]
       WantedBy=multi-user.target

   `sudo systemctl enable --now tenero-seed`, then `journalctl -u tenero-seed -f`. The node starts with a line `build: v..., commit ...` and the banner; `sudo -u tenero /opt/tenero/tenerod status --data /var/lib/tenero/data --control 127.0.0.1:38332` (on `alpha` the `--control` is needed: the command assumes the `test` network's port)
   shows height and peers. Stopping is `systemctl stop tenero-seed` (a clean shutdown: the node saves its peers and pool first).
6. **Check it from another machine,** not the server: `tenero-seedcheck --network alpha --seed YOUR.IP:38333`. With one seed it will warn that the policy wants at least three operators in different groups: that
   warning is true, not a fault of the node.

## Keeping it honest and safe

* **Tell nobody it is a safe or official anything.** It is one computer; the README and the pinned issue ([issue #1](https://github.com/zad112/Tenero/issues/1)) say what the network is.
* **A reset is announced in the pinned issue:** stop the service, install the new build (check its SHA-256), delete `/var/lib/tenero/data` (the chain, not the settings), start it again. A seed that is slow to do this keeps
  offering the old chain, and a new node will refuse it (different chain id), so it simply looks dead.
* **Watch it** now and then: `tenerod status`, the log, disk space, `tenero-seedcheck` from outside. It logs alarms (stale tip, few peers) in plain words; none is proof of an attack.
* **Keep the system updated,** and do not run anything else on it. If it is attacked or misbehaves, **delete the server** rather than cleaning it: nothing on it is worth keeping, which is the point of keeping it empty.
* **Never** put a wallet, a seed phrase, `control.cookie` copies, SSH keys that open other machines, or any password you use elsewhere on it.

## What nobody has checked

That these steps work on a real server; how much RAM and CPU a node on a public address actually uses under strangers' traffic; whether the systemd settings above are all accepted by the node (the node writes its
data, log and peers file under `/var/lib/tenero`, which is the only writable place given); how it behaves with many inbound connections from hostile peers beyond the fuzzing and the simulations in `THREAT_MODEL.md`.
