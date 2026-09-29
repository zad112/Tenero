"""The version 2 data model (docs/CONSENSUS_V2.md): the reference in tools/make_vectors_v2.py against its
committed vectors, and a few facts computed here independently of it (sizes, tags, an RFC 6962 check)."""
import hashlib
import json
import os
import struct

import pytest

from tools import make_vectors_v2 as v2

VEC = v2.VECTOR_DIR


def load(name):
    with open(os.path.join(VEC, name + ".json"), encoding="utf-8") as f:
        return json.load(f)


def test_the_committed_v2_vectors_are_current(capsys):
    assert v2.main(["--check"]) == 0, capsys.readouterr().out


def test_every_valid_object_round_trips_exactly():
    cases = load("v2_serialization")["valid"]
    assert len(cases) >= 14
    for c in cases:
        data = bytes.fromhex(c["hex"])
        assert v2.decode(c["kind"], data) == c["object"], c["note"]
        assert v2.encode(c["kind"], c["object"]) == data, c["note"]


def test_every_invalid_encoding_is_rejected_in_the_stated_way():
    cases = load("v2_serialization")["invalid"]
    assert len(cases) >= 20
    assert {c["error"] for c in cases} == set(v2.ERRORS)     # all four failure kinds are exercised
    for c in cases:
        with pytest.raises(v2.DecodeError) as e:
            v2.decode(c["kind"], bytes.fromhex(c["hex"]))
        assert e.value.kind == c["error"], c["note"]


def test_one_value_has_one_encoding():
    # decode -> encode reproduces any accepted input, and a re-encoding of a parsed value is never longer
    for c in load("v2_serialization")["valid"]:
        data = bytes.fromhex(c["hex"])
        assert v2.encode(c["kind"], v2.decode(c["kind"], data)) == data


def test_primitive_encodings_are_fixed_width_little_endian():
    for p in load("v2_serialization")["primitives"]:
        width = {"u8": 1, "u16": 2, "u32": 4, "u64": 8}[p["type"]]
        assert bytes.fromhex(p["hex"]) == p["value"].to_bytes(width, "little"), p


def test_object_sizes_are_what_the_document_says():
    assert len(v2.encode("output", v2.sample_output("s"))) == 123          # 32+32+8+3+32+16
    assert len(v2.encode("coinbase_output", v2.sample_cb_output("s", 1))) == 91   # 32+8+3+32+16
    assert len(v2.encode("header", v2.sample_header("s"))) == 146          # 2+32+8+32+8+64
    assert len(v2.HEADER_TAG) == 22


def test_the_header_hash_is_computed_the_documented_way():
    h = v2.sample_header("independent")
    manual = hashlib.sha256(b"tenero block header v2" + struct.pack("<H", 2) + bytes.fromhex(h["prev_id"])
                            + struct.pack("<Q", h["timestamp"]) + bytes.fromhex(h["tx_root"])).digest()
    assert v2.header_hash(h) == manual
    # the nonce and the mix are NOT in it, everything else is
    assert v2.header_hash({**h, "nonce": h["nonce"] + 1, "mix": "ff" * 64}) == manual
    for field, other in (("prev_id", "11" * 32), ("tx_root", "22" * 32), ("timestamp", h["timestamp"] + 1),
                         ("version", 3)):
        assert v2.header_hash({**h, field: other}) != manual, field


def test_the_block_id_commits_to_every_header_field():
    h = v2.sample_header("commit")
    base = v2.block_id(h, "matmul")
    for field, other in (("prev_id", "11" * 32), ("tx_root", "22" * 32), ("timestamp", h["timestamp"] + 1),
                         ("nonce", h["nonce"] + 1), ("mix", "33" * 64), ("version", 3)):
        assert v2.block_id({**h, field: other}, "matmul") != base, field
    z = {**h, "mix": "00" * 64}
    assert v2.block_id(z, "sha256") == hashlib.sha256(v2.header_hash(z) + struct.pack("<Q", z["nonce"])).digest()


def test_a_coinbase_and_a_transaction_never_share_an_id():
    same = load("v2_ids")["domain_separation"]
    assert same["as_transaction"] != same["as_coinbase"]


