"""The reference for the wallet's message signatures and payment proofs (crates/tenero-wallet/src/proofs.rs) and its vectors.

An independent implementation, in Python and the standard library only, built on the Ed25519 arithmetic and the INTERIM scheme
of `make_vectors_interim.py` (which shares no code with the Rust side either). **A test reference, not a wallet: it handles real
secrets in plain integers and is not constant-time.** These are small, standard constructions (a Schnorr signature; a
Chaum-Pedersen proof that two points have the same discrete logarithm) put together for this project: **unaudited**, and nothing
here is "proof" in a legal or financial sense. See docs/WALLET_PROOFS.md.

    python reference/tools/make_vectors_proofs.py --check     do the committed vectors match the reference?
    python reference/tools/make_vectors_proofs.py --write     regenerate them (a CHANGE OF THE PROOF FORMAT: explain it in the commit)

Nothing here is random and nothing depends on the clock: the random 32 bytes a signer draws are fixed inputs.
"""
import argparse
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import make_vectors_interim as I  # noqa: E402

ROOT = I.ROOT
VECTOR_DIR = I.VECTOR_DIR
NAME = "wallet_proofs"
L, G, encode, strict, mul, add = I.L, I.G, I.encode, I.strict, I.mul, I.add

SIG_PREFIX = "tnsig1"
PROOF_PREFIX = "tnpay1"
KIND_RECEIVED, KIND_SENT, KIND_KEY = 1, 2, 3


def le32(n):
    return (n % L).to_bytes(32, "little")


def scalar_of(b):
    """A canonical scalar (< L) or None."""
    n = int.from_bytes(b, "little")
    return n if n < L else None


# ---- messages --------------------------------------------------------------------------------------------
def message_hash(spend_pub, view_pub, message):
    return I.digest(b"message", spend_pub, view_pub, len(message).to_bytes(8, "little"), message)


def sign_message(seed, message, rnd):
    spend, _ = I.keys_of(seed)
    sp, vp = I.address_bytes(seed)
    h = message_hash(sp, vp, message)
    k = I.hs(b"sig nonce", le32(spend), h, rnd)
    r_pub = encode(mul(k, G))
    c = I.hs(b"sig challenge", r_pub, sp, h)
    z = (k + c * spend) % L
    return r_pub + le32(z)


def verify_message(spend_pub, view_pub, message, sig):
    if len(sig) != 64 or strict(spend_pub) is None or strict(view_pub) is None:
        return False
    r_pub, z_b = sig[:32], sig[32:]
    r_pt = strict(r_pub)
    z = scalar_of(z_b)
    if r_pt is None or z is None:
        return False
    h = message_hash(spend_pub, view_pub, message)
    c = I.hs(b"sig challenge", r_pub, spend_pub, h)
    return mul(z, G) == add(r_pt, mul(c, strict(spend_pub)))


# ---- payment proofs --------------------------------------------------------------------------------------
def binding(kind, height, global_index, spend_pub, view_pub, de):
    return bytes([kind]) + height.to_bytes(8, "little") + global_index.to_bytes(8, "little") + spend_pub + view_pub + de


def neg(pt):
    x, y = pt
    return ((-x) % I.P, y)


def prove(kind, x, b2_pt, p2_pt, bind, rnd):
    """`x` with P1 = x*G and P2 = x*B2; returns (D, c, z) as bytes."""
    k = I.hs(b"pay nonce", le32(x), bind, rnd)
    a1 = encode(mul(k, G))
    a2 = encode(mul(k, b2_pt))
    d = encode(p2_pt)
    c = I.hs(b"pay challenge", bind, d, a1, a2)
    z = (k + c * x) % L
    return d + le32(c) + le32(z)


def statement(kind, spend_pub, view_pub, de_b):
    """(P1, B2) for the proof kind: received: P1 = K_view, B2 = De; sent: P1 = De, B2 = K_view."""
    k_view = strict(view_pub)
    de = strict(de_b)
    if kind == KIND_RECEIVED:
        return k_view, de
    return de, k_view


