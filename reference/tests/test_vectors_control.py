"""The control protocol (docs/CONTROL_PROTOCOL.md): the reference in reference/tools/make_vectors_control.py against its committed
vectors, and facts computed here independently of it (one message has one encoding; the frame rule)."""
import json
import os

import pytest

from tools import make_vectors_control as c

VEC = c.VECTOR_DIR


def load():
    with open(os.path.join(VEC, "control.json"), encoding="utf-8") as f:
        return json.load(f)


def test_the_committed_control_vectors_are_current(capsys):
    assert c.main(["--check"]) == 0, capsys.readouterr().out


def test_every_valid_message_round_trips_and_is_framed():
    for case in load()["valid"]:
        body = bytes.fromhex(case["body"])
        dec, enc = (c.dec_request, c.enc_request) if case["direction"] == "request" else (c.dec_response, c.enc_response)
        assert dec(body) == case["message"], case["note"]
        assert enc(case["message"]) == body, case["note"]
        framed = bytes.fromhex(case["frame"])
        assert framed[:4] == len(body).to_bytes(4, "little") and framed[4:] == body


def test_one_message_has_one_encoding():
    """Change any single byte of any valid message: the result is refused, or it decodes to a message that encodes to
    exactly those bytes (so no two byte strings mean the same message)."""
    for case in load()["valid"]:
        body = bytes.fromhex(case["body"])
        dec, enc = (c.dec_request, c.enc_request) if case["direction"] == "request" else (c.dec_response, c.enc_response)
        for i in range(min(len(body), 300)):
            for flip in (0x01, 0x80):
                mutated = bytearray(body)
                mutated[i] ^= flip
                try:
                    m = dec(bytes(mutated))
                except c.ControlError:
                    continue
                assert enc(m) == bytes(mutated), (case["note"], i, flip)


def test_requests_and_answers_pair_up():
    assert set(c.REQUESTS) - {"submit_tx", "key_image_spent"} >= {"tip", "block", "blocks", "info", "stop"}
    for name, kind in c.RESPONSES.items():
        # every answer kind is a request kind with the top bit set (the answer to key_image_spent is "spent")
        assert kind in c.REQUESTS.values()
    assert c.ERROR == 0xFF and all(k | c.ANSWER != c.ERROR for k in c.REQUESTS.values())


def test_the_frame_rule():
    assert c.frame(b"\x01") == b"\x01\x00\x00\x00\x01"
    with pytest.raises(c.ControlError):
        c.frame(b"")
    with pytest.raises(c.ControlError):
        c.frame(b"\0" * (c.MAX_FRAME + 1))
    assert len(c.frame(b"\0" * c.MAX_FRAME)) == c.MAX_FRAME + 4


def test_the_genesis_block_is_a_valid_scan_block_with_no_coinbase_outputs():
    g = c.genesis_scan_block()
    assert g["coinbase"]["outputs"] == [] and g["txs"] == []
    body = c.enc_response({"type": "block", "block": g})
    assert c.dec_response(body)["block"] == g
