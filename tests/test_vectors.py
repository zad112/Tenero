"""The golden vectors in tests/vectors/, checked against the Python reference.

These tests read the committed JSON the way another implementation (a C++ or Rust rewrite) would,
and check every value. Where possible a second, independent computation is used as well (exact
integer matmul instead of float, hand-built JSON headers, OpenSSL), so the vectors are not just
the code agreeing with itself. `test_the_committed_vectors_are_current` fails if the reference
changed without the vectors being regenerated: that is a consensus change and must be deliberate.

The deep and full vectors are large: set TOYCOIN_SLOW_VECTORS=1 to check them.
"""
import copy
import dataclasses
import hashlib
import json
import os

import numpy as np
import pytest

from toycoin import chacha
from toycoin import chain as chain_module
from toycoin import matmulhash as mh
from toycoin import pow as powmod
from toycoin.block import Block
from toycoin.chain import Blockchain, min_fee_for
from toycoin.transaction import Transaction
from toycoin.units import fmt, to_units
from toycoin.wallet import Wallet
from tools import make_vectors as mv

pytestmark = pytest.mark.real_floor      # each test states the block-size floor it needs
SLOW = os.environ.get("TOYCOIN_SLOW_VECTORS") == "1"

RFC_BLOCK = ("10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4e"
             "d2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e")


def load(name):
    with open(mv.path_of(name)) as f:
        doc = json.load(f)
    assert doc["schema"] == mv.SCHEMA and doc["name"] == name
    return doc


def h(text):
    return bytes.fromhex(text)


def sha(b):
    return hashlib.sha256(b).hexdigest()


# ---------------------------------------------------------------- housekeeping

def test_every_vector_file_is_present_and_well_formed():
    for name in mv.FAST_FILES:
        doc = load(name)
        assert doc["description"]
    assert os.path.exists(os.path.join(mv.VECTOR_DIR, "README.md"))


def test_the_committed_vectors_are_current(capsys):
    # the reference and the committed files must agree; regenerate with `--write` ONLY for an
    # intentional consensus change
    assert mv.main(["--check"]) == 0
    assert "DIFFERS" not in capsys.readouterr().out


def test_generating_twice_gives_identical_bytes():
    assert mv.jdump(mv.units_vectors()) == mv.jdump(mv.units_vectors())
    assert mv.jdump(mv.chacha_vectors()) == mv.jdump(mv.chacha_vectors())
    assert mv.jdump(mv.difficulty_vectors()) == mv.jdump(mv.difficulty_vectors())


# ---------------------------------------------------------------- ChaCha20

def test_chacha20_blocks():
    doc = load("chacha20")
    assert doc["blocks"][0]["output"] == RFC_BLOCK             # the RFC 8439 section 2.3.2 block
    for v in doc["blocks"]:
        out = chacha.chacha20_blocks(chacha.key_words(h(v["key"]))[None], np.array([v["counter"]]),
                                     np.frombuffer(h(v["nonce"]), dtype="<u4")[None])[0]
        assert out.astype("<u4").tobytes().hex() == v["output"]


def test_chacha20_blocks_against_openssl():
    ciphers = pytest.importorskip("cryptography.hazmat.primitives.ciphers")
    for v in load("chacha20")["blocks"]:
        iv = v["counter"].to_bytes(4, "little") + h(v["nonce"])
        enc = ciphers.Cipher(ciphers.algorithms.ChaCha20(h(v["key"]), iv), mode=None).encryptor()
        assert enc.update(bytes(64)).hex() == v["output"]


def test_chacha_core_on_arbitrary_states():
    for v in load("chacha20")["core"]:
        out = chacha.chacha_core(np.array([v["state"]], dtype=np.uint32))[0]
        assert [int(w) for w in out] == v["output"]


