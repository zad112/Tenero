"""The version 3 (gamma) data model (docs/CONSENSUS_V2.md section 15): the reference in reference/tools/make_vectors_v3.py
against its committed vectors, and facts computed here independently of it (byte layouts built by hand, the weight
formula, the tree schedule's meaning)."""
import hashlib
import json
import os
import struct

import pytest

from tools import make_vectors_v3 as v3

VEC = v3.VECTOR_DIR


def load(name):
    with open(os.path.join(VEC, name + ".json"), encoding="utf-8") as f:
        return json.load(f)


def test_the_committed_v3_vectors_are_current(capsys):
    assert v3.main(["--check"]) == 0, capsys.readouterr().out


def test_every_valid_object_round_trips_and_every_invalid_one_fails_as_stated():
    f = load("v3_serialization")
    for c in f["valid"]:
        data = bytes.fromhex(c["hex"])
        assert v3.decode(c["kind"], data) == c["object"], c["note"]
        assert v3.encode(c["kind"], c["object"]) == data, c["note"]
    for c in f["invalid"]:
        with pytest.raises(v3.DecodeError) as e:
            v3.decode(c["kind"], bytes.fromhex(c["hex"]))
        assert e.value.kind == c["error"], c["note"]


def test_a_2_output_transaction_by_hand():
    # built here from the layout in the docstring, not with the reference's encoder
    t = v3.sample_tx("hand")
    ki = [bytes.fromhex(i["key_image"]) for i in t["inputs"]]
    outs = b"".join(bytes.fromhex(o["onetime_address"] + o["amount_commitment"] + o["amount_enc"] + o["view_tag"]
                                  + o["anchor_enc"]) for o in t["outputs"])
    prefix = (struct.pack("<H", 3) + struct.pack("<I", 2) + b"".join(ki) + struct.pack("<I", 2) + outs
              + bytes.fromhex(t["ephemeral_pubkeys"][0]) + struct.pack("<Q", t["fee"])
              + bytes.fromhex(t["encrypted_payment_id"]))
    proof = bytes.fromhex(t["proof_data"])
    prunable = struct.pack("<Q", t["reference_height"]) + struct.pack("<I", len(proof)) + proof
    assert v3.enc_tx(t) == prefix + prunable
    assert len(outs) == 2 * 91
    assert len(t["ephemeral_pubkeys"]) == 1
    ph = hashlib.sha256(b"tenero tx prunable v3" + prunable).digest()
    assert v3.tx_id(t) == hashlib.sha256(b"tenero tx v3" + prefix + ph).digest()
    assert v3.tx_weight(t) == len(prefix) + -(-len(prunable) // 4)


def test_the_ephemeral_key_count_follows_the_outputs():
    assert [v3.n_ephemeral_keys(n) for n in range(2, 17)] == [1] + list(range(3, 17))


def test_weight_rounds_up_and_a_typical_v3_transaction_weighs_less_than_a_v2_one():
    for c in load("v3_weight")["transactions"]:
        assert c["weight"] == c["prefix_size"] + -(-c["prunable_size"] // 4)
        assert c["size"] == c["prefix_size"] + c["prunable_size"]
    typical = load("v3_weight")["transactions"][0]          # 2 in, 2 out, a 3-layer FCMP++ proof's size
    assert typical["size"] > 6_000 and typical["weight"] < 2_400


def test_a_block_is_limited_by_weight_and_by_real_bytes():
    f = load("v3_weight")
    assert f["max_block_weight"] == 12 * 1024 * 1024 and f["max_block_bytes"] == 48 * 1024 * 1024
    for c in f["limits"]:
        assert c["too_large"] == (c["weight"] > min(2 * c["median"], f["max_block_weight"])
                                  or c["size"] > f["max_block_bytes"])
    # about 100 typical transactions a second at the ceiling (60-second blocks), whichever limit binds
    typical = f["transactions"][0]
    per_block = min(f["max_block_weight"] // typical["weight"], f["max_block_bytes"] // typical["size"])
    assert 95 <= per_block / 60 <= 110


def test_the_medians_follow_their_definition_written_again_here():
    f = load("v3_median")
    floor, short_window, multiple = f["min_block_median"], f["median_window"], f["short_term_multiple"]
    num, den = f["long_term_growth"]

    def upper_median(xs):
        return sorted(xs)[len(xs) // 2]

    for c in f["cases"]:
        window, weights, lts = c["window"], [], []
        for d, b in zip(c["demand"], c["blocks"]):
            recent = lts[-window:]
            ltm = max(floor, upper_median(recent + [floor] * (window - len(recent))))
            short = max(floor, upper_median(weights[-short_window:]) if weights else floor)
            median = min(short, multiple * ltm)
            weight = min(d, 2 * median, 12 * 1024 * 1024)
            lt = min(weight, ltm * num // den)
            assert (b["median"], b["long_term_median"], b["weight"], b["long_term_weight"]) == (median, ltm, weight, lt)
            weights.append(weight)
            lts.append(lt)


def test_every_output_enters_the_tree_once_and_exactly_when_it_becomes_spendable():
    f = load("v3_tree_schedule")
    blocks = [(b["coinbase_outputs"], b["other_outputs"]) for b in f["blocks"]]
    entered_at = {}
    for height, idx in enumerate(f["entering"]):
        for i in idx:
            assert i not in entered_at
            entered_at[i] = height
    n = 0
    for h, (cb, other) in enumerate(blocks):
        for k in range(cb + other):
            wait = 60 if k < cb else 10
            spendable_in = h + wait
            # it enters when block (spendable_in - 1) is applied, if the chain gets that far
            if spendable_in - 1 < len(blocks):
                assert entered_at[n] == spendable_in - 1, (h, k)
            else:
                assert n not in entered_at
            n += 1
    assert f["tree_leaves_after"][-1] == len(entered_at)


def test_the_three_networks_have_different_chain_ids_and_differ_from_version_2():
    ids = {c["label"]: c["chain_id"] for c in load("v3_genesis")["cases"]}
    assert len(set(ids.values())) == 3
    v2_ids = {c["chain_id"] for c in load("v2_genesis")["cases"]}
    assert not set(ids.values()) & v2_ids


def test_shape_cases_say_why():
    f = load("v3_shape")
    assert [c["error"] is None for c in f["transactions"]].count(True) == 2
    for c in f["transactions"]:
        t = v3.decode("transaction", bytes.fromhex(c["hex"]))
        assert v3.shape_error(t) == c["error"], c["note"]
    for c in f["reference"]:
        assert v3.reference_ok(c["reference_height"], c["block_height"], c["tree_leaves"]) == c["ok"]
