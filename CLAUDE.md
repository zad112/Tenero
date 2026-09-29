# CLAUDE.md

**Tenero**: an experimental proof-of-work coin in Python with a
GPU proof of work ("matmulhash v2"), being prepared for a rewrite to native code with Monero-style
privacy. It is a learning project: unaudited, one node, not for real value. Never describe it otherwise.

## Commands (owner's setup: Windows, PowerShell, a `.venv`, Python 3.14, an RTX 5070 Ti)

```powershell
.venv\Scripts\activate                       # use the venv's python, never the system one
pip install -r requirements-dev.txt
python -m pytest -q                          # about 550 tests, no GPU needed
python tools/make_vectors.py --check         # do the golden vectors still match the reference?
$env:TENERO_SLOW_VECTORS = "1"; python -m pytest tests/test_vectors.py -q     # also the deep vectors
.\gpu_test.bat                               # the REAL GPU check: only on the owner's machine
python miner.py <address> [--pow sha256]     # sha256 = a small chain a CPU can mine
$env:TENERO_DATA = "$HOME\scratch"          # do experiments on a scratch chain, not the real one
```

## Where things are

- `docs/CONSENSUS.md` the rules, in enough detail to build another implementation from
- `docs/KNOWN_ISSUES.md` verified flaws not to carry over; `docs/ARCHITECTURE.md` the parts;
  `docs/REWRITE_PLAN.md` the proposed plan and the open decisions
- `tests/vectors/` golden vectors (see its README); `tools/make_vectors.py` generates them

## Rules

1. **A consensus change is deliberate.** Change the reference, update `docs/CONSENSUS.md`, regenerate the
   vectors with `python tools/make_vectors.py --write` (add `--deep` if the proof of work changed), and say
   what changed and why in the commit. Never edit a vector file by hand. `test_the_committed_vectors_are_current`
   fails if the reference and the vectors disagree.
2. **Never commit secrets or data.** `wallets/`, `chain.json` and `mempool.json` are git-ignored and hold
   private keys and coins. Check `git status` before every commit, and never print or log a private key.
3. **No home-made cryptography.** Use audited libraries. Ask before adding a dependency, and note its licence.
4. **Bit for bit, or it is wrong.** Any implementation of ChaCha20, the dataset fill, the fold or the attempt
   must reproduce `tests/vectors/` exactly. CUDA changes must pass `tests/test_fused_kernels.py` (which
   includes mutation tests) and, before they are called done, the owner's `gpu_test.bat`.
5. **Say what is measured and what is estimated.** There is no GPU in CI or in a sandbox, so never claim GPU
   speed without a number from the owner's machine.
6. **Every rule gets a test.** A rule with no vector or test is not finished.
7. **Known issues stay visible.** Do not fix an item in `docs/KNOWN_ISSUES.md` silently: its strict-xfail test in
   `tests/test_known_issues.py` will start passing, which fails the suite until the marker is removed.
8. **Resource limits are design constraints.** The miner stays within 6 CPU cores (`--max-cores`), and the CPU
   check needs about 4.3 GiB of RAM per epoch dataset (about 8.6 GiB briefly during the prefetch).
9. Explain plainly, with the reasoning, and mention risks and uncertainty rather than hiding them.
