"""The coin's economics: 20 coins a block to start, 20 million coins of main emission, 60-second
blocks, a 0.5 coin tail.

These use the defaults in config.py with literal numbers, on purpose: if someone edits the
schedule, these fail and say what changed. The chain is never mined here: the reward schedule is
a pure function of the block height, so it can be checked for all 2.3 million blocks at once.
"""
import pytest

from toycoin import chain as chain_module
from toycoin import config
from toycoin.chain import Blockchain, min_fee_for
from toycoin.units import UNIT, to_units

pytestmark = pytest.mark.real_floor      # these check config.py's own numbers, not the test floor

CAP = 20_000_000 * UNIT
TAIL = UNIT // 2                        # 0.5 coins
FIRST_TAIL_BLOCK = 2_334_401
BLOCKS_PER_YEAR = 365 * 24 * 60         # at 60 seconds a block


def default_chain():
    return Blockchain()                 # every economic setting from config.py


# ---- what was asked for ----

def test_the_settings_are_the_ones_that_were_asked_for():
    assert config.MAX_SUPPLY == 20_000_000
    assert config.TARGET_BLOCK_TIME == 60
    assert config.TAIL_REWARD == 0.5
    assert config.INITIAL_REWARD == 20
    assert config.HALVING_INTERVAL == BLOCKS_PER_YEAR == 525_600


def test_a_default_chain_uses_them():
    bc = default_chain()
    assert (bc.initial_reward, bc.halving_interval) == (20 * UNIT, 525_600)
    assert (bc.max_supply, bc.tail_reward, bc.block_time) == (CAP, TAIL, 60)


def test_a_new_chain_file_records_them(tmp_path):
    bc = Blockchain.load_or_new(str(tmp_path / "chain.json"), algorithm="sha256")
    bc.save(str(tmp_path / "chain.json"))
    loaded = Blockchain.load(str(tmp_path / "chain.json"))
    assert (loaded.initial_reward, loaded.halving_interval) == (20 * UNIT, 525_600)
    assert (loaded.max_supply, loaded.tail_reward, loaded.block_time) == (CAP, TAIL, 60)


def test_an_old_chain_keeps_the_settings_it_was_created_with(tmp_path):
    old = Blockchain(initial_reward=128 * UNIT, halving_interval=2, max_supply=500 * UNIT,
                     tail_reward=UNIT, block_time=30, difficulty_window=0, target=2**250)
    old.mine_block("a" * 40)
    path = str(tmp_path / "chain.json")
    old.save(path)
    loaded = Blockchain.load(path)                       # config.py has changed since
    assert (loaded.initial_reward, loaded.halving_interval) == (128 * UNIT, 2)
    assert (loaded.max_supply, loaded.tail_reward, loaded.block_time) == (500 * UNIT, UNIT, 30)


# ---- the emission schedule ----

def test_the_reward_starts_at_twenty_coins_and_halves_every_year():
    bc = default_chain()
    era = 525_600
    expected = [200_000, 100_000, 50_000, 25_000, 12_500]      # 20, 10, 5, 2.5, 1.25 coins
    assert bc.reward_at(1) == 20 * UNIT
    for k, units in enumerate(expected):
        assert bc.reward_at(k * era + 1) == units              # the first block of each era
        if k < 4:
            assert bc.reward_at((k + 1) * era) == units        # ... and the last one


def test_the_main_emission_is_exactly_twenty_million_coins():
    bc = default_chain()
    assert bc.issued_before(FIRST_TAIL_BLOCK) == CAP
    # an independent sum: four full eras, then the coins left, at the fifth era's reward
    era = 525_600
    four_eras = sum(r * era for r in (200_000, 100_000, 50_000, 25_000))
    assert four_eras == 19_710_000 * UNIT
    assert (CAP - four_eras) % 12_500 == 0                     # divides evenly: nothing to trim
    assert (CAP - four_eras) // 12_500 == 232_000
    assert 4 * era + 232_000 == FIRST_TAIL_BLOCK - 1


