# Trying Tenero (for testers)

**Read this first.** Tenero is an experiment: **unaudited, one developer, no value, and the network will be reset.** Do not put anything on it that you cannot lose. The only place a
"stop mining" notice is ever posted is the pinned issue, https://github.com/zad112/Tenero/issues/1 (only the owner can post there): subscribe to it, and look at it before you start a miner.
This guide is for the `alpha` network of the test releases (the latest is `v0.1.0-alpha.4`). Everything here has been run by the author on one Windows 11 machine; the Linux build has been run once, as a node in WSL2 on that machine (it served and synced the 16-block chain); **the Linux wallet app, the GPU miner on Linux and a native Linux machine are untried**.

## What to expect to break

Plenty. Specifically: the wallet app has been drawn and tested by machines, not used by many people; the difficulty on `alpha` has not settled over a long run; only one node and one miner have ever been
on the network; **there is one seed server, the author's, built in (one computer, run by one person)**; the wallet's privacy is the interim scheme (not Carrot); and a rule change is handled by **resetting the chain**, not upgrading it. If something looks wrong, it
may well be; say so (see "Telling the author", below).

## What you need

* **Windows 11** (tried) or **Linux, Ubuntu 22.04 or newer** (a node has run in WSL2; nothing else is tried).
* About **4.3 GiB of free RAM** for a node on `alpha` (it holds one 4 GiB dataset and pauses about 3 seconds at the first block of each 100-block epoch while it builds the next; measured once on the real chain at block 100: the node stayed at 4.0 GiB on a PC and on the server, and the pause was not timed; mining uses its own memory on top), and a few GB of disk.
* **To mine: an NVIDIA GPU, a current driver and the CUDA Toolkit 13.x.** The miner uses NVIDIA's NVRTC and cuBLASLt libraries, which come with the Toolkit, not with the driver. It keeps one
  4 GiB dataset in video memory (about 5.0 GiB committed for the process on Windows, measured; it was 9.2 GiB when it kept two); **only a 16 GB card has been tried.** CPU mining of `alpha` takes about 53 minutes a block at the best measured speed and is not practical.
* **Check what you downloaded.** The files are **not code-signed**, so Windows will warn. Compare the SHA-256 with `SHA256SUMS` on the release page: PowerShell `Get-FileHash FILE -Algorithm SHA256`, Linux `sha256sum -c SHA256SUMS --ignore-missing`.

## The easy way: the wallet app

