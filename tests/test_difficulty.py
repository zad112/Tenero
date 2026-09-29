import itertools
import json
import random
import time

import pytest

import toycoin.block as block_mod
from toycoin.block import Block
from toycoin.chain import Blockchain
from toycoin.transaction import Transaction, COINBASE
from toycoin.units import UNIT

T = 30              # target block time used in these tests
START = 2**250      # starting target: about 64 attempts per block, so mining is instant
MINER = "a" * 40
BASE_TIME = 1_000_000


def new_chain(window=10, **overrides):
    params = dict(target=START, initial_reward=50 * UNIT, halving_interval=10_000,
                  max_supply=10**9 * UNIT, tail_reward=0, block_time=T,
                  difficulty_window=window)
    params.update(overrides)
    return Blockchain(**params)


def run(bc, gaps):
    # mine one block per gap, with timestamps that are `gap` seconds apart
    t = BASE_TIME
    for gap in gaps:
        t += gap
        bc.mine_block(MINER, timestamp=t)
    return t


def test_steady_block_times_keep_difficulty_constant():
    bc = new_chain()
    run(bc, [T] * 15)
    _, targets = bc._history()
    assert set(targets) == {START}
    assert bc.is_valid()


def test_fast_blocks_make_it_harder():
    bc = new_chain()
    run(bc, [1] * 8)
    _, targets = bc._history()
    assert targets[1] == targets[2] == START           # too early to adjust
    assert targets[3] == START // 4                    # limited to 4x per block
    assert all(targets[i + 1] < targets[i] for i in range(2, len(targets) - 1))
    assert bc.is_valid()


def test_slow_blocks_make_it_easier_up_to_the_maximum():
    bc = new_chain()
    run(bc, [6 * T] * 8)
    _, targets = bc._history()
    assert targets[3] == START * 4
    assert targets[-1] == 2**256 - 1                   # cannot get easier than this
    assert bc.is_valid()


def test_no_adjustment_when_window_is_zero():
    bc = new_chain(window=0)
    run(bc, [1] * 8)
    _, targets = bc._history()
    assert set(targets) == {START}
    assert bc.is_valid()


def simulate(bc, hashrate_at, blocks, seed=1):
    # pure-math mining: each block's time is random with a mean set by the current
    # difficulty and hashrate, like real mining. Returns the solve time of each block.
    rng = random.Random(seed)
    ts, targets = [0], [bc.target]
    t = BASE_TIME
    solves = []
    for pos in range(1, blocks + 1):
        target = bc._retarget(ts, targets, pos)
        hashes_needed = 2**256 / target
        dt = rng.expovariate(hashrate_at(pos) / hashes_needed)
        step = int(dt)
        t += step
        solves.append(step)
        targets.append(target)
        ts.append(t)
    return solves


def mean(xs):
    return sum(xs) / len(xs)


def test_difficulty_follows_a_hashrate_jump_up():
    bc = new_chain(window=30)
    base_rate = (2**256 / START) / T   # the rate that makes START exactly right
    solves = simulate(bc, lambda pos: base_rate * (10 if pos > 150 else 1), 500)
    assert mean(solves[100:150]) == pytest.approx(T, rel=0.25)   # settled before the jump
    assert mean(solves[150:160]) < T * 0.6                        # blocks come fast at first
    assert mean(solves[300:500]) == pytest.approx(T, rel=0.25)   # then it re-tunes


def test_difficulty_follows_a_hashrate_drop():
    bc = new_chain(window=30)
    base_rate = (2**256 / START) / T
    solves = simulate(bc, lambda pos: base_rate * (0.1 if pos > 150 else 1), 500)
    assert mean(solves[150:160]) > T * 2                          # blocks slow down at first
    assert mean(solves[300:500]) == pytest.approx(T, rel=0.25)   # and it recovers, no death spiral


def test_easier_than_required_target_is_rejected():
    bc = new_chain()
    t = run(bc, [1] * 6)
    _, targets = bc._history()
    old, required = targets[-1], bc.next_target()
    assert required < old

    height = len(bc.chain)
    reward = Transaction(COINBASE, MINER, bc.reward_at(height))
    cheat = Block(height, [reward], bc.chain[-1].hash, timestamp=t + 1)
    while True:  # meets the OLD (easier) target but not the one now required
        cheat.hash = cheat.compute_hash()
        if required <= int(cheat.hash, 16) < old:
            break
        cheat.nonce += 1
    bc.chain.append(cheat)
    assert not bc.is_valid()

    honest = new_chain()
    t = run(honest, [1] * 6)
    honest.mine_block(MINER, timestamp=t + 1)
    assert honest.is_valid()


