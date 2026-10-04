"""Verified flaws in the CURRENT design, kept as executable to-do items (see docs/KNOWN_ISSUES.md).

Each test asserts the CORRECT behaviour and is marked `xfail(strict=True)`: it fails today, which
is what the marker expects. When the flaw is fixed the test starts passing, strict mode turns that
into a failure, and whoever fixed it removes the marker. A rewrite should make all of these pass.

The consensus-critical numbers are written out here, not imported, so this file also documents them.
"""
import hashlib
import json

import pytest
from ecdsa import SECP256k1, VerifyingKey

from tenero.block import Block
from tenero.chain import Blockchain
from tenero.transaction import Transaction
from tenero.units import UNIT
from tenero.wallet import Wallet

from .test_chain import new_chain


def funded_chain(alice):
    bc = Blockchain(target=2**248, initial_reward=1000 * UNIT, halving_interval=10_000,
                    max_supply=10**9 * UNIT, tail_reward=0, difficulty_window=0)
    bc.mine_block(alice.address)
    return bc


@pytest.mark.xfail(strict=True, reason="KNOWN_ISSUES #1: ECDSA signatures are malleable and a "
                                       "transaction's identity is its signature, so a confirmed "
                                       "payment can be replayed once")
def test_a_confirmed_payment_cannot_be_replayed_with_a_flipped_signature():
    alice, bob = Wallet(), Wallet()
    bc = funded_chain(alice)
    tx = Transaction(alice.address, bob.address, 10 * UNIT, fee=UNIT // 10)
    tx.sign(alice)
    bc.add_transaction(tx)
    bc.mine_block(alice.address)
    assert bc.balance_of(bob.address) == 10 * UNIT
    raw = bytes.fromhex(tx.signature)
    n = SECP256k1.order
    flipped = raw[:32] + (n - int.from_bytes(raw[32:], "big")).to_bytes(32, "big")
    twin = Transaction(tx.sender, tx.recipient, tx.amount, fee=tx.fee, public_key=tx.public_key,
                       signature=flipped.hex(), memo=tx.memo)
    try:
        bc.mine_block(alice.address, [twin])
    except ValueError:
        pass
    assert bc.balance_of(bob.address) == 10 * UNIT          # today: 20, bob is paid twice


@pytest.mark.xfail(strict=True, reason="KNOWN_ISSUES #2: transactions are signed over SHA-1 (the "
                                       "python-ecdsa default), not SHA-256")
def test_signatures_use_sha256():
    w = Wallet()
    tx = Transaction(w.address, "0" * 40, UNIT, fee=UNIT // 10)
    tx.sign(w)
    vk = VerifyingKey.from_string(bytes.fromhex(tx.public_key), curve=SECP256k1)
    assert vk.verify(bytes.fromhex(tx.signature), tx.payload(), hashfunc=hashlib.sha256)


@pytest.mark.xfail(strict=True, reason="KNOWN_ISSUES #3: the genesis block is not checked, so a "
                                       "chain with a different genesis is accepted")
def test_a_chain_with_a_different_genesis_is_rejected():
    bc = new_chain()
    bc.chain[0] = Block(0, [], "0" * 64, timestamp=5)        # not the canonical genesis (timestamp 0)
    alice = Wallet()
    bc.mine_block(alice.address)
    bc.mine_block(alice.address)
    assert not bc.is_valid()


@pytest.mark.xfail(strict=True, reason="KNOWN_ISSUES #4: the signed data names no chain, so a "
                                       "signed transaction is valid on any chain with the same history")
def test_a_signed_transaction_is_bound_to_one_chain():
    w = Wallet()
    tx = Transaction(w.address, "0" * 40, UNIT, fee=UNIT // 10)
    tx.sign(w)
    signed_fields = json.loads(tx.payload())
    assert any(k in signed_fields for k in ("chain_id", "genesis_hash", "network"))


@pytest.mark.xfail(strict=True, reason="KNOWN_ISSUES #5: the block hash depends on Python's JSON "
                                       "float formatting (1700000000.0 hashes differently from 1700000000)")
def test_a_timestamp_hashes_the_same_however_it_is_written():
    a = Block(1, [], "ab" * 32, timestamp=1_700_000_000)
    b = Block(1, [], "ab" * 32, timestamp=1_700_000_000.0)
    assert a.compute_hash() == b.compute_hash()
