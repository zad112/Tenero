"""Shared test setup.

The block-size tests were written for a tiny floor, where a handful of transactions overflow it.
The real floor in config.py is 300,000 bytes (about 700 transactions), which would make every one
of those tests build hundreds of signed transactions to prove the same rule. So the rules are
tested with a small floor, and tests marked `real_floor` run against the value in config.py.
"""
import pytest

from tenero import chain as chain_module

SMALL_FLOOR = 2200      # bytes: about five plain transactions


def pytest_configure(config):
    config.addinivalue_line(
        "markers", "real_floor: run with the block-size floor from config.py, not the small "
                   "test floor")


@pytest.fixture(autouse=True)
def small_block_floor(request, monkeypatch):
    if "real_floor" not in request.keywords:
        monkeypatch.setattr(chain_module, "MIN_BLOCK_MEDIAN", SMALL_FLOOR)
