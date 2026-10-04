import os
import time
from fractions import Fraction

from .block import Block
from .config import (
    INITIAL_REWARD, HALVING_INTERVAL, MAX_SUPPLY, TAIL_REWARD,
    MIN_FEE_RATE, MAX_MEMO_BYTES, MIN_BLOCK_MEDIAN, MEDIAN_WINDOW, DEFAULT_TARGET,
    TARGET_BLOCK_TIME, DIFFICULTY_WINDOW, MAX_TARGET_STEP,
    FUTURE_TIME_LIMIT, MATMUL_START_ATTEMPTS,
)
from . import paths
from .pow import Sha256Pow, default_pow, make_pow
from .storage import atomic_write_json, read_json
from .transaction import Transaction, COINBASE
from .units import DECIMALS, UNIT, to_units

# chain.json lives in the project folder (or $TENERO_DATA), wherever you launch from
DEFAULT_PATH = paths.CHAIN_PATH

# everything below the config layer works in whole units (0.0001 coins)
_INITIAL_REWARD = to_units(INITIAL_REWARD)
_MAX_SUPPLY = to_units(MAX_SUPPLY)
_TAIL_REWARD = to_units(TAIL_REWARD)
MIN_FEE_RATE_UNITS = to_units(MIN_FEE_RATE)  # units per 1000 bytes

# size in bytes of a typical plain send (1 coin, 1 coin fee, no memo)
BASE_TX_SIZE = Transaction("0" * 40, "0" * 40, UNIT, fee=UNIT).size()


def format_tops(ops_per_second):
    # int8 operations per second, in trillions (comparable to a GPU's TOPS rating)
    return f"{ops_per_second / 1e12:.1f} TOPS"


def format_hashrate(h):
    # hashes per second, in friendly units
    for name, size in (("GH/s", 1e9), ("MH/s", 1e6), ("kH/s", 1e3)):
        if h >= size:
            return f"{h / size:.2f} {name}"
    return f"{h:.0f} H/s"


def format_duration(seconds):
    # a length of time in the friendliest single unit ("4.4 years", "12 days", "3 hours")
    seconds = float(seconds)
    for name, size in (("years", 365 * 86400), ("days", 86400), ("hours", 3600), ("minutes", 60)):
        if seconds >= 2 * size:
            return f"{seconds / size:.1f} {name}"
    return f"{seconds:.0f} seconds"


