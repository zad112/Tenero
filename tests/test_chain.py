import pytest
from tenero.block import Block
from tenero.chain import Blockchain, BASE_TX_SIZE, min_fee_for
from tenero import chain as chain_module
from tenero.config import MEDIAN_WINDOW, MAX_MEMO_BYTES
from tenero.transaction import Transaction, COINBASE
from tenero.units import UNIT, fmt, to_units
from tenero.wallet import Wallet

EASY = 2**248  # ~256 attempts per block, instant in tests


def c(coins):
    # coins -> whole units (the smallest step is 0.0001 coins)
    return round(coins * UNIT)


REWARD = c(50)  # the flat reward used by new_chain() unless overridden


def new_chain(**overrides):
    # a chain with a flat reward of 50 coins, an effectively unlimited supply and
    # no tail, so tests don't depend on the emission settings in config.py.
    # reward / supply overrides are in units: use c(...)
    params = dict(target=EASY, initial_reward=REWARD,
                  halving_interval=10_000, max_supply=c(10**9), tail_reward=0,
                  difficulty_window=0)  # fixed difficulty: these tests aren't about it
    params.update(overrides)
    return Blockchain(**params)


def signed_tx(sender_wallet, recipient_address, amount=c(1), fee=c(1), memo=""):
    tx = Transaction(sender_wallet.address, recipient_address, amount, fee=fee, memo=memo)
    tx.sign(sender_wallet)
    return tx


def make_chain(miner, blocks=2, **overrides):
    bc = new_chain(**overrides)
    for _ in range(blocks):
        bc.mine_block(miner.address)
    return bc


def manual_block(bc, miner_address, txs, claim=None):
    # build, mine and append a block by hand, bypassing the miner's own rules.
    # by default the reward claim is the correct one (base - penalty + fees).
    height = len(bc.chain)
    if claim is None:
        base = bc.reward_at(height)
        size = sum(t.size() for t in txs)
        claim = base - bc.penalty(base, size, bc.median_size()) + sum(t.fee for t in txs)
    block = Block(height, [Transaction(COINBASE, miner_address, claim)] + txs,
                  bc.chain[-1].hash)
    block.mine(bc.target)
    bc.chain.append(block)
    return block


def test_valid_chain():
    alice = Wallet()
    assert make_chain(alice).is_valid()


def test_mined_block_meets_target():
    alice = Wallet()
    bc = new_chain()
    block = bc.mine_block(alice.address)
    assert block.meets_target(bc.target)
    assert block.hash == block.compute_hash()


def test_unmined_block_fails_validation():
    bc = new_chain(target=2**200)
    reward = Transaction(COINBASE, "x", REWARD)
    bc.chain.append(Block(1, [reward], bc.chain[-1].hash))
    assert not bc.is_valid()


def test_signed_transfer_fee_and_balances():
    alice, bob = Wallet(), Wallet()
    bc = make_chain(alice)
    assert bc.balance_of(alice.address) == 2 * REWARD
    bc.add_transaction(signed_tx(alice, bob.address, c(30), fee=c(2)))
    bc.mine_block(bob.address)
    assert bc.balance_of(alice.address) == 2 * REWARD - c(32)
    assert bc.balance_of(bob.address) == c(30) + REWARD + c(2)  # payment + reward + fee
    assert sum(bc.all_balances().values()) == REWARD * (len(bc.chain) - 1)
    assert bc.is_valid()


def test_overspend_rejected():
    alice, bob = Wallet(), Wallet()
    bc = make_chain(alice)
    with pytest.raises(ValueError):
        bc.add_transaction(signed_tx(alice, bob.address, c(10_000)))


def test_overspend_counts_the_fee():
    alice, bob = Wallet(), Wallet()
    bc = make_chain(alice)  # alice has 100
    with pytest.raises(ValueError):
        bc.add_transaction(signed_tx(alice, bob.address, c(100), fee=c(1)))  # needs 101
    bc.add_transaction(signed_tx(alice, bob.address, c(99), fee=c(1)))       # exactly 100


