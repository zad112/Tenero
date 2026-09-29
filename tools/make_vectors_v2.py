"""The reference for the version 2 DATA MODEL (docs/CONSENSUS_V2.md) and its golden vectors.

This covers only what needs no cryptography: the canonical binary serialization, the block header and
its hashes, the block id inputs, transaction ids, the Merkle root, and the genesis block and chain id.
It uses nothing but the standard library, so it is an independent implementation the Rust code of
milestone M6 can be checked against.

It does NOT cover Carrot, CLSAG, Bulletproofs+ or FCMP++: for those we import the upstream projects' own
test vectors when their code is added (a vector made by our own reference would only prove we agree
with ourselves).

    python tools/make_vectors_v2.py --check     do the committed files match the reference?
    python tools/make_vectors_v2.py --write     regenerate them (a CONSENSUS CHANGE: explain it in the
                                                commit and update docs/CONSENSUS_V2.md)

Nothing here is random and nothing depends on the clock.
"""
import argparse
import hashlib
import json
import os
import struct
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
VECTOR_DIR = os.path.join(ROOT, "tests", "vectors")
SCHEMA = 1
FILES = ("v2_serialization", "v2_ids", "v2_merkle", "v2_genesis")

# ---------------------------------------------------------------- consensus limits (PROPOSED)
MAX_INPUTS, MIN_INPUTS = 32, 1
MAX_OUTPUTS, MIN_OUTPUTS = 16, 2
MAX_COINBASE_OUTPUTS, MIN_COINBASE_OUTPUTS = 16, 1
MAX_EXTRA = 128
MAX_PROOF = 32 * 1024
MAX_RING = 16
MAX_BLOCK_TXS = 8192
LIMITS = {"MAX_INPUTS": MAX_INPUTS, "MIN_INPUTS": MIN_INPUTS, "MAX_OUTPUTS": MAX_OUTPUTS,
          "MIN_OUTPUTS": MIN_OUTPUTS, "MAX_COINBASE_OUTPUTS": MAX_COINBASE_OUTPUTS,
          "MIN_COINBASE_OUTPUTS": MIN_COINBASE_OUTPUTS, "MAX_EXTRA": MAX_EXTRA,
          "MAX_PROOF": MAX_PROOF, "MAX_RING": MAX_RING, "MAX_BLOCK_TXS": MAX_BLOCK_TXS}

HEADER_TAG = b"tenero block header v2"
TX_TAG = b"tenero tx v2"
COINBASE_TAG = b"tenero coinbase v2"
GENESIS_TAG = b"tenero genesis"
GENESIS_ID_TAG = b"tenero genesis id v2"
NETWORK_LABEL = "tenero experimental network 1"
VERSION = 2


class DecodeError(Exception):
    """A strict-decoding failure. `kind` is one of the four strings in ERRORS."""

    def __init__(self, kind):
        super().__init__(kind)
        self.kind = kind


ERRORS = ("short read", "trailing bytes", "count out of range", "length over maximum")


# ---------------------------------------------------------------- writing (no range checks)

def w_u8(n):
    return struct.pack("<B", n)


def w_u16(n):
    return struct.pack("<H", n)


def w_u32(n):
    return struct.pack("<I", n)


def w_u64(n):
    return struct.pack("<Q", n)


def w_fixed(hex_text, size):
    b = bytes.fromhex(hex_text)
    assert len(b) == size, f"expected {size} bytes, got {len(b)}"
    return b


def w_var(hex_text):
    b = bytes.fromhex(hex_text)
    return w_u32(len(b)) + b


def w_list(items, encode):
    return w_u32(len(items)) + b"".join(encode(x) for x in items)


# ---------------------------------------------------------------- reading (strict)

class Reader:
    def __init__(self, data):
        self.data = bytes(data)
        self.pos = 0

    def take(self, n):
        if n > len(self.data) - self.pos:
            raise DecodeError("short read")
        out = self.data[self.pos:self.pos + n]
        self.pos += n
        return out

    def u8(self):
        return self.take(1)[0]

    def u16(self):
        return struct.unpack("<H", self.take(2))[0]

    def u32(self):
        return struct.unpack("<I", self.take(4))[0]

    def u64(self):
        return struct.unpack("<Q", self.take(8))[0]

    def fixed(self, size):
        return self.take(size).hex()

    def var(self, maximum):
        n = self.u32()
        if n > maximum:                       # checked BEFORE reading (or allocating) the bytes
            raise DecodeError("length over maximum")
        return self.take(n).hex()

    def count(self, low, high):
        n = self.u32()
        if not low <= n <= high:              # checked BEFORE reading the elements
            raise DecodeError("count out of range")
        return n

    def finish(self):
        if self.pos != len(self.data):
            raise DecodeError("trailing bytes")