def test_chacha20_keystreams():
    for v in load("chacha20")["keystream"]:
        ks = chacha.keystream(h(v["key"]), v["blocks"], v["start_counter"], tuple(v["nonce_words"]))
        assert sha(ks.astype("<u4").tobytes()) == v["sha256"]


# ---------------------------------------------------------------- matmulhash v2

def check_attempt(params, data, v):
    header = h(v["header_hash"])
    seed = mh.attempt_seed(header, v["nonce"])
    assert seed.hex() == v["seed"]
    b = mh.attempt_slice(seed, params.num_blocks)
    assert b == v["slice_index"]
    X = mh.make_x(seed, params)
    assert sha(X.astype(np.int8).tobytes()) == v["x_sha256"]
    W = mh.slice_matrix(data, b, params)
    # C two ways: the reference's float64 matmul, and an exact integer matmul
    C = (X.astype(np.int64) @ W.astype(np.int64)).astype(np.int32)
    assert sha(C.astype("<i4").tobytes()) == v["c_sha256"]
    sums = mh.fold_sums(C.reshape(1, -1))[0]
    assert [format(int(s), "016x") for s in sums] == v["sums"]
    assert mh.mix_bytes(sums).hex() == v["mix"]
    digest, mix = mh.compute_attempts(params, data, header, [v["nonce"]])[0]
    assert (digest.hex(), mix.hex()) == (v["digest"], v["mix"])
    assert mh.digest_of(seed, h(v["mix"])).hex() == v["digest"]


def test_small_datasets_and_attempts():
    for case in load("matmulhash_small")["cases"]:
        p = mh.Params(**case["params"])
        seed = h(case["epoch_seed"])
        assert mh.dataset_key(seed).hex() == case["dataset_key"]
        data = mh.build_dataset(p, seed, threads=1)
        assert sha(data.astype("<u4").tobytes()) == case["dataset_sha256"]
        assert [sha(data[j].astype("<u4").tobytes()) for j in range(p.num_blocks)] == case["slice_sha256"]
        for a in case["attempts"]:
            check_attempt(p, data, a)


def test_the_dataset_is_the_same_on_any_number_of_threads():
    case = load("matmulhash_small")["cases"][4]
    p, seed = mh.Params(**case["params"]), h(case["epoch_seed"])
    for threads, chunk in ((1, 5), (3, 1), (4, 7)):
        data = mh.build_dataset(p, seed, threads=threads, chunk_blocks=chunk)
        assert sha(data.astype("<u4").tobytes()) == case["dataset_sha256"]


def test_the_fold():
    for v in load("matmulhash_small")["fold"]:
        sums = mh.fold_sums(np.array([v["c"]], dtype=np.int32))[0]
        assert [format(int(s), "016x") for s in sums] == v["sums"]


def test_real_size_slices_and_attempts():
    doc = load("matmulhash_real")
    p = mh.Params(**doc["params"])
    assert p == mh.Params()                                     # the real parameters
    seed = h(doc["epoch_seed"])
    assert seed == powmod.epoch_seed(0) and mh.dataset_key(seed).hex() == doc["dataset_key"]
    data = mh.build_dataset(dataclasses.replace(p, num_blocks=doc["slices_built"]), seed)
    for j, want in doc["slice_sha256"].items():
        assert sha(data[int(j)].astype("<u4").tobytes()) == want
    assert len(doc["attempts"]) >= 3
    for a in doc["attempts"]:
        assert a["slice_index"] < doc["slices_built"]
        check_attempt(p, data, a)


@pytest.mark.skipif(not SLOW, reason="slow: set TOYCOIN_SLOW_VECTORS=1")
def test_deep_slices_and_attempts():
    doc = load("matmulhash_deep")
    p = mh.Params(**doc["params"])
    data = mh.build_dataset(dataclasses.replace(p, num_blocks=doc["slices_built"]), h(doc["epoch_seed"]), threads=4)
    for j, want in doc["slice_sha256"].items():
        assert sha(data[int(j)].astype("<u4").tobytes()) == want, f"slice {j}"
    for a in doc["attempts"]:
        check_attempt(p, data, a)


