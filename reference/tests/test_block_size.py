"""The real block-size floor from config.py: 300 kB, about 700 plain transactions a block.

Everything else in the suite tests the size rules with a small floor (see conftest.py). These run
against the real number, including blocks of hundreds of genuinely signed transactions, to show
the settings in config.py behave as intended: a block up to 300 kB pays the full reward, the hard
limit is 600 kB, the penalty above the floor is quadratic, and the chain still validates a busy
block.
"""
import pytest

from tenero import chain as chain_module
from tenero import config
from tenero.chain import Blockchain, min_fee_for
from tenero.units import UNIT
from tenero.wallet import Wallet

from .test_chain import c, make_chain, manual_block, signed_tx

pytestmark = pytest.mark.real_floor

FLOOR = 300_000
LIMIT = 600_000


@pytest.fixture(scope="module")
def parties():
    return Wallet(), Wallet()


@pytest.fixture(scope="module")
def many_txs(parties):
    """1,450 distinct signed transactions (more than fit under the hard limit), signed once and
    shared by the tests below. Different amounts make each one unique, so none is a replay."""
    alice, bob = parties
    return [signed_tx(alice, bob.address, c(0.01) + i, fee=c(0.005)) for i in range(1450)]


@pytest.fixture(scope="module")
def tx_size(many_txs):
    sizes = {t.size() for t in many_txs}
    assert max(sizes) - min(sizes) <= 2             # they differ only by a digit or two
    return max(sizes)


def fitting(txs, limit):
    """How many of these transactions, taken in order, fit in `limit` bytes."""
    total = 0
    for n, tx in enumerate(txs):
        total += tx.size()
        if total > limit:
            return n
    return len(txs)


def rich_chain(miner, **kw):
    # a flat 100,000-coin reward: alice can pay for every transaction below, and the penalty for
    # going over the floor is far too large for any fee to cover
    return make_chain(miner, blocks=1, initial_reward=c(100_000), **kw)


# ---- the numbers ----

def test_the_floor_is_300_kb_and_the_hard_limit_600_kb():
    assert config.MIN_BLOCK_MEDIAN == chain_module.MIN_BLOCK_MEDIAN == FLOOR
    bc = Blockchain()
    assert bc.median_size() == FLOOR                        # a new chain starts at the floor
    assert Blockchain._median([]) == FLOOR
    assert Blockchain._median([100] * config.MEDIAN_WINDOW) == FLOOR      # small blocks: still the floor
    assert Blockchain._median([10**6] * config.MEDIAN_WINDOW) == 10**6    # sustained big blocks raise it
    n = config.MEDIAN_WINDOW
    assert Blockchain._median([0] * (n - 1) + [10**7]) == FLOOR           # one giant block cannot move it


def test_about_seven_hundred_plain_transactions_fit_for_free(many_txs, tx_size):
    assert 420 <= tx_size <= 440                            # a plain payment is about 428 bytes
    free = fitting(many_txs, FLOOR)
    limit = fitting(many_txs, LIMIT)
    assert 695 <= free <= 705                               # about 700 a block
    assert 1390 <= limit <= 1410                            # and about twice that at the hard limit
    per_second = free / config.TARGET_BLOCK_TIME
    assert 11 < per_second < 12                             # about 11.7 transactions a second


def test_the_penalty_above_the_floor_is_quadratic_and_total_at_the_hard_limit():
    base = 20 * UNIT
    assert Blockchain.penalty(base, FLOOR, FLOOR) == 0                    # at the floor: free
    assert Blockchain.penalty(base, FLOOR - 1000, FLOOR) == 0
    assert Blockchain.penalty(base, 450_000, FLOOR) == 5 * UNIT           # 50% over: a quarter lost
    assert Blockchain.penalty(base, LIMIT, FLOOR) == base                 # 2x: the whole reward
    assert Blockchain.penalty(base, FLOOR + 1, FLOOR) == 1                # any overage costs a unit


# ---- a busy block, end to end ----

