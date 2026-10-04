# reference/: the frozen Python implementation (M11.1)

**This is not a program to run.** It is the original Python prototype's code that is kept on purpose, for three jobs:

1. **A second, independent implementation.** The Rust code is checked against it ("bit for bit, or it is wrong", `CLAUDE.md` rule 4). Deleting it would leave one
   implementation checking itself. The data model, the wire format, the interim wallet scheme, the control protocol and the wallet proofs each have their own
   Python reference in `tools/`, written separately from the Rust.
2. **The generator of the golden vectors** in `../tests/vectors/` (`tools/make_vectors*.py`; see that folder's README). A consensus change starts here (rule 1).
3. **The evidence for two claims:** `tenero/analysis.py` is the simulation behind the memory-hardness argument (an **estimate**, not a proof; `docs/KNOWN_ISSUES.md` 11),
   and `tenero/gpubackend.py` holds the CUDA kernel source, which `tests/test_fused_kernels.py` runs on the CPU (through `tests/cuda_emulator.py`, with g++) against the
   numpy reference: the only check of the kernel's logic that runs without a GPU. `tests/test_kernel_source_copy.py` keeps the Rust copy of the kernels
   (`crates/tenero-gpu/kernels/matmulhash.cu`) identical to it.

What is here: `tenero/` (the package: `chacha`, `matmulhash`, `pow`, `chain`, `block`, `transaction`, `wallet`, `units`, `config`, `storage`, `paths`, `analysis`,
`gpubackend`), `tools/` (the vector generators), `tests/`, `difficulty_sim.py` (a difficulty simulation), `pytest.ini`, `requirements*.txt`.

**What is NOT here, and where it went:** the Python miner, wallet, command line, viewer, GPU script and `.bat` files were retired at M11.1. They are on the
`legacy-python` branch and the `python-final` tag. The account-model chain code that remains (`chain.py`, `block.py`, `transaction.py`, `wallet.py`) is kept only
because the vector generators call it for the emission, difficulty and fee arithmetic; **it still has the flaws in `docs/KNOWN_ISSUES.md` 1 to 10**, and nothing
shipped uses it.

```powershell
cd reference
pip install -r requirements-dev.txt        # numpy, ecdsa, pytest (cryptography is optional)
python -m pytest -q                        # about 340 tests, no GPU needed; 5 expected failures are the known issues
cd ..
python reference/tools/make_vectors.py --check        # and the other make_vectors_*.py: do the golden vectors still match?
```
