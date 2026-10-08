"""The reference for version 3 (`gamma`) wallet keys and addresses (docs/CONSENSUS_V2.md 15.9) and their golden vectors.

Not consensus (a node never sees an address), but every wallet must agree on it, so it is written down and checked like
consensus: this independent Python implementation makes `tests/vectors/v3_address.json`, and the Rust wallet
(`crates/tenero-wallet/src/address.rs`) must reproduce it.

* An account's Carrot master secret: `s_m = SHA-256("tenero carrot master v1" || account seed)`, the account seed being the
  32 bytes the 24 words spell (account 0) or `SHA-256("tenero account v1" || master || i as u32 LE)` (account i > 0).
* An address: `varint(tag) || spend key 32 || view key 32 || [payment ID 8, integrated only] || checksum 4`, the checksum the
  first 4 bytes of `SHA-256("tenero address v3" || everything before it)`, written in Monero's block base58 (8-byte blocks
  as 11 characters, the last partial block of n bytes as [0, 2, 3, 5, 6, 7, 9, 10, 11][n] characters).
* The tags are 4-byte varints chosen so that EVERY address of a network starts with its four letters, whatever its keys:
  `TENg` (gamma), `TENd` (dev), `TENt` (test); the first three such tags of each prefix, in numerical order, are the main
  address, subaddress and integrated address. (`TENm`, `TENs`, `TENi` are kept for a main net.)

    python reference/tools/make_vectors_address.py --check
    python reference/tools/make_vectors_address.py --write
"""
import argparse
import hashlib
import json
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
VECTOR_DIR = os.path.join(ROOT, "tests", "vectors")
NAME = "v3_address"

ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
FULL_BLOCK = 8
ENCODED_SIZES = [0, 2, 3, 5, 6, 7, 9, 10, 11]
MASTER_TAG = b"tenero carrot master v1"
ACCOUNT_TAG = b"tenero account v1"
CHECKSUM_TAG = b"tenero address v3"
NETWORKS = {"gamma": "TENg", "dev": "TENd", "test": "TENt"}
KINDS = ("main", "subaddress", "integrated")
# the network tags of other CryptoNote coins (Monero main/test/stage net, Wownero, Aeon, Haven, Loki/Oxen, Masari): none
# may equal ours (ours are all above 2^27; these are listed so the check is explicit)
OTHER_TAGS = [18, 19, 42, 53, 54, 63, 24, 25, 36, 4146, 6810, 12208, 0xB2, 0x2A, 0x5AF4, 0x5AFC, 0x5AFD, 114, 115, 116, 28,
              52, 29]


class AddressError(Exception):
    def __init__(self, kind):
        super().__init__(kind)
        self.kind = kind


def sha256(*parts):
    h = hashlib.sha256()
    for p in parts:
        h.update(p)
    return h.digest()


# ---------------------------------------------------------------- keys

def account_seed(master, index):
    return master if index == 0 else sha256(ACCOUNT_TAG, master, index.to_bytes(4, "little"))


def carrot_master(seed):
    return sha256(MASTER_TAG, seed)


# ---------------------------------------------------------------- varints and tags

def varint(n):
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        if n:
            out.append(b | 0x80)
        else:
            out.append(b)
            return bytes(out)


def read_varint(data):
    """(value, length) of the canonical varint at the start of `data`, or an error."""
    n = 0
    for i, b in enumerate(data[:10]):
        n |= (b & 0x7F) << (7 * i)
        if not b & 0x80:
            if b == 0 and i > 0:
                raise AddressError("not canonical")
            return n, i + 1
    raise AddressError("bad tag")


def enc_block(v, size):
    s = ""
    for _ in range(size):
        s = ALPHABET[v % 58] + s
        v //= 58
    return s


