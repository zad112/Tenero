import time
from toycoin.block import Block

target = 2**256 // 2_000_000  # ~1-2 seconds per block
total_nonces, total_time = 0, 0.0
for i in range(15):
    b = Block(i + 1, [], "0" * 64)
    t = time.time()
    b.mine(target)
    total_time += time.time() - t
    total_nonces += b.nonce + 1

rate = total_nonces / total_time
print(f"real mining rate: {rate:,.0f} hashes/sec")
print(f"target = 2**256 // {int(rate * 30)}   # for ~30s blocks")