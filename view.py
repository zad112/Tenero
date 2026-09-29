import os

from tenero.chain import Blockchain
from tenero.units import fmt
from tenero.wallet import Wallet

from tenero import paths

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
