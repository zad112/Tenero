# Golden test vectors

Fixed inputs with the outputs the Python reference produces for them. They are how another
implementation of this coin (a C++ or Rust rewrite, a GPU kernel) is checked against the reference
**bit for bit**. Nothing here is random and nothing depends on the clock: regenerating gives
identical bytes.

Read `docs/CONSENSUS.md` for what each rule means. Every file is JSON with `schema`, `name` and
`description` fields; big numbers are hex strings (or ints that fit in 64 bits).

| file | what it pins down | size |
|---|---|---|
| `chacha20.json` | ChaCha20 (RFC 8439): the block function, the bare permutation on arbitrary states, keystream digests | small |
| `matmulhash_small.json` | the proof of work at small sizes: dataset hashes, every step of full attempts (seed, slice, X, C, fold sums, mix, digest), and the fold on its own | small |
| `matmulhash_real.json` | the same at the real parameters, for the first 8 of the 256 slices | small |
| `matmulhash_deep.json` | real parameters, slices up to 77 (each depends on all earlier ones) | slow to check |
| `matmulhash_full.json` | a hash of every one of the 256 slices (**generate it yourself**: see below) | slow, 4.3 GiB RAM |
| `pow_misc.json` | epoch seeds and numbering, `bits_to_target`, the cheap pre-check | small |
| `emission.json` | rewards by height for several schedules (including the edge cases) | small |
| `difficulty.json` | the difficulty adjustment and the median-time rule over 20 block-time scenarios | small |
| `fees_and_size.json` | minimum fee, oversize penalty, the block-size median and its window | small |
| `units.json` | parsing and formatting coin amounts | small |
| `chains.json` | 36 whole chains and whether they are valid; each `rule:` case breaks exactly one rule | small |
| `legacy_account_model.json` | the CURRENT account-model formats (Python-JSON serialization, ECDSA over SHA-1). **Legacy: documents what the reference does, not what to copy** | small |

## Using them

```
python tools/make_vectors.py --check          # do the committed files match the reference?
python -m pytest tests/test_vectors.py -q     # Python checks itself against them
TOYCOIN_SLOW_VECTORS=1 python -m pytest tests/test_vectors.py -q     # also deep and full
```

A new implementation should load these files and reproduce every value. Start with `chacha20.json`,
then `matmulhash_small.json`, then `chains.json`.

## The full vector

`matmulhash_full.json` is not committed because it needs the whole 4 GiB dataset in memory to
produce. Generate it once on a machine with about 4.3 GiB of free RAM (about a minute on 4 threads)
and commit it:

```
python tools/make_vectors.py --full --threads 4
```

## Changing a vector

A changed vector is a **consensus change**. Do not edit the files by hand. Change the reference,
update `docs/CONSENSUS.md`, run `python tools/make_vectors.py --write` (add `--deep` if the
proof of work changed), and say in the commit message what changed and why.
