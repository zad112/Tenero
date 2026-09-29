"""What does it cost to NOT keep the whole dataset in memory? A simulation on the CPU reference.

The dataset is built so that a block depends on the previous block of the previous slice and on
several data-dependent picks from anywhere earlier (see toycoin.matmulhash). A miner that keeps
only some slices must rebuild a missing one from what it has, and the picks make that snowball.

This measures it on a stack of `num_slices` slices (the REAL depth, 256 by default) whose slices
are tiny, so it runs in about a second. It counts blocks that would have to be recomputed; one
slice of the real dataset is `blocks_per_slice` blocks, so the result is in "slices' worth of
work". It is a simulation of the dependency structure, not a proof of memory-hardness.
"""
import random

import numpy as np

from . import matmulhash as mh


def small_dataset(num_slices=256, blocks=32, seed=b"analysis"):
    """(params, dataset) with `num_slices` real-depth slices of `blocks` 64-byte blocks each."""
    params = mh.Params(m=8, k=64, nb=blocks, num_blocks=num_slices).validate()
    return params, mh.build_dataset(params, seed)


def rebuild_cost(data, params, stored, b):
    """Slices' worth of blocks that must be recomputed to obtain slice b when only the slices in
    `stored` are kept (slice 0 can always be regenerated from the seed for free)."""
    blocks = params.blocks_per_slice
    view = data.reshape(params.num_blocks, blocks, mh.BLOCK_WORDS)
    needed, stack = set(), [(b, u) for u in range(blocks)]
    while stack:
        node = stack.pop()
        j, u = node
        if node in needed or j in stored or j == 0:
            continue
        needed.add(node)
        prev = view[j - 1, u]
        stack.append((j - 1, u))
        for i in range(mh.PICKS):
            stack.append((int(prev[2 * i] % j), int(prev[2 * i + 1] % blocks)))
    return len(needed) / blocks


def rebuild_cost_table(fractions=(0.95, 0.9, 0.8, 0.7, 0.6, 0.5), num_slices=256, blocks=32,
                       trials=12, seed=7):
    """{fraction stored: average slices' worth of work to rebuild one MISSING slice}, over
    `trials` random choices of which slices are kept and which one is wanted."""
    params, data = small_dataset(num_slices, blocks)
    table = {}
    for f in fractions:
        rng = random.Random(seed)
        keep = min(num_slices - 2, int(f * (num_slices - 1)))       # always leave one to rebuild
        costs = []
        for _ in range(trials):
            stored = set(rng.sample(range(1, num_slices), keep)) | {0}
            missing = [b for b in range(1, num_slices) if b not in stored]
            costs.append(rebuild_cost(data, params, stored, rng.choice(missing)))
        table[f] = float(np.mean(costs))
    return table


def estimated_slowdown(fraction, rebuild_slices, t_slice, t_attempt):
    """How many times slower a miner that keeps only `fraction` of the dataset runs, if a slice
    rebuild costs `t_slice` seconds per slice and an attempt on a stored slice `t_attempt`:
    every attempt pays for a rebuild with probability (1 - fraction)."""
    return 1.0 + (1.0 - fraction) * rebuild_slices * t_slice / t_attempt


def fraction_within(table, t_slice, t_attempt, factor=2.0):
    """The smallest stored fraction in the table whose slowdown is at most `factor`, or None."""
    ok = [f for f, cost in table.items()
          if estimated_slowdown(f, cost, t_slice, t_attempt) <= factor]
    return min(ok) if ok else None