def test_the_merkle_root_matches_rfc_6962():
    def mth(items):                      # RFC 6962 section 2.1, written directly from the RFC
        n = len(items)
        if n == 0:
            return hashlib.sha256(b"").digest()
        if n == 1:
            return hashlib.sha256(b"\x00" + items[0]).digest()
        k = 1 << ((n - 1).bit_length() - 1)
        return hashlib.sha256(b"\x01" + mth(items[:k]) + mth(items[k:])).digest()

    for c in load("v2_merkle")["cases"]:
        leaves = [bytes.fromhex(x) for x in c["leaves"]]
        assert len(leaves) == c["n"]
        assert mth(leaves).hex() == c["root"], c["n"]
        assert v2.merkle_root(leaves).hex() == c["root"], c["n"]


def test_the_merkle_root_has_no_duplicate_leaf_or_leaf_node_confusion():
    p = load("v2_merkle")["properties"]
    assert p["not_padded"]["root_of_a_b_c"] != p["not_padded"]["root_of_a_b_c_c"]
    assert p["node_is_not_a_leaf"]["root_of_a_b"] != p["node_is_not_a_leaf"]["root_of_one_leaf_holding_node_a_b"]


def test_a_pruned_transaction_has_the_same_id_as_the_full_one():
    for c in load("v2_ids")["transactions"]:
        full = bytes.fromhex(c["bytes"])
        prefix = bytes.fromhex(c["prefix_bytes"])
        prunable = bytes.fromhex(c["prunable_bytes"])
        pruned = bytes.fromhex(c["pruned_bytes"])
        assert full == prefix + prunable                       # the full form is the prefix then the prunable part
        assert pruned == prefix + bytes.fromhex(c["prunable_hash"])
        # pruned = prefix + 32, full = prefix + prunable: pruning saves bytes once the prunable part exceeds
        # 32 bytes (with rings of 16 it is over 250 bytes even when the proof is empty)
        assert len(pruned) - len(full) == 32 - len(prunable)
        assert c["id"] == c["pruned_id"]
        # from scratch: id = SHA-256(tag, prefix, SHA-256(tag, prunable part)); the prunable part is one ring
        # per input (a u32 count, then that many u64 indexes) followed by the proof (u32 length, bytes)
        n_inputs = len(c["transaction"]["inputs"])
        at = 0
        assert struct.unpack("<I", prunable[at:at + 4])[0] == n_inputs
        at += 4
        for ring in c["transaction"]["rings"]:
            assert struct.unpack("<I", prunable[at:at + 4])[0] == len(ring)
            at += 4 + 8 * len(ring)
        proof = bytes.fromhex(c["transaction"]["proof_data"])
        assert struct.unpack("<I", prunable[at:at + 4])[0] == len(proof)
        assert prunable[at + 4:] == proof
        inner = hashlib.sha256(b"tenero tx prunable v2" + prunable).digest()
        assert inner.hex() == c["prunable_hash"]
        assert hashlib.sha256(b"tenero tx v2" + prefix + inner).hexdigest() == c["id"]


def test_the_prefix_holds_no_ring_and_the_id_still_covers_every_ring_index():
    t = v2.sample_tx("rings")
    prefix = v2.enc_tx_prefix(t)
    # the ring indexes are not in the prefix: 2 (version) + 4 + 2 * 32 (key images) + outputs + fee + extra
    assert len(prefix) == 2 + 4 + 2 * 32 + 4 + 2 * 123 + 8 + 4 + 24
    assert not any(v2.w_u64(i) in prefix for ring in t["rings"] for i in ring[:1])
    base = v2.tx_id(t)
    for which in (0, 1):
        changed = [list(r) for r in t["rings"]]
        changed[which][3] += 1                                     # one ring member differs
        assert v2.tx_id({**t, "rings": changed}) != base, which   # so a ring cannot be swapped under an id
    swapped = [t["rings"][1], t["rings"][0]]
    assert v2.tx_id({**t, "rings": swapped}) != base                # nor the rings exchanged between inputs
    assert v2.tx_id({**t, "rings": [t["rings"][0][:-1], t["rings"][1]]}) != base
    # the pruned form still has the same id, from the prefix and the hash alone
    assert v2.pruned_tx_id(v2.prune(t)) == base