def test_a_block_of_almost_700_transactions_is_mined_paid_in_full_and_valid(many_txs, parties):
    alice, bob = parties
    k = fitting(many_txs, FLOOR) - 5                        # comfortably inside the free zone
    bc = rich_chain(alice)
    txs = many_txs[:k]
    for tx in txs:
        assert bc.add_transaction(tx)
    height = len(bc.chain)
    block = bc.mine_block(bob.address)
    assert len(block.transactions) == k + 1                 # every one of them fit
    size = bc.block_size(block)
    assert 250_000 < size <= FLOOR                          # a genuinely big block, not over the floor
    base = bc.reward_at(height)
    assert block.transactions[0].amount == base + sum(t.fee for t in txs)   # no penalty at all
    assert bc.pending == []
    assert bc.is_valid()
    assert bc.balance_of(bob.address) == sum(t.amount for t in txs) + block.transactions[0].amount


def test_the_miner_stops_at_the_floor_when_fees_do_not_cover_the_penalty(many_txs, parties, tx_size):
    alice, bob = parties
    offered = many_txs[:fitting(many_txs, FLOOR) + 20]      # 20 more than fit in the free zone
    bc = rich_chain(alice)
    for tx in offered:
        assert bc.add_transaction(tx)
    block = bc.mine_block(bob.address)
    included = len(block.transactions) - 1
    size = bc.block_size(block)
    assert size <= FLOOR                                    # never over the floor ...
    assert FLOOR - size < tx_size                           # ... and full: not one more fits
    assert len(bc.pending) == len(offered) - included == 20  # the rest wait for the next block
    assert bc.is_valid()


def test_the_hard_limit_is_600_kb(many_txs, parties):
    alice, bob = parties
    k = fitting(many_txs, LIMIT)                            # the most that fit under the limit
    assert k + 1 < len(many_txs)
    assert sum(t.size() for t in many_txs[:k]) <= LIMIT < sum(t.size() for t in many_txs[:k + 1])
    ok = rich_chain(alice)
    manual_block(ok, bob.address, many_txs[:k])             # claims base - penalty + fees, correctly
    assert ok.is_valid()

    too_big = rich_chain(alice)
    manual_block(too_big, bob.address, many_txs[:k + 1],
                 claim=sum(t.fee for t in many_txs[:k + 1]))
    assert not too_big.is_valid()                           # one transaction over the limit


def test_a_block_over_the_floor_must_pay_the_penalty(many_txs, parties):
    alice, bob = parties
    k = fitting(many_txs, FLOOR) + 50                       # a little over 300 kB
    txs = many_txs[:k]
    assert sum(t.size() for t in txs) > FLOOR

    cheat = rich_chain(alice)
    base = cheat.reward_at(len(cheat.chain))
    manual_block(cheat, bob.address, txs, claim=base + sum(t.fee for t in txs))     # skips the penalty
    assert not cheat.is_valid()

    honest = rich_chain(alice)
    manual_block(honest, bob.address, txs)                  # claims base - penalty + fees
    assert honest.is_valid()
    size = sum(t.size() for t in txs)
    assert honest.penalty(base, size, FLOOR) > 0


def test_the_median_rises_only_after_many_big_blocks(parties):
    # after the floor, capacity follows demand: half the last 10 blocks must be bigger than it
    n = config.MEDIAN_WINDOW
    assert Blockchain._median([400_000] * (n // 2 - 1) + [0] * (n // 2 + 1)) == FLOOR
    assert Blockchain._median([400_000] * (n // 2 + 1) + [0] * (n // 2 - 1)) == 400_000
    assert 2 * Blockchain._median([400_000] * n) == 800_000          # and the hard limit follows it


def test_fees_at_this_size(tx_size):
    # the minimum fee is per byte, so a typical payment costs little and a full block a few coins
    assert min_fee_for(tx_size) < c(0.005)                  # under half a cent of a coin
    assert min_fee_for(FLOOR) == 3 * UNIT
