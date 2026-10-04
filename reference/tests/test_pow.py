"""Tests for the proof-of-work layer: matmul v2 chains mined, saved, loaded and checked."""
import hashlib
import json
import os
import time

import pytest

np = pytest.importorskip("numpy")

from tenero import matmulhash as mh  # noqa: E402
from tenero import pow as powmod  # noqa: E402
from tenero.block import Block  # noqa: E402
from tenero.chain import Blockchain  # noqa: E402
from tenero.transaction import Transaction, COINBASE  # noqa: E402
from tenero.units import UNIT  # noqa: E402

TINY = mh.Params(m=8, k=64, nb=32, num_blocks=6)
START = 2**256 // 16          # about 16 attempts per block: instant even on the CPU
MINER = "b" * 40


def matmul_chain(epoch_blocks=3, window=0, params=TINY, target=START):
    return Blockchain(target=target, initial_reward=50 * UNIT, halving_interval=10_000,
                      max_supply=10**9 * UNIT, tail_reward=0, difficulty_window=window,
                      block_time=30, pow=powmod.MatmulPow(params, epoch_blocks))


def mine(bc, n, t0=1_000_000, gap=30):
    for i in range(n):
        bc.mine_block(MINER, timestamp=t0 + gap * (i + 1))
    return bc


class CountExpensive:
    """Counts full recomputations (each one needs the dataset)."""

    def __init__(self, monkeypatch):
        self.calls = 0
        real = mh.compute_attempts

        def counting(*a, **k):
            self.calls += 1
            return real(*a, **k)

        monkeypatch.setattr(mh, "compute_attempts", counting)


def forged_block(bc, gap=30):
    """A block with a MADE-UP mix, ground until its hash meets the target: it passes the cheap
    check but was never computed."""
    height = len(bc.chain)
    reward = Transaction(COINBASE, MINER, bc.reward_at(height))
    block = Block(height, [reward], bc.chain[-1].hash, timestamp=1_000_000 + gap * height)
    header_hash = hashlib.sha256(block._header_bytes()).digest()
    nonce = 0
    while True:
        fake_mix = hashlib.sha256(b"fake" + nonce.to_bytes(8, "little")).digest() * 2
        seed = mh.attempt_seed(header_hash, nonce)
        digest = mh.digest_of(seed, fake_mix)
        if mh.meets_target(digest, bc.next_target()):
            block.nonce, block.hash, block.mix = nonce, digest.hex(), fake_mix.hex()
            return block
        nonce += 1


def test_epoch_seeds_are_a_fixed_chain_of_hashes():
    s0, s1, s2 = powmod.epoch_seed(0), powmod.epoch_seed(1), powmod.epoch_seed(2)
    assert len({s0, s1, s2}) == 3 and all(len(s) == 32 for s in (s0, s1, s2))
    assert s1 == hashlib.sha256(s0).digest() and s2 == hashlib.sha256(s1).digest()


def test_epoch_boundaries():
    work = powmod.MatmulPow(TINY, epoch_blocks=3)
    assert [work.epoch_of(i) for i in range(1, 10)] == [0, 0, 0, 1, 1, 1, 2, 2, 2]


def test_a_matmul_chain_mines_and_validates():
    bc = mine(matmul_chain(), 7)                            # crosses two epoch changes
    assert bc.is_valid()
    for b in bc.chain[1:]:
        assert b.hash == bc.pow.hash_of(b)
        assert int(b.hash, 16) < START
        assert len(b.hash) == 64 and len(b.mix) == 128      # the 64-byte mix, as hex


def test_the_hash_is_the_digest_of_the_seed_and_the_mix():
    bc = mine(matmul_chain(), 2)
    b = bc.chain[1]
    seed = mh.attempt_seed(hashlib.sha256(b._header_bytes()).digest(), b.nonce)
    assert b.hash == mh.digest_of(seed, bytes.fromhex(b.mix)).hex()


def test_a_tampered_block_is_caught_by_the_cheap_check_alone():
    bc = mine(matmul_chain(), 4)
    assert bc.is_valid(check_pow=False) and bc.is_valid()
    bc.chain[2].transactions[0].amount += 1                 # any change to the contents ...
    assert not bc.is_valid(check_pow=False)                 # ... breaks the hash: no dataset needed
    assert not bc.is_valid()


