"""The peer-to-peer wire protocol (docs/WIRE_PROTOCOL.md): the reference in tools/make_vectors_wire.py against
its committed vectors, and a few facts computed here independently of it (the sizes in the document, the check
order, one message one encoding)."""
import json
import os
import random
import re
import struct

from tools import make_vectors_wire as w

VEC = w.VECTOR_DIR


def load():
    with open(os.path.join(VEC, "v2_wire.json"), encoding="utf-8") as f:
        return json.load(f)


def test_the_committed_wire_vectors_are_current(capsys):
    assert w.main(["--check"]) == 0, capsys.readouterr().out


def test_every_valid_message_round_trips_exactly():
    for c in load()["valid"]:
        frame = bytes.fromhex(c["frame"])
        assert w.encode(c["message"]) == frame, c["note"]
        assert w.decode_frame(frame) == c["message"], c["note"]


def test_every_invalid_frame_is_rejected_in_the_stated_way():
    cases = load()["invalid"]
    assert len(cases) >= 30
    for c in cases:
        try:
            w.decode_frame(bytes.fromhex(c["frame"]))
        except w.WireError as e:
            assert e.kind == c["error"], c["note"]
        else:
            raise AssertionError(f"accepted: {c['note']}")


def test_the_early_cases_are_what_a_stream_decoder_can_know():
    for c in load()["early"]:
        assert w.early_error(bytes.fromhex(c["prefix"])) == c["error"], c["note"]


def test_the_frame_caps_are_the_ones_the_document_states():
    doc = open(os.path.join(os.path.dirname(w.ROOT + "/x"), "docs", "WIRE_PROTOCOL.md"), encoding="utf-8").read()
    stated = {"hello": 117, "ping": 9, "pong": 9, "new_block": 73, "get_block_ids": 1029, "get_blocks": 1029,
              "not_found": 2053, "new_tx": 2053, "get_txs": 2053, "block_ids": 16013, "blocks": 16_777_216,
              "txs": 16_777_216}
    assert w.CAPS == stated
    # the document's own numbers agree with the table above
    for number in ("117", "1029", "2053", "16013", "16,777,216"):
        assert number in doc


def test_every_cap_is_what_the_layout_implies():
    # kind byte + the fixed fields (+ count + the largest list)
    assert w.CAPS["hello"] == 1 + 4 + 32 + 8 + 32 + 32 + 8 == 117
    assert w.CAPS["block_ids"] == 1 + 8 + 4 + 500 * 32
    assert w.CAPS["get_block_ids"] == 1 + 4 + 32 * 32
    assert w.CAPS["new_tx"] == 1 + 4 + 64 * 32
    assert w.CAPS["new_block"] == 1 + 32 + 8 + 32


def test_a_frame_is_length_kind_body_in_little_endian():
    f = w.encode({"kind": "ping", "nonce": 0x0102030405060708})
    assert f == struct.pack("<I", 9) + b"\x02" + bytes([8, 7, 6, 5, 4, 3, 2, 1])


def test_the_checks_run_in_the_documented_order():
    u32 = lambda n: struct.pack("<I", n)
    # an unknown kind beats a too-large length; a too-large length beats a short frame
    assert w.early_error(u32(0xFFFFFFFF) + b"\x00") == "unknown kind"
    assert w.early_error(u32(0xFFFFFFFF) + b"\x02") == "frame too large"
    for data, want in [(u32(0) + b"\x02", "empty frame"), (b"\x01\x00\x00", "short frame"),
                       (u32(10) + b"\x02", "frame too large")]:
        try:
            w.decode_frame(data)
        except w.WireError as e:
            assert e.kind == want
        else:
            raise AssertionError(data)


def test_one_message_has_one_encoding_under_random_damage():
    rng = random.Random(20260930)
    corpus = [bytes.fromhex(c["frame"]) for c in load()["valid"] if len(c["frame"]) < 12000]
    accepted = 0
    for _ in range(4000):
        f = bytearray(rng.choice(corpus))
        op = rng.randrange(4)
        if op == 0:
            f[rng.randrange(len(f))] ^= 1 << rng.randrange(8)
        elif op == 1:
            f[rng.randrange(len(f))] = rng.randrange(256)
        elif op == 2:
            del f[rng.randrange(len(f)):]
        else:
            f += bytes(rng.randrange(256) for _ in range(rng.randrange(1, 4)))
        try:
            m = w.decode_frame(bytes(f))
        except (w.WireError, ValueError):
            continue
        accepted += 1
        assert w.encode(m) == bytes(f), "two encodings of one message"
    assert accepted > 100


def test_the_encoder_refuses_what_the_decoder_would_refuse():
    for m in [{"kind": "get_block_ids", "locator": ["00" * 32] * 33},
              {"kind": "block_ids", "first_height": 1, "ids": ["00" * 32] * 501},
              {"kind": "get_blocks", "ids": ["00" * 32] * 33},
              {"kind": "new_tx", "ids": ["00" * 32] * 65}]:
        try:
            w.encode(m)
        except ValueError:
            continue
        raise AssertionError(m["kind"])


def test_the_vector_file_lists_every_kind_and_every_error():
    v = load()
    assert sorted(v["kinds"].values()) == sorted(w.KINDS.values())
    kinds = {c["message"]["kind"] for c in v["valid"]}
    assert kinds == set(w.KINDS.values())
    errors = {c["error"] for c in v["invalid"]}
    assert errors == set(w.ERRORS), errors ^ set(w.ERRORS)
