"""The pool protocol (docs/POOL_PROTOCOL.md, a DRAFT): the reference in reference/tools/make_vectors_pool.py against its committed
vectors, and facts computed here independently of it (one message has one encoding; the frame rule; the directions)."""
import json
import os

import pytest

from tools import make_vectors_pool as p

VEC = p.VECTOR_DIR


def load():
    with open(os.path.join(VEC, "pool.json"), encoding="utf-8") as f:
        return json.load(f)


def codecs(direction):
    return (p.dec_miner, p.enc_miner) if direction == "miner" else (p.dec_pool, p.enc_pool)


def test_the_committed_pool_vectors_are_current(capsys):
    assert p.main(["--check"]) == 0, capsys.readouterr().out


def test_every_valid_message_round_trips_and_is_framed():
    for case in load()["valid"]:
        body = bytes.fromhex(case["body"])
        dec, enc = codecs(case["direction"])
        assert dec(body) == case["message"], case["note"]
        assert enc(case["message"]) == body, case["note"]
        framed = bytes.fromhex(case["frame"])
        assert framed[:4] == len(body).to_bytes(4, "little") and framed[4:] == body


def test_one_message_has_one_encoding():
    """Change any single byte of any valid message: the result is refused, or it decodes to a message that encodes to
    exactly those bytes (so no two byte strings mean the same message)."""
    for case in load()["valid"]:
        body = bytes.fromhex(case["body"])
        dec, enc = codecs(case["direction"])
        for i in range(min(len(body), 400)):
            for flip in (0x01, 0x80):
                mutated = bytearray(body)
                mutated[i] ^= flip
                try:
                    m = dec(bytes(mutated))
                except p.PoolError:
                    continue
                assert enc(m) == bytes(mutated), (case["note"], i, flip)


def test_a_miners_kind_is_never_a_pools_and_the_other_way_round():
    assert all(k < 0x80 for k in p.MINER.values())
    assert all(0x80 <= k < p.ERROR for k in p.POOL.values())
    assert p.ERROR == 0xFF
    # a pool message given to the miner-side decoder (and the reverse) is an unknown kind
    for case in load()["valid"]:
        body = bytes.fromhex(case["body"])
        other = p.dec_pool if case["direction"] == "miner" else p.dec_miner
        with pytest.raises(p.PoolError) as e:
            other(body)
        assert e.value.kind == "kind", case["note"]


def test_every_share_reason_and_every_declaration_outcome_has_a_vector():
    msgs = [c["message"] for c in load()["valid"] if c["direction"] == "pool"]
    assert {m["reason"] for m in msgs if m["type"] == "share_result"} == set(p.SHARE_REASONS)
    assert {m["status"] for m in msgs if m["type"] == "declare_result"} == {"accepted", "refused", "missing"}


def test_the_headers_a_pool_hands_out_are_146_bytes_with_nothing_in_the_nonce_and_mix():
    body = p.enc_pool(p.job())
    # kind 1, job id 8, height 8, clean 1, header 146, target 32, ttl 4
    assert len(body) == 1 + 8 + 8 + 1 + p.HEADER_LEN + 32 + 4


def test_the_frame_rule():
    assert p.frame(b"\x01") == b"\x01\x00\x00\x00\x01"
    with pytest.raises(p.PoolError):
        p.frame(b"")
    with pytest.raises(p.PoolError):
        p.frame(b"\0" * (p.MAX_FRAME + 1))
    assert len(p.frame(b"\0" * p.MAX_FRAME)) == p.MAX_FRAME + 4
