"""Where chain.json, mempool.json and the wallets live.

Normally that is the folder above this package (`reference/`). Setting the TENERO_DATA environment variable points
everything at another folder instead. (The miner, wallet and viewer that used it were removed at M11.1 and are on the
`legacy-python` branch; what is left is the frozen reference chain code, which still saves and loads through here.)
"""
import os

PROJECT_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def data_dir(value=None):
    """The data folder: `value` if given, else $TENERO_DATA, else the project folder."""
    value = os.environ.get("TENERO_DATA") if value is None else value
    if not value:
        return PROJECT_DIR
    path = os.path.abspath(value)
    os.makedirs(path, exist_ok=True)
    return path


DATA_DIR = data_dir()
CHAIN_PATH = os.path.join(DATA_DIR, "chain.json")
MEMPOOL_PATH = os.path.join(DATA_DIR, "mempool.json")
WALLET_DIR = os.path.join(DATA_DIR, "wallets")


def using_scratch_folder():
    return DATA_DIR != PROJECT_DIR
