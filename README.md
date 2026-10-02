<p align="center">
  <img src="assets/banner.webp" alt="Tenero" width="100%">
</p>

An experimental proof-of-work cryptocurrency in Python, built to learn how these things work.
The interesting part is its **GPU proof of work, "matmulhash v2"**: an int8 matrix multiplication
against a 4 GiB dataset that is built from ChaCha20 in a way that makes every slice depend on the
earlier ones.

> **This is a learning project, not a currency.** The cryptography is standard (secp256k1 ECDSA,
> SHA-256, ChaCha20) but the design has not been audited, there is no networking yet (one node
> only), and the proof of work is new and unreviewed. Do not use it to hold anything of value.

Inspired by Monero's design ideas; not affiliated with or endorsed by the Monero project.

## What it has

- **Chain and wallet.** Signed transactions with fees and memos, amounts in units of 0.0001,
  a command-line wallet, a mempool, atomic saves.
- **Emission.** 20 coins per block to start, halving every 525,600 blocks (about a year), 20,000,000
  coins of main emission (reached at block 2,334,400, about 4.4 years in), then a 0.5 coin tail
  forever. 60-second blocks.
- **Difficulty.** LWMA (window 30, at most 4x per block) with timestamp rules.
- **Flexible block size.** Monero-style: blocks up to 300 kB are free, the reward shrinks
  quadratically above the median, and the hard limit is twice the median (600 kB at the floor).
- **GPU proof of work (matmulhash v2).**
  - Each attempt multiplies a ChaCha20-generated matrix by one slice of the dataset (int8 tensor
    cores, exact int32 results) and folds the product with the ChaCha20 permutation.
  - The 256-slice dataset changes every epoch (100 blocks). Each slice is built from the previous
    one plus three data-dependent picks from anywhere earlier, so keeping only part of the dataset
    makes rebuilding a missing slice snowball (see `tenero/analysis.py`).
  - Blocks carry the fold result (`mix`), so a node can reject a tampered block in microseconds
    (SHA-256 of seed + mix must equal the hash and meet the target) before doing the expensive
    recomputation.
- **Miner.** Runs on the GPU, and double-checks every block on the CPU in a separate low-priority
  process while the GPU searches for the next one. The whole program stays within a CPU core budget
  (`--max-cores`, default 6).

## Requirements

- Python 3.12 or newer (developed on 3.14 on Windows; the test suite also runs on 3.12 on Linux).
- `numpy` and `ecdsa`: `pip install -r requirements.txt`
- **GPU mining only:** an NVIDIA GPU with at least 8 GB of VRAM, a CUDA build of PyTorch, and CuPy.
  The 4 GiB dataset lives in VRAM.