# ---------------------------------------------------------------- the objects
# Every object is a dict of hex strings and ints, so it goes straight into JSON.

def enc_output(o):
    return (w_fixed(o["onetime_address"], 32) + w_fixed(o["amount_commitment"], 32)
            + w_fixed(o["amount_enc"], 8) + w_fixed(o["view_tag"], 3)
            + w_fixed(o["ephemeral_pubkey"], 32) + w_fixed(o["anchor_enc"], 16))


def dec_output(r):
    return {"onetime_address": r.fixed(32), "amount_commitment": r.fixed(32), "amount_enc": r.fixed(8),
            "view_tag": r.fixed(3), "ephemeral_pubkey": r.fixed(32), "anchor_enc": r.fixed(16)}


def enc_input(i):
    return w_fixed(i["key_image"], 32) + w_list(i["ring"], w_u64)


def dec_input(r):
    key_image = r.fixed(32)
    n = r.count(0, MAX_RING)
    return {"key_image": key_image, "ring": [r.u64() for _ in range(n)]}


def enc_tx(t):
    return (w_u16(t["version"]) + w_list(t["inputs"], enc_input) + w_list(t["outputs"], enc_output)
            + w_u64(t["fee"]) + w_var(t["extra"]) + w_var(t["proof_data"]))


def dec_tx(r):
    version = r.u16()
    inputs = [dec_input(r) for _ in range(r.count(MIN_INPUTS, MAX_INPUTS))]
    outputs = [dec_output(r) for _ in range(r.count(MIN_OUTPUTS, MAX_OUTPUTS))]
    fee = r.u64()
    return {"version": version, "inputs": inputs, "outputs": outputs, "fee": fee,
            "extra": r.var(MAX_EXTRA), "proof_data": r.var(MAX_PROOF)}


def enc_cb_output(o):
    return (w_fixed(o["onetime_address"], 32) + w_u64(o["amount"]) + w_fixed(o["view_tag"], 3)
            + w_fixed(o["ephemeral_pubkey"], 32) + w_fixed(o["anchor_enc"], 16))


def dec_cb_output(r):
    return {"onetime_address": r.fixed(32), "amount": r.u64(), "view_tag": r.fixed(3),
            "ephemeral_pubkey": r.fixed(32), "anchor_enc": r.fixed(16)}


def enc_coinbase(c):
    return w_u16(c["version"]) + w_u64(c["height"]) + w_list(c["outputs"], enc_cb_output) + w_var(c["extra"])


def dec_coinbase(r):
    version, height = r.u16(), r.u64()
    outputs = [dec_cb_output(r) for _ in range(r.count(MIN_COINBASE_OUTPUTS, MAX_COINBASE_OUTPUTS))]
    return {"version": version, "height": height, "outputs": outputs, "extra": r.var(MAX_EXTRA)}


def enc_header(h):
    return (w_u16(h["version"]) + w_fixed(h["prev_id"], 32) + w_u64(h["timestamp"])
            + w_fixed(h["tx_root"], 32) + w_u64(h["nonce"]) + w_fixed(h["mix"], 64))


def dec_header(r):
    return {"version": r.u16(), "prev_id": r.fixed(32), "timestamp": r.u64(), "tx_root": r.fixed(32),
            "nonce": r.u64(), "mix": r.fixed(64)}


def enc_block(b):
    return enc_header(b["header"]) + enc_coinbase(b["coinbase"]) + w_list(b["transactions"], enc_tx)


def dec_block(r):
    header = dec_header(r)
    coinbase = dec_coinbase(r)
    txs = [dec_tx(r) for _ in range(r.count(0, MAX_BLOCK_TXS))]
    return {"header": header, "coinbase": coinbase, "transactions": txs}


KINDS = {
    "output": (enc_output, dec_output),
    "input": (enc_input, dec_input),
    "transaction": (enc_tx, dec_tx),
    "coinbase_output": (enc_cb_output, dec_cb_output),
    "coinbase": (enc_coinbase, dec_coinbase),
    "header": (enc_header, dec_header),
    "block": (enc_block, dec_block),
}


