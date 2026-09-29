# Try out difficulty settings without waiting for real blocks.
#
# It pretends miners with a given hashrate are mining, using the SAME adjustment
# code as the chain, and prints how block times and difficulty respond when the
# hashrate jumps up and drops back. Change the numbers below and run:
#     python difficulty_sim.py
import random

from toycoin.chain import Blockchain, format_hashrate
from toycoin.config import TARGET_BLOCK_TIME

WINDOW = 30          # blocks the adjustment averages over (try 10, 30, 60, 120)
BLOCK_TIME = TARGET_BLOCK_TIME   # target seconds per block (from config.py)
START_RATE = 1_000_000            # hashes per second at the start
CHANGES = {150: 10, 300: 0.2}     # at block N, the hashrate becomes START_RATE * this
BLOCKS = 450
SEED = None          # set a number to repeat the exact same run

rng = random.Random(SEED)
bc = Blockchain(target=int(2**256 / (START_RATE * BLOCK_TIME)), block_time=BLOCK_TIME,
                difficulty_window=WINDOW)
ts, targets, solves, rates = [0], [bc.target], [], []
t, rate = 1_000_000, START_RATE
print(f"window {WINDOW}, target {BLOCK_TIME}s, starting at {format_hashrate(START_RATE)}\n")
print(f"{'blocks':>10}  {'hashrate':>10}  {'avg block time':>15}  {'difficulty (hashes/block)':>26}")
for pos in range(1, BLOCKS + 1):
    if pos in CHANGES:
        rate = START_RATE * CHANGES[pos]
    rates.append(rate)
    target = bc._retarget(ts, targets, pos)
    dt = rng.expovariate(rate / (2**256 / target))  # random, like real mining
    t += int(dt)
    solves.append(int(dt))
    targets.append(target)
    ts.append(t)
    if pos % 25 == 0:
        avg = sum(solves[-25:]) / 25
        print(f"{pos - 24:>4}-{pos:<5}  {format_hashrate(rates[pos - 25]):>10}  {avg:>14.1f}s  "
              f"{2**256 // target:>26,}")