def test_fee_below_minimum_rejected():
    alice, bob = Wallet(), Wallet()
    bc = make_chain(alice)
    with pytest.raises(ValueError):
        bc.add_transaction(signed_tx(alice, bob.address, c(10), fee=0))


def test_pending_spend_counts_against_balance():
    alice, bob, carol = Wallet(), Wallet(), Wallet()
    bc = make_chain(alice)  # alice has 100
    bc.add_transaction(signed_tx(alice, bob.address, c(80)))
    with pytest.raises(ValueError):
        bc.add_transaction(signed_tx(alice, carol.address, c(80)))


def test_unsigned_transaction_rejected():
    alice, bob = Wallet(), Wallet()
    bc = make_chain(alice)
    tx = Transaction(alice.address, bob.address, c(10), fee=c(1))  # never signed
    with pytest.raises(ValueError):
        bc.add_transaction(tx)


def test_cannot_spend_from_someone_elses_address():
    alice, mallory = Wallet(), Wallet()
    bc = make_chain(alice)
    # mallory claims to be alice but signs with her own key
    tx = Transaction(alice.address, mallory.address, c(50), fee=c(1))
    tx.sign(mallory)
    with pytest.raises(ValueError):
        bc.add_transaction(tx)


def test_tampered_amount_rejected():
    alice, bob = Wallet(), Wallet()
    bc = make_chain(alice)
    tx = signed_tx(alice, bob.address, c(10))
    tx.amount = c(90)  # changed after signing
    with pytest.raises(ValueError):
        bc.add_transaction(tx)


def test_tampered_fee_rejected():
    alice, bob = Wallet(), Wallet()
    bc = make_chain(alice)
    tx = signed_tx(alice, bob.address, c(10), fee=c(1))
    tx.fee = c(50)  # changed after signing
    with pytest.raises(ValueError):
        bc.add_transaction(tx)


def test_tampering_breaks_chain():
    alice, bob = Wallet(), Wallet()
    bc = make_chain(alice)
    bc.add_transaction(signed_tx(alice, bob.address, c(30)))
    bc.mine_block(alice.address)
    bc.chain[3].transactions[1].amount = c(99)
    assert not bc.is_valid()


def test_forged_transaction_in_mined_block_rejected():
    # even if someone mines a block containing a forged tx, validation catches it
    alice, mallory = Wallet(), Wallet()
    bc = make_chain(alice)
    forged = Transaction(alice.address, mallory.address, c(50), fee=c(1))
    forged.sign(mallory)
    reward = Transaction(COINBASE, mallory.address, REWARD + c(1))
    block = Block(3, [reward, forged], bc.chain[-1].hash)
    block.mine(bc.target)
    bc.chain.append(block)
    assert not bc.is_valid()


def test_inflated_reward_rejected():
    alice = Wallet()
    bc = make_chain(alice)
    cheat = Block(3, [Transaction(COINBASE, "mallory", c(1000))], bc.chain[-1].hash)
    cheat.mine(bc.target)
    bc.chain.append(cheat)
    assert not bc.is_valid()


def test_miner_cannot_claim_more_than_the_fees():
    alice, bob, mallory = Wallet(), Wallet(), Wallet()
    bc = make_chain(alice)
    tx = signed_tx(alice, bob.address, c(10), fee=c(1))
    reward = Transaction(COINBASE, mallory.address, REWARD + c(5))  # only 1 fee available
    block = Block(3, [reward, tx], bc.chain[-1].hash)
    block.mine(bc.target)
    bc.chain.append(block)
    assert not bc.is_valid()


def test_replayed_transaction_rejected():
    alice, bob = Wallet(), Wallet()
    bc = make_chain(alice)
    tx = signed_tx(alice, bob.address, c(10), fee=c(1))
    bc.add_transaction(tx)
    bc.mine_block(alice.address)  # block 3 confirms it
    assert bc.is_valid()
    replay = Block(4, [Transaction(COINBASE, alice.address, REWARD + c(1)), tx],
                   bc.chain[-1].hash)
    replay.mine(bc.target)
    bc.chain.append(replay)
    assert not bc.is_valid()


