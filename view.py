import os

from toycoin.chain import Blockchain
from toycoin.units import fmt
from toycoin.wallet import Wallet

from toycoin import paths

WALLET_DIR = paths.WALLET_DIR

names = {}
if os.path.isdir(WALLET_DIR):
    for f in sorted(os.listdir(WALLET_DIR)):
        if f.endswith(".json"):
            names[Wallet.load(os.path.join(WALLET_DIR, f)).address] = f[:-5]

bc = Blockchain.load()
bc.print_chain()
print()
for address, balance in sorted(bc.all_balances().items(), key=lambda kv: -kv[1]):
    label = names.get(address, address[:8] + "..")
    print(f"{label}: {fmt(balance)}")
print("valid:", bc.is_valid())