- About 4.3 GiB of RAM for the CPU double-check (about 8.6 GiB for the last 10 blocks of each epoch,
  while the next epoch's dataset is prepared).

## Quick start (Windows PowerShell)

```powershell
python -m venv .venv
.venv\Scripts\activate
pip install -r requirements.txt

python cli.py                                   # the wallet: type `help`
python miner.py <your-address> --pow sha256     # a small chain a CPU can mine, for trying things out
```

### GPU mining

```powershell
# install a CUDA build of PyTorch for your GPU: https://pytorch.org/get-started/locally/
pip install "cupy-cuda13x[ctk]"                 # choose the CuPy package that matches your CUDA
.\gpu_test.bat                                  # checks the GPU against the CPU bit for bit, then benchmarks
python miner.py <your-address>
```

`gpu_test.bat` (or `python gpu_pow_test.py`) is the real test of the GPU code. It compares the GPU's
dataset, ChaCha20 kernels, fold and full attempts with the CPU reference, mines and verifies a
solution, benchmarks the pipeline, and prints what keeping only part of the dataset would cost.

The wallet is a separate program from the miner:

| command | what it does |
|---|---|
| `address`, `balance` | your address, a balance |
| `send <address> <amount> [slow\|normal\|fast\|fee] ["memo"]` | sign and queue a payment |
| `fees`, `pending`, `history` | fee tiers and block space, queued and confirmed transactions |
| `blocks`, `chain`, `supply`, `difficulty` | look at the chain, the emission and the mining |
| `verify` | re-check every block's proof of work |
| `wallet <name>`, `wallets` | switch or list wallets |

## Where your data lives

By default the **wallets, `chain.json` and `mempool.json` are stored in the project folder**. A wallet
file holds a private key, so `.gitignore` excludes them, and **you should never commit them**. To keep
data elsewhere (a scratch chain for experiments):

```powershell
$env:TENERO_DATA = "$HOME\scratch"
```

Settings are in `tenero/config.py`. The supply, halving, tail, block-time and difficulty settings are
saved into `chain.json` when a chain is created, so editing them only affects a new chain: delete
`chain.json` and `mempool.json` to apply a change. The fee and block-size settings apply immediately
to whatever chain is loaded and are consensus rules, so changing them can invalidate an existing chain.

## Tests

```powershell
pip install -r requirements-dev.txt
python -m pytest -q
```

About 550 tests, no GPU needed. The CUDA kernels are checked by compiling the real kernel source with
g++ and running it on the CPU (`tests/cuda_emulator.py`) against a numpy reference, which is itself
checked against an independent OpenSSL-based implementation. If g++ is missing those tests are
skipped. GitHub Actions is set up to run the suite on Linux for Python 3.12, 3.13 and 3.14
(`.github/workflows/tests.yml`).

## Documentation

- [`docs/CONSENSUS.md`](docs/CONSENSUS.md): the rules, precisely enough to build another implementation from
- [`docs/KNOWN_ISSUES.md`](docs/KNOWN_ISSUES.md): verified flaws in the current design
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md): how the parts fit, and what is measured
- [`docs/REWRITE_PLAN.md`](docs/REWRITE_PLAN.md): the proposed plan for a native rewrite with privacy
- [`tests/vectors/`](tests/vectors/README.md): golden test vectors that pin the reference down bit for bit
  (`python tools/make_vectors.py --check`)

## Layout

```
miner.py            the miner            cli.py     the wallet
view.py             print the chain      demo.py    a tiny SHA-256 demo
gpu_pow_test.py     GPU vs CPU checks and benchmark
difficulty_sim.py   try difficulty settings without waiting for blocks
tenero/
  chain.py block.py transaction.py wallet.py mempool.py storage.py units.py config.py paths.py
  pow.py            proof-of-work algorithms and the searchers
  matmulhash.py     matmulhash v2 (CPU reference: dataset, attempt, fold, cheap pre-check)
  chacha.py         ChaCha20 in numpy
  gpubackend.py     CUDA kernels, the PyTorch backend, the GPU self-test and searcher
  checker.py        the separate-process CPU double-check
  analysis.py       simulation of the cost of not keeping the whole dataset
docs/               the specification and plans
tools/make_vectors.py  generates tests/vectors/ from the reference
tests/              the tests; tests/vectors/ holds the golden vectors
```

## Measured on one machine (RTX 5070 Ti)

About 22,000 attempts per second (roughly 47 TOPS of int8 work), the 4 GiB dataset built on the GPU in
0.10 s, a CPU check of a block in 0.1 s, and the CPU reference dataset built in about 37 s on 6 threads
(about 68 s on one). These are one machine's numbers, not guarantees.

## Known limits and ideas

- One node only: no networking or fork-choice rule yet.
- The chain is rewritten to a single JSON file on every save, which will not scale to sustained full
  blocks (a day of full 300 kB blocks is about 530 MB). A database would fix that.
- No privacy yet: addresses and amounts are public.
- The memory-hardness numbers come from a simulation of the dependency structure, not a proof, and the
  fill and fold functions are unaudited.
- Ideas: networking with a cumulative-work fork rule, an output model with stealth addresses and hidden
  amounts, a database for storage, a written threat model.

## License

BSD 3-Clause: see [`LICENSE`](LICENSE).
