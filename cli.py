import os
import re
import shlex
import sys
import time
from fractions import Fraction

from toycoin import paths
from toycoin.chain import (Blockchain, BASE_TX_SIZE, format_duration, format_hashrate,
                           format_tops)
from toycoin.config import MIN_FEE_RATE, MAX_MEMO_BYTES
from toycoin.mempool import Mempool
from toycoin.transaction import Transaction, COINBASE
from toycoin.units import DECIMALS, UNIT, fmt, to_units
from toycoin.wallet import Wallet

WALLET_DIR = paths.WALLET_DIR

TIER_NOTES = {
    "slow": "cheapest, may wait a long time",
    "normal": "should confirm within about 3 blocks",
    "fast": "should make the next block",
}

HELP = f"""
amounts can have up to {DECIMALS} decimal places (smallest step: 0.0001)

commands:
  address                          show your address (share this to receive coins)
  balance [address]                show a balance (yours by default)
  send <address> <amount> [speed] ["memo"]
                                   sign and queue a payment. speed is slow, normal
                                   (default), fast, or a fee in coins like 0.05.
                                   the memo (optional, in quotes) needs a speed before it.
  fees                             live fee rate per speed tier and block space
  blocks [count]                   recent blocks: time taken, size vs median, penalty
  difficulty                       proof of work, current difficulty, block times, hashrate
  verify                           re-check every block's proof of work (a GPU chain needs the
                                   CPU dataset per epoch: one to two minutes each at full size)
  supply                           coins issued, emission phase, and inflation
  pending                          list queued transactions
  history                          list confirmed transactions involving your address
  chain                            print the whole chain
  wallet <name>                    switch to another wallet (creates it if new)
  wallets                          list wallet files
  help                             show this
  quit                             leave

mining is a separate program: python miner.py <address>
"""


def is_address(s):
    return bool(re.fullmatch(r"[0-9a-fA-F]{40}", s))


def per_kb(rate):
    # a rate in units per byte -> text in coins per 1000 bytes
    return fmt(round(float(rate) * 1000))