def test_timestamp_older_than_recent_median_is_rejected():
    bc = new_chain()
    run(bc, [T] * 12)
    assert bc.is_valid()
    bc.mine_block(MINER, timestamp=BASE_TIME + 5)   # far older than the recent blocks
    assert not bc.is_valid()


def test_timestamp_from_the_future_is_rejected():
    bc = new_chain()
    run(bc, [T] * 5)
    bc.mine_block(MINER, timestamp=int(time.time()) + 10_000)
    assert not bc.is_valid()


def test_real_mining_stamps_the_time_it_was_found():
    bc = new_chain()
    run(bc, [T] * 3)
    block = bc.mine_block(MINER)                    # no fixed timestamp: real clock
    assert abs(int(block.timestamp) - time.time()) < 5
    assert bc.is_valid()


def test_mining_keeps_the_hash_consistent_when_the_timestamp_moves(monkeypatch):
    # every clock reading is one second later, so the timestamp changes mid-search
    clock = itertools.count(BASE_TIME)
    monkeypatch.setattr(block_mod.time, "time", lambda: next(clock))
    block = Block(1, [Transaction(COINBASE, MINER, 1)], "0" * 64, timestamp=0)
    target = 2**256 // 300_000                      # several refreshes' worth of work
    block.mine(target, refresh_timestamp=True)
    assert block.timestamp > BASE_TIME + 1          # it really was refreshed
    assert block.hash == block.compute_hash()
    assert block.meets_target(target)


def test_min_timestamp_is_respected():
    block = Block(1, [Transaction(COINBASE, MINER, 1)], "0" * 64, timestamp=0)
    floor = int(time.time()) + 1000
    block.mine(START, refresh_timestamp=True, min_timestamp=floor)
    assert block.timestamp >= floor


def test_recent_stats():
    bc = new_chain()
    assert bc.recent_stats() is None
    run(bc, [T] * 12)
    avg_time, rate = bc.recent_stats()
    assert avg_time == T
    assert rate == pytest.approx((2**256 // START) / T)


def test_difficulty_settings_are_saved_and_old_chains_stay_fixed(tmp_path):
    bc = new_chain(window=5, block_time=20)
    run(bc, [20] * 4)
    path = str(tmp_path / "chain.json")
    bc.save(path)
    loaded = Blockchain.load(path)
    assert (loaded.difficulty_window, loaded.block_time) == (5, 20)
    assert loaded.is_valid()

    # a chain saved before the adjustment existed has no such settings: fixed difficulty
    with open(path) as f:
        d = json.load(f)
    del d["params"]["difficulty_window"], d["params"]["block_time"]
    with open(path, "w") as f:
        json.dump(d, f)
    old = Blockchain.load(path)
    assert old.difficulty_window == 0


def test_recent_stats_ignores_time_when_nobody_was_mining():
    # 12 blocks at the target pace, then a 3 hour gap (the miner was stopped), then 6 more
    bc = new_chain()
    t = BASE_TIME
    for i in range(12):
        t += T
        bc.mine_block(MINER, timestamp=t)
    t += 3 * 3600
    for i in range(6):
        t += T
        bc.mine_block(MINER, timestamp=t)
    avg_time, rate = bc.recent_stats()
    ts, targets = bc._history()
    last = len(bc.chain) - 1
    window = range(last - 9, last + 1)                          # the 10 blocks the estimate looks at
    counted = [i for i in window if ts[i] - ts[i - 1] <= 6 * T]
    assert len(counted) == 9                                    # the block after the gap is left out
    assert avg_time == pytest.approx(T)                         # so the gap is not a slow block
    # the rate is the work of the counted blocks over their time (their targets differ,
    # because the difficulty adjustment reacted to the gap)
    expected = sum(2**256 // targets[i] for i in counted) / (T * len(counted))
    assert rate == pytest.approx(expected)
    # what dividing by the raw span would have said: the gap swamps it
    raw = sum(2**256 // targets[i] for i in window) / (ts[last] - ts[last - 10])
    assert rate > 5 * raw


def test_recent_stats_needs_at_least_two_normal_blocks():
    bc = new_chain()
    t = BASE_TIME
    for i in range(6):
        t += 10 * T                                            # every block after a long gap
        bc.mine_block(MINER, timestamp=t)
    assert bc.recent_stats() is None


def test_recent_stats_still_counts_blocks_that_are_merely_slow():
    bc = new_chain()
    t = BASE_TIME
    for i in range(8):
        t += 4 * T                                             # slow, but under the 6x limit
        bc.mine_block(MINER, timestamp=t)
    avg_time, _ = bc.recent_stats()
    assert avg_time == pytest.approx(4 * T)