def test_save_and_load(tmp_path):
    alice, bob = Wallet(), Wallet()
    bc = make_chain(alice)
    bc.add_transaction(signed_tx(alice, bob.address, c(25), fee=c(3)))
    bc.mine_block(alice.address)
    path = str(tmp_path / "chain.json")
    bc.save(path)
    loaded = Blockchain.load(path)
    assert loaded.is_valid()
    assert loaded.balance_of(bob.address) == c(25)
    assert loaded.chain[-1].hash == bc.chain[-1].hash


def test_wallet_save_and_load(tmp_path):
    w = Wallet()
    path = str(tmp_path / "w.json")
    w.save(path)
    assert Wallet.load(path).address == w.address


# ---------- decimals ----------

def test_to_units_and_fmt():
    assert to_units("1") == 10_000
    assert to_units("0.0001") == 1
    assert to_units("12.3456") == 123_456
    assert to_units(5) == 50_000
    assert to_units(" 2.5 ") == 25_000
    assert fmt(1) == "0.0001"
    assert fmt(123_456) == "12.3456"
    assert fmt(0) == "0.0000"
    assert fmt(-15_000) == "-1.5000"
    assert fmt(to_units("7.0100")) == "7.0100"


@pytest.mark.parametrize("bad", ["0.00001", "1.23456", "abc", "", "nan", "inf", "1e-5"])
def test_to_units_rejects_bad_input(bad):
    with pytest.raises(ValueError):
        to_units(bad)


def test_fractional_amounts_are_exact():
    alice, bob, carol = Wallet(), Wallet(), Wallet()
    bc = make_chain(alice)
    # ten payments of 0.1 must add up to exactly 1, with no floating point drift
    for _ in range(10):
        bc.add_transaction(signed_tx(alice, bob.address, to_units("0.1"), fee=to_units("0.9")))
    while bc.pending:
        bc.mine_block(carol.address)
    assert bc.balance_of(bob.address) == to_units("1")
    assert bc.balance_of(alice.address) == 2 * REWARD - to_units("10")
    assert bc.is_valid()


def test_smallest_amount_can_be_sent():
    alice, bob = Wallet(), Wallet()
    bc = make_chain(alice)
    bc.add_transaction(signed_tx(alice, bob.address, to_units("0.0001"), fee=c(1)))
    bc.mine_block(bob.address)
    assert bc.balance_of(bob.address) == 1 + REWARD + c(1)
    assert bc.is_valid()


def test_zero_and_negative_amounts_rejected():
    alice, bob = Wallet(), Wallet()
    bc = make_chain(alice)
    for bad in (0, -1):
        with pytest.raises(ValueError):
            bc.add_transaction(signed_tx(alice, bob.address, bad))


def test_fractional_fee_is_priced_in_units():
    alice, bob = Wallet(), Wallet()
    bc = make_chain(alice)
    # find the true minimum fee for a plain send by trying fees upward
    minimum = next(f for f in range(1, c(1))
                   if bc.tx_problem(signed_tx(alice, bob.address, fee=f)) is None)
    assert 0 < minimum < c(1)  # a plain send costs less than one coin now
    assert "fee too low" in bc.tx_problem(signed_tx(alice, bob.address, fee=minimum - 1))
    bc.add_transaction(signed_tx(alice, bob.address, fee=minimum))


def test_chain_from_other_precision_is_refused(tmp_path):
    import json
    alice = Wallet()
    bc = make_chain(alice)
    path = str(tmp_path / "chain.json")
    bc.save(path)
    with open(path) as f:
        d = json.load(f)
    d["params"]["decimals"] = 0  # pretend an older, whole-coin chain
    with open(path, "w") as f:
        json.dump(d, f)
    with pytest.raises(ValueError):
        Blockchain.load(path)


# ---------- halving schedule and supply cap ----------

def test_reward_schedule_halves_and_caps():
    bc = new_chain(initial_reward=c(128), halving_interval=2, max_supply=c(500))
    rewards = [bc.reward_at(h) for h in range(1, 14)]
    assert rewards == [c(x) for x in [128, 128, 64, 64, 32, 32, 16, 16, 8, 8, 4, 0, 0]]
    assert sum(rewards) == c(500)  # the last reward is trimmed so the cap is exact


