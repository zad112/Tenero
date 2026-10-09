<p align="center">
  <img src="assets/banner.webp" alt="Tenero" width="100%">
</p>

**Tenero is an experimental proof-of-work cryptocurrency, written in Rust, built to learn how these systems work.** Its centre is a GPU-friendly proof of work,
**matmulhash v2**, and its privacy is Monero's newest design: **FCMP++** (full-chain membership proofs) and **Carrot** addresses, on the `gamma` network from its first block.

> **This is a learning project, not a currency.**
> It is **unaudited** and written by **one person**. **Nothing on any Tenero network has any value**, and the test network **will be reset**.
> Its privacy code is **unaudited as used here**: Monero's FCMP++ libraries (only partly audited, and not yet on Monero's own main network), a Carrot written in Rust
> by this project, and message signatures and payment proofs that are **this project's own construction, reviewed by nobody**. The proof of work is new, **unreviewed**,
> and **not claimed to be ASIC-proof**. Do not use any of it to hold anything you cannot afford to lose.

Inspired by Monero's design ideas; not affiliated with or endorsed by the Monero project.

**Testers can join the Discord server: <https://discord.gg/QzfqaGDVcm>.** It is for questions, bug reports and hardware numbers. Nothing said there is an official notice: the only place a "stop mining" notice is posted is the pinned [issue #1](https://github.com/zad112/Tenero/issues/1), and only the author can post there. Only download the programs from this repository's [Releases](../../releases) page, whatever a link on Discord says.

## Where it stands (2026-10-08)