def test_the_last_main_block_pays_a_full_reward_and_nothing_overshoots():
    bc = default_chain()
    last = FIRST_TAIL_BLOCK - 1
    assert bc.main_reward_at(last) == 12_500                   # a full 1.25, not a trimmed remainder
    assert bc.main_reward_at(last) == bc.scheduled_reward(last)
    assert bc.main_reward_at(last + 1) == 0                    # and then the main emission is over
    assert bc.issued_before(last) + bc.main_reward_at(last) == CAP


def test_the_tail_starts_at_the_next_block_and_pays_half_a_coin_forever():
    bc = default_chain()
    assert not bc.in_tail(FIRST_TAIL_BLOCK - 1) and bc.in_tail(FIRST_TAIL_BLOCK)
    assert bc.reward_at(FIRST_TAIL_BLOCK - 1) == 12_500        # 1.25, still main emission
    for height in (FIRST_TAIL_BLOCK, FIRST_TAIL_BLOCK + 1, 10_000_000, 10**9, 10**12):
        assert bc.reward_at(height) == TAIL == 5_000
        assert bc.in_tail(height)


def test_no_block_before_the_tail_pays_less_than_the_tail():
    # a whole schedule check, era by era: the tail must never be the bigger of the two before the cap
    bc = default_chain()
    era = 525_600
    for k in range(5):
        assert bc.scheduled_reward(k * era + 1) >= TAIL


def test_the_emission_takes_about_four_and_a_half_years():
    years = (FIRST_TAIL_BLOCK - 1) / BLOCKS_PER_YEAR
    assert 4.4 < years < 4.5
    bc = default_chain()
    share = lambda blocks: bc.issued_before(blocks + 1) / CAP              # noqa: E731
    assert 0.525 < share(1 * BLOCKS_PER_YEAR) < 0.526          # over half in the first year
    assert 0.788 < share(2 * BLOCKS_PER_YEAR) < 0.789
    assert 0.919 < share(3 * BLOCKS_PER_YEAR) < 0.920
    assert 0.985 < share(4 * BLOCKS_PER_YEAR) < 0.986