def test_chain_ends_at_supply_cap():
    alice = Wallet()
    bc = new_chain(initial_reward=c(128), halving_interval=2, max_supply=c(500))
    while bc.reward_at(len(bc.chain)) > 0:
        bc.mine_block(alice.address)
    assert len(bc.chain) - 1 == 11
    assert bc.supply() == c(500)
    assert sum(bc.all_balances().values()) == c(500)
    assert bc.is_valid()


def test_scheduled_reward_cannot_be_exceeded():
    alice = Wallet()
    bc = new_chain(initial_reward=c(8), halving_interval=1)
    bc.mine_block(alice.address)  # reward 8
    bc.mine_block(alice.address)  # reward 4
    assert bc.is_valid()
    # block 3 should pay 2; claiming the old reward of 8 must fail
    cheat = Block(3, [Transaction(COINBASE, alice.address, c(8))], bc.chain[-1].hash)
    cheat.mine(bc.target)
    bc.chain.append(cheat)
    assert not bc.is_valid()


def test_block_index_must_follow_previous():
    alice = Wallet()
    bc = make_chain(alice)  # 2 blocks
    skip = Block(5, [Transaction(COINBASE, alice.address, REWARD)], bc.chain[-1].hash)
    skip.mine(bc.target)
    bc.chain.append(skip)
    assert not bc.is_valid()


def test_fee_only_blocks_after_cap():
    alice, bob = Wallet(), Wallet()
    bc = new_chain(initial_reward=c(4), halving_interval=1, max_supply=c(6))  # no tail
    bc.mine_block(alice.address)  # reward 4
    bc.mine_block(alice.address)  # reward 2 (cap reached: 6 total)
    assert bc.reward_at(len(bc.chain)) == 0
    bc.add_transaction(signed_tx(alice, bob.address, c(1), fee=c(1)))
    block = bc.mine_block(bob.address)
    assert block.transactions[0].amount == c(1)  # no reward left, just the fee
    assert bc.balance_of(bob.address) == c(2)    # payment 1 + fee 1
    assert bc.supply() == c(6)
    assert sum(bc.all_balances().values()) == c(6)
    assert bc.is_valid()


def test_params_saved_with_chain(tmp_path):
    alice = Wallet()
    bc = new_chain(initial_reward=c(64), halving_interval=3, max_supply=c(200),
                   tail_reward=c(2))
    bc.mine_block(alice.address)
    path = str(tmp_path / "chain.json")
    bc.save(path)
    loaded = Blockchain.load(path)
    assert (loaded.initial_reward, loaded.halving_interval,
            loaded.max_supply, loaded.tail_reward) == (c(64), 3, c(200), c(2))
    assert loaded.is_valid()


# ---------- tail emission ----------

def test_tail_emission_continues_forever():
    alice = Wallet()
    bc = new_chain(initial_reward=c(128), halving_interval=2, max_supply=c(500),
                   tail_reward=c(1))
    rewards = [bc.reward_at(h) for h in range(1, 16)]
    assert rewards == [c(x) for x in [128, 128, 64, 64, 32, 32, 16, 16, 8, 8, 4, 1, 1, 1, 1]]
    assert bc.reward_at(1_000_000) == c(1)
    assert not bc.in_tail(11) and bc.in_tail(12)  # main emission ends after block 11

    for _ in range(14):
        bc.mine_block(alice.address)
    assert bc.supply() == c(503)  # 500 from the main emission + 3 tail blocks
    assert sum(bc.all_balances().values()) == c(503)
    assert bc.is_valid()


def test_tail_reward_cannot_be_exceeded():
    alice = Wallet()
    bc = new_chain(initial_reward=c(4), halving_interval=1, max_supply=c(6), tail_reward=c(1))
    for _ in range(3):
        bc.mine_block(alice.address)  # rewards 4, 2, 1
    assert bc.is_valid()
    cheat = Block(4, [Transaction(COINBASE, alice.address, c(2))], bc.chain[-1].hash)
    cheat.mine(bc.target)
    bc.chain.append(cheat)
    assert not bc.is_valid()


