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
