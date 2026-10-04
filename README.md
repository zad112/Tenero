<p align="center">
  <img src="assets/banner.webp" alt="Tenero" width="100%">
</p>

An experimental proof-of-work cryptocurrency, built to learn how these things work. Its centre is a GPU-friendly proof of work, **"matmulhash v2"**: an int8
matrix multiplication against a 4 GiB dataset that is built from ChaCha20 so that every slice depends on the earlier ones. It began as a Python prototype and has
been rewritten in Rust, with Monero-style privacy (ring signatures, hidden amounts) as the design target.

> **This is a learning project, not a currency.** It is **unaudited**, there is **no launched network** (only test networks on one or a few machines), and
> **nothing on it has value**. The proof of work is new and unreviewed, and nothing cryptographic in it has been audited as it is used here. The wallet currently
> uses an **interim output scheme that is not Carrot and not private in Monero's sense**. Do not use it to hold anything of value.

Inspired by Monero's design ideas; not affiliated with or endorsed by the Monero project.

## Where it is

*This README is an interim one. A full rewrite (how it works, how to set it up, the test network plan) is planned as M11.4 in [`docs/M10_M11_PLAN.md`](docs/M10_M11_PLAN.md).*

- **The programs** (Rust, in `crates/`): `tenerod` (the node), `tenero-miner` (the miner: GPU, CPU, and a SHA-256 test backend), `tenero-wallet` (the wallet on the
  command line), `tenero-wallet-gui` (the wallet as a window: it starts and stops the node and the miner, several wallets and accounts, a 24-word seed phrase,
  sending with three fee levels, receiving with a QR code, message signatures and payment proofs, payment requests) and `tenero-seedcheck`.
- **Two networks:** `test` (a SHA-256 chain a CPU can mine) and `dev` (the real matmulhash proof of work; the GPU miner needs an NVIDIA GPU with enough memory
  for the 4 GiB dataset). A first public test release, on a fresh chain with no premine, is planned (M11.2 to M11.4) and **does not exist yet**.
- **Tested:** the Rust workspace has 893 passing tests (measured 2026-10-04; 24 more need a GPU or a lot of memory and are skipped by default), checked against
  golden test vectors made by an independent Python reference (`reference/`). A random-case fuzz run of the protocol engine ran 9 hours with no failure after the
  one bug it found was fixed. That says nothing about bugs it cannot find, and it is not an audit.
- **What is known to be wrong or missing:** [`docs/KNOWN_ISSUES.md`](docs/KNOWN_ISSUES.md) and [`docs/THREAT_MODEL.md`](docs/THREAT_MODEL.md).

## Build and run

Windows, PowerShell (Linux builds in CI and has not been run by hand yet). You need [Rust](https://rustup.rs). `docs/RUNNING.md` is the full guide.

```powershell
cargo build --release
cargo test --workspace           # no GPU needed
```

Run a node on the test network, or open the wallet app (which starts and stops the node and the miner itself):

```powershell
.\target\release\tenerod.exe --data $HOME\tenero-data --network test
.\target\release\tenero-wallet-gui.exe
```

## The Python reference

[`reference/`](reference/README.md) holds the original Python implementation, kept frozen on purpose: it is the **second, independent implementation** the Rust code is
checked against ("bit for bit, or it is wrong"), it generates the golden vectors in [`tests/vectors/`](tests/vectors/README.md), and it holds the CPU simulation of
the memory-hardness argument. It is not a program to run. The old Python miner, wallet and command line were removed; they are preserved on the
[`legacy-python`](../../tree/legacy-python) branch and the `python-final` tag, and their old README is in [`docs/PYTHON_LEGACY.md`](docs/PYTHON_LEGACY.md).

```powershell
cd reference
pip install -r requirements-dev.txt
python -m pytest -q
```

## Documentation

- [`docs/CONSENSUS.md`](docs/CONSENSUS.md) and [`docs/CONSENSUS_V2.md`](docs/CONSENSUS_V2.md): the rules, precisely enough to build another implementation from (version 2 is the Rust program's data model, a draft)
- [`docs/RUNNING.md`](docs/RUNNING.md): running the node, miner and wallets; [`docs/TESTNET.md`](docs/TESTNET.md): a private test network on one machine
- [`docs/THREAT_MODEL.md`](docs/THREAT_MODEL.md), [`docs/EMERGENCY_PLAN.md`](docs/EMERGENCY_PLAN.md), [`docs/KNOWN_ISSUES.md`](docs/KNOWN_ISSUES.md): what can go wrong, and what is done about it
- [`docs/M10_M11_PLAN.md`](docs/M10_M11_PLAN.md): what is done and what is left before a first test release
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) (describes the Python prototype), [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md): the parts and the measured numbers (one machine, an RTX 5070 Ti)

## Licence

See [`LICENSE`](LICENSE).