@pytest.mark.skipif(not SLOW or not os.path.exists(mv.path_of("matmulhash_full")),
                    reason="slow and needs 4.3 GiB of RAM: set TOYCOIN_SLOW_VECTORS=1 after `--full`")
def test_every_slice_of_the_full_dataset():
    doc = load("matmulhash_full")
    p = mh.Params(**doc["params"])
    data = mh.build_dataset(p, h(doc["epoch_seed"]), threads=4)
    assert sha(data.astype("<u4").tobytes()) == doc["dataset_sha256"]
    assert [sha(data[j].astype("<u4").tobytes()) for j in range(p.num_blocks)] == doc["slice_sha256"]


def test_the_full_vector_builder_works_at_a_small_scale():
    p = mh.Params(m=8, k=64, nb=64, num_blocks=12)
    seed = bytes(range(32))
    doc = mv.full_vectors(params=p, seed=seed, threads=2)
    data = mh.build_dataset(p, seed, threads=1)
    assert doc["slice_sha256"] == [sha(data[j].astype("<u4").tobytes()) for j in range(12)]
    assert doc["dataset_sha256"] == sha(data.astype("<u4").tobytes())


# ---------------------------------------------------------------- proof of work, misc

def test_epoch_seeds_numbering_and_targets():
    doc = load("pow_misc")
    assert powmod.epoch_seed(0) == hashlib.sha256(doc["epoch_seed_0_label"].encode()).digest()
    for e, want in doc["epoch_seeds"].items():
        assert powmod.epoch_seed(int(e)).hex() == want
    for e in range(1, 13):                                      # each seed is the hash of the last
        assert powmod.epoch_seed(e) == hashlib.sha256(powmod.epoch_seed(e - 1)).digest()
    for v in doc["epoch_of"]:
        assert (v["index"] - 1) // v["epoch_blocks"] == v["epoch"]
    for v in doc["bits_to_target"]:
        assert mh.bits_to_target(v["bits"]) == int(v["target"]) == 1 << (256 - v["bits"])


def test_the_cheap_precheck():
    for v in load("pow_misc")["precheck"]:
        assert mh.precheck(h(v["header_hash"]), v["nonce"], h(v["mix"]), v["hash"], int(v["target"])) is v["expect"], v["name"]


# ---------------------------------------------------------------- emission

def test_emission_schedule():
    for name, s in load("emission")["sets"].items():
        bc = Blockchain(target=2**240, block_time=60, difficulty_window=0, **s["params"])
        assert bc.main_emission_end(1) == s["main_emission_end_from_1"], name
        for row in s["rows"]:
            hgt = row["height"]
            got = {"scheduled": bc.scheduled_reward(hgt), "issued_before": bc.issued_before(hgt),
                   "main_reward": bc.main_reward_at(hgt), "reward": bc.reward_at(hgt), "in_tail": bc.in_tail(hgt)}
            assert got == {k: row[k] for k in got}, (name, hgt)


def test_the_default_schedule_matches_config():
    from toycoin import config
    d = load("emission")["sets"]["default"]["params"]
    assert d == {"initial_reward": to_units(config.INITIAL_REWARD), "halving_interval": config.HALVING_INTERVAL,
                 "max_supply": to_units(config.MAX_SUPPLY), "tail_reward": to_units(config.TAIL_REWARD)}


def test_the_default_emission_by_an_independent_sum():
    # four full years of 20, 10, 5 and 2.5 coins, then 232,000 blocks of 1.25: exactly 20,000,000
    unit = 10_000
    era = 525_600
    four = sum(r * unit * era for r in (20, 10, 5)) + 25_000 * era
    assert four == 19_710_000 * unit
    last = 4 * era + (20_000_000 * unit - four) // 12_500
    assert load("emission")["sets"]["default"]["main_emission_end_from_1"] == last + 1 == 2_334_401


