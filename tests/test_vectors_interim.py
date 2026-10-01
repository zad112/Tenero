"""The INTERIM wallet output scheme (crates/tenero-wallet/src/interim.rs): the reference in
tools/make_vectors_interim.py against its committed vectors, and some facts computed independently of it."""
import json
import os

from tools import make_vectors_interim as w

VEC = w.VECTOR_DIR


def load():
    with open(os.path.join(VEC, "interim_scheme.json"), encoding="utf-8") as f:
        return json.load(f)


def test_the_committed_interim_vectors_are_current(capsys):
    assert w.main(["--check"]) == 0, capsys.readouterr().out


def test_the_curve_constants_are_right():
    # the base point has the group order, and H (the amount generator) is a valid prime-order point
    assert w.mul(w.L, w.G) == w.IDENTITY
    assert w.mul(w.L, w.H) == w.IDENTITY
    assert w.G != w.H
    assert w.encode(w.G).hex() == "5866666666666666666666666666666666666666666666666666666666666666"


def test_a_receiver_recomputes_the_shared_secret_from_its_view_key():
    # 8 * r * K_v (the sender's side) equals 8 * v * De (the receiver's), the whole point of the exchange
    seed = w.det_bytes(b"seed alice", 32)
    _, view = w.keys_of(seed)
    r = 123456789123456789
    k_view = w.mul(view, w.G)
    de = w.mul(r, w.G)
    assert w.mul(8, w.mul(r, k_view)) == w.mul(8, w.mul(view, de))


def test_every_enote_vector_is_internally_consistent():
    for c in load()["enotes"]:
        e = c["enote"]
        # the commitment opens to the amount and the mask
        mask = int.from_bytes(bytes.fromhex(e["mask"]), "little")
        assert w.commit(mask, c["amount"]).hex() == e["amount_commitment"], c["note"]
        if c["coinbase"]:
            assert mask == 1 and e["amount_enc"] == "00" * 8, c["note"]
        # the receiver's secret opens the one-time address
        secret = int.from_bytes(bytes.fromhex(c["onetime_secret"]), "little")
        assert w.encode(w.mul(secret, w.G)).hex() == e["onetime_address"], c["note"]
        # and every point in the output is canonical and of prime order
        for field in ("onetime_address", "amount_commitment", "ephemeral_pubkey"):
            assert w.strict(bytes.fromhex(e[field])) is not None, (c["note"], field)


def test_no_two_vectors_share_randomness():
    rngs = [c["rng"] for c in load()["enotes"]]
    assert len(set(rngs)) == len(rngs)