def check_dleq(kind, bind, spend_pub, view_pub, de_b, body):
    d_b, c_b, z_b = body[:32], body[32:64], body[64:96]
    d = strict(d_b)
    c, z = scalar_of(c_b), scalar_of(z_b)
    if d is None or c is None or z is None:
        return None
    p1, b2 = statement(kind, spend_pub, view_pub, de_b)
    if p1 is None or b2 is None:
        return None
    a1 = add(mul(z, G), neg(mul(c, p1)))
    a2 = add(mul(z, b2), neg(mul(c, d)))
    if I.hs(b"pay challenge", bind, d_b, encode(a1), encode(a2)) != c:
        return None
    return encode(mul(8, d))


def recompute(spend_pub, view_pub, out, s_bytes):
    """What the shared secret says about the output: its amount, or None if it is not addressed to this address."""
    ctx, index = bytes.fromhex(out["context"]), out["index"]
    i = index.to_bytes(4, "little")
    t = I.hs(b"onetime", s_bytes, ctx, i)
    if encode(add(mul(t, G), strict(spend_pub))) != bytes.fromhex(out["onetime_address"]):
        return None
    if out["coinbase"]:
        return out["public_amount"]
    mask = I.hs(b"mask", s_bytes, ctx, i)
    amount = int.from_bytes(I.xor(bytes.fromhex(out["amount_enc"]), I.digest(b"amount", s_bytes, ctx, i)[:8]), "little")
    if I.commit(mask, amount) != bytes.fromhex(out["amount_commitment"]):
        return None
    return amount


def make_proof(kind, seed_or_r, spend_pub, view_pub, height, global_index, out, rnd):
    """kind 1: `seed_or_r` is the RECEIVER's seed; kinds 2 and 3: the sender's secret r (an int)."""
    de_b = bytes.fromhex(out["ephemeral_pubkey"])
    de = strict(de_b)
    k_view = strict(view_pub)
    bind = binding(kind, height, global_index, spend_pub, view_pub, de_b)
    if kind == KIND_RECEIVED:
        _, v = I.keys_of(seed_or_r)
        body = prove(kind, v, de, mul(v, de), bind, rnd)
    elif kind == KIND_SENT:
        r = seed_or_r
        body = prove(kind, r, k_view, mul(r, k_view), bind, rnd)
    else:
        body = le32(seed_or_r)
    return bytes([kind]) + height.to_bytes(8, "little") + global_index.to_bytes(8, "little") + spend_pub + view_pub + body


def check_proof(proof, out):
    """The amount the proof shows, or None. `out` is the chain's output (as a dict of hex fields)."""
    if len(proof) < 1 + 8 + 8 + 64:
        return None
    kind = proof[0]
    height = int.from_bytes(proof[1:9], "little")
    gi = int.from_bytes(proof[9:17], "little")
    spend_pub, view_pub = proof[17:49], proof[49:81]
    body = proof[81:]
    if strict(spend_pub) is None or strict(view_pub) is None:
        return None
    de_b = bytes.fromhex(out["ephemeral_pubkey"])
    if strict(de_b) is None:
        return None
    if kind in (KIND_RECEIVED, KIND_SENT):
        if len(body) != 96:
            return None
        bind = binding(kind, height, gi, spend_pub, view_pub, de_b)
        s_bytes = check_dleq(kind, bind, spend_pub, view_pub, de_b, body)
    elif kind == KIND_KEY:
        if len(body) != 32:
            return None
        r = scalar_of(body)
        if r is None or encode(mul(r, G)) != de_b:
            return None
        s_bytes = encode(mul(8, mul(r, strict(view_pub))))
    else:
        return None
    if s_bytes is None:
        return None
    return recompute(spend_pub, view_pub, out, s_bytes)


# ---- the vectors -----------------------------------------------------------------------------------------
def det(label, n):
    return I.det_bytes(b"proofs " + label, n)


def seed_of(label):
    return det(b"seed " + label, 32)