def test_tail_blocks_still_collect_fees():
    alice, bob = Wallet(), Wallet()
    bc = new_chain(initial_reward=c(4), halving_interval=1, max_supply=c(6), tail_reward=c(1))
    for _ in range(3):
        bc.mine_block(alice.address)  # 4, 2, then the first tail block
    bc.add_transaction(signed_tx(alice, bob.address, c(1), fee=c(2)))
    block = bc.mine_block(bob.address)
    assert block.transactions[0].amount == c(1) + c(2)  # tail reward + fee
    assert bc.is_valid()


# ---------- memo ----------

def test_memo_is_signed():
    alice, bob = Wallet(), Wallet()
    bc = make_chain(alice)
    tx = signed_tx(alice, bob.address, c(10), fee=c(2), memo="lunch")
    tx.memo = "dinner"  # changed after signing
    with pytest.raises(ValueError):
        bc.add_transaction(tx)


def test_memo_too_long_rejected():
    alice, bob = Wallet(), Wallet()
    bc = make_chain(alice)
    with pytest.raises(ValueError):
        bc.add_transaction(signed_tx(alice, bob.address, c(1), fee=c(10),
                                     memo="x" * (MAX_MEMO_BYTES + 1)))


def test_memo_costs_more_in_fees():
    alice, bob = Wallet(), Wallet()
    bc = make_chain(alice)
    memo = "x" * MAX_MEMO_BYTES
    plain_min = min_fee_for(signed_tx(alice, bob.address).size())
    memo_tx = signed_tx(alice, bob.address, c(1), fee=plain_min, memo=memo)
    memo_min = min_fee_for(memo_tx.size())
    # a full memo makes the transaction bigger, so the minimum fee goes up
    assert memo_min > plain_min
    with pytest.raises(ValueError):
        bc.add_transaction(memo_tx)                      # the plain fee is not enough with a memo
    bc.add_transaction(signed_tx(alice, bob.address, c(1), fee=2 * memo_min, memo=memo))


def test_memo_survives_save_and_load(tmp_path):
    alice, bob = Wallet(), Wallet()
    bc = make_chain(alice)
    bc.add_transaction(signed_tx(alice, bob.address, c(5), fee=c(2), memo="for the pizza"))
    bc.mine_block(alice.address)
    path = str(tmp_path / "chain.json")
    bc.save(path)
    loaded = Blockchain.load(path)
    assert loaded.chain[-1].transactions[1].memo == "for the pizza"
    assert loaded.is_valid()


def test_size_is_the_same_signed_or_not():
    alice, bob = Wallet(), Wallet()
    tx = Transaction(alice.address, bob.address, c(1), fee=c(1))
    before = tx.size()
    tx.sign(alice)
    assert tx.size() == before == BASE_TX_SIZE


# ---------- flexible block size ----------

def test_penalty_formula():
    assert Blockchain.penalty(128, 2200, 2200) == 0       # at the median: free
    assert Blockchain.penalty(100, 1000, 2200) == 0       # under the median: free
    assert Blockchain.penalty(100, 3300, 2200) == 25      # 50% over -> 25% lost
    assert Blockchain.penalty(100, 4400, 2200) == 100     # 2x the median -> everything
    assert Blockchain.penalty(1, 2201, 2200) == 1         # any overage costs a unit
    assert Blockchain.penalty(0, 4400, 2200) == 0         # no reward, nothing to lose


def test_median_floor_and_window():
    n = MEDIAN_WINDOW
    assert Blockchain._median([]) == chain_module.MIN_BLOCK_MEDIAN
    assert Blockchain._median([100] * n) == chain_module.MIN_BLOCK_MEDIAN            # below the floor
    assert Blockchain._median([10**6] * n) == 10**6                     # all big
    # one giant block cannot move the median
    assert Blockchain._median([0] * (n - 1) + [10**6]) == chain_module.MIN_BLOCK_MEDIAN


