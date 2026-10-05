# Trying Tenero (for testers)

**Read this first.** Tenero is an experiment: **unaudited, one developer, no value, and the network will be reset.** Do not put anything on it that you cannot lose. The only place a
"stop mining" notice is ever posted is the pinned issue, https://github.com/zad112/Tenero/issues/1 (only the owner can post there): subscribe to it, and look at it before you start a miner.
This guide is for the `alpha` network of the first test release, `v0.1.0-alpha.1`. Everything here has been run by the author on one Windows 11 machine; the Linux build has been run once, as a node in WSL2 on that machine (it served and synced the 16-block chain); **the Linux wallet app, the GPU miner on Linux and a native Linux machine are untried**.

## What to expect to break

Plenty. Specifically: the wallet app has been drawn and tested by machines, not used by many people; the difficulty on `alpha` has not settled over a long run; only one node and one miner have ever been
on the network; **there is no public seed node**; the wallet's privacy is the interim scheme (not Carrot); and a rule change is handled by **resetting the chain**, not upgrading it. If something looks wrong, it
may well be; say so (see "Telling the author", below).

## What you need

* **Windows 11** (tried) or **Linux, Ubuntu 22.04 or newer** (a node has run in WSL2; nothing else is tried).
* About **4.3 GiB of free RAM** for a node on `alpha` (it holds one 4 GiB dataset and pauses about 3 seconds at the first block of each 100-block epoch while it builds the next; mining uses its own memory on top), and a few GB of disk.
* **To mine: an NVIDIA GPU, a current driver and the CUDA Toolkit 13.x.** The miner uses NVIDIA's NVRTC and cuBLASLt libraries, which come with the Toolkit, not with the driver. It keeps up to two
  4 GiB datasets in video memory; **only a 16 GB card has been tried.** CPU mining of `alpha` takes about 53 minutes a block at the best measured speed and is not practical.
* **Check what you downloaded.** The files are **not code-signed**, so Windows will warn. Compare the SHA-256 with `SHA256SUMS` on the release page: PowerShell `Get-FileHash FILE -Algorithm SHA256`, Linux `sha256sum -c SHA256SUMS --ignore-missing`.

## The easy way: the wallet app

1. Unzip the release somewhere (a folder you own, not the Desktop's root). Start **`tenero-wallet-gui.exe`**. The node and the miner start from it and run hidden; nothing else opens.
2. Choose the **`alpha`** network in the settings. **Create a wallet**, **write the 24 words on paper**, and type the three it asks for. The words are the wallet; the password only locks the file on this computer. Do not photograph or store the words online.
3. On the **Node** tab, start the node. It needs peers: there is **no public seed node yet**, so you need the address of another tester's node (see "Finding peers"). A node alone is still a working one-node network.
4. On the **Mining** tab, press Start (it says what it will use: the GPU at full load). Rewards from mined blocks are spendable after 60 blocks.
5. **Receive** shows your address (`tni1...`) and a QR code; **Send** asks for an address and an amount and shows everything before it sends.

Files: settings, wallets and the node's data are under `%LOCALAPPDATA%\Tenero` (Linux: `~/.tenero`). The node's and miner's output are in `node-output.txt` and `miner-output.txt` there.

## The command-line way

The programs are in the same folder as the app. (`docs/RUNNING.md` explains every setting.) On `alpha` the **control port is 38332**, and the wallet and miner assume the `test` network's 18332 unless you tell them.

```
tenerod --data DIR --network alpha --seed HOST:PORT --mine gpu --mine_to tni1...
tenero-wallet create  --wallet me.wallet --data DIR --control 127.0.0.1:38332
tenero-wallet balance --wallet me.wallet --data DIR --control 127.0.0.1:38332
tenero-wallet pay     --wallet me.wallet --data DIR --control 127.0.0.1:38332 --to tni1... --amount 1.5
tenero-miner --data DIR --control 127.0.0.1:38332 --address tni1... --backend gpu
```

That node only dials out. To let others connect to you, add `--listen IP:PORT` (a port of your choosing: there is no default; see "Finding peers" for the risk).

`tenerod --version` (and the other programs') says which build you have: **put it in every report.** The command-line wallet keeps a **raw seed of 64 hexadecimal digits** and a wallet file of its own; the
app keeps **24 words** and its own files; **they are two different wallets.** The wallet shows your seed once: write it down.

## Finding peers

A node needs at least one address to start from. On `alpha` there is no list built in. Get the address of a node run by someone you trust (a friend who is also testing; the author may name one in the pinned
issue, but none is promised) and give it with `--seed HOST:PORT` (or `trusted_peer`, for an address you got **outside** the network). To run two nodes on one machine or on one home network, set
`allow_private_peers yes`: `alpha` refuses such addresses by default. If you want others to reach your node, forward its port; **a node that accepts connections from strangers is the least-hardened
part of this project**, so do it only on a machine you do not mind exposing, and not on the computer that has your wallet.

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