def encode(kind, obj):
    return KINDS[kind][0](obj)


def decode(kind, data):
    """Strict: the whole input must be exactly one object."""
    r = Reader(data)
    obj = KINDS[kind][1](r)
    r.finish()
    return obj


# ---------------------------------------------------------------- hashes and ids

def sha256(*parts):
    h = hashlib.sha256()
    for p in parts:
        h.update(p)
    return h.digest()


def header_hash(h):
    """The 32-byte input of the proof of work: the header without nonce and mix, with a domain tag."""
    return sha256(HEADER_TAG, w_u16(h["version"]), bytes.fromhex(h["prev_id"]), w_u64(h["timestamp"]),
                  bytes.fromhex(h["tx_root"]))


def pow_seed(hh, nonce):
    return sha256(hh, w_u64(nonce))


def block_id(h, pow_kind):
    """The proof-of-work digest, which is the block id."""
    hh = header_hash(h)
    if pow_kind == "matmul":
        return sha256(pow_seed(hh, h["nonce"]), bytes.fromhex(h["mix"]))
    if pow_kind == "sha256":
        return sha256(hh, w_u64(h["nonce"]))
    raise ValueError(pow_kind)


def tx_id(t):
    return sha256(TX_TAG, enc_tx(t))


def coinbase_id(c):
    return sha256(COINBASE_TAG, enc_coinbase(c))


def merkle_leaf(x):
    return sha256(b"\x00", x)


def merkle_node(left, right):
    return sha256(b"\x01", left, right)


def merkle_root(ids):
    """RFC 6962: the split is at the largest power of two below n; nothing is ever duplicated."""
    n = len(ids)
    if n == 0:
        return sha256(b"")
    if n == 1:
        return merkle_leaf(ids[0])
    k = 1
    while k * 2 < n:
        k *= 2
    return merkle_node(merkle_root(ids[:k]), merkle_root(ids[k:]))


def genesis_header(label=NETWORK_LABEL):
    return {"version": VERSION, "prev_id": "00" * 32, "timestamp": 0,
            "tx_root": sha256(GENESIS_TAG, label.encode()).hex(), "nonce": 0, "mix": "00" * 64}


def genesis_id(label=NETWORK_LABEL):
    return sha256(GENESIS_ID_TAG, enc_header(genesis_header(label)))


# ---------------------------------------------------------------- deterministic sample data

def det_bytes(label, n):
    out, i = b"", 0
    while len(out) < n:
        out += hashlib.sha256(f"tenero vectors v2 {label} {i}".encode()).digest()
        i += 1
    return out[:n]


def h(label, n):
    return det_bytes(label, n).hex()


def sample_output(tag):
    return {"onetime_address": h(tag + " ko", 32), "amount_commitment": h(tag + " ca", 32),
            "amount_enc": h(tag + " enc", 8), "view_tag": h(tag + " vt", 3),
            "ephemeral_pubkey": h(tag + " de", 32), "anchor_enc": h(tag + " anchor", 16)}


def sample_input(tag, ring_size=16):
    ring = sorted({int.from_bytes(det_bytes(f"{tag} ring {i}", 4), "little") for i in range(ring_size)})
    return {"key_image": h(tag + " ki", 32), "ring": ring}


def sample_tx(tag, n_in=2, n_out=2, proof=200, extra=24, fee=123456):
    return {"version": VERSION, "inputs": [sample_input(f"{tag} in{i}") for i in range(n_in)],
            "outputs": [sample_output(f"{tag} out{i}") for i in range(n_out)], "fee": fee,
            "extra": h(tag + " extra", extra), "proof_data": h(tag + " proof", proof)}


def sample_cb_output(tag, amount):
    return {"onetime_address": h(tag + " ko", 32), "amount": amount, "view_tag": h(tag + " vt", 3),
            "ephemeral_pubkey": h(tag + " de", 32), "anchor_enc": h(tag + " anchor", 16)}


def sample_coinbase(height, n_out=1, amount=2_000_000_000):
    return {"version": VERSION, "height": height,
            "outputs": [sample_cb_output(f"cb {height} {i}", amount + i) for i in range(n_out)],
            "extra": h(f"cb {height} extra", 8)}