# ---------------------------------------------------------------- difficulty

def test_difficulty_scenarios():
    for sc in load("difficulty")["scenarios"]:
        p = sc["params"]
        bc = mv.fake_chain({**p, "start_target": int(p["start_target"])}, sc["timestamps"])
        ts, targets = bc._history()
        got = [str(t) for t in targets[1:]] + [str(bc.next_target())]
        assert got == sc["required_targets"], sc["name"]
        assert [bc._median_time(ts, pos) for pos in range(1, len(ts) + 1)] == sc["median_times"], sc["name"]


def test_difficulty_by_an_independent_reimplementation():
    # the LWMA written out again from the spec, straight from the vector data
    def retarget(ts, targets, pos, window, T, start):
        if window == 0:
            return start
        n = min(window, pos - 2)
        if n < 1:
            return start
        weighted = sum(max(1, min(6 * T, ts[pos - n + k - 1] - ts[pos - n + k - 2])) * k for k in range(1, n + 1))
        avg = sum(targets[pos - n:pos]) // n
        new = avg * weighted // (T * n * (n + 1) // 2)
        prev = targets[pos - 1]
        return max(1, min(2**256 - 1, max(prev // 4, min(prev * 4, new))))

    for sc in load("difficulty")["scenarios"]:
        p = sc["params"]
        start = int(p["start_target"])
        ts, targets = [0] + sc["timestamps"], [start]
        for pos in range(1, len(ts) + 1):
            new = retarget(ts, targets, pos, p["window"], p["block_time"], start)
            assert str(new) == sc["required_targets"][pos - 1], (sc["name"], pos)
            targets.append(new)


# ---------------------------------------------------------------- fees, block size, units

def test_minimum_fee():
    doc = load("fees_and_size")
    assert doc["constants"]["min_fee_rate_units_per_1000_bytes"] == chain_module.MIN_FEE_RATE_UNITS == 100
    for v in doc["min_fee"]:
        assert min_fee_for(v["size"]) == v["fee"] == max(1, -(-100 * v["size"] // 1000))


def test_the_oversize_penalty():
    for v in load("fees_and_size")["penalty"]:
        assert Blockchain.penalty(v["base"], v["size"], v["median"]) == v["penalty"]
        if v["size"] > v["median"] and v["base"] > 0:            # ceil(base * over^2 / median^2)
            over = v["size"] - v["median"]
            assert v["penalty"] == -(-v["base"] * over * over // (v["median"] ** 2))


def test_the_median_and_its_window():
    doc = load("fees_and_size")
    for v in doc["median"]:
        with mv.min_block_median(v["floor"]):
            assert Blockchain._median(v["sizes"]) == v["median"]
    hist = doc["median_history"]["sizes_by_position_0_is_genesis"]
    assert doc["constants"]["median_window"] == chain_module.MEDIAN_WINDOW == 10
    with mv.min_block_median(300_000):
        for v in doc["median_history"]["windowed"]:
            pos = v["pos"]
            assert Blockchain._median(hist[max(1, pos - 10):pos]) == v["median"]


def test_units():
    doc = load("units")
    for v in doc["parse"]:
        if v.get("error"):
            with pytest.raises(ValueError):
                to_units(v["text"])
        else:
            assert to_units(v["text"]) == v["units"], v["text"]
    for v in doc["format"]:
        assert fmt(v["units"]) == v["text"]


# ---------------------------------------------------------------- the legacy account model

def test_legacy_wallets_and_transactions():
    doc = load("legacy_account_model")
    for w in doc["wallets"]:
        wallet = Wallet(w["private_key"])
        assert (wallet.public_key, wallet.address) == (w["public_key"], w["address"])
        assert w["address"] == hashlib.sha256(h(w["public_key"])).hexdigest()[:40]     # address rule
    bc = Blockchain()
    for v in doc["transactions"]:
        tx = Transaction.from_dict(v["tx"])
        assert tx.payload().hex() == v["payload_hex"], v["name"]
        assert tx.size() == v["size"] and min_fee_for(tx.size()) == v["min_fee"], v["name"]
        assert tx.is_signature_valid() is v["signature_valid"], v["name"]
        problem = bc.tx_problem(tx)
        assert (problem is None) is v["acceptable"] and problem == v["problem"], v["name"]


def test_legacy_signatures_are_ecdsa_over_sha1():
    # documented in docs/KNOWN_ISSUES.md #2: a rewrite must not copy this
    from ecdsa import SECP256k1, VerifyingKey
    v = load("legacy_account_model")["transactions"][0]
    tx = Transaction.from_dict(v["tx"])
    vk = VerifyingKey.from_string(h(tx.public_key), curve=SECP256k1)
    assert vk.verify(h(tx.signature), tx.payload(), hashfunc=hashlib.sha1)
    with pytest.raises(Exception):
        vk.verify(h(tx.signature), tx.payload(), hashfunc=hashlib.sha256)


def test_legacy_block_headers_by_hand():
    for v in load("legacy_account_model")["blocks"]:
        # built here from the fields, without the Block class, to show the exact format
        header = json.dumps({"index": v["index"], "timestamp": v["timestamp"], "transactions": v["transactions"],
                             "previous_hash": v["previous_hash"]}, sort_keys=True).encode()
        assert header.hex() == v["header_bytes_hex"], v["name"]
        assert hashlib.sha256(header + str(v["nonce"]).encode()).hexdigest() == v["hash"], v["name"]
        block = Block.from_dict({k: v[k] for k in ("index", "timestamp", "transactions", "previous_hash", "nonce")})
        assert block.compute_hash() == v["hash"]
    genesis = load("legacy_account_model")["blocks"][0]
    assert genesis["hash"] == "310d91e47584e2c931967e372ab65e1a3e414e50b0792c86dd1989fc0ecbf207"


# ---------------------------------------------------------------- whole chains

def chain_cases():
    return load("chains")["cases"]


@pytest.mark.parametrize("case", chain_cases(), ids=lambda c: c["name"])
def test_chain_verdicts(case):
    got = mv.evaluate_chain_case(case)
    assert got["valid"] is case["expect_valid"]
    assert got["valid_without_pow_recheck"] is case["expect_valid_without_pow_recheck"]
    if case["expect_valid"]:
        assert got["balances"] == case["balances"] and got["supply"] == case["supply"]


def test_every_rule_case_breaks_exactly_one_thing():
    # the last block of each "rule:" chain is the only reason it fails: cut it off and the rest is valid
    rules = [c for c in chain_cases() if c["name"].startswith(("rule:", "size:")) and not c["expect_valid"]]
    assert len(rules) >= 15
    for case in rules:
        doc = copy.deepcopy(case["doc"])
        doc["chain"].pop()
        with mv.min_block_median(case["constants"].get("min_block_median", 300_000)):
            assert mv.load_chain(doc).is_valid(), case["name"]


def test_the_chain_cases_cover_the_rules_a_validator_must_check():
    names = " ".join(c["name"] for c in chain_cases())
    for rule in ("coinbase claims", "no transactions", "not from COINBASE", "more than the sender has", "fee below",
                 "wrong key", "does not match the sender", "positive", "twice", "median of the last 11",
                 "future", "target", "previous_hash", "parent's + 1", "penalty", "hard limit", "epoch",
                 "made-up mix"):
        assert rule in names, rule


def test_a_cheap_only_check_accepts_a_forged_mix_but_the_full_check_does_not():
    forged = [c for c in chain_cases() if "made-up mix" in c["name"]][0]
    assert forged["expect_valid_without_pow_recheck"] is True and forged["expect_valid"] is False
