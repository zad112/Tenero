"""Message signatures and payment proofs on Carrot (docs/WALLET_PROOFS.md): the reference against its committed vectors, and
the properties the construction promises, checked on random keys. The signatures are our own construction (unreviewed)."""
import json
import os
import random

from tools import make_vectors_proofs as p


def load():
    with open(os.path.join(p.VECTOR_DIR, "wallet_proofs.json"), encoding="utf-8") as f:
        return json.load(f)


def test_the_committed_proof_vectors_are_current(capsys):
    assert p.main(["--check"]) == 0, capsys.readouterr().out


def test_every_signature_verifies_and_every_refused_one_does_not():
    v = load()
    for c in v["signatures"]:
        assert p.verify_message(bytes.fromhex(c["spend"]), bytes.fromhex(c["view"]), bytes.fromhex(c["message"]),
                                bytes.fromhex(c["signature"]))
        assert c["text"].startswith("TENsig1")
    for c in v["refused_signatures"]:
        assert not p.verify_message(bytes.fromhex(c["spend"]), bytes.fromhex(c["view"]), bytes.fromhex(c["message"]),
                                    bytes.fromhex(c["signature"])), c["note"]


def test_a_signature_is_bound_to_the_message_and_both_keys_on_random_keys():
    rng = random.Random(20261008)
    for i in range(12):
        a, b = rng.randrange(1, p.L), rng.randrange(1, p.L)
        spend = p.encode(p.add(p.mul(a, p.G), p.mul(b, p.T)))
        view = p.encode(p.mul(rng.randrange(1, p.L), p.G))
        msg = rng.randbytes(rng.randrange(0, 40))
        sig = p.sign_message(a, b, spend, view, msg, rng.randbytes(32))
        assert p.verify_message(spend, view, msg, sig)
        assert not p.verify_message(spend, view, msg + b"!", sig)
        assert not p.verify_message(spend, p.encode(p.mul(3, p.G)), msg, sig)
        # knowing only one of the two secrets is not enough: the wrong b gives a signature that fails
        bad = p.sign_message(a, (b + 1) % p.L, spend, view, msg, rng.randbytes(32))
        assert not p.verify_message(spend, view, msg, bad)


def test_the_proofs_read_back_and_their_signatures_hold():
    v = load()
    for c in v["proofs"]:
        b = bytes.fromhex(c["bytes"])
        got = p.parse_proof(b)
        assert (got["address"], got["height"], got["onetime_address"], got["anchor"], got["signature"]) == \
            (c["address"], c["height"], c["onetime_address"], c["anchor"], c["signature"])
        assert p.proof_text(b) == c["text"]
        if c["signature"]:
            spend, view = p.A.decode(c["address"], c["network"])[1:3]
            bound = p.proof_bound(c["height"], bytes.fromhex(c["onetime_address"]), bytes.fromhex(c["anchor"]),
                                  bytes.fromhex(c["message"]))
            assert p.verify_raw(p.PAYMENT_DOMAIN, spend, view, bound, bytes.fromhex(c["signature"]))
    for c in v["invalid_proofs"]:
        try:
            p.parse_proof(bytes.fromhex(c["bytes"]))
        except ValueError:
            continue
        raise AssertionError(c["note"])