1. Unzip the release somewhere (a folder you own, not the Desktop's root). Start **`tenero-wallet-gui.exe`**. The node and the miner start from it and run hidden; nothing else opens.
2. Choose the **`alpha`** network in the settings. **Create a wallet**, **write the 24 words on paper**, and type the three it asks for. The words are the wallet; the password only locks the file on this computer. Do not photograph or store the words online.
3. On the **Node** tab, start the node. It finds its first peer by itself: **the author's seed server is built in** (Settings shows it under Seeds; add others there if you like, see "Finding peers"). Optionally tick **"Let other nodes connect to me"** (see "Letting others connect to you" first). A node alone is still a working one-node network.
4. On the **Mining** tab, press Start (it says what it will use: the GPU at full load). Rewards from mined blocks are spendable after 60 blocks.
5. **Receive** shows your address (`tni1...`) and a QR code; **Send** asks for an address and an amount and shows everything before it sends.

Files: settings, wallets and the node's data are under `%LOCALAPPDATA%\Tenero` (Linux: `~/.tenero`). The node's and miner's output are in `node-output.txt` and `miner-output.txt` there. **The chain grows, and can be moved to another drive:** Settings, "Move the node's data" (stop the node and the miner first); it copies, checks every file against the original, and only then uses the new folder, leaving the old one for you to delete. Details in `docs/RUNNING.md`.

## The command-line way

The programs are in the same folder as the app. (`docs/RUNNING.md` explains every setting.) On `alpha` the **control port is 38332**, and the wallet and miner assume the `test` network's 18332 unless you tell them.

```
tenerod --data DIR --network alpha --seed HOST:PORT --mine gpu --mine_to tni1...
tenero-wallet create  --wallet me.wallet --data DIR --control 127.0.0.1:38332
tenero-wallet balance --wallet me.wallet --data DIR --control 127.0.0.1:38332
tenero-wallet pay     --wallet me.wallet --data DIR --control 127.0.0.1:38332 --to tni1... --amount 1.5
tenero-miner --data DIR --control 127.0.0.1:38332 --address tni1... --backend gpu
```

That node only dials out. To let others connect to you, add `--listen 0.0.0.0:PORT --advertise 0.0.0.0:PORT` (a port of your choosing; `docs/RUNNING.md` says what `advertise` does; see "Letting others connect to you" for the router and the risk).

`tenerod --version` (and the other programs') says which build you have: **put it in every report.** The command-line wallet keeps a **raw seed of 64 hexadecimal digits** and a wallet file of its own; the
app keeps **24 words** and its own files; **they are two different wallets.** The wallet shows your seed once: write it down.

## Finding peers

A node needs at least one address to start from. On `alpha` the program carries **one built in: the author's server**, so a new node needs no setting. It is one computer run by one person, so **it can be down, or
wrong**: if a brand-new node cannot connect, or you do not want to depend on it, get the address of a node run by someone you trust (a friend who is also testing) and give it with `--seed HOST:PORT` (or `trusted_peer`, for an address you got **outside** the network). To run two nodes on one machine or on one home network, set
`allow_private_peers yes`: `alpha` refuses such addresses by default. If you want others to reach your node, see the next section; **a node that accepts connections from strangers is the least-hardened
part of this project**, so do it only on a machine you do not mind exposing, and not on the computer that has your wallet.

## Letting others connect to you

By default your node only **dials out**: it works, but every tester then talks to the seed and not to each other, so the seed does all the work. In the wallet app, **Settings, "Let other nodes connect to me"**
(a tick box and a TCP port; `38333` on alpha) makes the node accept connections and tell its peers where it is. It does not open your router for you:

1. **Forward the TCP port** on your router to this computer (the same port, inside and out), and **allow the program in Windows Firewall** (Windows asks the first time the node listens).
2. **Your internet address may change; there is nothing to set.** The node tells each peer "reach me on this port, at the address you see me at" every time it connects, and an old address simply stops answering and is forgotten. You do not need a fixed IP or a
   dynamic-DNS name (seeds are plain IP addresses: this is a design choice, a name would have to be resolved by whoever dials it).
3. **It cannot work behind CGNAT** (a provider that shares one public address between customers; your router's "internet address" is then a private one such as 100.64.x.x or 10.x.x.x): nobody can reach you, and the box does nothing.
4. **To check** that it works, look at the node's status line: `peers N (in M, out K)`. `in` is the strangers who connected to you; it stays 0 until someone else has your address, which takes a few minutes and another node that asks the seed.
5. **The risk:** strangers can connect to this computer, and the network code has had fuzzing and simulations, **not an audit**. A flood of connections or a bug could use this computer's memory, processor and bandwidth. Do not do this on a computer you cannot afford to slow down or restart.

**What is not tested yet:** the address learned from the seed is checked only by whoever dials it; an address whose owner moved on lingers in the seed's list for up to 30 days and costs each newcomer one failed dial (it is then
retried with growing waits). Nobody has run this across a real home router yet.

## Telling the author

* **Always:** the output of `--version`; what you did, in order; what you expected and what happened; the OS and, for GPU problems, the card and driver version.
* **Useful:** the node's log (start it with `--log_file FILE`, or send `node-output.txt` and `miner-output.txt`), and the `status:` lines in it. Read it first and remove anything you do not want public.
* **Never:** your wallet file, your seed or 24 words, your password, or `control.cookie`. There is **no automatic diagnostics bundle** (it was planned and is not built).
* **Where:** a **public issue** on https://github.com/zad112/Tenero/issues for ordinary problems. For anything that could be used against someone running a node, miner or wallet, use the repository's Security tab,
  **"Report a vulnerability"**, privately ([`SECURITY.md`](../SECURITY.md)). There is one maintainer and no promised response time.

## Resets and upgrades

There is **no upgrade path for a rule change: it is a reset** (a new chain id, announced in the pinned issue). When a reset is announced you stop, install the new build (check its SHA-256 as above), and **delete the node's
data folder** (the chain), not your wallet file or your seed; coins on a reset test chain are gone and had no value. A bad block found before anyone has spent from it may instead be handled by a fixed build and
`tenerod rewind` ([`docs/EMERGENCY_PLAN.md`](EMERGENCY_PLAN.md) says when). **That plan has been rehearsed once, by the author, on one machine, for one kind of failure.**