| | |
|---|---|
| **Built** | a node (pruned by default), a miner (GPU and CPU), a command-line wallet, a wallet app with a window, a block explorer, a seed checker, a network protocol with an encrypted channel, **a mining pool** (`tenero-pool`, with a miner mode and an app option that need no node, and a checker for pools), and **the `gamma` network: FCMP++ and Carrot from block 0** (version 0.3.0) |
| **Tested** | about 1,170 automated tests (1,148 passing and 37 skipped in the last full run, 2026-10-08, on the author's PC; the skipped ones need the author's GPU or are long runs started by hand), the Rust code checked bit for bit against an independent Python reference and, for Carrot and FCMP++, against Monero's own C++ results and test proofs |
| **Run on `gamma`** | **nothing yet: `gamma` starts with the `v0.3.0-gamma.1` release.** Every number about `gamma` on this page is from tests on the author's PC or worked out from the rules, not seen on the real network |
| **Reviewed by anyone else** | **no.** There has been no independent cryptographic, security or hardware review. Parts of the FCMP++ libraries have been audited for Monero; how this project uses them has not |
| **Released** | **the latest is `v0.3.0-gamma.1`** (the `gamma` network); before it, `v0.2.0-beta.1` to `beta.4` (the `beta` network, 2026-10-07) and `v0.1.0-alpha.1` to `alpha.4` (the `alpha` network, 2026-10-05 and 06), all on the [Releases](../../releases) page. **They are TEST releases**: "released" means published for people to try, not finished, reviewed or safe; see [Get it](#get-it) |
| **Planned** | **The author plans to launch a permanent main network on 1 November 2026**: a chain the author does not intend to reset, built on the `gamma` rules. **That is an intention, not a promise: the date can move, and a launch would not make the software finished, reviewed or safe.** Everything above stays true until then and after it: the program is unaudited and written by one person, and the network's seeds are run by one person. A main network existing does not change that, so put nothing into it that you cannot lose |

What is known to be wrong or missing is written down, including what nobody has fixed: [`docs/KNOWN_ISSUES.md`](docs/KNOWN_ISSUES.md) and [`docs/THREAT_MODEL.md`](docs/THREAT_MODEL.md).

## How it works

*For a reader who knows what a blockchain is.*

**The chain.** Blocks are aimed at one every **60 seconds**. The difficulty is re-worked for every block from the last **30** (a weighted average of solve times, called LWMA),
and a block's timestamp must be **later than its parent's** (a rule chosen after a simulation showed the older "median of 11" rule let a miner with 30 % of the hash rate pull the
difficulty to 0.40 times the honest value; with the new rule the same attacker leaves it at 1.02 times).

**Coins.** One coin is 100,000,000 units (8 decimals); the ticker is TNR. A block pays **20 coins** at the start, **halving every 525,600 blocks** (one year at 60 seconds),
up to a main supply of **20,000,000 coins**, after which a **tail of 0.5 coins a block** continues. **There is no premine:** the `gamma` genesis block (like `alpha`'s and `beta`'s) creates no coin; every coin is
mined, the author's included (a test checks this on the real store). Mined coins mature after 60 blocks, received coins after 10.

**Block size.** Flexible, as in Monero, and counted by **weight**: a transaction's proofs (the bulk of it) weigh a quarter of their bytes, so FCMP++'s bigger proofs do not
shrink how many transactions fit. Blocks up to the median weight of the last 10 blocks (never below 150,000) carry no penalty; a heavier block gives up part of its reward,
growing with the square of the excess, and a block of more than twice the median is invalid. **The median grows slowly:** it is at most ten times a long-term median of the last
100,000 blocks, which itself grows at most 1.4 times per half window, so blocks can jump for a spike but grow lastingly only with months of real demand. **The ceiling** is
12 MiB of weight and 48 MiB of real bytes, about 6,300 typical transactions a block (about 100 a second): **worked out from the rules, not measured on a network**. A quiet chain
stays small.

**Transactions.** A transaction takes at most **75,000 bytes** and 16 outputs, and has **no limit on the number of inputs** besides its size (about 48 to 126, depending on the
depth of the curve tree). Every transaction has the same shape (outputs in a fixed order, the same `extra`), so no wallet's transactions stand out. The wallet splits a payment
that needs more, or that has more than 15 recipients, into several transactions, and can combine many small pieces into fewer large ones.

**Fees.** Every transaction pays a fee to the miner, and **there is a minimum, set by a rule every node checks**: `fee = reward x 1000 x weight / median^2`, in units (1 coin =
100,000,000 units), where `reward` is the block's reward before any penalty, `weight` the transaction's weight and `median` the block median above. **The minimum falls as the
reward does**, and filling a quiet block always costs the same share of the reward, which is what makes flooding the chain expensive. **Worked out from the formula, not
measured:** a typical payment (2 inputs, 2 outputs) costs about 0.0062 coins at the start and about 0.00016 at the tail of 0.5 coins a block. The wallet pays the minimum plus
a margin (**Low** 1.25 times, **Normal** 2 times, **High** 5 times: a higher fee only buys a better place when the pool of waiting transactions is full), and a pool pays the
fees of its payouts itself. **The design follows Monero's; the numbers have not been tried against real traffic.** The rules are in `docs/CONSENSUS_V2.md` section 15 and
`crates/tenero-core/src/v3/rules.rs`.

**Privacy.** On `gamma`, a spend proves with **FCMP++** that the coin it spends is one of **all** the outputs on the chain (not one of 16, as a ring signature does), and
amounts are hidden by commitments with **Bulletproofs+** range proofs: Monero's design, through Monero's own Rust libraries (monero-oxide), pinned to the exact commit Monero's
test network uses. Outputs and addresses are **Carrot**: subaddresses, integrated addresses, protection against a sender linking two of your addresses ("Janus"), and
**view-only wallets** (one that sees every payment and the balance, or one that sees only what comes in). This project wrote Carrot in Rust from the specification; it
reproduces Monero's C++ results bit for bit in the tests. **What that does not make it:**

* **Not audited as used.** The FCMP++ libraries are only partly audited (their own notes say which parts), and Monero does not run them on its main network yet. This
  project's Carrot, its curve-tree bookkeeping and the glue around them are not audited at all.
* **A small crowd at the start.** A spend hides among the outputs that exist: on a new chain that is none at first, then about one more a block, almost all of them block rewards of a few miners.
  **The first spends on `gamma` hide among very few people**, and timing still leaks. The wallet does not warn about this yet.
* **Signatures and payment proofs are our own.** Neither Monero nor the Carrot specification defines them yet, so this project composed them from textbook parts
  ([`docs/WALLET_PROOFS.md`](docs/WALLET_PROOFS.md)). **Nobody has reviewed them.** The program says so wherever it offers them.

### The proof of work, and what is and is not claimed about ASICs

Each attempt takes a block header and a nonce and derives a **64 x 8192 matrix of 8-bit integers** from ChaCha20. It multiplies that by **one 16 MiB slice** of a dataset, chosen by
the attempt itself, in exact integer arithmetic (the kind of work GPU tensor cores do), and folds the result into a hash. The dataset is **4 GiB** (256 slices) and is rebuilt
every **epoch** (100 blocks); each slice is built from earlier ones with data-dependent picks, so it has to be built in order and cannot be generated on the fly cheaply.
Checking a block needs the dataset too: about **4.3 GiB of RAM**, and about 0.1 seconds per check.

**The argument for ASIC resistance** is that an attempt is limited by how fast memory can be read (it reads a whole 16 MiB slice), which is what a GPU's memory system is built
for, and a special-purpose chip would need the same memory to compete. **That is an argument, not a proof.** Specifically:

* **Found broken, and fixed (2026-10-07):** in the first design the slice an attempt reads is known from two cheap
  hashes of its nonce, so a miner can choose nonces 16 to a slice and multiply them against ONE read of it, which makes it limited by int8 multiply speed,
  not memory. An attempt now gathers 2,048 columns of 8 KiB from the whole dataset, and memory is the limit again
  ([`docs/CONSENSUS.md`](docs/CONSENSUS.md) 8.3): on `gamma` from block 0 (on `beta` from a fork at height 500; `alpha` keeps the first design).
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

* **`tenerod`**, the node: keeps the chain, relays blocks and payments, checks everything. It is **pruned by default** (it keeps the proofs of the last 5,500 blocks only, so its disk grows slowly; the setting `prune_keep = 0` keeps everything, as seeds and the explorer do). Peers talk over an encrypted channel (the Noise protocol, through the `snow` library, which has not had a formal audit).
* **`tenero-miner`** and the miner built into the node: GPU (NVIDIA, through CUDA), CPU, and a SHA-256 backend that only the `test` network uses. `tenero-miner --pool` works for a **mining pool** with no node at all.
* **`tenero-pool`** is a mining pool (PPLNS accounts, payouts from its own wallet) and **`tenero-poolcheck`** checks any pool against the written protocol ([`docs/POOL_PROTOCOL.md`](docs/POOL_PROTOCOL.md), [`docs/RUNNING_A_POOL.md`](docs/RUNNING_A_POOL.md)).
* **`tenero-wallet`**, the command-line wallet, and **`tenero-wallet-gui`**, the wallet app: a window with several wallets and accounts, a 24-word seed phrase, sending with three fee levels, receiving
  with a QR code, subaddresses, message signatures and payment proofs (**our own construction, unreviewed**), and view-only wallets (made from a view key: they see and cannot spend or sign). It starts and stops the node and the miner itself, and has a tick box, off by default, to let other nodes connect to yours (see [The `gamma` network](#the-gamma-network-and-its-pool)). Its Mining tab can mine alone or for a pool, and its Send tab splits large payments and combines pieces.
* **`tenero-explorer`**, the block explorer: a read-only window onto a node on the same machine (difficulty, an ESTIMATED hash rate, emission, the latest blocks).
* **`tenero-seedcheck`** checks a list of seed nodes (it cannot tell whether one is honest).
* The programs talk to the node over a **control interface that only listens on the local machine**.

## Get it

**The latest release, `v0.3.0-gamma.1`, is on the [Releases](../../releases) page.** It runs the new `gamma` network (and the `dev` and `test` networks for testing); **it does
not run `beta` or `alpha`**: for those, keep the 0.2.0 programs, and nothing moves from `beta` to `gamma`. Each release is a Windows zip and a Linux tar.gz with a `SHA256SUMS`
file. **It is a test release and says so in its notes and this page: "released" means published for people to try, not finished, audited or safe, and the network may be
reset.** How it is made and checked is in [`docs/RELEASING.md`](docs/RELEASING.md). The files are **not code-signed** (Windows SmartScreen will warn), and the Linux build has
been run by hand **only as a node and a pool** (in WSL2 and on rented servers; the Linux wallet app and GPU miner have not been run). Prefer to build it yourself? The next
section does that.

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
| Run a node or the wallet | Windows 11 (the only Windows tried) or Linux (built for Ubuntu 22.04 or newer; **run by hand as a node, the pool and the command-line wallet**: in WSL2 and on one rented server); on `gamma` or `dev`, about **4.3 GiB of free RAM** for the node (it holds one 4 GiB dataset at a time, and pauses about 3 seconds for the first block of each 100-block epoch while it builds the next); **mining adds its own memory** |
| Mine on a GPU | an NVIDIA GPU of the RTX 30 series or newer (compute capability 8.0+; A100, H100 and the like too) with a **driver for CUDA 13 (R580 or newer)**. **The CUDA Toolkit is not needed** (the kernels are compiled into the miner and the driver loads them; releases up to v0.2.0-beta.3 needed the Toolkit); nothing from NVIDIA is shipped here. One 4 GiB dataset is kept in video memory (the miner process commits about 5 GiB on Windows, of which under 0.5 GiB is RAM in use): **only a 16 GB card has been tried**; whether an 8 GB card works is untested |
| Mine on a CPU | nothing extra, but on `gamma` it is **impractical** (see the next section) |

**An AMD (or other non-NVIDIA) GPU miner:** today the GPU miner is **NVIDIA only** (it is written for CUDA). If someone wants to build one for AMD cards, the author is 100% fine with that and would be glad to see it; the author
**cannot test it, because they do not own an AMD card.** What such a miner has to match is exact: the proof of work is specified in [`docs/CONSENSUS.md`](docs/CONSENSUS.md) (section 8.2) and pinned by the golden vectors in [`tests/vectors/`](tests/vectors/README.md), the existing
CUDA kernels are in [`crates/tenero-gpu/kernels/matmulhash.cu`](crates/tenero-gpu/kernels/matmulhash.cu), and the rule is **bit for bit, or it is wrong**: a block the node's check refuses is just wasted work. Nothing about its speed or safety would be claimed until someone measures it on a card they own.

## The `gamma` network, and its pool

**`gamma` is the network of `v0.3.0-gamma.1`**: a fresh chain (label "tenero gamma network 1", chain id
`bd366b37dc59f25d5d2e15aecd1d5c14810b5c20643f2c0f90384db9ee4c28c4`) with no premine, FCMP++ and Carrot from block 0, the gathered proof of work from block 0, a starting
target of 2^237 (about 524,000 attempts a block), control port 38352 and peer port 38353. Its addresses start `TENg`. **It is a test network: it can be reset.**

* **Two seeds are built in: the author's servers `195.26.244.245:38353` and `194.238.27.60:38353`.** **Two computers, one person**: if the author is gone, both are, and their
  operator could show a new node a false chain. The plan wants several independent operators ([`docs/SEED_POLICY.md`](docs/SEED_POLICY.md)). You can add your own
  (`--seed ip:port`, see [`docs/RUNNING.md`](docs/RUNNING.md)) or drop the built-in ones (`no_builtin_seeds`). A node that holds three outbound peers that are not seeds leaves
  its seed.
* **GPU or nothing:** one GPU at about 43,000 attempts a second (one RTX 5070 Ti, measured) needs roughly 12 seconds for 524,000 attempts (arithmetic, not a measurement of
  the network). A 6-thread CPU managed 164 attempts a second on the first design (the gathered attempt on a CPU was not measured): **CPU mining of `gamma` is impractical** at
  the start. The difficulty adjusts, so these change as miners come and go.
* **Mining pools.** In a pool the block reward goes to **the pool**, which pays the miners **by its own rules, and nothing makes it pay**; the pool also sees each miner's
  address and internet address. A miner needs **no node** to mine for a pool (a node is still needed to see and spend what the pool pays). The miner pins the pool's public
  key, so a person between them is refused. **One pool is built in: the author's test pool on `gamma`** (`195.26.244.245:38335`): PPLNS, pays once an hour every miner owed at
  least 0.1 coins, no fee. **It is one computer run by one person.** The app mines alone unless you choose a pool.
* **Letting others connect to you** (the wallet app's Settings, "Let other nodes connect to me", off by default) lets your node serve blocks to others, so they do not all rely
  on the seeds. **It needs a TCP port forwarded on your router and allowed in your firewall, it lets strangers connect to your computer, and the software is unaudited.** See
  [`docs/TESTING.md`](docs/TESTING.md).
* **If something goes wrong,** the only place a "stop mining" notice is posted is the pinned issue, [issue #1](https://github.com/zad112/Tenero/issues/1), and only the author
  can post there. The plan behind it, and what is not yet rehearsed, is [`docs/EMERGENCY_PLAN.md`](docs/EMERGENCY_PLAN.md).
* The release notes are [`docs/releases/v0.3.0-gamma.1.md`](docs/releases/v0.3.0-gamma.1.md): read them before running anything.

## The older networks: `beta` and `alpha`

`beta` (2026-10-07, chain id `577cc63d…641b`, ports 38342 and 38343) and `alpha` (2026-10-05, control port 38332) ran ring signatures and an *interim* output scheme that was
**not Carrot and gave no Monero-style privacy**. **The 0.3.0 programs do not run them.** They keep going with the 0.2.0 and 0.1.0 programs for as long as their seeds run (the
`beta` pool stops when the `gamma` pool starts on the same server), and nothing on them moves to `gamma`. What happened on them, measured, is in their release notes
([`docs/releases/`](docs/releases/)): on `alpha`, a chain past block 689 mined by the author's GPU and at least one other miner; on `beta`, a seed, a pool and one payout seen.

## Helping with the seed server

The seed servers are rented computers that the author pays for. If you would like to chip in towards that bill, there is a Buy Me a Coffee page: **[buymeacoffee.com/zad112](https://buymeacoffee.com/zad112)**.

**It is a gift towards a server bill, and nothing else.** It buys no coins or tokens, no say in the project, no support and no early access. **Nothing on any Tenero network has any value, the project is not selling coins or
tokens, and none of this is an investment.** Nobody has to give, and the project does not depend on it.

## What is next (no dates are promised)

The author wants an **independent review** of the cryptography (this project's Carrot and signatures first) and of the proof of work before anything could carry value, and a
**longer public test** on `gamma`. Then: a remote wallet for computers without the memory for a node, and Tor for wallets and seeds. None of these has a date.

## The Python reference

[`reference/`](reference/README.md) is the original Python implementation, kept frozen on purpose: the **second, independent implementation** the Rust code is checked against
("bit for bit, or it is wrong"), the generator of the golden vectors in [`tests/vectors/`](tests/vectors/README.md), and the CPU simulation behind the memory-hardness argument. It is
not a program to run. The old Python miner, wallet and command line are on the [`legacy-python`](../../tree/legacy-python) branch and the `python-final` tag, and their old README is in
[`docs/PYTHON_LEGACY.md`](docs/PYTHON_LEGACY.md).

## Documentation

* Rules: [`docs/CONSENSUS.md`](docs/CONSENSUS.md) (the proof of work and the Python reference's data model) and [`docs/CONSENSUS_V2.md`](docs/CONSENSUS_V2.md) (the Rust program's data model, version 3 in section 15)
* FCMP++ and Carrot: [`docs/FCMP_CARROT_PLAN.md`](docs/FCMP_CARROT_PLAN.md) (the plan, the decisions and the risks), [`docs/WALLET_PROOFS.md`](docs/WALLET_PROOFS.md) (signatures and payment proofs: our own)
* Using it: [`docs/RUNNING.md`](docs/RUNNING.md), [`docs/TESTING.md`](docs/TESTING.md), [`docs/TESTNET.md`](docs/TESTNET.md) (a private test network on one machine)
* What can go wrong: [`docs/THREAT_MODEL.md`](docs/THREAT_MODEL.md), [`docs/EMERGENCY_PLAN.md`](docs/EMERGENCY_PLAN.md), [`docs/KNOWN_ISSUES.md`](docs/KNOWN_ISSUES.md)
* Pools: [`docs/POOL_PROTOCOL.md`](docs/POOL_PROTOCOL.md), [`docs/RUNNING_A_POOL.md`](docs/RUNNING_A_POOL.md), [`docs/REMOTE_MINING_PLAN.md`](docs/REMOTE_MINING_PLAN.md)
* The numbers: [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md); the wire format: [`docs/WIRE_PROTOCOL.md`](docs/WIRE_PROTOCOL.md); the plan and its history: [`docs/M10_M11_PLAN.md`](docs/M10_M11_PLAN.md)

## Security, licence

Report a security problem **privately** (the repository's Security tab, "Report a vulnerability"): see [`SECURITY.md`](SECURITY.md). The project is under the
[BSD-3-Clause licence](LICENSE); its dependencies' licences are listed in `THIRD-PARTY-LICENCES.txt` in each release.