@pytest.mark.parametrize("what", ["timestamp", "nonce", "hash", "mix", "previous_hash"])
def test_each_field_of_a_block_is_protected(what):
    bc = mine(matmul_chain(), 3)
    b = bc.chain[2]
    assert bc.is_valid(check_pow=False)
    if what == "timestamp":
        b.timestamp += 5
    elif what == "nonce":
        b.nonce += 1
    elif what == "hash":
        b.hash = "0" * 64
    elif what == "mix":
        b.mix = ("00" if b.mix[:2] != "00" else "01") + b.mix[2:]
    else:
        b.previous_hash = "1" * 64
    assert not bc.is_valid(check_pow=False)


@pytest.mark.parametrize("bad", [-1, 2**64, 1.5, "7", True, None])
def test_unusable_nonces_are_rejected_not_crashes(bad):
    bc = mine(matmul_chain(), 2)
    bc.chain[1].nonce = bad
    assert not bc.is_valid(check_pow=False)
    assert not bc.is_valid()


@pytest.mark.parametrize("bad", ["", "zz", "ab" * 63, "ab" * 65, None, 5])
def test_a_missing_or_malformed_mix_is_rejected(bad):
    bc = mine(matmul_chain(), 2)
    bc.chain[1].mix = bad
    assert not bc.pow.precheck(bc.chain[1], START)
    assert not bc.is_valid(check_pow=False)


def test_the_expensive_check_never_runs_for_a_block_that_fails_the_cheap_one(monkeypatch):
    bc = mine(matmul_chain(), 4)
    bc.chain[3].mix = "00" * 64                             # inconsistent with the hash
    bc.pow._cache.clear()
    counter = CountExpensive(monkeypatch)
    assert not bc.is_valid()
    # blocks 1 and 2 are fine and get their full check; block 3 is rejected in microseconds,
    # and block 4 is never looked at
    assert counter.calls == 2

    first = mine(matmul_chain(), 3)
    first.chain[1].mix = "00" * 64                          # now the very first block is bad
    first.pow._cache.clear()
    counter = CountExpensive(monkeypatch)
    assert not first.is_valid()
    assert counter.calls == 0                               # nothing expensive ran at all


def test_a_forged_mix_passes_the_cheap_check_but_not_the_full_one():
    bc = mine(matmul_chain(), 2)
    bc.chain.append(forged_block(bc))
    assert bc.pow.precheck(bc.chain[-1], START)             # it hashes under the target ...
    assert bc.is_valid(check_pow=False)                     # ... so the cheap check accepts it
    assert not bc.is_valid()                                # the recomputation exposes it


def test_a_block_mined_against_the_previous_epochs_dataset_is_rejected():
    # epoch_blocks=2: block 3 is the first block of epoch 1. Mine it with epoch 0's dataset.
    bc = mine(matmul_chain(epoch_blocks=2), 2)
    height = len(bc.chain)
    reward = Transaction(COINBASE, MINER, bc.reward_at(height))
    block = Block(height, [reward], bc.chain[-1].hash, timestamp=1_000_000 + 90)
    header_hash = hashlib.sha256(block._header_bytes()).digest()
    old_data = mh.cached_dataset(TINY, powmod.epoch_seed(0))
    nonce = 0
    while True:      # a real solution, but for the WRONG epoch (0 instead of 1)
        (digest, mix), = mh.compute_attempts(TINY, old_data, header_hash, [nonce])
        if mh.meets_target(digest, START):
            break
        nonce += 1
    block.nonce, block.hash, block.mix = nonce, digest.hex(), mix.hex()
    bc.chain.append(block)
    assert bc.pow.precheck(block, START)                    # consistent, and under the target
    assert not bc.is_valid()                                # but not for THIS epoch's dataset

    honest = mine(matmul_chain(epoch_blocks=2), 3)          # mined with the right dataset
    assert honest.is_valid()


def test_progress_is_reported_for_every_block():
    bc = mine(matmul_chain(), 4)
    seen = []
    assert bc.is_valid(progress=lambda done, total: seen.append((done, total)))
    assert seen == [(1, 4), (2, 4), (3, 4), (4, 4)]


