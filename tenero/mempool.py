import os
from fractions import Fraction

from . import paths
from .chain import BASE_TX_SIZE, min_fee_for
from .storage import atomic_write_json, read_json
from .transaction import Transaction

DEFAULT_MEMPOOL = paths.MEMPOOL_PATH


class Mempool:
    def __init__(self, path=DEFAULT_MEMPOOL):
        self.path = path

    def load(self):
        if not os.path.exists(self.path):
            return []
        return [Transaction.from_dict(t) for t in read_json(self.path)]

    def _save(self, txs):
        atomic_write_json(self.path, [t.to_dict() for t in txs])

    def outgoing(self, address):
        return sum(t.amount + t.fee for t in self.load() if t.sender == address)

    def add(self, tx, chain):
        problem = chain.tx_problem(tx)
        if problem:
            raise ValueError(problem)
        if tx.signature in chain.confirmed_signatures():
            raise ValueError("transaction is already confirmed")
        txs = self.load()
        if any(t.signature == tx.signature for t in txs):
            raise ValueError("transaction is already pending")
        outgoing = sum(t.amount + t.fee for t in txs if t.sender == tx.sender)
        if chain.balance_of(tx.sender) - outgoing < tx.amount + tx.fee:
            raise ValueError("insufficient funds (amount + fee)")
        txs.append(tx)
        self._save(txs)
        return tx

    def usable(self, chain):
        # what the next block would contain
        return chain.select_transactions(self.load())

    # ---------- fee estimates (rates are units per byte) ----------
    def rate_needed(self, chain, blocks=1):
        # the fee per byte a new transaction must beat to land within `blocks` blocks
        capacity = chain.median_size() * blocks
        chosen, full = chain.plan_capacity(self.load(), capacity)
        if not full or not chosen:
            return Fraction(0)
        return min(Fraction(t.fee, t.size()) for t in chosen)

    def fee_tiers(self, chain):
        # live speed tiers as fee rates to beat; recalculated from what is
        # waiting right now
        fast = self.rate_needed(chain, 1)                 # next block
        normal = min(fast, self.rate_needed(chain, 3))    # within ~3 blocks
        return {"slow": Fraction(0), "normal": normal, "fast": fast}

    def quote(self, rate, sender, recipient, amount, memo=""):
        # the smallest fee (in units) that beats `rate` for this exact transaction.
        # The fee's digits are part of the size, so this settles on a value that holds.
        fee = 1
        while True:
            size = Transaction(sender, recipient, amount, fee=fee, memo=memo).size()
            need = max(min_fee_for(size), (rate.numerator * size) // rate.denominator + 1)
            if need <= fee:
                return fee
            fee = need

    def remove(self, included):
        # remove only what was mined; everything else stays queued
        done = {t.signature for t in included}
        self._save([t for t in self.load() if t.signature not in done])