def test_median_rises_when_most_recent_blocks_are_big():
    alice, bob = Wallet(), Wallet()
    big = [signed_tx(alice, bob.address) for _ in range(8)]  # bigger than the floor
    big_size = sum(t.size() for t in big)
    assert big_size > chain_module.MIN_BLOCK_MEDIAN
    needed = MEDIAN_WINDOW - MEDIAN_WINDOW // 2  # how many big blocks it takes

    def chain_with(big_count):
        bc = new_chain()
        for i in range(MEDIAN_WINDOW):
            txs = big if i < big_count else []
            reward = Transaction(COINBASE, alice.address, 1)
            bc.chain.append(Block(len(bc.chain), [reward] + txs, bc.chain[-1].hash))
        return bc

    assert chain_with(needed - 1).median_size() == chain_module.MIN_BLOCK_MEDIAN
    assert chain_with(needed).median_size() == big_size


def test_capacity_free_zone_and_fee_priority():
    alice, bob = Wallet(), Wallet()
    # a big reward makes going over the median far too expensive to be worth it
    bc = make_chain(alice, blocks=1, initial_reward=c(100_000))
    fees = [3, 8, 1, 6, 2, 7, 4, 5]
    for f in fees:
        bc.add_transaction(signed_tx(alice, bob.address, c(1), fee=c(f)))
    block = bc.mine_block(bob.address)
    included = sorted((t.fee for t in block.transactions[1:]), reverse=True)
    assert included == [c(f) for f in sorted(fees, reverse=True)[:5]]  # the 5 best fit
    assert len(bc.pending) == 3
    assert bc.block_size(block) <= bc.median_size(block.index)
    assert bc.is_valid()


def test_miner_goes_over_median_when_fees_cover_penalty():
    alice, bob = Wallet(), Wallet()
    bc = make_chain(alice, blocks=6)  # alice has 300
    for _ in range(12):
        bc.add_transaction(signed_tx(alice, bob.address, c(1), fee=c(20)))
    block = bc.mine_block(bob.address)
    body = block.transactions[1:]
    median = bc.median_size(block.index)
    assert len(body) > 5                       # went past the free zone
    assert bc.block_size(block) > median
    assert bc.block_size(block) <= 2 * median  # but never past the hard limit
    assert bc.is_valid()


def test_low_fees_do_not_cover_the_penalty():
    alice, bob = Wallet(), Wallet()
    bc = make_chain(alice, blocks=1, initial_reward=c(500))
    for _ in range(5):
        bc.add_transaction(signed_tx(alice, bob.address, c(1), fee=c(20)))
    for _ in range(3):
        bc.add_transaction(signed_tx(alice, bob.address, c(1), fee=c(1)))
    block = bc.mine_block(bob.address)
    body = block.transactions[1:]
    assert len(body) == 5
    assert all(t.fee == c(20) for t in body)  # the cheap ones were not worth the penalty
    assert len(bc.pending) == 3
    assert bc.is_valid()


def test_hard_limit_enforced():
    alice, bob = Wallet(), Wallet()
    limit = 2 * make_chain(alice).median_size()
    size = signed_tx(alice, bob.address).size()
    k = limit // size  # the most transactions that fit under the hard limit
    txs = [signed_tx(alice, bob.address) for _ in range(k + 1)]

    ok = make_chain(alice)
    manual_block(ok, bob.address, txs[:k])
    assert ok.is_valid()

    too_big = make_chain(alice)
    manual_block(too_big, bob.address, txs, claim=sum(t.fee for t in txs))
    assert not too_big.is_valid()


def test_penalty_cannot_be_skipped():
    alice, bob = Wallet(), Wallet()
    txs = [signed_tx(alice, bob.address) for _ in range(6)]  # over the free zone

    cheat = make_chain(alice)
    assert sum(t.size() for t in txs) > cheat.median_size()
    base = cheat.reward_at(3)
    manual_block(cheat, bob.address, txs, claim=base + sum(t.fee for t in txs))  # no penalty
    assert not cheat.is_valid()

    honest = make_chain(alice)
    manual_block(honest, bob.address, txs)  # claims base - penalty + fees
    assert honest.is_valid()