def tags_for(prefix):
    """The 4-byte varint tags whose addresses always start with `prefix`, in numerical order."""
    v = 0
    for c in prefix:
        v = v * 58 + ALPHABET.index(c)
    width = 58 ** (11 - len(prefix))
    lo, hi = v * width, (v + 1) * width
    unit = 1 << 32
    out = []
    for t in range(-(-lo // unit), (hi - unit) // unit + 1):
        tb = t.to_bytes(4, "big")
        try:
            n, used = read_varint(tb)
        except AddressError:
            continue
        if used == 4:
            out.append(n)
    return sorted(out)


TAGS = {}
for _net, _prefix in NETWORKS.items():
    _t = tags_for(_prefix)
    for _kind, _tag in zip(KINDS, _t[:3]):
        TAGS[(_net, _kind)] = _tag
BY_TAG = {v: k for k, v in TAGS.items()}
assert len(BY_TAG) == len(TAGS) == 9
assert not set(BY_TAG) & set(OTHER_TAGS)


# ---------------------------------------------------------------- base58 (Monero's block form)

def b58encode(data):
    out = ""
    for i in range(0, len(data), FULL_BLOCK):
        block = data[i:i + FULL_BLOCK]
        out += enc_block(int.from_bytes(block, "big"), ENCODED_SIZES[len(block)])
    return out


def b58decode(text):
    full, rest = divmod(len(text), 11)
    if rest not in ENCODED_SIZES:
        raise AddressError("bad length")
    out = b""
    for i in range(full + (1 if rest else 0)):
        chunk = text[i * 11:(i + 1) * 11]
        size = ENCODED_SIZES.index(len(chunk))
        v = 0
        for c in chunk:
            if c not in ALPHABET:
                raise AddressError("bad character")
            v = v * 58 + ALPHABET.index(c)
        if v >> (8 * size):
            raise AddressError("bad block")
        out += v.to_bytes(size, "big")
    return out


# ---------------------------------------------------------------- addresses

def encode(network, kind, spend, view, payment_id=None):
    assert (payment_id is not None) == (kind == "integrated")
    body = varint(TAGS[(network, kind)]) + spend + view + (payment_id or b"")
    return b58encode(body + sha256(CHECKSUM_TAG, body)[:4])


def decode(text, network):
    """The address `text` for `network`: (kind, spend, view, payment ID or None), or an error named as in the vectors."""
    if text.startswith("tni1"):
        raise AddressError("an interim address")
    data = b58decode(text)
    if len(data) < 5:
        raise AddressError("bad length")
    body, check = data[:-4], data[-4:]
    if sha256(CHECKSUM_TAG, body)[:4] != check:
        raise AddressError("bad checksum")
    tag, used = read_varint(body)
    if tag not in BY_TAG:
        raise AddressError("unknown network")
    net, kind = BY_TAG[tag]
    if net != network:
        raise AddressError("another network")
    keys = body[used:]
    if len(keys) != (72 if kind == "integrated" else 64):
        raise AddressError("bad length")
    return kind, keys[:32], keys[32:64], keys[64:] if kind == "integrated" else None


# ---------------------------------------------------------------- vectors

def det(label, n=32):
    out, i = b"", 0
    while len(out) < n:
        out += sha256(f"tenero address vectors {label} {i}".encode())
        i += 1
    return out[:n]


def build():
    keys = [{"seed": det(f"seed {i}").hex(), "account": a, "account_seed": account_seed(det(f"seed {i}"), a).hex(),
             "carrot_master": carrot_master(account_seed(det(f"seed {i}"), a)).hex()}
            for i in range(3) for a in (0, 1, 7)]
    valid = []
    for net in NETWORKS:
        for kind in KINDS:
            for k in range(3):
                spend, view = det(f"{net} {kind} {k} spend"), det(f"{net} {kind} {k} view")
                if k == 1:
                    spend, view = b"\x00" * 32, b"\x00" * 32
                if k == 2:
                    spend, view = b"\xff" * 32, b"\xff" * 32
                pid = det(f"{net} {kind} {k} pid", 8) if kind == "integrated" else None
                text = encode(net, kind, spend, view, pid)
                assert text.startswith(NETWORKS[net]), (net, kind, text)
                assert decode(text, net) == (kind, spend, view, pid)
                valid.append({"network": net, "kind": kind, "spend": spend.hex(), "view": view.hex(),
                              "payment_id": pid.hex() if pid else None, "text": text})
    good = valid[0]["text"]
    flip = good[:20] + ("2" if good[20] != "2" else "3") + good[21:]
    invalid = []

    def bad(note, text, network, error):
        try:
            decode(text, network)
        except AddressError as e:
            assert e.kind == error, (note, e.kind)
        else:
            raise AssertionError(note)
        invalid.append({"note": note, "text": text, "network": network, "error": error})

    bad("one character changed", flip, "gamma", "bad checksum")
    bad("a gamma address on the dev network", good, "dev", "another network")
    bad("a character not in the alphabet (0)", good[:-1] + "0", "gamma", "bad character")
    bad("a character not in the alphabet (l)", good[:-1] + "l", "gamma", "bad character")
    bad("one character missing (the last block then overflows)", good[:-1], "gamma", "bad block")
    bad("an interim (beta) address", "tni1" + "00" * 40, "gamma", "an interim address")
    bad("empty", "", "gamma", "bad length")
    # well-formed but a Monero main net tag (18)
    body = varint(18) + det("monero spend") + det("monero view")
    bad("a Monero-style address", b58encode(body + sha256(CHECKSUM_TAG, body)[:4]), "gamma", "unknown network")
    # a main tag with the integrated length
    body = varint(TAGS[("gamma", "main")]) + det("x") + det("y") + det("z", 8)
    bad("a main address carrying a payment ID", b58encode(body + sha256(CHECKSUM_TAG, body)[:4]), "gamma", "bad length")
    # a last block that overflows its size: 2 characters for 1 byte, "zz" = 3363 > 255
    bad("a block that does not fit its size", good[:99 - 11] + "zz", "gamma", "bad block")
    return {
        "schema": 1, "name": NAME,
        "description": "Version 3 (gamma) wallet keys and addresses (docs/CONSENSUS_V2.md 15.9): an account's Carrot master "
                       "secret from its seed, the network tags, and addresses in Monero's block base58 with a SHA-256 "
                       "checksum. Made by reference/tools/make_vectors_address.py.",
        "prefixes": NETWORKS,
        "tags": {f"{n} {k}": TAGS[(n, k)] for (n, k) in TAGS},
        "keys": keys, "valid": valid, "invalid": invalid,
    }


def jdump(obj):
    return json.dumps(obj, indent=1, sort_keys=True) + "\n"


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--check", action="store_true")
    ap.add_argument("--write", action="store_true")
    args = ap.parse_args(argv)
    if not (args.check or args.write):
        ap.print_help()
        return 1
    text = jdump(build())
    path = os.path.join(VECTOR_DIR, NAME + ".json")
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