def sample_header(tag, nonce=42, timestamp=1_700_000_060, txs=(), mix=None):
    root = merkle_root([bytes.fromhex(x) for x in txs])
    return {"version": VERSION, "prev_id": h(tag + " prev", 32), "timestamp": timestamp,
            "tx_root": root.hex(), "nonce": nonce,
            "mix": mix if mix is not None else h(tag + " mix", 64)}


# ---------------------------------------------------------------- the vector files

def jdump(obj):
    return json.dumps(obj, indent=1, sort_keys=True) + "\n"


def wrap(name, description, body):
    return {"schema": SCHEMA, "name": name, "description": description, **body}


def valid_case(kind, obj, note=""):
    return {"kind": kind, "note": note, "object": obj, "hex": encode(kind, obj).hex()}


def invalid_case(kind, data, error, note):
    assert error in ERRORS
    # the reference itself must reject it in exactly that way (the generator checks itself)
    try:
        decode(kind, data)
    except DecodeError as e:
        assert e.kind == error, (note, e.kind, error)
    else:
        raise AssertionError(f"reference accepted an invalid case: {note}")
    return {"kind": kind, "note": note, "hex": bytes(data).hex(), "error": error}


def serialization_vectors():
    prim = []
    for v in (0, 1, 255):
        prim.append({"type": "u8", "value": v, "hex": w_u8(v).hex()})
    for v in (0, 1, 2, 255, 256, 65535):
        prim.append({"type": "u16", "value": v, "hex": w_u16(v).hex()})
    for v in (0, 1, 256, 65536, 4294967295):
        prim.append({"type": "u32", "value": v, "hex": w_u32(v).hex()})
    for v in (0, 1, 2 ** 32, 2 ** 63, 2 ** 64 - 1):
        prim.append({"type": "u64", "value": v, "hex": w_u64(v).hex()})

    valid = [
        valid_case("output", sample_output("a"), "a fixed 123-byte output"),
        valid_case("input", sample_input("a"), "a ring of 16 ascending indexes"),
        valid_case("input", {"key_image": h("k", 32), "ring": []}, "an empty ring encodes (validation rejects it later)"),
        valid_case("coinbase_output", sample_cb_output("a", 2_000_000_000), "a plaintext amount"),
        valid_case("coinbase", sample_coinbase(1), "one output"),
        valid_case("coinbase", sample_coinbase(525_601, n_out=3), "three outputs"),
        valid_case("header", sample_header("a"), "a matmul header"),
        valid_case("header", sample_header("b", nonce=2 ** 64 - 1, timestamp=2 ** 64 - 1, mix="00" * 64),
                   "largest nonce and timestamp, zero mix"),
        valid_case("transaction", sample_tx("a"), "2 inputs, 2 outputs"),
        valid_case("transaction", sample_tx("b", n_in=1, n_out=2, proof=0, extra=0), "empty extra and proof"),
        valid_case("transaction", sample_tx("c", n_in=MAX_INPUTS, n_out=MAX_OUTPUTS, proof=1000, extra=MAX_EXTRA),
                   "the maximum number of inputs and outputs and the maximum extra"),
        valid_case("transaction", sample_tx("d", proof=MAX_PROOF), "the maximum proof length"),
    ]
    txs = [sample_tx("blk1"), sample_tx("blk2", n_in=1)]
    ids = [tx_id(t).hex() for t in txs]
    cb = sample_coinbase(7)
    root = merkle_root([coinbase_id(cb)] + [bytes.fromhex(i) for i in ids])
    hdr = sample_header("blk", txs=[coinbase_id(cb).hex()] + ids)
    assert hdr["tx_root"] == root.hex()
    valid.append(valid_case("block", {"header": hdr, "coinbase": cb, "transactions": txs},
                            "a header, a coinbase and two transactions"))
    valid.append(valid_case("block", {"header": sample_header("empty"), "coinbase": sample_coinbase(8), "transactions": []},
                            "no transactions besides the coinbase"))

    tx = sample_tx("e")
    good = enc_tx(tx)
    out = sample_output("z")
    invalid = [
        invalid_case("output", enc_output(out)[:-1], "short read", "an output one byte short"),
        invalid_case("output", enc_output(out) + b"\x00", "trailing bytes", "an output with a trailing byte"),
        invalid_case("header", enc_header(sample_header("a"))[:-1], "short read", "a header one byte short"),
        invalid_case("header", enc_header(sample_header("a")) + b"\x00", "trailing bytes", "a header with a trailing byte"),
        invalid_case("transaction", b"", "short read", "empty input"),
        invalid_case("transaction", good[:-1], "short read", "a transaction one byte short"),
        invalid_case("transaction", good + b"\x00", "trailing bytes", "a transaction with a trailing byte"),
        invalid_case("transaction", good + good, "trailing bytes", "two transactions in a row"),
        invalid_case("transaction", enc_tx({**tx, "inputs": []}), "count out of range", "no inputs"),
        invalid_case("transaction", enc_tx({**tx, "inputs": [sample_input(f"i{i}") for i in range(MAX_INPUTS + 1)]}),
                     "count out of range", "one input too many"),
        invalid_case("transaction", enc_tx({**tx, "outputs": [sample_output("o")]}), "count out of range",
                     "only one output (at least two are required)"),
        invalid_case("transaction", enc_tx({**tx, "outputs": [sample_output(f"o{i}") for i in range(MAX_OUTPUTS + 1)]}),
                     "count out of range", "one output too many"),
        invalid_case("transaction", w_u16(VERSION) + w_u32(0xFFFFFFFF), "count out of range",
                     "an input count of 2^32-1 (rejected without reading or allocating anything)"),
        invalid_case("transaction", enc_tx({**tx, "extra": h("x", MAX_EXTRA + 1)}), "length over maximum",
                     "extra one byte too long"),
        invalid_case("transaction", enc_tx({**tx, "proof_data": h("p", MAX_PROOF + 1)}), "length over maximum",
                     "proof one byte too long"),
        invalid_case("transaction", good[:-4 - 200] + w_u32(0xFFFFFFFF) + b"", "length over maximum",
                     "a proof length of 2^32-1"),
        invalid_case("input", enc_input({"key_image": h("k", 32), "ring": list(range(MAX_RING + 1))}),
                     "count out of range", "a ring of 17"),
        invalid_case("coinbase", enc_coinbase({**sample_coinbase(1), "outputs": []}), "count out of range",
                     "a coinbase with no outputs"),
        invalid_case("coinbase", enc_coinbase({**sample_coinbase(1),
                                               "outputs": [sample_cb_output(f"c{i}", 1) for i in range(MAX_COINBASE_OUTPUTS + 1)]}),
                     "count out of range", "a coinbase with too many outputs"),
        invalid_case("block", enc_header(sample_header("a")) + enc_coinbase(sample_coinbase(1)) + w_u32(MAX_BLOCK_TXS + 1),
                     "count out of range", "more transactions than the block maximum"),
        invalid_case("block", enc_block({"header": sample_header("a"), "coinbase": sample_coinbase(1), "transactions": []})[:-2],
                     "short read", "a block whose transaction count is cut short"),
    ]
    return wrap("v2_serialization",
                "Canonical binary serialization of the version 2 objects (docs/CONSENSUS_V2.md section 4): fixed-width "
                "little-endian integers, no varints, u32 counts and lengths checked against maximums before reading, strict "
                "decoding (trailing bytes are an error). `valid` cases give an object and its exact bytes; `invalid` cases "
                "give bytes and the kind of failure. The limits are PROPOSED constants and are listed in `limits`.",
                {"limits": LIMITS, "primitives": prim, "valid": valid, "invalid": invalid})


