import os
import time

from tenero.chain import Blockchain
from tenero.transaction import Transaction
from tenero.units import fmt, to_units
from tenero.wallet import Wallet

HERE = os.path.dirname(os.path.abspath(__file__))
WALLET_DIR = os.path.join(HERE, "wallets")
os.makedirs(WALLET_DIR, exist_ok=True)

# wallets are created once and reused between runs
alice = Wallet.load_or_create(os.path.join(WALLET_DIR, "alice.json"))
bob = Wallet.load_or_create(os.path.join(WALLET_DIR, "bob.json"))
carol = Wallet.load_or_create(os.path.join(WALLET_DIR, "carol.json"))

target = 2**256 // 32_719_438  # ~30s blocks on your machine
bc = Blockchain(target=target, difficulty_window=0)  # fixed difficulty


def send(sender, recipient, amount, fee):
    # amount and fee are typed in coins, e.g. "30.5"
    tx = Transaction(sender.address, recipient.address, to_units(amount), fee=to_units(fee))
    tx.sign(sender)
    return bc.add_transaction(tx)


def mine(miner_name, miner_wallet):
    t = time.time()
    b = bc.mine_block(miner_wallet.address)
    print(f"block {b.index} mined by {miner_name}: nonce={b.nonce:,} time={time.time() - t:.1f}s")


mine("alice", alice)
mine("alice", alice)

send(alice, bob, "30", "2")
send(alice, carol, "20.1234", "5")

# an overspend: carol has no coins yet
try:
    send(carol, alice, "1000", "1")
except ValueError as e:
    print("rejected overspend:", e)

# a forgery: mallory pretends to be alice but signs with her own key
mallory = Wallet()
forged = Transaction(alice.address, mallory.address, to_units("40"), fee=to_units("1"))
forged.sign(mallory)
try:
    bc.add_transaction(forged)
except ValueError as e:
    print("rejected forgery:", e)

mine("bob", bob)

print()
bc.print_chain()
print()
for name, w in (("alice", alice), ("bob", bob), ("carol", carol)):
    print(f"{name} ({w.address[:8]}..): {fmt(bc.balance_of(w.address))}")
print("valid:", bc.is_valid())
print("saved to", bc.save(os.path.join(HERE, "demo_chain.json")))
