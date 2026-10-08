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

**Testers can join the Discord server: <https://discord.gg/QzfqaGDVcm>.** It is for questions, bug reports and hardware numbers. Nothing said there is an official notice: the only place a "stop mining" notice is posted is the pinned [issue #1](https://github.com/zad112/Tenero/issues/1), and only the author can post there. Only download the programs from this repository's [Releases](../../releases) page, whatever a link on Discord says.

## Where it stands (2026-10-07)

| | |
|---|---|
| **Built** | a node, a miner (GPU and CPU), a command-line wallet, a wallet app with a window, a seed checker, a network protocol with an encrypted channel, two test networks called **`alpha`** (the first, kept for a short time) and **`beta`** (the new one, after a hard fork), each with one seed server, and **a mining pool** (`tenero-pool`, with a miner mode and an app option that need no node, and a checker for pools) |
| **Tested** | about 1,100 automated tests (1,088 passing and 28 skipped in the last full run, 2026-10-07; the skipped ones need the author's GPU), the Rust code checked bit for bit against an independent Python reference, six fuzzing targets run for 30 minutes each with no crash |
| **Run on `beta`** | **(2026-10-07)** the new chain started that day. The author's server runs its seed, and beside it a pool with a node of its own; a GPU miner on a Windows PC worked for the pool and the chain grew (the pool's log: 6 blocks found in its first minutes, 5 in the chain). The seed's node and the pool's node each held about **4.0 GiB** and the machine (11 GiB) had 3.2 GiB to spare. **Seen once:** a payout by the pool to the author's own wallet (219.75 coins, 2026-10-07). **Not yet seen:** a payout to anyone else, more than one miner, the pool under load. See [The `beta` test network](#the-beta-test-network-and-mining-pools) |
| **Run on `alpha`** | the chain is past block 689 (2026-10-05), mined by the author's one GPU **and by at least one other miner**: about 24% of the blocks in one 9½-hour session (at least 139 of about 578) were not found by the author's miner, by the miner's own count; **nothing is known about who they are or what they run** (the rate points to a GPU, which is an estimate). The average since block 206 is about 58 seconds a block against a target of 60 (measured from timestamped height readings on the author's node); the earlier blocks swung more. A Windows PC and a rented Linux server (the seed) stayed on one chain, and the seed has had three inbound connections at once (the author's PC and two from one outside address). **The epoch boundaries up to block 600 have been crossed** by the PC node and the server, both in sync; **memory was measured at block 100 (the node and the server, 4.0 GiB) and at block 200 (the GPU miner, 5.19 GiB committed, unchanged across it), not at the later ones**, and the length of the pause was not measured. **Not yet seen:** a sharp change in hash power, more than three connections to the seed at once, a CPU miner |
| **Reviewed by anyone else** | **no.** There has been no independent cryptographic, security or hardware review |
| **Released** | **yes: the latest is `v0.2.0-beta.3` (a small, optional update), after `v0.2.0-beta.2` (the hard fork of `beta` at block 500) and `v0.2.0-beta.1` (2026-10-07), and `v0.1.0-alpha.4` (2026-10-06 UTC), `v0.1.0-alpha.3` (2026-10-06), `v0.1.0-alpha.1` and `v0.1.0-alpha.2` (2026-10-05), all on the [Releases](../../releases) page. They are still TEST releases**: "released" means published for people to try, not finished, reviewed or safe; see [Get it](#get-it) |
| **Planned** | **The author plans to launch a permanent main network on 1 November 2026**: a chain the author does not intend to reset (the `alpha` test network can be reset at any time, and what happens to it then is not decided). **That is an intention, not a promise: the date can move, and a launch would not make the software finished, reviewed or safe.** Everything above stays true until then and after it: the program is unaudited and written by one person, the wallet scheme is an interim one that is not Carrot and gives no Monero-style privacy, and the network has one seed run by one person. A main network existing does not change that, so put nothing into it that you cannot lose |

What is known to be wrong or missing is written down, including what nobody has fixed: [`docs/KNOWN_ISSUES.md`](docs/KNOWN_ISSUES.md) and [`docs/THREAT_MODEL.md`](docs/THREAT_MODEL.md).

## How it works

*For a reader who knows what a blockchain is.*

**The chain.** Blocks are aimed at one every **60 seconds**. The difficulty is re-worked for every block from the last **30** (a weighted average of solve times, called LWMA),
and a block's timestamp must be **later than its parent's** (a rule chosen after a simulation showed the older "median of 11" rule let a miner with 30 % of the hash rate pull the
difficulty to 0.40 times the honest value; with the new rule the same attacker leaves it at 1.02 times).

**Coins.** One coin is 100,000,000 units (8 decimals); the ticker is TNR. A block pays **20 coins** at the start, **halving every 525,600 blocks** (one year at 60 seconds),
up to a main supply of **20,000,000 coins**, after which a **tail of 0.5 coins a block** continues. **There is no premine:** the `alpha` and `beta` genesis blocks create no coin; every coin is
mined, the author's included (a test checks this on the real store). Mined coins mature after 60 blocks, received coins after 10.

**Block size.** Flexible, as in Monero: blocks up to the median size of the last 10 blocks (never below 150,000 bytes) carry no penalty; a larger block gives up part of its reward,
growing with the square of the excess, and a block of more than twice the median (or more than 4 MiB) is invalid.

**Transactions.** A transaction takes at most **75,000 bytes** (since the hard fork of `v0.2.0-beta.1`; before it, at most 32 inputs), at most 16 outputs, and has **no limit on the number of inputs** besides its size: **95 fit in a payment to one person** (measured with real proofs). The wallet splits a payment that needs more, or that has more than 15 recipients, into several transactions, and can combine many small pieces into fewer large ones.

**Fees.** Every transaction pays a fee to the miner, and **there is a minimum, set by a rule every node checks** (a block with a transaction below it is invalid): `fee = reward x 3000 x size / median^2`, in units (1 coin = 100,000,000 units), where `reward` is the block's reward before any penalty, `size` the transaction's size in bytes and `median` the block-size median above (never below 150,000 bytes). **The minimum falls as the reward does**: the reward starts at 20 coins and halves every 525,600 blocks (about a year at one block a minute) down to a tail of 0.5 coins, so the same transaction costs 40 times less at the tail than at the start, and **filling a whole 150,000-byte block always costs the same share of the reward (2 %)**, which is what makes flooding the chain expensive. **Worked out from the formula, not measured:** at the start, 266.7 units a byte (a 2,100-byte transaction: 0.0056 coins); at the tail, 6.67 units a byte (the same transaction: 0.00014 coins; 75,000 bytes, the largest allowed: 0.005 coins). A bigger median lowers it with its square. The wallet pays the minimum plus a margin by default (**Low** 1.25 times, **Normal** 2 times, **High** 5 times: a higher fee only buys a better place when the pool of waiting transactions is full), and a pool pays the fees of its payouts itself. **The design follows Monero's; the numbers have not been tried against real traffic, and what a coin is worth in money is not something this design controls (these coins have none).** The rule is in `docs/CONSENSUS_V2.md` section 8.1 (where the 3000 is marked **provisional**: changing it, or the median's floor, would be a hard fork), the formula in `crates/tenero-core/src/fees.rs`.

**Privacy (the design target).** Spending uses **ring signatures** (CLSAG, a ring of 16) and amounts are hidden by commitments with **Bulletproofs+** range proofs, from the
`monero-oxide` libraries. **What the wallet gives today is less:** it uses an *interim* output scheme (classic CryptoNote style) that is **not Carrot**: no protection against a
malicious sender linking two of your addresses (no "Janus protection"), one address per wallet, no way to recover whom you paid, and a composition of well-known building blocks
that is itself unaudited. Treat it as a way to exercise the machinery, not as private money. The list is at the top of
[`crates/tenero-wallet/src/interim.rs`](crates/tenero-wallet/src/interim.rs).

### The proof of work, and what is and is not claimed about ASICs

Each attempt takes a block header and a nonce and derives a **64 x 8192 matrix of 8-bit integers** from ChaCha20. It multiplies that by **one 16 MiB slice** of a dataset, chosen by
the attempt itself, in exact integer arithmetic (the kind of work GPU tensor cores do), and folds the result into a hash. The dataset is **4 GiB** (256 slices) and is rebuilt
every **epoch** (100 blocks on `alpha` and `beta`); each slice is built from earlier ones with data-dependent picks, so it has to be built in order and cannot be generated on the fly cheaply.
Checking a block needs the dataset too: about **4.3 GiB of RAM**, and about 0.1 seconds per check.

**The argument for ASIC resistance** is that an attempt is limited by how fast memory can be read (it reads a whole 16 MiB slice), which is what a GPU's memory system is built
for, and a special-purpose chip would need the same memory to compete. **That is an argument, not a proof.** Specifically:

* **Found broken, and fixed by a fork at height 500 on `beta` and `dev` (2026-10-07):** in the first design the slice an attempt reads is known from two cheap
  hashes of its nonce, so a miner can choose nonces 16 to a slice and multiply them against ONE read of it, which makes it limited by int8 multiply speed,
  not memory. From height 500 an attempt instead gathers 2,048 columns of 8 KiB from the whole dataset, and memory is the limit again
  ([`docs/CONSENSUS.md`](docs/CONSENSUS.md) 8.3). `alpha` keeps the first design.
* **Measured** (one RTX 5070 Ti, [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md)): about **45,000 attempts a second** with the gathered attempt, at the card's memory
  bandwidth; with the first design about 79,000 with the grouping on the miner's own multiply (120,000 to 130,000 when it still used
  NVIDIA's cuBLASLt, which needed the CUDA Toolkit; 33,000 to 36,000 without the grouping). The CPU rate of a
  6-thread Ryzen 9 5900X is 164 attempts a second with a build tuned for that machine and 31.7 with the default build ([`docs/BENCHMARKS.md`](docs/BENCHMARKS.md); one machine, so
  treat them as examples).
* **Simulated, not proven:** the dataset's dependency structure was measured on a small version, and the slowdown from keeping only part of the dataset is an estimate
  ([`docs/KNOWN_ISSUES.md`](docs/KNOWN_ISSUES.md) item 11).
* **Not reviewed:** the fill and fold construction is original to this project and has had **no cryptanalysis and no independent hardware or economic review**.
* **Not protected against:** a large owner or renter of GPUs, the ordinary 51 % risk of a small network, or a design mistake nobody has found yet.

### The programs and how they talk

* **`tenerod`**, the node: keeps the chain, relays blocks and payments, checks everything. Peers talk over an encrypted channel (the Noise protocol, through the `snow` library, which has not had a formal audit).
* **`tenero-miner`** and the miner built into the node: GPU (NVIDIA, through CUDA), CPU, and a SHA-256 backend that only the `test` network uses. `tenero-miner --pool` works for a **mining pool** with no node at all.
* **`tenero-pool`** is a mining pool (PPLNS accounts, payouts from its own wallet) and **`tenero-poolcheck`** checks any pool against the written protocol ([`docs/POOL_PROTOCOL.md`](docs/POOL_PROTOCOL.md), [`docs/RUNNING_A_POOL.md`](docs/RUNNING_A_POOL.md)).
* **`tenero-wallet`**, the command-line wallet, and **`tenero-wallet-gui`**, the wallet app: a window with several wallets and accounts, a 24-word seed phrase, sending with three fee levels, receiving
  with a QR code, message signatures and payment proofs. It starts and stops the node and the miner itself, and has a tick box, off by default, to let other nodes connect to yours (see [The `alpha` test network](#the-alpha-test-network)). Its Mining tab can mine alone or for a pool, and its Send tab splits large payments and combines pieces.
* **`tenero-seedcheck`** checks a list of seed nodes (it cannot tell whether one is honest).
* The programs talk to the node over a **control interface that only listens on the local machine**.

## Get it

**The latest release, `v0.2.0-beta.3`, is on the [Releases](../../releases) page** (the `beta` releases are described under "The `beta` test network" below; `v0.1.0-alpha.4` was published 2026-10-06 UTC; `v0.1.0-alpha.1` and `v0.1.0-alpha.2` came out on 2026-10-05 and `v0.1.0-alpha.3` on 2026-10-06). They change how nodes find each other, not the chain, the rules or the wire protocol, so all four work together: **since alpha.3 a node that holds three outbound peers that are not seeds leaves its seed (tested in a simulation only, not seen on the real network), a seed repeats its address answer for 15 minutes instead of 24 hours, failed addresses are retried sooner, inbound connections have no limit unless you set one, and the log shows what a node announces. alpha.4 fixes one thing found by running alpha.3 on the real network: a node that its seed dials (a seed dials every reachable node it knows) never asked that seed for addresses and never refreshed the link, and now does (tested in the program's own tests, and seen working once on the real network: the seed's log shows it answering a node on a link the seed had dialled; leaving a seed has not happened there yet, as the network is three nodes). It is the nodes that need alpha.4, not the seed: the fix acts on a node that a seed dials, and a seed behaves the same on alpha.3 and alpha.4.** Each release is a Windows zip and a Linux tar.gz with a `SHA256SUMS` file. **It is a test release and says so in its notes and
this page: "released" means published for people to try, not finished, audited or safe, and the network may be reset.** How it is made and checked is in [`docs/RELEASING.md`](docs/RELEASING.md). The files are **not code-signed** (Windows SmartScreen will warn), and the Linux build has been run by hand **only as a node**: in WSL2 on the author's PC, and as a service on a rented Ubuntu 24.04 server (the Linux wallet, wallet app and GPU miner have not been run). Prefer to build it yourself? The next section does that.

**Antivirus warning: your antivirus may flag these programs as a "coin miner".** Some scanners do (the labels vary: "coin miner", "PUA" or "potentially unwanted application", sometimes "trojan"), and **that is partly true: `tenero-miner` is a
miner**, and the wallet app and the node can start it. It runs your GPU or CPU at full load, which is exactly what those detectors look for. **Nothing mines unless you start it** (the Start button on the wallet app's Mining tab, the `tenero-miner`
program, or the node's `mine` setting, which is off by default). The programs are also **not code-signed**, so a scanner has only their behaviour to go on; the author has not tested them against any named antivirus and cannot say which will flag them.
**Do not take anyone's word for what a file is, this page's included:** check the SHA-256 against `SHA256SUMS` on the Releases page, or **build from source** and run what you built. If you decide to trust a download, exclude **only its own folder**, not the
whole drive, and do not switch your protection off. And only download from this repository's [Releases](../../releases) page: real malware is often hidden in "free miner" downloads from elsewhere.

**Build from source** (needs [Rust](https://rustup.rs)); [`docs/RUNNING.md`](docs/RUNNING.md) is the full guide:

```
cargo build --release
cargo test --workspace            # no GPU needed
```

**What you need**

| To do this | You need |
|---|---|
| Run a node or the wallet | Windows 11 (the only Windows tried) or Linux (built for Ubuntu 22.04 or newer; **run by hand as a node, the pool and the command-line wallet**: in WSL2 and on one rented server); on `alpha`, `beta` or `dev`, about **4.3 GiB of free RAM** for the node (it holds one 4 GiB dataset at a time, and pauses about 3 seconds for the first block of each 100-block epoch while it builds the next); **mining adds its own memory** |
| Mine on a GPU | an NVIDIA GPU of the RTX 30 series or newer (compute capability 8.0+; A100, H100 and the like too) with a **driver for CUDA 13 (R580 or newer)**. **The CUDA Toolkit is not needed** (the kernels are compiled into the miner and the driver loads them; releases up to v0.2.0-beta.3 needed the Toolkit); nothing from NVIDIA is shipped here. One 4 GiB dataset is kept in video memory (the miner process commits about 5 GiB on Windows, of which under 0.5 GiB is RAM in use): **only a 16 GB card has been tried**; whether an 8 GB card works is untested |
| Mine on a CPU | nothing extra, but see the next section: on `alpha` it is **impractical** |

**An AMD (or other non-NVIDIA) GPU miner:** today the GPU miner is **NVIDIA only** (it is written for CUDA). If someone wants to build one for AMD cards, the author is 100% fine with that and would be glad to see it; the author
**cannot test it, because they do not own an AMD card.** What such a miner has to match is exact: the proof of work is specified in [`docs/CONSENSUS.md`](docs/CONSENSUS.md) (section 8.2) and pinned by the golden vectors in [`tests/vectors/`](tests/vectors/README.md), the existing
CUDA kernels are in [`crates/tenero-gpu/kernels/matmulhash.cu`](crates/tenero-gpu/kernels/matmulhash.cu), and the rule is **bit for bit, or it is wrong**: a block the node's check refuses is just wasted work. Nothing about its speed or safety would be claimed until someone measures it on a card they own.

## The `beta` test network, and mining pools

**`beta` is the network of `v0.2.0-beta.1` and later** (`v0.2.0-beta.2` is a second hard fork, at block 500 of `beta`: older programs stop at 499), a **hard fork** of `alpha`: a fresh chain (label "tenero beta network 1", chain id
`577cc63dfdf445eb26b712fa422b95a082ddcac2249e34de9489cb87be23641b`) with no premine, the same real proof of work and starting difficulty as `alpha`, control port 38342 and peer port 38343. **Programs of
`v0.1.0-alpha.4` and earlier cannot follow it, and nothing moves from `alpha` to `beta`.** `alpha` stays for a short time, where this version keeps `alpha.4`'s limits so that it agrees with the nodes still there.

* **There are two `beta` seeds, built in (from `v0.2.0-beta.3`; `v0.2.0-beta.2` and earlier have only the first): the author's servers `195.26.244.245:38343` and `194.238.27.60:38343`**, on different addresses, so a node can hold a connection to each. **Two computers, one person**: the same caveats as `alpha`'s; if the author is gone, both are.
* **Mining pools.** In a pool the block reward goes to **the pool**, which pays the miners **by its own rules, and nothing makes it pay**; the pool also sees each miner's address and internet address. A miner needs **no node** to mine for a pool (a node is still needed to see and spend what the pool pays). The miner pins the pool's public key, so a person between them is refused.
  **One pool is built in: the author's test pool on `beta`** (`195.26.244.245:38335`): PPLNS, pays once an hour every miner owed at least 0.1 coins, no fee. **It is one computer run by one person, it has run for one GPU miner for a short time, and one payout has been seen on the real chain (to the author's own wallet, 2026-10-07).** The app mines alone unless you choose a pool.
* **What a pool costs a server, measured once:** a seed, the pool's node and the pool on one rented Linux server (11 GiB): 4.0 GiB for each node, a small pool process, 3.2 GiB to spare. The first version of the pool held a dataset of its own and would not have fitted; the pool now asks its node to check shares.
* The release notes are [`docs/releases/v0.2.0-beta.1.md`](docs/releases/v0.2.0-beta.1.md), [`v0.2.0-beta.2.md`](docs/releases/v0.2.0-beta.2.md) and [`v0.2.0-beta.3.md`](docs/releases/v0.2.0-beta.3.md): read them before running anything.

## The `alpha` test network

`alpha` is the network of the first four test releases (kept for a short time after `beta` starts): the real proof of work at a real starting difficulty, a fresh genesis with no premine (label "tenero alpha network 1", chain id
`430ca70081d3e52c618fd9af46fecdf6d6fc8f7965dc8ed2aa53c92ecfe069d3`), a starting target of 2^237 (about 524,000 attempts a block), epochs of 100 blocks and control port 38332.
Its chain **restarts from block 1 whenever a rule changes**, and the author expects to reset it.

* **It is for** trying the programs end to end with a few people who know what this is. **It is not for** holding value, or for anyone who has not read the labels above.
* **GPU or nothing:** one GPU at about 34,000 attempts a second needs roughly 15 seconds for 524,000 attempts (arithmetic, not a measurement of the network). A 6-thread CPU needs about
  53 minutes at 164 attempts a second (and about 4.6 hours with the default build): **CPU mining of `alpha` is impractical.** The difficulty adjusts, so these change as miners come and go.
* **Measured so far:** one node and one GPU, 16 blocks in 12 minutes in one run (gaps between blocks 11 to 183 seconds, mean 48 s) and 19 blocks in about 12 minutes in another, while the difficulty was still settling from its start. A Windows PC and a Linux server
  (2026-10-05) and a third node on the PC stayed on one chain; the server was stopped, upgraded and restarted with its chain intact.
* **There is one seed, run by the author, and it is built in** (the source has its address, and so do the releases): a new `alpha` node finds the network with no setting. It is **one computer run by one
  person**: if it is down, a brand-new node has nowhere to start (one that has run before remembers its peers), and its operator could show a new node a false chain. The plan wants several independent operators
  ([`docs/SEED_POLICY.md`](docs/SEED_POLICY.md)). You can add your own (`--seed ip:port`, see [`docs/RUNNING.md`](docs/RUNNING.md)) or drop the built-in one (`no_builtin_seeds`).
* **Letting others connect to you** (the wallet app's Settings, "Let other nodes connect to me", off by default) lets your node serve blocks to others, so they do not all rely on the seed. **It needs a TCP port forwarded on your router and allowed in your firewall, it
  lets strangers connect to your computer, and the software is unaudited.** Your node tells peers "the address you see me at", so a changing home address needs no setting (not yet watched happening; it cannot work behind CGNAT). It has been tried once, between the author's PC and the
  server. See [`docs/TESTING.md`](docs/TESTING.md).
* **If something goes wrong,** the only place a "stop mining" notice is posted is the pinned issue, [issue #1](https://github.com/zad112/Tenero/issues/1), and only the author can post there.
  The plan behind it, and what is not yet rehearsed, is [`docs/EMERGENCY_PLAN.md`](docs/EMERGENCY_PLAN.md).
* **Trying it as a tester:** [`docs/TESTING.md`](docs/TESTING.md).

## Helping with the seed server

The one seed server is a rented computer that the author pays for. If you would like to chip in towards that bill, there is a Buy Me a Coffee page: **[buymeacoffee.com/zad112](https://buymeacoffee.com/zad112)**.

**It is a gift towards a server bill, and nothing else.** It buys no coins or tokens, no say in the project, no support and no early access. **Nothing on any Tenero network has any value, the project is not selling coins or
tokens, and none of this is an investment.** Nobody has to give, and the project does not depend on it.

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
* Pools: [`docs/POOL_PROTOCOL.md`](docs/POOL_PROTOCOL.md), [`docs/RUNNING_A_POOL.md`](docs/RUNNING_A_POOL.md), [`docs/REMOTE_MINING_PLAN.md`](docs/REMOTE_MINING_PLAN.md)
* The numbers: [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md); the wire format: [`docs/WIRE_PROTOCOL.md`](docs/WIRE_PROTOCOL.md); the plan and its history: [`docs/M10_M11_PLAN.md`](docs/M10_M11_PLAN.md)

## Security, licence

Report a security problem **privately** (the repository's Security tab, "Report a vulnerability"): see [`SECURITY.md`](SECURITY.md). The project is under the
[BSD-3-Clause licence](LICENSE); its dependencies' licences are listed in `THIRD-PARTY-LICENCES.txt` in each release.
