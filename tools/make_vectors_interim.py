"""The reference for the INTERIM output scheme of the wallet (crates/tenero-wallet/src/interim.rs) and its vectors.

An independent implementation, in Python and the standard library only, of how a wallet derives its keys and
address, makes an output for a recipient, and what the receiver recovers. Ed25519 arithmetic is written out
here (RFC 8032 formulas, affine coordinates: slow and plain on purpose), not taken from a library, so that it
shares no code with the Rust side. **This is a test reference, not a wallet**: it handles real secrets in plain
integers and is not constant-time. Nothing here is Carrot; the scheme is an interim stand-in.

    python tools/make_vectors_interim.py --check     do the committed vectors match the reference?
    python tools/make_vectors_interim.py --write     regenerate them (a CHANGE OF THE WALLET SCHEME: explain it
                                                     in the commit)

Nothing here is random and nothing depends on the clock.
"""
import argparse
import hashlib
import json
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
VECTOR_DIR = os.path.join(ROOT, "tests", "vectors")
NAME = "interim_scheme"

DOMAIN = b"tenero interim v1"
ADDRESS_PREFIX = "tni1"

# ---- Ed25519 (RFC 8032), affine ----------------------------------------------------------------------
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
    n = int.from_bytes(b, "little")
    y = n & ((1 << 255) - 1)
    x = recover_x(y, n >> 255)
    return None if x is None else (x, y)


def strict(b):
    """A canonical, prime-order, non-identity point (or None)."""
    pt = decode(b)
    if pt is None or encode(pt) != bytes(b) or pt == IDENTITY or mul(L, pt) != IDENTITY:
        return None
    return pt


GY = 4 * inv(5) % P
G = (recover_x(GY, 0), GY)
# Monero's second generator (the commitment base for amounts)
H = decode(bytes.fromhex("8b655970153799af2aeadc9ff1add0ea6c7251d54154cfa92c173a0dd39c1f94"))


# ---- the scheme ---------------------------------------------------------------------------------------
def sha(*parts):
    h = hashlib.sha256()
    for p in parts:
        h.update(p)
    return h.digest()


def hs(tag, *parts):
    wide = sha(DOMAIN, tag, b"\x00", *parts) + sha(DOMAIN, tag, b"\x01", *parts)
    return int.from_bytes(wide, "little") % L


def digest(tag, *parts):
    return sha(DOMAIN, tag, *parts)


def xor(a, b):
    return bytes(x ^ y for x, y in zip(a, b))


def tx_context(key_image):
    return digest(b"tx context", key_image)


def coinbase_context(height):
    return digest(b"coinbase context", height.to_bytes(8, "little"))


def keys_of(seed):
    spend = hs(b"spend key", seed)
    view = hs(b"view key", spend.to_bytes(32, "little"))
    return spend, view


def address_bytes(seed):
    spend, view = keys_of(seed)
    return encode(mul(spend, G)), encode(mul(view, G))


def checksum(spend_pub, view_pub):
    return digest(b"address checksum", spend_pub, view_pub)[:4]


def address_text(spend_pub, view_pub):
    return ADDRESS_PREFIX + (spend_pub + view_pub + checksum(spend_pub, view_pub)).hex()


def commit(mask, amount):
    return encode(add(mul(mask, G), mul(amount, H)))


def make_enote(spend_pub, view_pub, amount, ctx, index, coinbase, rng):
    """`rng` is the 80 bytes the Rust function draws: 64 for the ephemeral scalar, 16 for the anchor."""
    k_spend, k_view = strict(spend_pub), strict(view_pub)
    r = int.from_bytes(rng[:64], "little") % L
    anchor = rng[64:80]
    de = encode(mul(r, G))
    s = encode(mul(8, mul(r, k_view)))
    i = index.to_bytes(4, "little")
    t = hs(b"onetime", s, ctx, i)
    onetime = encode(add(mul(t, G), k_spend))
    tag = digest(b"viewtag", s, ctx)[:3]
    if coinbase:
        mask = 1
        commitment = commit(1, amount)
        amount_enc = bytes(8)
    else:
        mask = hs(b"mask", s, ctx, i)
        commitment = commit(mask, amount)
        amount_enc = xor(amount.to_bytes(8, "little"), digest(b"amount", s, ctx, i))
    anchor_enc = xor(anchor, digest(b"anchor", s, ctx, i))
    return {
        "onetime_address": onetime.hex(),
        "amount_commitment": commitment.hex(),
        "amount_enc": amount_enc.hex(),
        "view_tag": tag.hex(),
        "ephemeral_pubkey": de.hex(),
        "anchor_enc": anchor_enc.hex(),
        "mask": mask.to_bytes(32, "little").hex(),
        "offset": t.to_bytes(32, "little").hex(),
    }


