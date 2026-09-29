"""Where chain.json, mempool.json and the wallets live.

Normally that is the project folder. Setting the TOYCOIN_DATA environment variable points
everything (miner, wallet, view) at another folder instead, which is handy for a scratch
chain you can experiment on without touching your real one:

    PowerShell:   $env:TOYCOIN_DATA = "scratch"      (then run miner.bat / cli.bat as usual)
    cmd:          set TOYCOIN_DATA=scratch
    back to normal:   Remove-Item Env:TOYCOIN_DATA      (or close the window)
"""
import os

PROJECT_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def data_dir(value=None):
    """The data folder: `value` if given, else $TOYCOIN_DATA, else the project folder."""
    value = os.environ.get("TOYCOIN_DATA") if value is None else value
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