def test_checking_a_block_is_cached(monkeypatch):
    bc = mine(matmul_chain(), 3)
    bc.pow._cache.clear()
    counter = CountExpensive(monkeypatch)
    bc.is_valid()
    first = counter.calls
    assert first == 3
    bc.is_valid()
    assert counter.calls == first                           # nothing re-computed the second time


def test_check_returns_seconds_and_raises_gpufault_for_a_bad_block():
    bc = mine(matmul_chain(), 2)
    block = bc.chain[1]
    assert bc.pow.check(block) >= 0
    assert bc.pow.last_check_seconds >= 0
    block.hash = "1" * 64                                   # inconsistent with its own mix
    with pytest.raises(powmod.GpuFault, match="does not match its own"):
        bc.pow.check(block)


def test_check_catches_a_consistent_but_untrue_block():
    bc = mine(matmul_chain(), 2)
    bc.pow._cache.clear()
    fake = forged_block(bc)
    with pytest.raises(powmod.GpuFault, match="disagrees"):
        bc.pow.check(fake)                                  # passes the cheap check, fails the real one


# ---- a candidate block, before anything expensive ----

def test_precheck_block_accepts_a_good_candidate_and_gives_reasons_for_bad_ones():
    bc = mine(matmul_chain(), 3)
    donor = mine(matmul_chain(), 4)                          # the same chain plus one more block
    good = donor.chain[4]
    assert bc.precheck_block(good) == (True, "")

    wrong_index = Block.from_dict(good.to_dict())
    wrong_index.index = 9
    assert bc.precheck_block(wrong_index) == (False, "wrong block number")

    wrong_link = Block.from_dict(good.to_dict())
    wrong_link.previous_hash = "2" * 64
    assert bc.precheck_block(wrong_link) == (False, "does not follow the current tip")

    tampered = Block.from_dict(good.to_dict())
    tampered.transactions[0].amount += 1
    ok, reason = bc.precheck_block(tampered)
    assert not ok and "hash does not match" in reason

    empty = Block.from_dict(good.to_dict())
    empty.transactions = []
    assert bc.precheck_block(empty) == (False, "no reward transaction")


def test_precheck_block_checks_timestamps_when_difficulty_adjusts():
    bc = mine(matmul_chain(window=5), 6)
    height = len(bc.chain)
    for stamp, expected in ((1_000_000, "older"), (int(time.time()) + 10_000, "future")):
        block = Block(height, [Transaction(COINBASE, MINER, 1)], bc.chain[-1].hash,
                      timestamp=stamp)
        ok, reason = bc.precheck_block(block)
        assert not ok and expected in reason


def test_precheck_block_never_touches_the_dataset(monkeypatch):
    bc = mine(matmul_chain(), 3)
    donor = mine(matmul_chain(), 4)
    counter = CountExpensive(monkeypatch)
    bc.precheck_block(donor.chain[4])
    bc.precheck_block(forged_block(bc))
    assert counter.calls == 0


def test_a_forger_pays_at_least_the_difficulty_in_hashes():
    # the only way past the cheap check without doing the work is grinding a made-up mix to
    # the target: about `difficulty` SHA-256 hashes per block
    bc = mine(matmul_chain(), 2)
    height = len(bc.chain)
    block = Block(height, [Transaction(COINBASE, MINER, 1)], bc.chain[-1].hash, timestamp=1_000_100)
    header_hash = hashlib.sha256(block._header_bytes()).digest()
    target = bc.next_target()
    tries = 0
    for n in range(5000):
        fake = hashlib.sha256(n.to_bytes(8, "little")).digest() * 2
        tries += 1
        if mh.meets_target(mh.digest_of(mh.attempt_seed(header_hash, n), fake), target):
            break
    assert 1 <= tries < 5000                                 # it took hashing, of the right order


# ---- saving and loading ----