class App:
    def __init__(self, wallet_name):
        os.makedirs(WALLET_DIR, exist_ok=True)
        self.mempool = Mempool()
        self.bc = Blockchain.load_or_new()
        self.use_wallet(wallet_name)

    def use_wallet(self, name):
        if not re.fullmatch(r"[A-Za-z0-9_-]+", name):
            raise ValueError("wallet names may only use letters, numbers, _ and -")
        path = os.path.join(WALLET_DIR, f"{name}.json")
        existed = os.path.exists(path)
        self.wallet = Wallet.load_or_create(path)
        self.wallet_name = name
        if not existed:
            print(f"created new wallet '{name}'")

    # ---------- commands ----------
    def cmd_address(self, args):
        print(self.wallet.address)

    def cmd_balance(self, args):
        addr = args[0].lower() if args else self.wallet.address
        if not is_address(addr):
            raise ValueError("that is not a valid address (expected 40 hex characters)")
        confirmed = self.bc.balance_of(addr)
        out = self.mempool.outgoing(addr)
        print(addr)
        print(f"  confirmed: {fmt(confirmed)}")
        if out:
            print(f"  pending outgoing: {fmt(out)}  (available: {fmt(confirmed - out)})")

    def cmd_send(self, args):
        if not 2 <= len(args) <= 4:
            raise ValueError('usage: send <address> <amount> [slow|normal|fast|fee] ["memo"]')
        addr, amount_s = args[0], args[1]
        if not is_address(addr):
            raise ValueError("that is not a valid address (expected 40 hex characters)")
        try:
            amount = to_units(amount_s)
        except ValueError as e:
            raise ValueError(f"amount: {e}")
        if amount <= 0:
            raise ValueError("amount must be greater than 0")

        speed = args[2].lower() if len(args) >= 3 else "normal"
        memo = args[3] if len(args) == 4 else ""
        if len(memo.encode()) > MAX_MEMO_BYTES:
            raise ValueError(f"memo too long (max {MAX_MEMO_BYTES} bytes)")

        sender, recipient = self.wallet.address, addr.lower()
        tiers = self.mempool.fee_tiers(self.bc)
        if speed in tiers:
            fee = self.mempool.quote(tiers[speed], sender, recipient, amount, memo)
        else:
            try:
                fee = to_units(speed)
            except ValueError as e:
                raise ValueError(f"speed must be slow, normal, fast, or a fee in coins ({e})")

        tx = Transaction(sender, recipient, amount, fee=fee, memo=memo)
        tx.sign(self.wallet)
        self.mempool.add(tx, self.bc)

        size = tx.size()
        rate = Fraction(fee, size)
        print(f"queued: {fmt(amount)} -> {addr}")
        if memo:
            print(f'memo: "{memo}"')
        print(f"size: {size} bytes   fee: {fmt(fee)} ({per_kb(rate)} coins/kB)   "
              f"total cost: {fmt(amount + fee)}")
        if rate > tiers["fast"]:
            print("this fee should get it into the next block")
        elif rate > tiers["normal"]:
            print("this fee should get it in within about 3 blocks")
        else:
            print("blocks are busy: this fee rate is below what the next block needs, "
                  "so it may wait a while")

    def cmd_fees(self, args):
        bc = self.bc
        median = bc.median_size()
        tiers = self.mempool.fee_tiers(bc)
        waiting = len(self.mempool.load())
        print(f"block space: {median} bytes free (about {median // BASE_TX_SIZE} plain sends), "
              f"hard limit {2 * median}")
        print("going over the free space costs part of the block reward, so miners only do "
              "it when the fees cover it")
        print(f"minimum fee rate: {fmt(to_units(MIN_FEE_RATE))} coins per 1000 bytes; "
              f"a plain send is about {BASE_TX_SIZE} bytes")
        print(f"waiting in mempool: {waiting}")
        for name in ("slow", "normal", "fast"):
            rate = tiers[name]
            fee = self.mempool.quote(rate, self.wallet.address, self.wallet.address, UNIT)
            shown = "minimum" if rate == 0 else f"> {per_kb(rate)} coins/kB"
            print(f"  {name:<7} {shown:<22} plain send costs {fmt(fee):<10} {TIER_NOTES[name]}")

    def cmd_blocks(self, args):
        try:
            n = int(args[0]) if args else 10
        except ValueError:
            raise ValueError("usage: blocks [count]")
        bc = self.bc
        if len(bc.chain) == 1:
            print("no blocks yet")
            return
        for pos in range(max(1, len(bc.chain) - n), len(bc.chain)):
            b = bc.chain[pos]
            size = bc.block_size(b)
            median = bc.median_size(pos)
            base = bc.reward_at(b.index)
            pen = bc.penalty(base, size, median)
            note = f"penalty -{fmt(pen)}" if pen else "no penalty"
            took = ""
            if pos >= 2:
                took = f", took {int(b.timestamp) - int(bc.chain[pos - 1].timestamp)}s"
            print(f"block {b.index}: {len(b.transactions) - 1} tx, {size} bytes, "
                  f"median {median}, reward {fmt(base)} ({note}){took}")

    def cmd_difficulty(self, args):
        bc = self.bc
        print(f"proof of work: {bc.pow.describe()}")
        print(f"target block time: {bc.block_time}s")
        if bc.difficulty_window == 0:
            print("difficulty adjustment: OFF (this chain has a fixed difficulty)")
        else:
            print(f"difficulty adjustment: on, weighted over the last "
                  f"{bc.difficulty_window} blocks")
        unit = "attempts" if bc.pow.name == "matmul" else "hashes"
        print(f"current difficulty: ~{2**256 // bc.next_target():,} {unit} per block")
        if bc.pow.name == "matmul":
            p = bc.pow.params
            print(f"one attempt = {p.ops_per_attempt() / 1e9:.2f} billion int8 operations plus a "
                  f"{p.slice_bytes / 2**20:.0f} MiB memory read (so 'attempts per second' is "
                  f"not comparable with SHA-256-style hashes)")
        stats = bc.recent_stats()
        if stats:
            print(f"recent blocks: {stats[0]:.1f}s on average, "
                  f"estimated network rate {format_hashrate(stats[1])}")
            if bc.pow.name == "matmul":
                print(f"  = about {format_tops(stats[1] * bc.pow.params.ops_per_attempt())} "
                      f"of int8 matmul (an estimate: it undercounts while the miner is busy "
                      f"double-checking blocks)")
        else:
            print("recent blocks: not enough blocks yet to measure")

    def cmd_supply(self, args):
        bc = self.bc
        height = len(bc.chain)
        issued = bc.supply()
        reward = bc.reward_at(height)
        print(f"issued: {fmt(issued)}   (main emission cap: {fmt(bc.max_supply)})")
        print(f"next block reward: {fmt(reward)}")

        if bc.in_tail(height):
            print(f"phase: TAIL EMISSION - every block pays {fmt(bc.tail_reward)} "
                  f"from now on, forever")
            per_day = bc.tail_reward * 86400 / bc.block_time
            pct = per_day / issued * 100 if issued else 0
            print(f"at {bc.block_time}s blocks that creates about "
                  f"{fmt(round(per_day))} coins/day, {pct:.1f}% of the current supply")
            print("the percentage keeps falling as the supply grows, but never reaches zero")
        elif reward == 0:
            print("phase: FEES ONLY - all coins are issued; miners earn transaction fees only")
        else:
            blocks = bc.main_emission_end(height) - height
            print(f"phase: MAIN EMISSION - {blocks:,} more block(s) until it ends "
                  f"(about {format_duration(blocks * bc.block_time)} at {bc.block_time}s blocks)")
            if bc.tail_reward:
                print(f"after that: tail emission of {fmt(bc.tail_reward)} per block, forever")
            else:
                print("after that: no new coins, miners earn fees only")

    def cmd_pending(self, args):
        txs = self.mempool.load()
        if not txs:
            print("no pending transactions")
        for tx in sorted(txs, key=lambda t: -Fraction(t.fee, t.size())):
            line = (f"{tx.sender} -> {tx.recipient}: {fmt(tx.amount)} "
                    f"(fee {fmt(tx.fee)}, {tx.size()} bytes)")
            if tx.memo:
                line += f'  "{tx.memo}"'
            print(line)

    def cmd_history(self, args):
        me = self.wallet.address
        found = False
        for b in self.bc.chain:
            for tx in b.transactions:
                if tx.sender == me or tx.recipient == me:
                    found = True
                    note = f'  "{tx.memo}"' if tx.memo else ""
                    if tx.sender == COINBASE:
                        print(f"block {b.index}: mining reward +{fmt(tx.amount)}")
                    elif tx.sender == me:
                        print(f"block {b.index}: sent {fmt(tx.amount)} to {tx.recipient} "
                              f"(fee {fmt(tx.fee)}){note}")
                    else:
                        print(f"block {b.index}: received {fmt(tx.amount)} "
                              f"from {tx.sender}{note}")
        if not found:
            print("no confirmed transactions yet")

    def cmd_chain(self, args):
        self.bc.print_chain()
        if self.bc.pow.cheap:
            print("valid:", self.bc.is_valid())
        else:
            print("structure valid:", self.bc.is_valid(check_pow=False),
                  "(links, timestamps and each block's hash against its own contents and the "
                  "target: this catches any tampering. The proof of work was NOT recomputed, so "
                  "a block with a made-up mix ground to the target would still pass. Run "
                  "'verify' for the full check)")

    def cmd_verify(self, args):
        total = len(self.bc.chain) - 1
        if total == 0:
            print("no blocks yet")
            return
        if not self.bc.pow.cheap:
            p = self.bc.pow.params
            print(f"checking {total} block(s). Each epoch's {p.dataset_bytes / 2**30:.2f} GiB "
                  f"dataset is built on the CPU first (one to two minutes at full size, "
                  f"{p.dataset_bytes / 2**30:.2f} GiB of RAM); then each block takes a fraction "
                  f"of a second...")
        t0 = time.time()

        def progress(done, count):
            if not self.bc.pow.cheap:
                print(f"  block {done}/{count} checked ({time.time() - t0:.0f}s)", flush=True)

        ok = self.bc.is_valid(check_pow=True, progress=progress)
        print(f"valid: {ok}   ({time.time() - t0:.1f}s)")

    def cmd_wallet(self, args):
        if len(args) != 1:
            raise ValueError("usage: wallet <name>")
        self.use_wallet(args[0])
        print(f"now using wallet '{self.wallet_name}': {self.wallet.address}")

    def cmd_wallets(self, args):
        for f in sorted(os.listdir(WALLET_DIR)):
            if f.endswith(".json"):
                name = f[:-5]
                mark = "*" if name == self.wallet_name else " "
                addr = Wallet.load(os.path.join(WALLET_DIR, f)).address
                print(f"{mark} {name}: {addr}")

    def cmd_help(self, args):
        print(HELP)

    # ---------- dispatch ----------
    def run(self, argv):
        if not argv:
            return True
        cmd, args = argv[0].lower(), argv[1:]
        if cmd in ("quit", "exit", "q"):
            return False
        handler = getattr(self, f"cmd_{cmd}", None)
        if handler is None:
            print(f"unknown command '{cmd}' (type: help)")
            return True
        try:
            self.bc = Blockchain.load_or_new()  # pick up blocks the miner found
            handler(args)
        except ValueError as e:
            print("error:", e)
        return True


def main():
    argv = sys.argv[1:]
    wallet_name = "me"
    if len(argv) >= 2 and argv[0] in ("-w", "--wallet"):
        wallet_name = argv[1]
        argv = argv[2:]

    try:
        app = App(wallet_name)
    except ValueError as e:
        print("error:", e)
        return

    if argv:  # one-shot mode: python cli.py [-w name] <command> [args]
        app.run(argv)
        return

    if paths.using_scratch_folder():
        print(f"(scratch data folder: {paths.DATA_DIR})")
    print(f"toycoin wallet '{app.wallet_name}': {app.wallet.address}")
    print("type 'help' for commands")
    while True:
        try:
            line = input(f"[{app.wallet_name}] > ")
        except (EOFError, KeyboardInterrupt):
            print()
            break
        try:
            parts = shlex.split(line)
        except ValueError as e:
            print("error:", e)
            continue
        if not app.run(parts):
            break


if __name__ == "__main__":
    main()