def ids_vectors():
    headers = [sample_header("id1"), sample_header("id2", nonce=0, timestamp=0),
               sample_header("id3", nonce=2 ** 64 - 1, mix="00" * 64)]
    hcases = []
    for hd in headers:
        hh = header_hash(hd)
        hcases.append({
            "header": hd, "header_bytes": enc_header(hd).hex(), "header_hash": hh.hex(),
            "seed": pow_seed(hh, hd["nonce"]).hex(),
            "block_id_matmul": block_id(hd, "matmul").hex(),
            "block_id_sha256": block_id({**hd, "mix": "00" * 64}, "sha256").hex()})
    txs = [sample_tx("t1"), sample_tx("t2", n_in=1, n_out=3, proof=0, extra=0)]
    tcases = [{"transaction": t, "bytes": enc_tx(t).hex(), "id": tx_id(t).hex()} for t in txs]
    cbs = [sample_coinbase(1), sample_coinbase(60, n_out=2)]
    ccases = [{"coinbase": c, "bytes": enc_coinbase(c).hex(), "id": coinbase_id(c).hex()} for c in cbs]
    # a change of any single byte of a transaction changes its id, and a coinbase and a transaction with the
    # same bytes would have different ids because of the domain tags
    t = txs[0]
    same_bytes = {"bytes": enc_tx(t).hex(), "as_transaction": tx_id(t).hex(),
                  "as_coinbase": sha256(COINBASE_TAG, enc_tx(t)).hex()}
    return wrap("v2_ids",
                "Header hash (the proof-of-work input, over the header without nonce and mix, with the tag "
                "'tenero block header v2'), the proof-of-work seed, the block id (the PoW digest) for matmul and for the "
                "SHA-256 test chain, and transaction and coinbase ids (SHA-256 over a domain tag and the serialized bytes). "
                "The mix in `header` is arbitrary here: these vectors test the id arithmetic, not a solved proof of work.",
                {"tags": {"header": HEADER_TAG.decode(), "transaction": TX_TAG.decode(),
                          "coinbase": COINBASE_TAG.decode()},
                 "headers": hcases, "transactions": tcases, "coinbases": ccases, "domain_separation": same_bytes})