def test_save_and_load_keep_the_algorithm_the_hashes_and_the_mix(tmp_path):
    bc = mine(matmul_chain(epoch_blocks=2), 5)
    path = str(tmp_path / "chain.json")
    bc.save(path)
    saved = json.load(open(path))
    assert saved["params"]["pow"] == {"name": "matmul", "version": 2, "epoch_blocks": 2, "m": 8,
                                      "k": 64, "nb": 32, "num_blocks": 6}
    assert all(len(b["mix"]) == 128 for b in saved["chain"][1:]) and "mix" not in saved["chain"][0]
    loaded = Blockchain.load(path)
    assert loaded.pow.name == "matmul" and loaded.pow.epoch_blocks == 2
    assert loaded.pow.params == TINY
    assert [(b.hash, b.mix) for b in loaded.chain] == [(b.hash, b.mix) for b in bc.chain]
    assert loaded.is_valid()


def test_a_tampered_saved_file_is_caught_cheaply(tmp_path):
    bc = mine(matmul_chain(), 3)
    path = str(tmp_path / "chain.json")
    bc.save(path)
    data = json.load(open(path))
    data["chain"][2]["transactions"][0]["amount"] += 1
    json.dump(data, open(path, "w"))
    assert not Blockchain.load(path).is_valid(check_pow=False)


def test_a_chain_made_with_the_old_algorithm_is_refused_with_a_clear_message(tmp_path):
    bc = mine(matmul_chain(), 2)
    path = str(tmp_path / "chain.json")
    bc.save(path)
    data = json.load(open(path))
    del data["params"]["pow"]["version"]                     # what a v1 chain looks like
    data["params"]["pow"]["rounds"] = 48
    json.dump(data, open(path, "w"))
    with pytest.raises(ValueError, match="matmulhash v1.*Delete chain.json"):
        Blockchain.load(path)


def test_old_chains_without_a_pow_setting_load_as_sha256(tmp_path):
    bc = Blockchain(target=2**250, initial_reward=50 * UNIT, halving_interval=10_000,
                    max_supply=10**9 * UNIT, tail_reward=0, difficulty_window=0)
    bc.mine_block(MINER)
    path = str(tmp_path / "chain.json")
    bc.save(path)
    data = json.load(open(path))
    del data["params"]["pow"]
    json.dump(data, open(path, "w"))
    loaded = Blockchain.load(path)
    assert loaded.pow.name == "sha256" and loaded.is_valid()
    assert "mix" not in data["chain"][1]                     # SHA-256 blocks carry no mix