def test_the_id_changes_with_any_proof_byte_and_any_prefix_byte():
    t = v2.sample_tx("flip")
    base = v2.tx_id(t)
    proof = bytearray(bytes.fromhex(t["proof_data"]))
    proof[len(proof) // 2] ^= 1
    assert v2.tx_id({**t, "proof_data": bytes(proof).hex()}) != base
    assert v2.tx_id({**t, "fee": t["fee"] + 1}) != base
    assert v2.tx_id({**t, "extra": "00" + t["extra"][2:]}) != base


def test_the_dynamic_minimum_fee_is_the_documented_formula():
    v = load("v2_fees")
    assert v["constants"]["FEE_REFERENCE_WEIGHT"] == 3000
    for c in v["dynamic_min_fee"]:
        exact = (c["base_reward"] * 3000 * c["size"] + c["median"] ** 2 - 1) // c["median"] ** 2   # ceil, by hand
        exact = max(1, exact)
        assert c["fee"] == (exact if exact < 2 ** 64 else None), c
    # the headline numbers used in the design document
    got = {(c["base_reward"], c["median"], c["size"]): c["fee"] for c in v["dynamic_min_fee"]}
    # the version 2 floor is 150,000 bytes
    assert v["constants"]["MIN_BLOCK_MEDIAN"] == 150_000
    assert got[(2_000_000_000, 150_000, 2500)] == 666_667                 # 0.00666667 coins
    assert got[(2_000_000_000, 150_000, 100_000)] == 26_666_667
    assert got[(2_000_000_000, 300_000, 2500)] == 166_667                 # the same rule at a busier median
    assert got[(2_000_000_000, 300_000, 300_000)] == 20_000_000


def test_the_v2_block_size_median_uses_the_150k_floor():
    v = load("v2_fees")
    floor = v["constants"]["MIN_BLOCK_MEDIAN"]
    assert floor == 150_000
    for c in v["median"]:
        s = sorted(c["sizes"])
        assert c["floor"] == floor
        assert c["median"] == max(floor, s[len(s) // 2] if s else 0), c       # by hand
    # the window is the 10 blocks before, from position 1 on, and an empty window gives the floor
    h = v["median_history"]
    sizes = h["sizes_by_position_0_is_genesis"]
    for w in h["windowed"]:
        window = sorted(sizes[max(1, w["pos"] - 10):w["pos"]])
        assert w["median"] == max(floor, window[len(window) // 2] if window else 0), w
    assert h["windowed"][0]["median"] == floor                                 # position 1: nothing before it


def test_the_v2_emission_is_the_v1_schedule_scaled_by_ten_thousand():
    v2s = load("v2_emission")["sets"]["default_8_decimals"]
    v1s = load("emission")["sets"]["default"]
    assert v2s["main_emission_end_from_1"] == v1s["main_emission_end_from_1"]
    assert v2s["params"]["max_supply"] == v1s["params"]["max_supply"] * 10 ** 4
    by_height = {r["height"]: r for r in v1s["rows"]}
    checked = 0
    for r in v2s["rows"]:
        old = by_height.get(r["height"])
        if old is None:
            continue
        checked += 1
        for key in ("scheduled", "issued_before", "main_reward", "reward"):
            assert r[key] == old[key] * 10 ** 4, (r["height"], key)
        assert r["in_tail"] == old["in_tail"]
    assert checked >= 8                      # they share enough heights for this to mean something
    assert v2s["params"]["max_supply"] < 2 ** 64 // 9000                 # room to spare in a u64
    assert 20_000_000 * 10 ** 12 > 2 ** 64                             # why 12 decimals were not chosen


def test_genesis_and_the_chain_id():
    cases = load("v2_genesis")["cases"]
    assert cases[0]["label"] == v2.NETWORK_LABEL
    assert len({c["chain_id"] for c in cases}) == len(cases)          # a different label is a different chain
    for c in cases:
        h = c["header"]
        assert (h["version"], h["prev_id"], h["timestamp"], h["nonce"], h["mix"]) == (2, "00" * 32, 0, 0, "00" * 64)
        assert h["tx_root"] == hashlib.sha256(b"tenero genesis" + c["label"].encode()).hexdigest()
        assert c["chain_id"] == c["genesis_id"] == hashlib.sha256(
            b"tenero genesis id v2" + bytes.fromhex(c["header_bytes"])).hexdigest()


def test_the_reference_is_strict_where_it_matters():
    tx = v2.sample_tx("strict")
    data = v2.encode("transaction", tx)
    for cut in (0, 1, 2, len(data) // 2, len(data) - 1):
        with pytest.raises(v2.DecodeError):
            v2.decode("transaction", data[:cut])
    with pytest.raises(v2.DecodeError):
        v2.decode("transaction", data + b"\x00")