def min_fee_for(size):
    # minimum fee, in units, for a transaction of this many bytes
    # (rounded up, and never below one unit)
    return max(1, (MIN_FEE_RATE_UNITS * size + 999) // 1000)


class Blockchain:
    # initial_reward, max_supply and tail_reward are in units (0.0001 coins)
    def __init__(self, target=2**240, initial_reward=_INITIAL_REWARD,
                 halving_interval=HALVING_INTERVAL, max_supply=_MAX_SUPPLY,
                 tail_reward=_TAIL_REWARD, block_time=TARGET_BLOCK_TIME,
                 difficulty_window=DIFFICULTY_WINDOW, pow=None):
        if halving_interval < 1:
            raise ValueError("halving_interval must be at least 1")
        if tail_reward < 0:
            raise ValueError("tail_reward cannot be negative")
        if block_time < 1 or difficulty_window < 0:
            raise ValueError("block_time must be at least 1 and difficulty_window at least 0")
        self.pow = pow or Sha256Pow()   # how blocks are hashed and mined
        self.target = target  # the STARTING target; later ones come from the adjustment
        self.block_time = block_time
        self.difficulty_window = difficulty_window  # 0 = fixed difficulty
        self.initial_reward = initial_reward
        self.halving_interval = halving_interval
        self.max_supply = max_supply
        self.tail_reward = tail_reward
        self.chain = [Block(0, [], "0" * 64, timestamp=0)]
        self.pending = []  # in-memory only (used by tests)

    # ---------- coin supply ----------
    def scheduled_reward(self, height):
        # the main-emission reward before the cap is applied: halves every interval
        era = (height - 1) // self.halving_interval
        return self.initial_reward >> era

    def issued_before(self, height):
        # main-emission coins scheduled for blocks 1 .. height-1
        total = 0
        h = 1
        while h < height:
            r = self.scheduled_reward(h)
            if r == 0:
                break
            era_last = ((h - 1) // self.halving_interval + 1) * self.halving_interval
            last = min(era_last, height - 1)
            total += r * (last - h + 1)
            h = last + 1
        return min(total, self.max_supply)

    def main_reward_at(self, height):
        # main emission only: halving schedule, trimmed so MAX_SUPPLY is never passed
        remaining = self.max_supply - self.issued_before(height)
        return max(0, min(self.scheduled_reward(height), remaining))

    def main_emission_end(self, height):
        # the first block height at which the main emission pays nothing. Walks the eras, so it is
        # instant even when the end is millions of blocks away
        h = height
        while True:
            r = self.main_reward_at(h)
            if r == 0:
                return h
            era_end = ((h - 1) // self.halving_interval + 1) * self.halving_interval
            remaining = self.max_supply - self.issued_before(h)
            if remaining <= r * (era_end - h + 1):
                return h + -(-remaining // r)     # blocks needed to use up the rest of the cap
            h = era_end + 1

    def reward_at(self, height):
        # the base reward for this height (before any oversize penalty)
        return max(self.main_reward_at(height), self.tail_reward)

    def in_tail(self, height):
        # True once the main emission no longer pays more than the tail
        return self.tail_reward > 0 and self.main_reward_at(height) < self.tail_reward

    def supply(self):
        # coins actually created so far: what each block paid, minus the fees in it
        total = 0
        for b in self.chain[1:]:
            total += b.transactions[0].amount - sum(t.fee for t in b.transactions[1:])
        return total

    # ---------- difficulty ----------
    # A block is valid if its hash is BELOW the target: a smaller target means
    # more work. Expected attempts per block = 2**256 / target.
    #
    # The target each block must meet is worked out from the chain before it, never
    # read from the block itself, so a miner cannot pick an easier one.
    def _retarget(self, ts, targets, pos):
        # ts[i] / targets[i]: timestamp and required target of block i (i < pos).
        # Returns the target the block at position `pos` must meet.
        if self.difficulty_window == 0:
            return self.target
        # blocks 2 .. pos-1 have a measurable solve time (block 1 follows the
        # genesis block, which has no real timestamp)
        n = min(self.difficulty_window, pos - 2)
        if n < 1:
            return self.target
        T = self.block_time
        weighted = 0
        for k in range(1, n + 1):  # k = 1 is the oldest block in the window, n the newest
            i = pos - n - 1 + k
            solve = max(1, min(6 * T, ts[i] - ts[i - 1]))
            weighted += solve * k
        avg_target = sum(targets[pos - n:pos]) // n
        new = avg_target * weighted // (T * n * (n + 1) // 2)
        prev = targets[pos - 1]
        new = max(prev // MAX_TARGET_STEP, min(prev * MAX_TARGET_STEP, new))
        return max(1, min(2**256 - 1, new))

    @staticmethod
    def _earliest_time(ts, pos):
        # the earliest timestamp the block at position `pos` may carry: one second after its parent's
        # (the genesis block counts as time 0, so block 1 may carry any timestamp from 1 on)
        return ts[pos - 1] + 1

    def _history(self):
        # (timestamps, required targets) for every block on the chain
        ts, targets = [0], [self.target]
        for pos in range(1, len(self.chain)):
            targets.append(self._retarget(ts, targets, pos))
            ts.append(int(self.chain[pos].timestamp))
        return ts, targets

    def next_target(self):
        # the target the next block must meet
        ts, targets = self._history()
        return self._retarget(ts, targets, len(self.chain))

    def min_timestamp(self):
        # the earliest timestamp the next block may carry
        ts, _ = self._history()
        return self._earliest_time(ts, len(self.chain))

    def recent_stats(self):
        # (average block time in seconds, estimated network attempts/second) over the recent
        # window, or None until there are enough blocks.
        # A gap longer than 6x the target block time is treated as a period when nobody was
        # mining (the miner was stopped), not as a slow block: it is left out of both the time
        # and the work, so restarting the miner does not drag the estimate down. (The difficulty
        # adjustment limits the same intervals to 6x for the same reason.)
        n = min(self.difficulty_window or 10, len(self.chain) - 2)
        if n < 2:
            return None
        ts, targets = self._history()
        last = len(self.chain) - 1
        gap_limit = 6 * self.block_time
        span = hashes = used = 0
        for i in range(last - n + 1, last + 1):
            interval = ts[i] - ts[i - 1]
            if interval > gap_limit:
                continue
            span += max(interval, 1)
            hashes += 2**256 // targets[i]
            used += 1
        if used < 2:
            return None
        return span / used, hashes / span

    # ---------- block size ----------
    @staticmethod
    def block_size(block):
        # bytes of transactions in a block (the reward transaction is not counted)
        return sum(t.size() for t in block.transactions[1:])

    @staticmethod
    def _median(sizes):
        s = sorted(sizes)
        return max(MIN_BLOCK_MEDIAN, s[len(s) // 2] if s else 0)

    def median_size(self, pos=None):
        # the median block size the block at position `pos` is judged against
        # (default: the next block to be mined)
        pos = len(self.chain) if pos is None else pos
        window = self.chain[max(1, pos - MEDIAN_WINDOW):pos]
        return self._median([self.block_size(b) for b in window])

    @staticmethod
    def penalty(base, size, median):
        # reward lost for a block over the median: base * (overage / median)^2,
        # rounded up so any overage costs at least one unit
        if size <= median or base <= 0:
            return 0
        over = size - median
        return (base * over * over + median * median - 1) // (median * median)

    # ---------- balances ----------
    def balance_of(self, address):
        balance = 0
        for block in self.chain:
            for tx in block.transactions:
                if tx.recipient == address:
                    balance += tx.amount
                if tx.sender == address:
                    balance -= tx.amount + tx.fee
        return balance

    def all_balances(self):
        balances = {}
        for block in self.chain:
            for tx in block.transactions:
                if tx.sender != COINBASE:
                    balances[tx.sender] = balances.get(tx.sender, 0) - (tx.amount + tx.fee)
                balances[tx.recipient] = balances.get(tx.recipient, 0) + tx.amount
        return balances

    def confirmed_signatures(self):
        return {tx.signature for b in self.chain for tx in b.transactions if tx.signature}

    # ---------- transaction checks ----------
    def tx_problem(self, tx):
        # returns a message if the transaction is not acceptable on its own, else None
        if tx.sender == COINBASE:
            return "cannot send from COINBASE"
        if not isinstance(tx.amount, int) or tx.amount <= 0:
            return "amount must be positive (smallest step is 0.0001)"
        if not isinstance(tx.fee, int):
            return "fee must be a whole number of units"
        if not isinstance(tx.memo, str):
            return "memo must be text"
        if len(tx.memo.encode()) > MAX_MEMO_BYTES:
            return f"memo too long (max {MAX_MEMO_BYTES} bytes)"
        size = tx.size()
        need = min_fee_for(size)
        if tx.fee < need:
            from .units import fmt
            return (f"fee too low: this {size}-byte transaction needs at least "
                    f"{fmt(need)}")
        if not tx.is_signature_valid():
            return "invalid signature"
        return None

    # ---------- transactions and mining ----------
    def add_transaction(self, tx):
        problem = self.tx_problem(tx)
        if problem:
            raise ValueError(problem)
        already_pending = sum(t.amount + t.fee for t in self.pending if t.sender == tx.sender)
        if self.balance_of(tx.sender) - already_pending < tx.amount + tx.fee:
            raise ValueError("insufficient funds")
        self.pending.append(tx)
        return tx

    @staticmethod
    def _by_rate(candidates):
        # highest fee per byte first; ties keep arrival order
        good = [t for t in candidates
                if t.sender != COINBASE
                and isinstance(t.amount, int) and isinstance(t.fee, int)]
        return sorted(good, key=lambda t: -Fraction(t.fee, t.size()))

    def _select(self, candidates, accept):
        # walks candidates best-rate-first, keeping only valid, unconfirmed,
        # affordable ones that `accept` also approves
        confirmed = self.confirmed_signatures()
        balances, seen, chosen = {}, set(), []
        for tx in self._by_rate(candidates):
            if tx.signature in confirmed or tx.signature in seen:
                continue
            if self.tx_problem(tx) is not None:
                continue
            bal = balances.get(tx.sender)
            if bal is None:
                bal = self.balance_of(tx.sender)
            cost = tx.amount + tx.fee
            if bal < cost:
                continue
            if not accept(tx, tx.size(), chosen):
                continue
            balances[tx.sender] = bal - cost
            seen.add(tx.signature)
            chosen.append(tx)
        return chosen

    def select_transactions(self, candidates):
        # what a miner includes. Up to the median is free; beyond it a transaction
        # is only worth including if its fee is bigger than the extra penalty it
        # causes. Never beyond the hard limit (2x the median).
        base = self.reward_at(len(self.chain))
        median = self.median_size()
        limit = 2 * median

        def accept(tx, s, chosen):
            size = sum(t.size() for t in chosen)
            fees = sum(t.fee for t in chosen)
            if size + s > limit:
                return False
            before = base - self.penalty(base, size, median) + fees
            after = base - self.penalty(base, size + s, median) + fees + tx.fee
            return after > before

        return self._select(candidates, accept)

    def plan_capacity(self, candidates, capacity):
        # used for fee estimates: fill `capacity` bytes best-rate-first.
        # returns (chosen, full) where full means new arrivals would be squeezed out
        state = {"full": False}

        def accept(tx, s, chosen):
            if sum(t.size() for t in chosen) + s > capacity:
                state["full"] = True
                return False
            return True

        chosen = self._select(candidates, accept)
        if capacity - sum(t.size() for t in chosen) < BASE_TX_SIZE:
            state["full"] = True
        return chosen, state["full"]

    def mine_block(self, miner_address, transactions=None, timestamp=None, verify=None,
                   on_chunk=None):
        # transactions=None uses the in-memory pending list; the miner program
        # passes in candidates taken from the mempool instead.
        # timestamp=None stamps the block with the time it is found; passing one
        # fixes it (used to simulate block times in tests).
        # verify=False (matmul chains) returns the block WITHOUT the CPU double-check: the
        # caller must run self.pow.check(block) and discard the block if it fails.
        # on_chunk is called between search chunks and may raise to abort the search.
        candidates = self.pending if transactions is None else transactions
        txs = self.select_transactions(candidates)
        fees = sum(t.fee for t in txs)
        prev = self.chain[-1]
        height = prev.index + 1
        base = self.reward_at(height)
        size = sum(t.size() for t in txs)
        paid = base - self.penalty(base, size, self.median_size())
        reward = Transaction(COINBASE, miner_address, paid + fees)
        block = Block(height, [reward] + txs, prev.hash, timestamp=timestamp)
        target = self.next_target()
        refresh = timestamp is None and self.difficulty_window > 0
        self.pow.mine(block, target, refresh_timestamp=refresh,
                      min_timestamp=self.min_timestamp() if refresh else 0,
                      verify=verify, on_chunk=on_chunk)
        self.chain.append(block)
        included = {t.signature for t in txs}
        self.pending = [t for t in self.pending if t.signature not in included]
        return block

    # ---------- validation ----------
    def is_valid(self, check_pow=True, progress=None):
        # check_pow=False skips re-hashing blocks whose proof of work is expensive to check
        # (matmul): links, targets, rewards and transactions are still checked, but a
        # tampered block would go unnoticed. progress(done, total) is called per block.
        balances = {}
        seen = set()  # signatures already used; blocks replayed transactions
        sizes = [0]   # sizes[i] = transaction bytes in chain[i] (genesis = 0)
        ts, targets = [0], [self.target]  # for the difficulty adjustment
        now = time.time()
        for pos in range(1, len(self.chain)):
            prev, cur = self.chain[pos - 1], self.chain[pos]
            # ---- cheap checks first: links, timestamps, then the block's own hash ----
            if cur.index != prev.index + 1:
                return False
            if cur.previous_hash != prev.hash:
                return False
            required = self._retarget(ts, targets, pos)
            if self.difficulty_window > 0:
                # timestamps drive the difficulty, so they are policed: later than
                # the parent's, and not from the future
                stamp = int(cur.timestamp)
                if stamp < self._earliest_time(ts, pos):
                    return False
                if stamp > now + FUTURE_TIME_LIMIT:
                    return False
            # the hash matches the block's contents, nonce and mix, and meets the target
            # (microseconds; enough to catch any tampering, but a forger who grinds a made-up
            # mix to the target passes it)
            if not self.pow.precheck(cur, required):
                return False
            # ---- then the expensive recomputation ----
            if check_pow and not self.pow.cheap:
                if cur.hash != self.pow.hash_of(cur):   # None (an unusable nonce) never matches
                    return False
            if progress:
                progress(pos, len(self.chain) - 1)
            targets.append(required)
            ts.append(int(cur.timestamp))
            if not cur.transactions:
                return False

            first, body = cur.transactions[0], cur.transactions[1:]
            if first.sender != COINBASE:
                return False
            if any(not isinstance(t.amount, int) or not isinstance(t.fee, int)
                   for t in body):
                return False

            # block size rules: hard limit, and the reward shrinks above the median
            body_size = sum(t.size() for t in body)
            median = self._median(sizes[max(1, pos - MEDIAN_WINDOW):pos])
            if body_size > 2 * median:
                return False
            base = self.reward_at(cur.index)
            expected = (base - self.penalty(base, body_size, median)
                        + sum(t.fee for t in body))
            if first.amount != expected:
                return False
            sizes.append(body_size)
            balances[first.recipient] = balances.get(first.recipient, 0) + first.amount

            for tx in body:
                if self.tx_problem(tx) is not None:
                    return False
                if tx.signature in seen:
                    return False
                seen.add(tx.signature)
                cost = tx.amount + tx.fee
                if balances.get(tx.sender, 0) < cost:
                    return False
                balances[tx.sender] -= cost
                balances[tx.recipient] = balances.get(tx.recipient, 0) + tx.amount
        return True

    def precheck_block(self, block):
        """The CHEAP checks for a block that would extend this chain: (ok, reason). No dataset,
        no signatures, no recomputation: microseconds. A block that passes is not yet known to be
        valid; it has earned the expensive checks, which a fake block should not get for free."""
        tip = self.chain[-1]
        if block.index != tip.index + 1:
            return False, "wrong block number"
        if block.previous_hash != tip.hash:
            return False, "does not follow the current tip"
        if not block.transactions:
            return False, "no reward transaction"
        if self.difficulty_window > 0:
            stamp = int(block.timestamp)
            if stamp < self.min_timestamp():
                return False, "timestamp not later than its parent's"
            if stamp > time.time() + FUTURE_TIME_LIMIT:
                return False, "timestamp too far in the future"
        if not self.pow.precheck(block, self.next_target()):
            return False, "hash does not match the block's contents or does not meet the target"
        return True, ""

    # ---------- save / load / display ----------
    def save(self, path=DEFAULT_PATH, *, upto=None):
        # upto: save only the first `upto` blocks (genesis included). The miner uses it to
        # write only blocks whose CPU check has passed while a newer block is still unchecked.
        blocks = self.chain if upto is None else self.chain[:upto]
        atomic_write_json(path, {
            "target": str(self.target),
            "params": {
                "pow": self.pow.to_dict(),
                "decimals": DECIMALS,
                "block_time": self.block_time,
                "difficulty_window": self.difficulty_window,
                "initial_reward": self.initial_reward,
                "halving_interval": self.halving_interval,
                "max_supply": self.max_supply,
                "tail_reward": self.tail_reward,
            },
            "chain": [b.to_dict() for b in blocks],
        })
        return path

    @classmethod
    def load(cls, path=DEFAULT_PATH, searcher=None):
        d = read_json(path)
        p = d.get("params", {})
        if p.get("decimals") != DECIMALS:
            raise ValueError(
                f"{os.path.basename(path)} was made with different coin precision "
                f"(this version uses {DECIMALS} decimals). Delete chain.json and "
                f"mempool.json to start a fresh chain.")
        bc = cls(
            target=int(d["target"]),
            initial_reward=p["initial_reward"],
            halving_interval=p["halving_interval"],
            max_supply=p["max_supply"],
            tail_reward=p.get("tail_reward", 0),
            block_time=p.get("block_time", TARGET_BLOCK_TIME),
            # chains made before the adjustment existed keep their fixed difficulty
            difficulty_window=p.get("difficulty_window", 0),
            pow=make_pow(p.get("pow")),   # chains saved before this existed used SHA-256
        )
        bc.chain = [Block.from_dict(b) for b in d["chain"]]
        if searcher is not None and hasattr(bc.pow, "searcher"):
            bc.pow.searcher = searcher
        return bc

    @classmethod
    def load_or_new(cls, path=DEFAULT_PATH, searcher=None, algorithm=None, *, epoch_blocks=None):
        # an existing chain keeps its own proof of work; `algorithm` and `epoch_blocks`
        # only matter for a new one
        if os.path.exists(path):
            return cls.load(path, searcher=searcher)
        work = default_pow(algorithm, epoch_blocks)
        target = DEFAULT_TARGET if work.name == "sha256" else 2**256 // MATMUL_START_ATTEMPTS
        bc = cls(target=target, pow=work)
        if searcher is not None and hasattr(work, "searcher"):
            work.searcher = searcher
        return bc

    def print_chain(self):
        for b in self.chain:
            print(f"#{b.index}  nonce={b.nonce:,}")
            print(f"    hash: {b.hash}")
            print(f"    prev: {b.previous_hash}")
            for tx in b.transactions:
                print(f"    tx:   {tx}")
