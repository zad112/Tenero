"""The memory-hardness analysis (reference/tenero/analysis.py): the rebuild-cost simulation behind KNOWN_ISSUES 11 and the README's ASIC-resistance
argument. Split out of test_gpu_script.py when the Python GPU program was retired (M11.1); the analysis stays because the argument
rests on it. It is an ESTIMATE from a small simulation, not a proof."""
import pytest

np = pytest.importorskip("numpy")

from tenero import analysis  # noqa: E402
from .test_matmulhash import rebuild_cost as independent_rebuild_cost  # noqa: E402


def test_rebuild_cost_agrees_with_an_independent_version():
    params, data = analysis.small_dataset(num_slices=32, blocks=32)
    import random
    rng = random.Random(3)
    for _ in range(10):
        stored = set(rng.sample(range(1, 32), 20)) | {0}
        b = rng.choice([x for x in range(1, 32) if x not in stored])
        assert analysis.rebuild_cost(data, params, stored, b) == independent_rebuild_cost(data, params, stored, b)


def test_a_stored_slice_costs_nothing_to_rebuild():
    params, data = analysis.small_dataset(num_slices=16, blocks=16)
    assert analysis.rebuild_cost(data, params, set(range(16)), 7) == 0
    assert analysis.rebuild_cost(data, params, {0}, 1) == 1.0            # only slice 0 is free


def test_the_cost_table_climbs_as_less_is_stored():
    table = analysis.rebuild_cost_table(fractions=(0.9, 0.7, 0.5), num_slices=64, blocks=16, trials=6)
    assert table[0.9] < table[0.7] < table[0.5]
    assert table[0.9] >= 1.0                                            # a missing slice costs at least itself
    assert table == analysis.rebuild_cost_table(fractions=(0.9, 0.7, 0.5), num_slices=64,
                                                blocks=16, trials=6)     # deterministic


def test_estimated_slowdown_and_the_within_helper():
    assert analysis.estimated_slowdown(1.0, 99.0, 1.0, 1.0) == 1.0     # nothing missing, nothing to pay
    assert analysis.estimated_slowdown(0.8, 5.0, 2.0, 1.0) == pytest.approx(1 + 0.2 * 5 * 2)
    table = {0.9: 1.0, 0.7: 3.0, 0.5: 20.0}
    assert analysis.fraction_within(table, 1.0, 1.0, factor=2.0) == 0.7   # 1 + 0.3*3 = 1.9
    assert analysis.fraction_within(table, 1.0, 1.0, factor=1.05) is None
    assert analysis.fraction_within({}, 1.0, 1.0) is None