def test_difficulty_adjustment_works_with_matmul_blocks():
    bc = matmul_chain(window=4, target=2**256 // 8)
    t = 1_000_000
    for i in range(7):
        t += 15                                              # blocks twice as fast as the 30 s target
        bc.mine_block(MINER, timestamp=t)
    _, targets = bc._history()
    assert targets[-1] < targets[2]                          # it got harder
    assert bc.is_valid()


def test_sha256_chains_are_unchanged():
    bc = Blockchain(target=2**250, initial_reward=50 * UNIT, halving_interval=10_000,
                    max_supply=10**9 * UNIT, tail_reward=0, difficulty_window=0)
    assert bc.pow.name == "sha256" and bc.pow.cheap
    for _ in range(3):
        b = bc.mine_block(MINER)
        assert b.hash == b.compute_hash() and b.mix == ""
    assert bc.is_valid() and bc.precheck_block(bc.chain[-1]) == (False, "wrong block number")
    tampered = bc.chain[2]
    tampered.transactions[0].amount += 1
    assert not bc.is_valid()


def test_a_new_chain_takes_its_algorithm_from_the_config(monkeypatch, tmp_path):
    from tenero import config
    monkeypatch.setattr(config, "POW_DATASET_GIB", 0.00002)
    monkeypatch.setattr(config, "POW_M", 8)
    monkeypatch.setattr(config, "POW_K", 64)
    monkeypatch.setattr(config, "POW_NB", 32)
    monkeypatch.setattr(config, "POW_EPOCH_BLOCKS", 7)
    path = str(tmp_path / "none.json")
    bc = Blockchain.load_or_new(path)
    assert bc.pow.name == "matmul" and bc.pow.epoch_blocks == 7
    assert bc.pow.params.k == 64 and bc.pow.params.num_blocks >= 1
    from tenero import chain as chain_module
    assert bc.target == 2**256 // chain_module.MATMUL_START_ATTEMPTS
    assert Blockchain.load_or_new(path, algorithm="sha256").pow.name == "sha256"
    with pytest.raises(ValueError):
        Blockchain.load_or_new(path, algorithm="nonsense")


def test_default_pow_takes_an_epoch_override(monkeypatch):
    from tenero import config
    monkeypatch.setattr(config, "POW_DATASET_GIB", 0.00002)
    monkeypatch.setattr(config, "POW_M", 8)
    monkeypatch.setattr(config, "POW_K", 64)
    monkeypatch.setattr(config, "POW_NB", 32)
    assert powmod.default_pow("matmul").epoch_blocks == config.POW_EPOCH_BLOCKS
    assert powmod.default_pow("matmul", epoch_blocks=9).epoch_blocks == 9
    assert powmod.default_pow("sha256", epoch_blocks=9).name == "sha256"   # ignored


def test_describe_explains_the_new_cost_model():
    text = powmod.MatmulPow(TINY, 5).describe()
    assert "v2" in text and "earlier ones" in text and "RAM" in text


# ---- the search protocol ----

class Slow:
    """A searcher that finds a (fake) result after a few chunks."""
    attempts = 0

    def __init__(self, chunks_needed=4):
        self.chunks, self.needed = 0, chunks_needed

    def search(self, params, seed, header_hash, target, start, seconds=None):
        self.chunks += 1
        if self.chunks >= self.needed:
            return start, b"\x01" * 32, b"\x02" * 64, start + 1
        return None, None, None, start + 1


class Lying:
    attempts = 0

    def search(self, params, seed, header_hash, target, start, seconds=None):
        return start, b"\x00" * 32, b"\x00" * 64, start + 1


def test_the_cpu_searcher_returns_the_digest_and_the_mix():
    searcher = powmod.CpuSearcher()
    seed = powmod.epoch_seed(0)
    header = hashlib.sha256(b"h").digest()
    nonce, digest, mix, nxt = searcher.search(TINY, seed, header, START, 0)
    assert len(digest) == 32 and len(mix) == 64 and nxt == nonce + 1
    assert mh.precheck(header, nonce, mix, digest.hex(), START)
    assert searcher.attempts >= 1
    assert searcher.search(TINY, seed, header, 0, 0, 0.0)[0] is None      # target 0: never found


def test_mine_can_skip_the_check_so_the_caller_can_run_it_later():
    bc = matmul_chain()
    bc.pow.searcher = Lying()
    block = bc.mine_block(MINER, timestamp=1_000_030, verify=False)   # returned unchecked
    assert bc.chain[-1] is block
    with pytest.raises(powmod.GpuFault):
        bc.pow.check(block)                                            # the later check catches it
    bc2 = matmul_chain()
    bc2.pow.searcher = Lying()
    bc2.pow.verify_solutions = False
    with pytest.raises(powmod.GpuFault):
        bc2.mine_block(MINER, timestamp=1_000_030, verify=True)


def test_a_bad_result_is_never_kept():
    bc = matmul_chain()
    bc.pow.searcher = Lying()
    with pytest.raises(powmod.GpuFault, match="own|disagrees"):
        bc.mine_block(MINER, timestamp=1_000_030)
    assert len(bc.chain) == 1                                # nothing was appended


def test_on_chunk_is_called_between_chunks_and_can_abort():
    class Chunked(Slow):
        def search(self, *a, **k):
            assert a[-1] is not None                          # a hook forces chunked searching
            return super().search(*a, **k)

    bc = matmul_chain()
    bc.pow.searcher = Chunked()
    bc.pow.verify_solutions = False
    calls = []
    bc.mine_block(MINER, timestamp=1_000_030, on_chunk=lambda: calls.append(1))
    assert len(calls) == 3                                    # after each of the 3 unsuccessful chunks

    class Abort(Exception):
        pass

    def boom():
        raise Abort

    bc2 = matmul_chain()
    bc2.pow.searcher = Slow()
    bc2.pow.verify_solutions = False
    with pytest.raises(Abort):
        bc2.mine_block(MINER, timestamp=1_000_030, on_chunk=boom)
    assert len(bc2.chain) == 1                                # an aborted search appends nothing


def test_a_fixed_timestamp_is_not_changed_by_chunked_searching():
    bc = matmul_chain()
    bc.pow.searcher = Slow(3)
    bc.pow.verify_solutions = False
    block = bc.mine_block(MINER, timestamp=1_234_567, on_chunk=lambda: None)
    assert block.timestamp == 1_234_567
    assert block.mix == ("02" * 64)                           # the mix the searcher returned is kept


def test_real_time_mining_refreshes_the_timestamp_and_stays_valid():
    bc = matmul_chain(window=5)
    bc.pow.CHUNK_SECONDS = 0.05
    b = bc.mine_block(MINER)                                 # no fixed timestamp
    assert abs(int(b.timestamp) - time.time()) < 5
    assert bc.is_valid()


def test_save_can_write_only_a_verified_prefix(tmp_path):
    bc = mine(matmul_chain(), 4)
    path = str(tmp_path / "chain.json")
    bc.save(path, upto=3)                          # genesis + blocks 1 and 2
    loaded = Blockchain.load(path)
    assert [b.index for b in loaded.chain] == [0, 1, 2] and loaded.is_valid()
    bc.save(path)
    assert len(Blockchain.load(path).chain) == 5   # the default still saves everything


def test_prepare_builds_the_epochs_dataset_for_cpu_checks():
    work = powmod.MatmulPow(TINY, 3)
    data = work.prepare(0)
    assert data.shape == (TINY.num_blocks, TINY.slice_words)
    assert work.prepare(0) is data                            # cached, not rebuilt


# ---- memory: a finished epoch's CPU dataset is freed ----

def epochs_cached(params=TINY):
    return {e for e in range(12) if (params, powmod.epoch_seed(e)) in mh.DEFAULT_CACHE._items}


def test_a_finished_epochs_dataset_is_freed_once_checks_move_to_the_next_epoch():
    mh.DEFAULT_CACHE.clear()
    bc = mine(matmul_chain(epoch_blocks=2), 6)                 # epochs 0, 1, 2
    work = bc.pow
    mh.DEFAULT_CACHE.clear()
    work.prepare(0)
    work.prepare(1)
    assert epochs_cached() == {0, 1}
    block = bc.chain[3]                                         # the first block of epoch 1
    work.digest_for(block.index, block._header_bytes(), block.nonce)
    assert epochs_cached() == {1}                               # epoch 0 is dead weight: gone


def test_the_prefetched_next_epoch_is_kept_while_the_current_one_is_checked():
    mh.DEFAULT_CACHE.clear()
    bc = mine(matmul_chain(epoch_blocks=2), 6)
    work = bc.pow
    mh.DEFAULT_CACHE.clear()
    work.prepare(1)
    work.prepare(2)                                             # the current epoch and the next
    block = bc.chain[3]                                         # epoch 1
    work.digest_for(block.index, block._header_bytes(), block.nonce)
    assert epochs_cached() == {1, 2}
    block = bc.chain[5]                                         # epoch 2: now epoch 1 goes
    work.digest_for(block.index, block._header_bytes(), block.nonce)
    assert epochs_cached() == {2}


def test_a_full_validation_leaves_only_the_last_epoch_in_memory():
    mh.DEFAULT_CACHE.clear()
    bc = mine(matmul_chain(epoch_blocks=2), 6)
    bc.pow._cache.clear()
    mh.DEFAULT_CACHE.clear()
    assert bc.is_valid()
    assert epochs_cached() == {2}


def test_memory_over_many_epochs_never_holds_more_than_the_current_and_the_next():
    # the miner's pattern: prepare the current epoch, prefetch the next one near the end, then
    # check blocks in order. Track how many datasets are in memory at every step.
    mh.DEFAULT_CACHE.clear()
    bc = mine(matmul_chain(epoch_blocks=2), 16)                 # 8 epochs
    work = bc.pow
    work._cache.clear()
    mh.DEFAULT_CACHE.clear()
    peak = 0
    for block in bc.chain[1:]:
        epoch = work.epoch_of(block.index)
        work.prepare(epoch)
        if block.index % 2 == 0:                                # the last block of its epoch
            work.prepare(epoch + 1)
        work.digest_for(block.index, block._header_bytes(), block.nonce)
        held = epochs_cached()
        peak = max(peak, len(held))
        assert held <= {epoch, epoch + 1}, (block.index, held)   # nothing finished is kept
    assert peak <= 2