def det_bytes(label, n):
    out, c = b"", 0
    while len(out) < n:
        out += hashlib.sha256(b"interim vectors " + label + c.to_bytes(4, "little")).digest()
        c += 1
    return out[:n]


def cases():
    out = []
    specs = [
        # name, sender-seed label, recipient-seed label, amount, context, index, coinbase
        ("ordinary output 0", b"a", b"alice", 123456789, ("tx", det_bytes(b"ki1", 32)), 0, False),
        ("ordinary output 1 (the change)", b"b", b"bob", 7, ("tx", det_bytes(b"ki2", 32)), 1, False),
        ("ordinary output 5, a large amount", b"c", b"alice", 18446744073709551615,
         ("tx", det_bytes(b"ki3", 32)), 5, False),
        ("a zero-amount output", b"d", b"carol", 0, ("tx", det_bytes(b"ki4", 32)), 1, False),
        ("coinbase output 0", b"e", b"alice", 4000000000, ("coinbase", 1000), 0, True),
        ("coinbase at height 0", b"f", b"bob", 1, ("coinbase", 0), 0, True),
    ]
    for name, rng_label, seed_label, amount, ctx_spec, index, coinbase in specs:
        seed = det_bytes(b"seed " + seed_label, 32)
        spend_pub, view_pub = address_bytes(seed)
        ctx = tx_context(ctx_spec[1]) if ctx_spec[0] == "tx" else coinbase_context(ctx_spec[1])
        rng = det_bytes(b"rng " + rng_label, 80)
        e = make_enote(spend_pub, view_pub, amount, ctx, index, coinbase, rng)
        spend, _ = keys_of(seed)
        secret = (int.from_bytes(bytes.fromhex(e["offset"]), "little") + spend) % L
        assert encode(mul(secret, G)).hex() == e["onetime_address"], "the one-time secret does not match"
        out.append({
            "note": name,
            "seed": seed.hex(),
            "address": address_text(spend_pub, view_pub),
            "rng": rng.hex(),
            "amount": amount,
            "context_kind": ctx_spec[0],
            "context_input": ctx_spec[1].hex() if ctx_spec[0] == "tx" else ctx_spec[1],
            "context": ctx.hex(),
            "index": index,
            "coinbase": coinbase,
            "enote": {k: e[k] for k in ("onetime_address", "amount_commitment", "amount_enc", "view_tag",
                                         "ephemeral_pubkey", "anchor_enc", "mask")},
            "offset": e["offset"],
            "onetime_secret": secret.to_bytes(32, "little").hex(),
        })
    return out


def address_cases():
    seed = det_bytes(b"seed alice", 32)
    sp, vp = address_bytes(seed)
    good = address_text(sp, vp)
    identity = encode(IDENTITY)
    # a key of small order (order 8) with a CORRECT checksum: refused for its key, not its checksum
    low = bytes.fromhex("c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a")
    invalid = [
        ("wrong prefix", "tni2" + good[4:], "format"),
        ("no prefix at all", good[4:], "format"),
        ("upper-case digits", ADDRESS_PREFIX + good[4:].upper(), "format"),
        ("too short", good[:-2], "format"),
        ("too long", good + "00", "format"),
        ("not hexadecimal", good[:-1] + "g", "format"),
        ("a changed digit", good[:10] + ("0" if good[10] != "0" else "1") + good[11:], "checksum"),
        ("the spend key is the identity", address_text(identity, vp), "key"),
        ("the view key has small order", address_text(sp, low), "key"),
    ]
    return {"valid": [{"seed": seed.hex(), "address": good, "spend": sp.hex(), "view": vp.hex()}],
            "invalid": [{"note": n, "address": a, "error": e} for n, a, e in invalid]}


def build():
    return {
        "schema": 1,
        "name": NAME,
        "description": "The INTERIM wallet output scheme (crates/tenero-wallet/src/interim.rs; NOT Carrot): keys, "
                       "addresses, making an output for a recipient, and what the receiver recovers. An independent "
                       "Python reference (tools/make_vectors_interim.py).",
        "enotes": cases(),
        "addresses": address_cases(),
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