def merkle_vectors():
    cases = []
    for n in list(range(0, 18)) + [31, 32, 33, 100]:
        ids = [sha256(f"tenero vectors v2 leaf {i}".encode()) for i in range(n)]
        cases.append({"n": n, "leaves": [x.hex() for x in ids], "root": merkle_root(ids).hex()})
    a, b, c = (sha256(x) for x in (b"a", b"b", b"c"))
    props = {
        "leaf_prefix": "00", "node_prefix": "01",
        "empty_root": sha256(b"").hex(),
        "not_padded": {"root_of_a_b_c": merkle_root([a, b, c]).hex(), "root_of_a_b_c_c": merkle_root([a, b, c, c]).hex()},
        "node_is_not_a_leaf": {"root_of_a_b": merkle_root([a, b]).hex(),
                               "root_of_one_leaf_holding_node_a_b": merkle_root([merkle_node(merkle_leaf(a), merkle_leaf(b))]).hex()},
    }
    return wrap("v2_merkle",
                "The transaction Merkle root (docs/CONSENSUS_V2.md 5.3), RFC 6962 style: leaf = SHA-256(0x00 || id), "
                "node = SHA-256(0x01 || left || right), split at the largest power of two below n, empty tree = "
                "SHA-256(''), an odd leaf is never duplicated. `leaves` are 32-byte ids.",
                {"cases": cases, "properties": props})


def genesis_vectors():
    labels = [NETWORK_LABEL, "tenero test network 2", ""]
    cases = []
    for label in labels:
        hd = genesis_header(label)
        cases.append({"label": label, "header": hd, "header_bytes": enc_header(hd).hex(),
                      "genesis_id": genesis_id(label).hex(), "chain_id": genesis_id(label).hex()})
    return wrap("v2_genesis",
                "The fixed genesis header and the chain id (docs/CONSENSUS_V2.md 5.4): version 2, no parent, timestamp 0, "
                "nonce 0, zero mix, tx_root = SHA-256('tenero genesis' || label); genesis_id = SHA-256('tenero genesis id "
                "v2' || header bytes); chain_id = genesis_id. The default label is the first case.",
                {"cases": cases})


BUILDERS = {"v2_serialization": serialization_vectors, "v2_ids": ids_vectors,
            "v2_merkle": merkle_vectors, "v2_genesis": genesis_vectors}


def path_of(name):
    return os.path.join(VECTOR_DIR, name + ".json")


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--check", action="store_true", help="compare the committed files with the reference")
    ap.add_argument("--write", action="store_true", help="regenerate the files (a consensus change)")
    args = ap.parse_args(argv)
    if not (args.check or args.write):
        ap.print_help()
        return 1
    bad = 0
    os.makedirs(VECTOR_DIR, exist_ok=True)
    for name in FILES:
        text = jdump(BUILDERS[name]())
        path = path_of(name)
        if args.check:
            same = os.path.exists(path) and open(path).read() == text
            print(f"{'ok      ' if same else 'DIFFERS '}{name}")
            bad += 0 if same else 1
        else:
            with open(path, "w", newline="\n") as f:
                f.write(text)
            print(f"wrote {os.path.relpath(path, ROOT)}  ({len(text) / 1024:.0f} KiB)")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
