"""The reference for the wallet's message signatures and payment proofs on Carrot (crates/tenero-wallet/src/proofs.rs,
docs/WALLET_PROOFS.md) and their vectors.

An independent implementation, in Python and the standard library only: the Ed25519 arithmetic is written out here, the hash
is the standard library's Blake2b with Carrot's personalisation ("Monero"), and the address text comes from the address
reference (make_vectors_address.py). It shares no code with the Rust side. **A test reference, not a wallet: it handles real
secrets in plain integers and is not constant-time.**

The SIGNATURES are our own construction (the owner's second exception to rule 3, 2026-10-08): a Schnorr proof of knowledge
of the two secrets a, b of an address's spend key K = a G + b T. **Nobody has reviewed them.** The payment proofs' CHECK is
Carrot's own sender scan, which this reference does not repeat (Carrot is checked against Monero's C++ elsewhere): here are
the proof's bytes and text, and the receiver's signature inside a received proof.

    python reference/tools/make_vectors_proofs.py --check     do the committed vectors match the reference?
    python reference/tools/make_vectors_proofs.py --write     regenerate them (a CHANGE OF THE FORMAT: explain it in the commit)

Nothing here is random and nothing depends on the clock: the 32 random bytes a signer draws are fixed inputs.
(The 0.2.0 programs' signatures and proofs, on the interim scheme, are this file's history: `git log`.)
"""
import argparse
import hashlib
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import make_vectors_address as A  # noqa: E402  (the address text: Tenero's own, with its own vectors)

ROOT = A.ROOT
VECTOR_DIR = A.VECTOR_DIR
NAME = "wallet_proofs"

SIGNATURE_PREFIX = "TENsig1"
PROOF_PREFIX = "TENpay1"
PROOF_VERSION = 1
MAX_PROOF_MESSAGE = 4096
MESSAGE_DOMAIN = "Tenero message signature v1"
PAYMENT_DOMAIN = "Tenero payment proof signature v1"
NONCE_DOMAIN = "Tenero signature nonce v1"

