"""The reference for the version 2 DATA MODEL (docs/CONSENSUS_V2.md) and its golden vectors.

This covers only what needs no cryptography: the canonical binary serialization (including the
pruned forms), the block header and its hashes, the block id inputs, transaction ids, the Merkle root,
the genesis block and chain id, the dynamic minimum fee and the emission in the version 2 units.
It uses only the standard library, except that the emission vectors are computed with the version 1
reference (`tenero.chain`), whose arithmetic is unit-agnostic and is what the Rust code already matches.
It is an independent implementation the Rust code of milestone M6 can be checked against.

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
FILES = ("v2_serialization", "v2_ids", "v2_merkle", "v2_genesis", "v2_fees", "v2_emission")

# ---------------------------------------------------------------- consensus limits (PROVISIONAL)
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
PRUNABLE_TAG = b"tenero tx prunable v2"
COINBASE_TAG = b"tenero coinbase v2"
GENESIS_TAG = b"tenero genesis"
GENESIS_ID_TAG = b"tenero genesis id v2"
NETWORK_LABEL = "tenero experimental network 1"
VERSION = 2

# Units: 8 decimals, so 1 coin = 10**8 units and the 20,000,000-coin cap is 2 * 10**15 units.
DECIMALS = 8
UNIT = 10 ** DECIMALS
# The dynamic minimum fee (section 8): fee >= ceil(base_reward * FEE_REFERENCE_WEIGHT * size / median^2).
FEE_REFERENCE_WEIGHT = 3000
# The block-size median never goes below this (version 1 used 300,000): it is the size up to which a block
# carries no penalty, so it bounds the free growth of the chain. DECIDED 150,000 (docs/CONSENSUS_V2.md 8.2).
MIN_BLOCK_MEDIAN = 150_000
MEDIAN_WINDOW = 10
U64_MAX = 2 ** 64 - 1


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
    """An input in the prefix is only its key image: the ring it was signed against is in the prunable part."""
    return w_fixed(i["key_image"], 32)


def dec_input(r):
    return {"key_image": r.fixed(32)}


# A transaction is a PREFIX (everything a node needs after it has been verified: the key images, the
# outputs, the fee) followed by the PRUNABLE part (the rings and the proofs, needed only to verify it once).
# The id commits to the prefix and to a hash of the prunable part, so a node that has discarded the
# rings and proofs can still recompute every transaction id and every Merkle root
# (docs/CONSENSUS_V2.md section 14).

def enc_tx_prefix(t):
    return (w_u16(t["version"]) + w_list(t["inputs"], enc_input) + w_list(t["outputs"], enc_output)
            + w_u64(t["fee"]) + w_var(t["extra"]))


def dec_tx_prefix(r):
    version = r.u16()
    inputs = [dec_input(r) for _ in range(r.count(MIN_INPUTS, MAX_INPUTS))]
    outputs = [dec_output(r) for _ in range(r.count(MIN_OUTPUTS, MAX_OUTPUTS))]
    fee = r.u64()
    return {"version": version, "inputs": inputs, "outputs": outputs, "fee": fee, "extra": r.var(MAX_EXTRA)}


def enc_prunable(t):
    """The prunable part: one ring (a list of global output indexes) per input, then the proof bytes."""
    return w_list(t["rings"], lambda ring: w_list(ring, w_u64)) + w_var(t["proof_data"])


def enc_tx(t):
    return enc_tx_prefix(t) + enc_prunable(t)


def dec_prunable(r, n_inputs):
    # exactly one ring per input; the count is checked before any ring is read
    rings = [[r.u64() for _ in range(r.count(0, MAX_RING))] for _ in range(r.count(n_inputs, n_inputs))]
    return {"rings": rings, "proof_data": r.var(MAX_PROOF)}


def dec_tx(r):
    t = dec_tx_prefix(r)
    t.update(dec_prunable(r, len(t["inputs"])))
    return t


def enc_pruned_tx(p):
    """A transaction with its rings and proofs discarded: the prefix and the 32-byte hash of the prunable part."""
    return enc_tx_prefix(p) + w_fixed(p["prunable_hash"], 32)


def dec_pruned_tx(r):
    p = dec_tx_prefix(r)
    p["prunable_hash"] = r.fixed(32)
    return p


def prunable_hash(t):
    """SHA-256 over the tag and the serialized prunable part (the rings and the proof bytes)."""
    return sha256(PRUNABLE_TAG, enc_prunable(t))


def prune(t):
    """The pruned form of a transaction: the prefix and the hash of everything that was dropped."""
    p = {k: v for k, v in t.items() if k not in ("proof_data", "rings")}
    p["prunable_hash"] = prunable_hash(t).hex()
    return p


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


def enc_pruned_block(b):
    return enc_header(b["header"]) + enc_coinbase(b["coinbase"]) + w_list(b["transactions"], enc_pruned_tx)


def dec_pruned_block(r):
    header = dec_header(r)
    coinbase = dec_coinbase(r)
    txs = [dec_pruned_tx(r) for _ in range(r.count(0, MAX_BLOCK_TXS))]
    return {"header": header, "coinbase": coinbase, "transactions": txs}


KINDS = {
    "output": (enc_output, dec_output),
    "input": (enc_input, dec_input),
    "transaction": (enc_tx, dec_tx),
    "pruned_transaction": (enc_pruned_tx, dec_pruned_tx),
    "pruned_block": (enc_pruned_block, dec_pruned_block),
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
    """Over the prefix and the HASH of the prunable part, so it is the same for the full and the pruned form."""
    return sha256(TX_TAG, enc_tx_prefix(t), prunable_hash(t))


def pruned_tx_id(p):
    return sha256(TX_TAG, enc_tx_prefix(p), bytes.fromhex(p["prunable_hash"]))


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


def sample_input(tag):
    return {"key_image": h(tag + " ki", 32)}


def sample_ring(tag, size=16):
    return sorted({int.from_bytes(det_bytes(f"{tag} ring {i}", 4), "little") for i in range(size)})


def sample_tx(tag, n_in=2, n_out=2, proof=200, extra=24, fee=123456, ring_size=16):
    return {"version": VERSION, "inputs": [sample_input(f"{tag} in{i}") for i in range(n_in)],
            "outputs": [sample_output(f"{tag} out{i}") for i in range(n_out)], "fee": fee,
            "extra": h(tag + " extra", extra),
            "rings": [sample_ring(f"{tag} in{i}", ring_size) for i in range(n_in)],
            "proof_data": h(tag + " proof", proof)}


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
        valid_case("input", sample_input("a"), "an input in the prefix is only its key image"),
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
        valid_case("transaction", sample_tx("f", ring_size=0), "empty rings encode (validation rejects them later)"),
        valid_case("transaction", sample_tx("g", n_in=3, ring_size=MAX_RING), "three inputs, a full ring each"),
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
    # the pruned forms: the same prefix, the rings and proofs replaced by their 32-byte hash
    valid.append(valid_case("pruned_transaction", prune(sample_tx("a")), "the pruned form of the first transaction"))
    valid.append(valid_case("pruned_transaction", prune(sample_tx("c", n_in=MAX_INPUTS, n_out=MAX_OUTPUTS, proof=1000,
                                                                  extra=MAX_EXTRA)), "the pruned form at the size limits"))
    valid.append(valid_case("pruned_block", {"header": hdr, "coinbase": cb, "transactions": [prune(t) for t in txs]},
                            "the pruned form of the block above"))

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
        invalid_case("input", enc_input(sample_input("a"))[:-1], "short read", "a key image one byte short"),
        invalid_case("input", enc_input(sample_input("a")) + b"\x00", "trailing bytes", "an input with a trailing byte"),
        invalid_case("transaction", enc_tx({**tx, "rings": [list(range(MAX_RING + 1)), tx["rings"][1]]}),
                     "count out of range", "a ring of 17"),
        invalid_case("transaction", enc_tx({**tx, "rings": tx["rings"][:1]}), "count out of range",
                     "one ring for two inputs"),
        invalid_case("transaction", enc_tx({**tx, "rings": tx["rings"] + tx["rings"][:1]}), "count out of range",
                     "three rings for two inputs"),
        invalid_case("transaction", enc_tx({**tx, "rings": []}), "count out of range", "no rings at all"),
        invalid_case("coinbase", enc_coinbase({**sample_coinbase(1), "outputs": []}), "count out of range",
                     "a coinbase with no outputs"),
        invalid_case("coinbase", enc_coinbase({**sample_coinbase(1),
                                               "outputs": [sample_cb_output(f"c{i}", 1) for i in range(MAX_COINBASE_OUTPUTS + 1)]}),
                     "count out of range", "a coinbase with too many outputs"),
        invalid_case("block", enc_header(sample_header("a")) + enc_coinbase(sample_coinbase(1)) + w_u32(MAX_BLOCK_TXS + 1),
                     "count out of range", "more transactions than the block maximum"),
        invalid_case("block", enc_block({"header": sample_header("a"), "coinbase": sample_coinbase(1), "transactions": []})[:-2],
                     "short read", "a block whose transaction count is cut short"),
        invalid_case("pruned_transaction", enc_pruned_tx(prune(tx))[:-1], "short read",
                     "a pruned transaction one byte short"),
        invalid_case("pruned_transaction", enc_pruned_tx(prune(tx)) + b"\x00", "trailing bytes",
                     "a pruned transaction with a trailing byte"),
        invalid_case("pruned_transaction", good, "trailing bytes",
                     "a FULL transaction is not a pruned one: its rings and proofs are extra bytes"),
        invalid_case("pruned_block", enc_header(sample_header("a")) + enc_coinbase(sample_coinbase(1))
                     + w_u32(MAX_BLOCK_TXS + 1), "count out of range",
                     "a pruned block with more transactions than the maximum"),
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
    tcases = []
    for t in txs:
        p = prune(t)
        assert pruned_tx_id(p) == tx_id(t)              # the pruned form has the same id
        assert enc_tx(t) == enc_tx_prefix(t) + enc_prunable(t)
        tcases.append({"transaction": t, "bytes": enc_tx(t).hex(), "prefix_bytes": enc_tx_prefix(t).hex(),
                       "prunable_bytes": enc_prunable(t).hex(),
                       "prunable_hash": p["prunable_hash"], "id": tx_id(t).hex(),
                       "pruned_bytes": enc_pruned_tx(p).hex(), "pruned_id": pruned_tx_id(p).hex()})
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
                "SHA-256 test chain, transaction ids (SHA-256 over 'tenero tx v2', the serialized PREFIX and the hash of "
                "the prunable part, which is SHA-256 over 'tenero tx prunable v2' and the serialized prunable part; the "
                "pruned form has the same id) and coinbase ids (SHA-256 over a tag and the serialized bytes). "
                "The mix in `header` is arbitrary here: these vectors test the id arithmetic, not a solved proof of work.",
                {"tags": {"header": HEADER_TAG.decode(), "transaction": TX_TAG.decode(),
                          "prunable": PRUNABLE_TAG.decode(), "coinbase": COINBASE_TAG.decode()},
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


def dynamic_min_fee(size, base_reward, median):
    """The minimum fee, in units, of a transaction of `size` bytes in a block whose base reward is
    `base_reward` and whose block-size median is `median` (both known before the block):
    max(1, ceil(base_reward * FEE_REFERENCE_WEIGHT * size / median^2)). None if it does not fit in a u64."""
    assert median > 0
    fee = max(1, -(-(base_reward * FEE_REFERENCE_WEIGHT * size) // (median * median)))
    return fee if fee <= U64_MAX else None


def oversize_penalty(base, size, median):
    """ceil(base * over^2 / median^2) for size > median, else 0 (the same rule as version 1)."""
    if size <= median or base == 0:
        return 0
    over = size - median
    return -(-(base * over * over) // (median * median))


def block_median(sizes, floor):
    """The upper median of the recent block sizes (the element at index len // 2 once sorted), at least
    `floor`; with no sizes it is `floor`."""
    s = sorted(sizes)
    return max(floor, s[len(s) // 2] if s else 0)


def median_at(sizes_by_position, pos, floor, window=MEDIAN_WINDOW):
    """The median the block at position `pos` is judged against: the `window` positions before it, from
    position 1 on (position 0 is the genesis block and is never counted)."""
    return block_median(sizes_by_position[max(1, pos - window):pos], floor)


def fees_vectors():
    rewards = [20 * UNIT, 10 * UNIT, UNIT // 2, 1]
    medians = [MIN_BLOCK_MEDIAN, 300_000, 600_000, 10_000_000]
    sizes = [0, 1, 427, 2500, 100_000, 300_000]
    fee_cases = [{"base_reward": r, "median": m, "size": s, "fee": dynamic_min_fee(s, r, m)}
                 for r in rewards for m in medians for s in sizes]
    fee_cases += [{"base_reward": r, "median": m, "size": s, "fee": dynamic_min_fee(s, r, m)} for r, m, s in (
        (2 ** 63, MIN_BLOCK_MEDIAN, 2 ** 32),        # does not fit in a u64: null
        (U64_MAX, 1, 10 ** 12),                      # does not fit in a u64: null
        (U64_MAX, MIN_BLOCK_MEDIAN, U64_MAX),        # does not fit: null
        (20 * UNIT, 1, 1),                           # a median of 1 byte
    )]
    floor = MIN_BLOCK_MEDIAN
    size_lists = [[], [100] * 10, [1_000_000] * 10, [0] * 9 + [10_000_000], [200_000] * 4 + [0] * 6,
                  [200_000] * 5 + [0] * 5, [floor - 1] * 10, [floor + 1] * 10, [floor] * 10, [floor + 1] * 5 + [0] * 5]
    median_cases = [{"floor": floor, "sizes": sl, "median": block_median(sl, floor)} for sl in size_lists]
    by_position = [0, 300, 500_000, 0, 620_000, 10, 700_000, 400_000, 350_000, 900, 450_000, 5, 320_000, 600_000]
    history = {"sizes_by_position_0_is_genesis": by_position,
               "windowed": [{"pos": pos, "median": median_at(by_position, pos, floor)}
                            for pos in range(1, len(by_position) + 1)]}
    penalty_cases = [{"base": b, "median": m, "size": s, "penalty": oversize_penalty(b, s, m)}
                     for b in (20 * UNIT, UNIT // 2) for m in (MIN_BLOCK_MEDIAN, 1_000_000)
                     for s in (m - 1, m, m + 1, m * 3 // 2, m * 2, 777_777)]
    return wrap("v2_fees",
                "The dynamic minimum fee of the version 2 rules (docs/CONSENSUS_V2.md section 8): fee >= max(1, "
                "ceil(base_reward * FEE_REFERENCE_WEIGHT * size / median^2)), in units of 10^-8 coins, where base_reward "
                "is the block's reward before any penalty and median is the block-size median that block is judged "
                "against. `fee` is null when the result does not fit in a u64. Also the oversize penalty in the new units "
                "(the version 1 rule, unchanged), and the block-size median with the version 2 floor of 150,000 bytes "
                "(the rule of version 1, with a different floor).",
                {"constants": {"FEE_REFERENCE_WEIGHT": FEE_REFERENCE_WEIGHT, "MIN_BLOCK_MEDIAN": MIN_BLOCK_MEDIAN,
                               "MEDIAN_WINDOW": MEDIAN_WINDOW, "DECIMALS": DECIMALS},
                 "dynamic_min_fee": fee_cases, "penalty": penalty_cases,
                 "median": median_cases, "median_history": history})


def emission_vectors():
    if ROOT not in sys.path:
        sys.path.insert(0, ROOT)
    from tenero.chain import Blockchain          # the version 1 reference: its arithmetic does not care about units

    def rows(bc, heights):
        return [{"height": h, "scheduled": bc.scheduled_reward(h), "issued_before": bc.issued_before(h),
                 "main_reward": bc.main_reward_at(h), "reward": bc.reward_at(h), "in_tail": bc.in_tail(h)}
                for h in heights]

    sets = {}
    p = {"initial_reward": 20 * UNIT, "halving_interval": 525_600, "max_supply": 20_000_000 * UNIT,
         "tail_reward": UNIT // 2}
    bc = Blockchain(**p)
    era = 525_600
    heights = sorted({1, 2, 3, era - 1, era, era + 1, era + 2, 2 * era, 2 * era + 1, 3 * era, 3 * era + 1,
                      4 * era, 4 * era + 1, 5 * era, 5 * era + 1, 2_334_399, 2_334_400, 2_334_401, 2_334_402,
                      3_000_000, 10_000_000, 2 ** 40})
    sets["default_8_decimals"] = {"params": p, "main_emission_end_from_1": bc.main_emission_end(1),
                                  "rows": rows(bc, heights)}
    q = {"initial_reward": 100_000 * 10 ** 4, "halving_interval": 10, "max_supply": 1_375_000 * 10 ** 4,
         "tail_reward": 100 * 10 ** 4}
    bq = Blockchain(**q)
    sets["trimmed_final_reward_scaled"] = {"params": q, "main_emission_end_from_1": bq.main_emission_end(1),
                                           "rows": rows(bq, range(1, 31))}
    # the last three rows of the default schedule must reproduce the version 1 numbers times 10^4
    return wrap("v2_emission",
                "Block rewards in the version 2 units (8 decimals: 1 coin = 10^8 units, the 20,000,000-coin cap is "
                "2 * 10^15 units). The rules are those of version 1 (docs/CONSENSUS.md section 5); the schedule is the "
                "same as v1's default with every amount multiplied by 10^4, and a small schedule that trims its final "
                "reward. These vectors guard the scaling: a wrong constant or an overflow shows here.",
                {"sets": sets})


BUILDERS = {"v2_serialization": serialization_vectors, "v2_ids": ids_vectors,
            "v2_merkle": merkle_vectors, "v2_genesis": genesis_vectors,
            "v2_fees": fees_vectors, "v2_emission": emission_vectors}


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