def message_cases():
    out = []
    for note, label, msg in [
        ("a short message", b"alice", b"hello"),
        ("the empty message", b"alice", b""),
        ("a message with every byte value", b"bob", bytes(range(256))),
        ("non-ASCII text", b"carol", "Tenero: 試験 ✓".encode()),
    ]:
        seed = seed_of(label)
        sp, vp = I.address_bytes(seed)
        rnd = det(b"rnd " + label + msg[:4], 32)
        sig = sign_message(seed, msg, rnd)
        assert verify_message(sp, vp, msg, sig)
        out.append({"note": note, "seed": seed.hex(), "address": I.address_text(sp, vp), "message": msg.hex(),
                    "rnd": rnd.hex(), "signature": sig.hex()})
    return out


def output_cases():
    """Outputs made by the interim scheme, for the proofs to be about."""
    cases = []
    specs = [
        ("an ordinary output", b"alice", 123456789, ("tx", det(b"ki a", 32)), 0, False, 5000, 17),
        ("the second output of a transaction", b"bob", 7, ("tx", det(b"ki b", 32)), 1, False, 5001, 18),
        ("a block reward", b"alice", 4000000000, ("coinbase", 1000), 0, True, 1000, 3),
    ]
    for note, label, amount, ctx_spec, index, coinbase, height, gi in specs:
        seed = seed_of(label)
        sp, vp = I.address_bytes(seed)
        ctx = I.tx_context(ctx_spec[1]) if ctx_spec[0] == "tx" else I.coinbase_context(ctx_spec[1])
        rng = det(b"enote rng " + label + bytes([index]), 80)
        e = I.make_enote(sp, vp, amount, ctx, index, coinbase, rng)
        r = int.from_bytes(rng[:64], "little") % L
        out = {k: e[k] for k in ("onetime_address", "amount_commitment", "amount_enc", "view_tag", "ephemeral_pubkey")}
        out.update({"context": ctx.hex(), "index": index, "coinbase": coinbase, "public_amount": amount if coinbase else 0})
        cases.append((note, seed, sp, vp, amount, height, gi, out, r))
    return cases


def proof_cases():
    out = []
    for note, seed, sp, vp, amount, height, gi, o, r in output_cases():
        for kind, name in [(KIND_RECEIVED, "received"), (KIND_SENT, "sent"), (KIND_KEY, "key")]:
            rnd = det(b"proof rnd " + name.encode() + bytes([gi]), 32)
            proof = make_proof(kind, seed if kind == KIND_RECEIVED else r, sp, vp, height, gi, o, rnd)
            shown = check_proof(proof, o)
            assert shown == amount, (note, name, shown, amount)
            out.append({"note": f"{note}: {name}", "kind": kind, "receiver_seed": seed.hex(), "tx_secret": le32(r).hex(),
                        "address": I.address_text(sp, vp), "height": height, "global_index": gi, "output": o,
                        "rnd": rnd.hex(), "proof": proof.hex(), "amount": amount})
    return out


def build():
    return {
        "schema": 1,
        "name": NAME,
        "description": "Message signatures and payment proofs of the INTERIM wallet scheme (crates/tenero-wallet/src/proofs.rs, "
                       "docs/WALLET_PROOFS.md). UNAUDITED. An independent Python reference (reference/tools/make_vectors_proofs.py).",
        "messages": message_cases(),
        "proofs": proof_cases(),
    }


def jdump(obj):
    return json.dumps(obj, indent=1, sort_keys=True) + "\n"


def path_of():
    return os.path.join(VECTOR_DIR, NAME + ".json")


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--check", action="store_true")
    ap.add_argument("--write", action="store_true")
    args = ap.parse_args(argv)
    if not (args.check or args.write):
        ap.print_help()
        return 1
    text = jdump(build())
    path = path_of()
    if args.check:
        same = os.path.exists(path) and open(path).read() == text
        print(f"{'ok      ' if same else 'DIFFERS '}{NAME}")
        return 0 if same else 1
    os.makedirs(VECTOR_DIR, exist_ok=True)
    with open(path, "w", newline="\n") as f:
        f.write(text)
    print(f"wrote {os.path.relpath(path, ROOT)}  ({len(text) / 1024:.0f} KiB)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
