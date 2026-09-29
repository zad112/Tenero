# Architecture

What the code is, how the parts fit, and what is expected to survive a rewrite. Rules are in
`CONSENSUS.md`; flaws are in `KNOWN_ISSUES.md`.

## The parts

| file | job | survives a rewrite? |
|---|---|---|
| `tenero/chacha.py` | ChaCha20 in numpy | as a **reference** (the algorithm is fixed by RFC 8439) |
| `tenero/matmulhash.py` | matmulhash v2 on the CPU: dataset, attempt, fold, cheap pre-check | as the **reference**; a fast native version must match it bit for bit |
| `tenero/gpubackend.py` | the three CUDA kernels, the PyTorch backend, the GPU self-test, the searcher | the **CUDA source is already C++** and should carry over unchanged |
| `tenero/pow.py` | proof-of-work algorithms, epoch seeds, the searchers | logic carries over |
| `tenero/checker.py`, `miner.py` | the miner and its separate-process CPU double-check | the design carries over |
| `tenero/chain.py` | rules: rewards, difficulty, block size, validation | **emission, difficulty and fee/size rules carry over; the account-model validation is replaced** |
| `tenero/block.py`, `transaction.py`, `wallet.py` | account model, ECDSA, JSON | **replaced** by the output model |
| `tenero/mempool.py`, `storage.py`, `cli.py` | policy, JSON files, wallet UI | replaced |
| `tenero/analysis.py` | simulation of the cost of not keeping the dataset | reference |
| `tools/make_vectors.py`, `tests/vectors/` | the golden vectors | **the bridge to any new implementation** |
| `tests/cuda_emulator.py` | compiles the real CUDA source with g++ and runs it on the CPU | carries over (it already tests C++) |

## How mining works

1. The miner builds the epoch's 4 GiB dataset **on the GPU** (0.10 s on an RTX 5070 Ti) and searches
   attempts in batches of 32: ChaCha20 kernel for `X`, PyTorch's int8 matmul (`torch._int_mm`), ChaCha20
   fold kernel, SHA-256 on the CPU.
2. A found block is double-checked on the **CPU with an independent implementation** (numpy) in a separate
   low-priority process, while the GPU already searches for the next block. The CPU needs the epoch's dataset
   in RAM (built in the background, prefetched during the last 10 blocks of an epoch, a finished epoch's
   dataset freed at once).
3. Only blocks whose check passed are saved. If a check fails, the block and everything built on it are
   discarded and the search restarts on a fresh dataset. Three failures in a row stop the miner.
4. The whole program stays within a CPU core budget (`--max-cores`, default 6).

## Why there are several implementations

The dataset fill, the fold and the attempt exist as numpy (the reference), a pure-Python-plus-OpenSSL
version in the tests, CUDA kernels (run on the CPU through the emulator in CI, and on the real GPU by
`gpu_test.bat`). They are checked against each other bit for bit. That is what caught mistakes, and it is
the method for any further implementation: **load `tests/vectors/` and match every value**.

## Measured facts (one Windows PC with an RTX 5070 Ti; the CPU numbers are from that PC or a 1-core sandbox)

- GPU: about 22,000 attempts/s (about 47 TOPS of int8 work); the matmul is 66 % of a batch, the ChaCha20
  generation and fold about 23 %, CPU hashing and setup about 11 %. The matmul streams about 65 % of the
  card's memory bandwidth.
- A CPU check with the dataset in RAM: about 0.1 s. Building the CPU dataset in numpy: about 37 s on 6
  threads, about 68 s on one; about 4.3 GiB of RAM per dataset.
- With the check in a separate process the GPU search runs at 72 to 97 % of its speed while a dataset is
  being built; in a thread of the miner it dropped to 14 to 50 %.
- In a sandbox, a hand-written AVX2 C++ fill of the dataset was 4.3x faster than numpy on one thread
  (bit-identical output), and libsecp256k1 verifies a signature 67x faster than python-ecdsa. A naive C++
  port of the fill was only 2x faster than numpy.

## The tests

About 480 tests: chain rules, difficulty, economics, block size (including full 300 kB blocks of real signed
transactions), the CPU reference, the kernels through the emulator (with mutation tests that break the kernel
on purpose), the miner, the checker process, the CLI, the vectors, and the known issues.
`python -m pytest -q` needs no GPU.