# ---- Ed25519 ----------------------------------------------------------------------------------------------------------
P = 2 ** 255 - 19
L = 2 ** 252 + 27742317777372353535851937790883648493
D = (-121665 * pow(121666, P - 2, P)) % P
SQRT_M1 = pow(2, (P - 1) // 4, P)
IDENTITY = (0, 1)


def inv(x):
    return pow(x, P - 2, P)


def recover_x(y, sign):
    if y >= P:
        return None
    x2 = (y * y - 1) * inv(D * y * y + 1) % P
    if x2 == 0:
        return None if sign else 0
    x = pow(x2, (P + 3) // 8, P)
    if (x * x - x2) % P != 0:
        x = x * SQRT_M1 % P
    if (x * x - x2) % P != 0:
        return None
    if x & 1 != sign:
        x = P - x
    return x


def add(a, b):
    x1, y1 = a
    x2, y2 = b
    t = D * x1 * x2 * y1 * y2 % P
    return ((x1 * y2 + x2 * y1) * inv(1 + t) % P, (y1 * y2 + x1 * x2) * inv(1 - t) % P)


def mul(k, pt):
    r = IDENTITY
    while k:
        if k & 1:
            r = add(r, pt)
        pt = add(pt, pt)
        k >>= 1
    return r


def encode(pt):
    x, y = pt
    return (y | ((x & 1) << 255)).to_bytes(32, "little")


def decode(b):
    """A canonically encoded point (any of the 8l), or None."""
    n = int.from_bytes(b, "little")
    y = n & ((1 << 255) - 1)
    x = recover_x(y, n >> 255)
    if x is None:
        return None
    pt = (x, y)
    return pt if encode(pt) == bytes(b) else None


GY = 4 * inv(5) % P
G = (recover_x(GY, 0), GY)
# the FCMP++ generator T, as monero-oxide encodes it (an output key is x G + y T); the Rust tests check this is theirs
T_BYTES = bytes.fromhex("dc42e1d3307b2d4b3b02729abe577e231d79478141cb5b310ca9fa6e127616a3")
T = decode(T_BYTES)
assert T is not None and mul(L, T) == IDENTITY


# ---- Carrot's hash: H_n(transcript) = Blake2b-512 personalised "Monero", reduced mod l -------------------------------
def hash_to_scalar(domain, *fields):
    data = bytes([len(domain)]) + domain.encode() + b"".join(fields)
    return int.from_bytes(hashlib.blake2b(data, digest_size=64, person=b"Monero").digest(), "little") % L


def le32(n):
    return (n % L).to_bytes(32, "little")


def u64(n):
    return n.to_bytes(8, "little")


def canonical(b):
    n = int.from_bytes(b, "little")
    return n if n < L else None


# ---- signatures -------------------------------------------------------------------------------------------------------
def sign_raw(domain, a, b, spend, view, bound, rnd):
    def nonce(which):
        return hash_to_scalar(NONCE_DOMAIN, which, le32(a), le32(b), spend, view, domain.encode(), *bound, rnd)
    r_g, r_t = nonce(b"G"), nonce(b"T")
    r = encode(add(mul(r_g, G), mul(r_t, T)))
    c = hash_to_scalar(domain, spend, view, r, *bound)
    return le32(c) + le32(r_g - c * a) + le32(r_t - c * b)


def verify_raw(domain, spend, view, bound, sig):
    if len(sig) != 96:
        return False
    c, s_g, s_t = canonical(sig[:32]), canonical(sig[32:64]), canonical(sig[64:])
    if c is None or s_g is None or s_t is None:
        return False
    k = decode(spend)
    # the spend key: a point of the prime-order group, and not the identity (nobody holds its secrets)
    if k is None or mul(L, k) != IDENTITY or k == IDENTITY or decode(view) is None:
        return False
    r = encode(add(add(mul(s_g, G), mul(s_t, T)), mul(c, k)))
    return hash_to_scalar(domain, spend, view, r, *bound) == c


def message_bound(message):
    return [u64(len(message)), message]


def sign_message(a, b, spend, view, message, rnd):
    return sign_raw(MESSAGE_DOMAIN, a, b, spend, view, message_bound(message), rnd)


def verify_message(spend, view, message, sig):
    return verify_raw(MESSAGE_DOMAIN, spend, view, message_bound(message), sig)


def signature_text(sig):
    return SIGNATURE_PREFIX + A.b58encode(sig)


# ---- payment proofs ---------------------------------------------------------------------------------------------------
def proof_bound(height, onetime, anchor, message):
    return [u64(height), onetime, anchor, u64(len(message)), message]


def proof_bytes(address_text, height, onetime, anchor, sig):
    out = bytes([PROOF_VERSION, len(address_text)]) + address_text.encode() + u64(height) + onetime + anchor
    return out + (b"\x01" + sig if sig is not None else b"\x00")


def proof_text(b):
    return PROOF_PREFIX + A.b58encode(b)


def parse_proof(b):
    """The fields of a proof's bytes, or the error class a decoder gives ("format")."""
    if not b or b[0] != PROOF_VERSION:
        raise ValueError("format")
    if len(b) < 2:
        raise ValueError("format")
    n = b[1]
    rest = b[2:]
    if len(rest) < n + 8 + 32 + 16 + 1:
        raise ValueError("format")
    try:
        text = rest[:n].decode()
    except UnicodeDecodeError:
        raise ValueError("format")
    ok = False
    for net in A.NETWORKS:
        try:
            A.decode(text, net)
            ok = True
        except Exception:
            pass
    if not ok:
        raise ValueError("format")
    rest = rest[n:]
    flag, sig = rest[56], rest[57:]
    if (flag, len(sig)) not in ((0, 0), (1, 96)):
        raise ValueError("format")
    return {"address": text, "height": int.from_bytes(rest[:8], "little"), "onetime_address": rest[8:40].hex(),
            "anchor": rest[40:56].hex(), "signature": sig.hex() if flag else None}


# ---- the cases --------------------------------------------------------------------------------------------------------
def det(label, n=32):
    out, i = b"", 0
    while len(out) < n:
        out += hashlib.sha256(f"tenero proofs vectors {label} {i}".encode()).digest()
        i += 1
    return out[:n]


def keys(label):
    """An address's secrets and keys: K = a G + b T; the view key some other point."""
    a = int.from_bytes(det(label + " a"), "little") % L
    b = int.from_bytes(det(label + " b"), "little") % L
    spend = encode(add(mul(a, G), mul(b, T)))
    view = encode(mul(int.from_bytes(det(label + " v"), "little") % L, decode(spend)))
    return a, b, spend, view


SMALL_ORDER = bytes.fromhex("26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc05")   # a point of order 8


def signature_cases():
    out = []
    for i, msg in enumerate([b"", b"hello", "fee, pay, rent: €5".encode(), bytes(range(256)) * 3]):
        a, b, spend, view = keys(f"sig {i}")
        rnd = det(f"sig rnd {i}")
        sig = sign_message(a, b, spend, view, msg, rnd)
        assert verify_message(spend, view, msg, sig)
        out.append({"a": le32(a).hex(), "b": le32(b).hex(), "spend": spend.hex(), "view": view.hex(),
                    "message": msg.hex(), "rnd": rnd.hex(), "signature": sig.hex(), "text": signature_text(sig)})
    return out


def refused_signature_cases():
    """Signatures that must NOT verify, each with why."""
    a, b, spend, view = keys("refused")
    msg = b"the message"
    sig = sign_message(a, b, spend, view, msg, det("refused rnd"))
    _, _, other_spend, other_view = keys("refused other")
    torsioned = encode(add(decode(spend), decode(SMALL_ORDER)))
    cases = [
        ("another message", spend, view, b"the messagE", sig),
        ("another address's spend key", other_spend, view, msg, sig),
        ("another view key (the address is bound)", spend, other_view, msg, sig),
        ("c changed", spend, view, msg, le32(int.from_bytes(sig[:32], "little") + 1) + sig[32:]),
        ("s_g changed", spend, view, msg, sig[:32] + le32(int.from_bytes(sig[32:64], "little") + 1) + sig[64:]),
        ("s_t changed", spend, view, msg, sig[:64] + le32(int.from_bytes(sig[64:], "little") + 1)),
        ("s_g not canonical (s + l)", spend, view, msg,
         sig[:32] + (int.from_bytes(sig[32:64], "little") + L).to_bytes(32, "little") + sig[64:]),
        ("a spend key with a small-order part", torsioned, view, msg, sig),
        ("the identity as the spend key", encode(IDENTITY), view, msg, sig),
        ("a spend key that is not a point", bytes([2]) + bytes(31), view, msg, sig),
        ("cut short", spend, view, msg, sig[:95]),
    ]
    out = []
    for note, sp, vw, m, s in cases:
        assert not verify_message(sp, vw, m, s), note
        out.append({"note": note, "spend": sp.hex(), "view": vw.hex(), "message": m.hex(), "signature": s.hex()})
    return out


def proof_cases():
    out = []
    for i, (network, kind, signed, message) in enumerate([
            ("gamma", "main", False, b""), ("gamma", "subaddress", True, b"invoice 42"),
            ("test", "integrated", True, b""), ("dev", "main", True, b"x" * MAX_PROOF_MESSAGE)]):
        a, b, spend, view = keys(f"proof {i}")
        pid = det(f"proof pid {i}", 8) if kind == "integrated" else None
        address = A.encode(network, kind, spend, view, pid)
        height, onetime, anchor = 1000 + 37 * i, det(f"proof ko {i}"), det(f"proof anchor {i}", 16)
        sig = None
        if signed:
            sig = sign_raw(PAYMENT_DOMAIN, a, b, spend, view, proof_bound(height, onetime, anchor, message),
                           det(f"proof rnd {i}"))
            assert verify_raw(PAYMENT_DOMAIN, spend, view, proof_bound(height, onetime, anchor, message), sig)
            # bound to the message and to every field
            assert not verify_raw(PAYMENT_DOMAIN, spend, view, proof_bound(height + 1, onetime, anchor, message), sig)
            assert not verify_raw(MESSAGE_DOMAIN, spend, view, proof_bound(height, onetime, anchor, message), sig)
        b_ = proof_bytes(address, height, onetime, anchor, sig)
        parsed = parse_proof(b_)
        assert parsed["address"] == address and parsed["height"] == height
        out.append({"network": network, "address": address, "height": height, "onetime_address": onetime.hex(),
                    "anchor": anchor.hex(), "message": message.hex(), "a": le32(a).hex(), "b": le32(b).hex(),
                    "rnd": det(f"proof rnd {i}").hex() if signed else None,
                    "signature": sig.hex() if sig else None, "bytes": b_.hex(), "text": proof_text(b_)})
    return out


def invalid_proof_cases():
    good = proof_bytes(A.encode("gamma", "main", *keys("bad")[2:]), 5, det("bad ko"), det("bad anchor", 16), None)
    signed = proof_bytes(A.encode("gamma", "main", *keys("bad")[2:]), 5, det("bad ko"), det("bad anchor", 16),
                         bytes(96))
    n = good[1]
    cases = [
        ("empty", b""),
        ("an unknown version", bytes([2]) + good[1:]),
        ("cut short", good[:-1]),
        ("a byte after the end", good + b"\x00"),
        ("a signature flag of 2", good[:-1] + b"\x02"),
        ("a signature cut short", signed[:-1]),
        ("a flag of 1 with no signature", good[:-1] + b"\x01"),
        ("an address with a changed character", good[:22] + bytes([good[22] ^ 0x02]) + good[23:]),
        ("an address length that runs past the end", good[:1] + bytes([255]) + good[2:]),
        ("not an address", good[:2] + b"x" * n + good[2 + n:]),
    ]
    out = []
    for note, b in cases:
        try:
            parse_proof(b)
        except ValueError:
            out.append({"note": note, "bytes": b.hex()})
            continue
        raise AssertionError(f"the reference accepted an invalid proof: {note}")
    return out


def build():
    return {
        "schema": 1,
        "name": NAME,
        "description": "Message signatures (our own construction: a Schnorr proof of knowledge of a, b with K = a G + b T, "
                       "Carrot's Blake2b hash; UNREVIEWED) and the payment proofs' bytes, text and signatures, from an "
                       "independent Python reference (reference/tools/make_vectors_proofs.py; docs/WALLET_PROOFS.md). The "
                       "payment check itself is Carrot's sender scan, checked against Monero's C++ elsewhere.",
        "domains": {"message": MESSAGE_DOMAIN, "payment": PAYMENT_DOMAIN, "nonce": NONCE_DOMAIN},
        "prefixes": {"signature": SIGNATURE_PREFIX, "proof": PROOF_PREFIX},
        "generators": {"T": T_BYTES.hex()},
        "max_proof_message": MAX_PROOF_MESSAGE,
        "signatures": signature_cases(),
        "refused_signatures": refused_signature_cases(),
        "proofs": proof_cases(),
        "invalid_proofs": invalid_proof_cases(),
    }


def jdump(obj):
    return json.dumps(obj, indent=1, sort_keys=True, ensure_ascii=True) + "\n"


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
        same = os.path.exists(path) and open(path, encoding="utf-8").read() == text
        print(f"{'ok      ' if same else 'DIFFERS '}{NAME}")
        return 0 if same else 1
    with open(path, "w", newline="\n", encoding="utf-8") as f:
        f.write(text)
    print(f"wrote {os.path.relpath(path, ROOT)}  ({len(text) / 1024:.0f} KiB)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
