# All the settings in one place.
#
# Amounts here are in COINS and may have up to 4 decimal places (see units.py).
#
# INITIAL_REWARD, HALVING_INTERVAL, MAX_SUPPLY and TAIL_REWARD are saved inside
# chain.json when a chain is created, so editing them here only affects a NEW
# chain. To apply changes: delete chain.json and mempool.json.
#
# The fee and block-size settings below apply immediately to whatever chain you
# load. Changing them can make an old chain invalid, so delete chain.json after
# changing any of them.

# --- coin supply (in coins) ---
INITIAL_REWARD = 20        # block reward at the start: 20 coins a block, 28,800 coins a day
HALVING_INTERVAL = 525_600     # blocks between each halving: 1 year of 60-second blocks
MAX_SUPPLY = 20_000_000    # cap on the MAIN emission (halving phase)
TAIL_REWARD = 0.5          # reward per block forever after the main emission ends
                           # (0 = no tail: supply stops at MAX_SUPPLY)
# The rewards run 20, 10, 5, 2.5, 1.25: four eras of one year, then a fifth that is cut off by
# the cap after 232,000 blocks. The cap is reached exactly at block 2,334,400 (about 4.4 years)
# and that last block pays a full 1.25, nothing trimmed; the tail starts at the next block.
#
# CAUTION when changing these numbers. The tail takes over as soon as the halving schedule
# would pay LESS than TAIL_REWARD, even if MAX_SUPPLY has not been reached, so a schedule that
# halves below the tail too early ends short of the cap (20 coins halving every 500,000 blocks
# stops at 19,687,500; 50 coins every 200,000 blocks at 19,843,740). And the last main reward
# is trimmed to hit the cap exactly: if the trimmed amount is below TAIL_REWARD the tail pays
# more than it, so the supply ends slightly ABOVE the cap. Here the coins left after four full
# eras (290,000) divide evenly by the fifth era's reward (1.25), so neither happens.
# Also: at 20 coins a block the cap is only 694 days away without any halving, so a halving
# interval of 1,051,200 (2 years) would never halve at all. The interval has to stay below about
# 600,000 blocks for the halvings to happen before the cap is reached.

# --- transactions and fees ---
MIN_FEE_RATE = 0.01      # minimum fee in coins per 1000 bytes (never below 0.0001 per tx).
                         # A full block at the size floor pays about 0.02 coins of fees, a
                         # few percent of the tail reward; congestion raises it (see `fees`).
MAX_MEMO_BYTES = 100     # longest memo a transaction may carry

# --- flexible block size (Monero style) ---
MIN_BLOCK_MEDIAN = 300_000  # bytes (300 kB); floor for the median: about 700 plain transactions
                            # (428 bytes each) fit in a block for free, and the hard limit is twice
                            # this (600 kB). It is a consensus rule: every node must use the same value.
MEDIAN_WINDOW = 10       # the median is taken over this many recent blocks
# A block up to the median pays the full reward. Above it, the reward shrinks by
# base_reward * (overage / median)^2. The hard limit is 2x the median.

# --- proof of work (saved inside chain.json when a chain is created) ---
POW_ALGORITHM = "matmul"    # "matmul" (GPU miner) or "sha256" (CPU miner, for testing)
POW_EPOCH_BLOCKS = 100      # matmul: blocks between dataset changes
POW_DATASET_GIB = 4.0       # matmul: dataset size (sized so an 8 GB card can hold it)
POW_M, POW_K, POW_NB = 64, 8192, 2048   # matmul: rows per attempt, and the 16 MiB slice shape
MINER_MAX_CORES = 6         # the whole mining program uses at most this many CPU cores
MATMUL_START_ATTEMPTS = 1_400_000   # matmul: expected attempts for the first blocks (~60 s on a
                                    # mid-range GPU); the difficulty adjustment takes over from here

# --- mining and difficulty ---
TARGET_BLOCK_TIME = 60   # seconds: the difficulty adjusts to hit this on average
DEFAULT_TARGET = 2**256 // 32_719_438  # SHA-256 chains only: starting difficulty (about 30 s on a
                                       # CPU; the adjustment retunes it to TARGET_BLOCK_TIME quickly)

# Difficulty adjustment (LWMA: a weighted average of the last DIFFICULTY_WINDOW
# block times, newest blocks counting most). It re-tunes after EVERY block.
DIFFICULTY_WINDOW = 30   # blocks to look back. Bigger = smoother but slower to react.
                         # 0 = fixed difficulty (adjustment off)
MAX_TARGET_STEP = 4      # difficulty can change at most 4x up or down per block
# A block's timestamp must be LATER than its parent's (M11.2: this replaced "not below the median of the last 11", which let a miner with 30 % of the hash
# rate pull the difficulty to 0.40x by backdating; docs/THREAT_MODEL.md E3)...
FUTURE_TIME_LIMIT = 120  # ...and may not be more than this many seconds ahead of your clock
# DIFFICULTY_WINDOW and TARGET_BLOCK_TIME are saved inside chain.json when a chain
# is created, like the supply settings: editing them only affects a NEW chain.
