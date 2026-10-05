<p align="center">
  <img src="assets/banner.webp" alt="Tenero" width="100%">
</p>

**Tenero is an experimental proof-of-work cryptocurrency, written in Rust, built to learn how these systems work.** Its centre is a GPU-friendly proof of work,
**matmulhash v2**, and its design target is Monero-style privacy (ring signatures, hidden amounts).

> **This is a learning project, not a currency.**
> It is **unaudited** and written by **one person**. **Nothing on any Tenero network has any value**, and the test network **will be reset**.
> The wallet uses an **interim output scheme that is not Carrot and is not private in Monero's sense**. The proof of work is new, **unreviewed**, and **not claimed to be
> ASIC-proof**. Do not use any of it to hold anything you cannot afford to lose.

Inspired by Monero's design ideas; not affiliated with or endorsed by the Monero project.

## Where it stands (2026-10-04)

| | |
|---|---|
| **Built** | a node, a miner (GPU and CPU), a command-line wallet, a wallet app with a window, a seed checker, a network protocol with an encrypted channel, a test network called **`alpha`** |
| **Tested** | about 900 automated tests (898 passing and 24 skipped when last counted, 2026-10-04), the Rust code checked bit for bit against an independent Python reference, six fuzzing targets run for 30 minutes each with no crash |
| **Run on `alpha`** | one node and one GPU mined 16 blocks in 12 minutes. **Not yet seen:** a settled difficulty, a second miner, several nodes, an epoch boundary |
| **Reviewed by anyone else** | **no.** There has been no independent cryptographic, security or hardware review |
| **Released** | **not yet.** The first test release, `v0.1.0-alpha.1`, has been built once as a dry run and not published; see [Get it](#get-it) |

What is known to be wrong or missing is written down, including what nobody has fixed: [`docs/KNOWN_ISSUES.md`](docs/KNOWN_ISSUES.md) and [`docs/THREAT_MODEL.md`](docs/THREAT_MODEL.md).

## How it works

*For a reader who knows what a blockchain is.*

**The chain.** Blocks are aimed at one every **60 seconds**. The difficulty is re-worked for every block from the last **30** (a weighted average of solve times, called LWMA),
and a block's timestamp must be **later than its parent's** (a rule chosen after a simulation showed the older "median of 11" rule let a miner with 30 % of the hash rate pull the
difficulty to 0.40 times the honest value; with the new rule the same attacker leaves it at 1.02 times).

**Coins.** One coin is 100,000,000 units (8 decimals); the ticker is TNR. A block pays **20 coins** at the start, **halving every 525,600 blocks** (one year at 60 seconds),
up to a main supply of **20,000,000 coins**, after which a **tail of 0.5 coins a block** continues. **There is no premine:** the `alpha` genesis block creates no coin; every coin is
mined, the author's included (a test checks this on the real store). Mined coins mature after 60 blocks, received coins after 10.

**Block size.** Flexible, as in Monero: blocks up to the median size of the last 10 blocks (never below 150,000 bytes) carry no penalty; a larger block gives up part of its reward,
growing with the square of the excess, and a block of more than twice the median (or more than 4 MiB) is invalid.

**Privacy (the design target).** Spending uses **ring signatures** (CLSAG, a ring of 16) and amounts are hidden by commitments with **Bulletproofs+** range proofs, from the
`monero-oxide` libraries. **What the wallet gives today is less:** it uses an *interim* output scheme (classic CryptoNote style) that is **not Carrot**: no protection against a
malicious sender linking two of your addresses (no "Janus protection"), one address per wallet, no way to recover whom you paid, and a composition of well-known building blocks
that is itself unaudited. Treat it as a way to exercise the machinery, not as private money. The list is at the top of
[`crates/tenero-wallet/src/interim.rs`](crates/tenero-wallet/src/interim.rs).

### The proof of work, and what is and is not claimed about ASICs

Each attempt takes a block header and a nonce and derives a **64 x 8192 matrix of 8-bit integers** from ChaCha20. It multiplies that by **one 16 MiB slice** of a dataset, chosen by
the attempt itself, in exact integer arithmetic (the kind of work GPU tensor cores do), and folds the result into a hash. The dataset is **4 GiB** (256 slices) and is rebuilt
every **epoch** (100 blocks on `alpha`); each slice is built from earlier ones with data-dependent picks, so it has to be built in order and cannot be generated on the fly cheaply.
Checking a block needs the dataset too: about **4.3 GiB of RAM**, and about 0.1 seconds per check.

**The argument for ASIC resistance** is that an attempt is limited by how fast memory can be read (it reads a whole 16 MiB slice), which is what a GPU's memory system is built
for, and a special-purpose chip would need the same memory to compete. **That is an argument, not a proof.** Specifically:

* **Measured:** about **33,000 to 36,000 attempts a second** on one RTX 5070 Ti (batch 128 to 256), which is about 550 to 590 GB/s of slice reads by arithmetic; the CPU rate of a
  6-thread Ryzen 9 5900X is 164 attempts a second with a build tuned for that machine and 31.7 with the default build ([`docs/BENCHMARKS.md`](docs/BENCHMARKS.md); one machine, so
  treat them as examples).
* **Simulated, not proven:** the dataset's dependency structure was measured on a small version, and the slowdown from keeping only part of the dataset is an estimate
  ([`docs/KNOWN_ISSUES.md`](docs/KNOWN_ISSUES.md) item 11).
* **Not reviewed:** the fill and fold construction is original to this project and has had **no cryptanalysis and no independent hardware or economic review**.
* **Not protected against:** a large owner or renter of GPUs, the ordinary 51 % risk of a small network, or a design mistake nobody has found yet.

### The programs and how they talk

* **`tenerod`**, the node: keeps the chain, relays blocks and payments, checks everything. Peers talk over an encrypted channel (the Noise protocol, through the `snow` library, which has not had a formal audit).
* **`tenero-miner`** and the miner built into the node: GPU (NVIDIA, through CUDA), CPU, and a SHA-256 backend that only the `test` network uses.
* **`tenero-wallet`**, the command-line wallet, and **`tenero-wallet-gui`**, the wallet app: a window with several wallets and accounts, a 24-word seed phrase, sending with three fee levels, receiving
  with a QR code, message signatures and payment proofs. It starts and stops the node and the miner itself.
* **`tenero-seedcheck`** checks a list of seed nodes (it cannot tell whether one is honest).
* The programs talk to the node over a **control interface that only listens on the local machine**.

## Get it

**There is no published release yet.** The first, `v0.1.0-alpha.1`, will appear on the [Releases](../../releases) page as a Windows zip and a Linux tar.gz with a `SHA256SUMS` file;
how it is made and checked is in [`docs/RELEASING.md`](docs/RELEASING.md). The files are **not code-signed** (Windows SmartScreen will warn), and the Linux build has been run by hand **only once, as a node in WSL2 on the author's PC** (the Linux wallet app and the GPU miner on Linux have not been run). Until a release exists, build from source.

**Build from source** (needs [Rust](https://rustup.rs)); [`docs/RUNNING.md`](docs/RUNNING.md) is the full guide:

```
cargo build --release
cargo test --workspace            # no GPU needed
```

**What you need**

| To do this | You need |
|---|---|
| Run a node or the wallet | Windows 11 (the only Windows tried) or Linux (built for Ubuntu 22.04 or newer; **run by hand once, as a node in WSL2 only**); on `alpha` or `dev`, about **4.3 GiB of free RAM** for the node (it holds one 4 GiB dataset at a time, and pauses about 3 seconds for the first block of each 100-block epoch while it builds the next); **mining adds its own memory** |
| Mine on a GPU | an NVIDIA GPU with a current driver **and the CUDA Toolkit 13.x** (the miner uses NVIDIA's NVRTC and cuBLASLt, which come with the Toolkit, not the driver; nothing from NVIDIA is shipped here). Up to two 4 GiB datasets are kept in video memory: **only a 16 GB card has been tried**; whether an 8 GB card works is untested |
| Mine on a CPU | nothing extra, but see the next section: on `alpha` it is **impractical** |

## The `alpha` test network

`alpha` is the network of the first test release: the real proof of work at a real starting difficulty, a fresh genesis with no premine (label "tenero alpha network 1", chain id
`430ca70081d3e52c618fd9af46fecdf6d6fc8f7965dc8ed2aa53c92ecfe069d3`), a starting target of 2^237 (about 524,000 attempts a block), epochs of 100 blocks and control port 38332.
Its chain **restarts from block 1 whenever a rule changes**, and the author expects to reset it.

* **It is for** trying the programs end to end with a few people who know what this is. **It is not for** holding value, or for anyone who has not read the labels above.
* **GPU or nothing:** one GPU at about 34,000 attempts a second needs roughly 15 seconds for 524,000 attempts (arithmetic, not a measurement of the network). A 6-thread CPU needs about
  53 minutes at 164 attempts a second (and about 4.6 hours with the default build): **CPU mining of `alpha` is impractical.** The difficulty adjusts, so these change as miners come and go.
* **Measured so far:** one node and one GPU, 16 blocks in 12 minutes, gaps between blocks 11 to 183 seconds (mean 48 s) while the difficulty was still settling from its start.
* **There is no public seed node yet.** To try it you run your own nodes and point them at each other (`--seed ip:port`, see [`docs/RUNNING.md`](docs/RUNNING.md)). A seed run by the author
  would be one computer; the plan wants several independent operators ([`docs/SEED_POLICY.md`](docs/SEED_POLICY.md)).
* **If something goes wrong,** the only place a "stop mining" notice is posted is the pinned issue, [issue #1](https://github.com/zad112/Tenero/issues/1), and only the author can post there.
  The plan behind it, and what is not yet rehearsed, is [`docs/EMERGENCY_PLAN.md`](docs/EMERGENCY_PLAN.md).
* **Trying it as a tester:** [`docs/TESTING.md`](docs/TESTING.md).

## What is next (no dates are promised)

The wallet's interim scheme is to be replaced by **Carrot**; the author wants an **independent review** of the cryptography and of the proof of work before anything could carry value;
and a **longer public test** would follow. None of these has a date, and none has started.

## The Python reference

[`reference/`](reference/README.md) is the original Python implementation, kept frozen on purpose: the **second, independent implementation** the Rust code is checked against
("bit for bit, or it is wrong"), the generator of the golden vectors in [`tests/vectors/`](tests/vectors/README.md), and the CPU simulation behind the memory-hardness argument. It is
not a program to run. The old Python miner, wallet and command line are on the [`legacy-python`](../../tree/legacy-python) branch and the `python-final` tag, and their old README is in
[`docs/PYTHON_LEGACY.md`](docs/PYTHON_LEGACY.md).

## Documentation

* Rules: [`docs/CONSENSUS.md`](docs/CONSENSUS.md) (the proof of work and the Python reference's data model) and [`docs/CONSENSUS_V2.md`](docs/CONSENSUS_V2.md) (the Rust program's data model, a draft)
* Using it: [`docs/RUNNING.md`](docs/RUNNING.md), [`docs/TESTING.md`](docs/TESTING.md), [`docs/TESTNET.md`](docs/TESTNET.md) (a private test network on one machine)
* What can go wrong: [`docs/THREAT_MODEL.md`](docs/THREAT_MODEL.md), [`docs/EMERGENCY_PLAN.md`](docs/EMERGENCY_PLAN.md), [`docs/KNOWN_ISSUES.md`](docs/KNOWN_ISSUES.md)
* The numbers: [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md); the wire format: [`docs/WIRE_PROTOCOL.md`](docs/WIRE_PROTOCOL.md); the plan and its history: [`docs/M10_M11_PLAN.md`](docs/M10_M11_PLAN.md)

## Security, licence

Report a security problem **privately** (the repository's Security tab, "Report a vulnerability"): see [`SECURITY.md`](SECURITY.md). The project is under the
[BSD-3-Clause licence](LICENSE); its dependencies' licences are listed in `THIRD-PARTY-LICENCES.txt` in each release.