def test_twenty_coins_a_block_is_twenty_eight_thousand_eight_hundred_a_day():
    bc = default_chain()
    assert bc.reward_at(1) * (86400 // bc.block_time) == 28_800 * UNIT


def test_the_first_tail_year_adds_about_1_3_percent_to_the_supply():
    per_year = BLOCKS_PER_YEAR * 0.5
    assert per_year == 262_800
    assert 0.013 < per_year / 20_000_000 < 0.0132


def first_tail_height(bc):
    """The height the tail takes over, found by walking the eras (no mining)."""
    h = 1
    while not bc.in_tail(h):
        era_end = ((h - 1) // bc.halving_interval + 1) * bc.halving_interval
        remaining = bc.max_supply - bc.issued_before(h)
        r = bc.main_reward_at(h)
        if 0 < r and remaining <= r * (era_end - h + 1):
            h += -(-remaining // r)
        else:
            h = era_end + 1
    return h


def test_the_config_warnings_are_true():
    # the two schedules the config comment warns about really do end short of the cap
    early = Blockchain(initial_reward=50 * UNIT, halving_interval=200_000, max_supply=CAP,
                       tail_reward=TAIL)
    assert early.issued_before(first_tail_height(early)) == 19_843_740 * UNIT
    round_number = Blockchain(initial_reward=20 * UNIT, halving_interval=500_000, max_supply=CAP,
                              tail_reward=TAIL)
    assert round_number.issued_before(first_tail_height(round_number)) == 19_687_500 * UNIT
    # and a halving interval that is too long never halves at all before the cap
    two_years = Blockchain(initial_reward=20 * UNIT, halving_interval=1_051_200, max_supply=CAP,
                           tail_reward=TAIL)
    assert first_tail_height(two_years) == 1_000_001
    # while the chosen one reaches it exactly
    assert default_chain().issued_before(first_tail_height(default_chain())) == CAP
    assert first_tail_height(default_chain()) == FIRST_TAIL_BLOCK


# ---- 60-second blocks ----

def stable_and_moving(gap):
    bc = Blockchain(target=2**240, difficulty_window=30)        # block_time from config.py
    ts = [0] + [1_000_000 + i * gap for i in range(40)]
    targets = [bc.target] * len(ts)
    return bc.target, bc._retarget(ts, targets, len(ts))


def test_the_difficulty_holds_steady_at_sixty_seconds_a_block():
    start, new = stable_and_moving(60)
    assert new == start


def test_the_difficulty_rises_for_faster_blocks_and_falls_for_slower_ones():
    start, faster = stable_and_moving(30)
    assert faster < start                                       # a smaller target is harder
    assert start // 3 < faster < start * 3 // 4                 # about twice as hard
    start, slower = stable_and_moving(120)
    assert slower > start


def test_a_gap_longer_than_six_blocks_counts_as_the_miner_being_stopped():
    bc = default_chain()
    assert 6 * bc.block_time == 360                             # six minutes, not three


def test_the_matmul_start_difficulty_is_about_a_minute_on_a_mid_range_gpu():
    assert config.MATMUL_START_ATTEMPTS == 1_400_000
    assert chain_module.MATMUL_START_ATTEMPTS == 1_400_000
    assert 55 < config.MATMUL_START_ATTEMPTS / 23_000 < 65      # at about 23 thousand attempts/s


# ---- fees at this scale ----

def test_a_typical_transaction_costs_far_less_than_the_tail_reward():
    assert config.MIN_FEE_RATE == 0.01
    typical = min_fee_for(300)                                  # a plain payment is a few hundred bytes
    assert typical == 30                                        # 0.0030 coins
    assert typical * 100 < TAIL                                 # 100 of them still cost less than the tail


def test_a_block_full_of_minimum_fee_transactions_pays_three_coins():
    # 300 kB at the minimum 0.01 coins per 1000 bytes: fees alone are worth six tail rewards, so
    # a busy chain's miners live mostly on fees, and filling a block costs a spammer 3 coins
    assert config.MIN_BLOCK_MEDIAN == 300_000
    full = min_fee_for(config.MIN_BLOCK_MEDIAN)
    assert full == to_units("3") == 30_000
    assert full / TAIL == 6
    assert full / (20 * UNIT) == 0.15                           # 15% of the starting reward


# ---- reading the schedule ----

def test_the_end_of_the_main_emission_is_found_instantly_and_exactly():
    import time
    bc = default_chain()
    t0 = time.perf_counter()
    assert bc.main_emission_end(1) == FIRST_TAIL_BLOCK
    assert bc.main_emission_end(2_000_000) == FIRST_TAIL_BLOCK
    assert bc.main_emission_end(FIRST_TAIL_BLOCK - 1) == FIRST_TAIL_BLOCK
    assert bc.main_emission_end(FIRST_TAIL_BLOCK) == FIRST_TAIL_BLOCK      # already in the tail
    assert time.perf_counter() - t0 < 0.5                                  # not 2.3 million steps


def test_the_end_of_the_main_emission_matches_a_slow_step_by_step_walk():
    small = Blockchain(initial_reward=10 * UNIT, halving_interval=7, max_supply=100 * UNIT,
                       tail_reward=UNIT // 2)
    walk = 1
    while small.main_reward_at(walk) > 0:
        walk += 1
    assert small.main_emission_end(1) == walk
    for start in (1, 5, 8, walk - 1, walk):
        assert small.main_emission_end(start) == walk
    assert small.main_emission_end(walk + 6) == walk + 6      # already over: this very height


def test_a_trimmed_final_reward_is_handled():
    # 10 * 5 * 2 = 100 is not reached by a schedule of 10, 5, 2 ... so use a cap inside an era
    bc = Blockchain(initial_reward=10 * UNIT, halving_interval=10, max_supply=137 * UNIT + 5000,
                    tail_reward=UNIT // 100)
    end = bc.main_emission_end(1)
    assert bc.main_reward_at(end) == 0 and bc.main_reward_at(end - 1) > 0
    assert bc.issued_before(end) == bc.max_supply


def test_durations_are_shown_in_a_friendly_unit():
    from toycoin.chain import format_duration
    assert format_duration(2_334_400 * 60).startswith("4.4 years")
    assert format_duration(3 * 86400) == "3.0 days"
    assert format_duration(5 * 3600) == "5.0 hours"
    assert format_duration(150) == "2.5 minutes"
    assert format_duration(90) == "90 seconds"
    assert format_duration(0) == "0 seconds"
