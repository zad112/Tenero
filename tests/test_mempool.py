import pytest

from tenero.chain import Blockchain, min_fee_for
from tenero.config import MAX_MEMO_BYTES
from tenero.mempool import Mempool
from tenero.transaction import Transaction
from tenero.units import UNIT
from tenero.wallet import Wallet

EASY = 2**248


def c(coins):
    # coins -> whole units (the smallest step is 0.0001 coins)
    return round(coins * UNIT)


def new_chain(reward=c(50)):
    # flat reward and unlimited supply, independent of config.py (reward in units)
    return Blockchain(target=EASY, initial_reward=reward,
                      halving_interval=10_000, max_supply=c(10**9), tail_reward=0,
                      difficulty_window=0)


def signed(sender, recipient_address, amount=c(1), fee=c(1), memo=""):
    tx = Transaction(sender.address, recipient_address, amount, fee=fee, memo=memo)
    tx.sign(sender)
    return tx


def funded_chain(wallet, blocks=1, reward=c(50)):
    bc = new_chain(reward)
    for _ in range(blocks):
        bc.mine_block(wallet.address)
    return bc


def test_wallet_to_miner_flow(tmp_path):
    alice, bob = Wallet(), Wallet()
    bc = funded_chain(alice)  # alice has 50
    mp = Mempool(str(tmp_path / "mempool.json"))

    mp.add(signed(alice, bob.address, c(30), fee=c(1)), bc)
    with pytest.raises(ValueError):  # 31 pending, only 19 left
        mp.add(signed(alice, bob.address, c(30), fee=c(1)), bc)

    txs = mp.usable(bc)
    assert len(txs) == 1
    block = bc.mine_block(bob.address, txs)  # a miner with no wallet access does this
    mp.remove(block.transactions[1:])

    assert mp.load() == []
    assert bc.balance_of(alice.address) == c(19)
    assert bc.balance_of(bob.address) == c(30) + c(50) + c(1)  # payment + reward + fee
    assert bc.is_valid()


def test_confirmed_transaction_cannot_be_requeued(tmp_path):
    alice, bob = Wallet(), Wallet()
    bc = funded_chain(alice)
    mp = Mempool(str(tmp_path / "mempool.json"))

    tx = signed(alice, bob.address, c(10))
    mp.add(tx, bc)
    block = bc.mine_block(bob.address, mp.usable(bc))
    mp.remove(block.transactions[1:])

    with pytest.raises(ValueError):
        mp.add(tx, bc)
    assert mp.usable(bc) == []


def test_remove_keeps_transactions_added_while_mining(tmp_path):
    alice, bob, carol = Wallet(), Wallet(), Wallet()
    bc = funded_chain(alice)
    mp = Mempool(str(tmp_path / "mempool.json"))

    mp.add(signed(alice, bob.address, c(10)), bc)
    taken = mp.usable(bc)
    mp.add(signed(alice, carol.address, c(5)), bc)  # arrives during "mining"
    mp.remove(taken)

    left = mp.load()
    assert len(left) == 1 and left[0].amount == c(5)


def test_fee_below_minimum_rejected(tmp_path):
    alice, bob = Wallet(), Wallet()
    bc = funded_chain(alice)
    mp = Mempool(str(tmp_path / "mempool.json"))
    with pytest.raises(ValueError):
        mp.add(signed(alice, bob.address, c(10), fee=0), bc)


def test_fractional_amounts_go_through_the_mempool(tmp_path):
    alice, bob = Wallet(), Wallet()
    bc = funded_chain(alice)
    mp = Mempool(str(tmp_path / "mempool.json"))
    mp.add(signed(alice, bob.address, 12_345, fee=c(1)), bc)  # 1.2345 coins
    assert mp.load()[0].amount == 12_345
    block = bc.mine_block(bob.address, mp.usable(bc))
    mp.remove(block.transactions[1:])
    assert bc.balance_of(bob.address) == 12_345 + c(50) + c(1)


def test_memo_is_priced_in_the_mempool(tmp_path):
    alice, bob = Wallet(), Wallet()
    bc = funded_chain(alice)
    mp = Mempool(str(tmp_path / "mempool.json"))
    memo = "x" * MAX_MEMO_BYTES
    plain_min = min_fee_for(signed(alice, bob.address, c(1), fee=1).size())
    memo_tx = signed(alice, bob.address, c(1), fee=plain_min, memo=memo)
    memo_min = min_fee_for(memo_tx.size())
    assert memo_min > plain_min                          # a full memo makes the minimum fee higher
    with pytest.raises(ValueError):
        mp.add(memo_tx, bc)                              # the plain fee is not enough with a memo
    mp.add(signed(alice, bob.address, c(1), fee=2 * memo_min, memo=memo), bc)
    assert mp.load()[0].memo == memo


def test_fee_tiers_quiet_network(tmp_path):
    alice, bob = Wallet(), Wallet()
    bc = funded_chain(alice)
    mp = Mempool(str(tmp_path / "mempool.json"))
    tiers = mp.fee_tiers(bc)
    assert tiers["slow"] == tiers["normal"] == tiers["fast"] == 0

    # on a quiet network the quote is exactly the minimum fee: one unit less is refused
    q = mp.quote(tiers["fast"], alice.address, bob.address, c(1))
    assert 0 < q < c(1)
    assert bc.tx_problem(signed(alice, bob.address, c(1), fee=q)) is None
    assert "fee too low" in bc.tx_problem(signed(alice, bob.address, c(1), fee=q - 1))


def test_fee_tiers_busy_network(tmp_path):
    alice, bob = Wallet(), Wallet()
    bc = funded_chain(alice, reward=c(100_000))  # plenty of coins; going over the median never pays
    mp = Mempool(str(tmp_path / "mempool.json"))
    for f in range(1, 21):
        mp.add(signed(alice, bob.address, c(1), fee=c(f)), bc)

    tiers = mp.fee_tiers(bc)
    assert tiers["slow"] == 0
    assert 0 < tiers["normal"] < tiers["fast"]

    # paying the quoted "fast" fee really does make the next block; "slow" does not
    fast_tx = signed(alice, bob.address, c(1),
                     fee=mp.quote(tiers["fast"], alice.address, bob.address, c(1)))
    slow_tx = signed(alice, bob.address, c(1),
                     fee=mp.quote(tiers["slow"], alice.address, bob.address, c(1)))
    mp.add(fast_tx, bc)
    mp.add(slow_tx, bc)
    chosen = {t.signature for t in bc.select_transactions(mp.load())}
    assert fast_tx.signature in chosen
    assert slow_tx.signature not in chosen
