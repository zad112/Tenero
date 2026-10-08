"""The version 3 data model of the `gamma` network (docs/CONSENSUS_V2.md section 15; docs/FCMP_CARROT_PLAN.md), as
an independent, standard-library Python reference that the Rust code is checked against (CLAUDE.md rule 1).

Version 3 is FCMP++ and Carrot from genesis. Against version 2 it changes:
  * an OUTPUT is 91 bytes: its Carrot ephemeral key moved to the transaction, which carries ONE key when it has two
    outputs and one per output otherwise (the count follows from the outputs, so it is never written);
  * `extra` is replaced by a fixed 8-byte ENCRYPTED PAYMENT ID (every transaction looks alike);
  * the PRUNABLE part is the REFERENCE HEIGHT (the block whose curve-tree root the membership proof uses) and the
    proof bytes; there are no rings;
  * a transaction's WEIGHT is its prefix bytes plus a quarter of its prunable bytes (rounded up); block limits and the
    block-size median count weight, the minimum fee counts real bytes, at a third of version 2's rate per byte;
  * new domain tags ("... v3"), so no version 3 object can be taken for a version 2 one;
  * the SHAPE rules (sorted outputs, the ephemeral keys, ascending key images) and the curve tree's SCHEDULE (which
    outputs enter the tree when) are consensus.

The cryptography itself (Carrot, the curve tree's hashing, FCMP++) is not here: its vectors come from Monero's own
code (tests/vectors/carrot_monero.json, curve_tree_monero.json, upstream_monero_fcmp_pp.json).

    python reference/tools/make_vectors_v3.py --check     # the committed files match this reference
    python reference/tools/make_vectors_v3.py --write     # regenerate them (a consensus change: say why in the commit)
"""
import argparse
import hashlib
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import make_vectors_v2 as v2  # noqa: E402  (the byte reader and writer, Merkle, the fee and median rules)
from make_vectors_v2 import (  # noqa: E402
    DecodeError, Reader, w_u16, w_u32, w_u64, w_fixed, w_var, w_list, sha256, merkle_root, U64_MAX)

ROOT = v2.ROOT
VECTOR_DIR = v2.VECTOR_DIR
SCHEMA = 1
FILES = ("v3_serialization", "v3_ids", "v3_genesis", "v3_weight", "v3_shape", "v3_tree_schedule")

# ---------------------------------------------------------------- constants
VERSION = 3
MIN_INPUTS = v2.MIN_INPUTS
MAX_TX_SIZE = v2.MAX_TX_SIZE
INPUT_COUNT_GUARD = v2.INPUT_COUNT_GUARD
MIN_OUTPUTS = v2.MIN_OUTPUTS
MAX_OUTPUTS = v2.MAX_OUTPUTS
MIN_COINBASE_OUTPUTS = v2.MIN_COINBASE_OUTPUTS
MAX_COINBASE_OUTPUTS = v2.MAX_COINBASE_OUTPUTS
MAX_EXTRA = v2.MAX_EXTRA            # the COINBASE's extra only
MAX_PROOF = v2.MAX_PROOF
MAX_BLOCK_TXS = v2.MAX_BLOCK_TXS
OUTPUT_SIZE = 91
PROOF_WEIGHT_DIVISOR = 4           # a prunable byte weighs a quarter
# the most REAL transaction bytes a block may carry, whatever its weight (owner's limit pending; my recommendation,
# 2026-10-08): the quarter weight of proofs would otherwise let a 4 MiB-weight block reach about 16.5 MB, at the edge
# of the 16 MiB network frame, and cost archive nodes and verifiers four times the bytes
MAX_BLOCK_BYTES = 12 * 1024 * 1024
FEE_REFERENCE_WEIGHT = 1000        # a third of version 2's 3000 (owner, 2026-10-08): a typical v3 transaction costs what a v2 one did
MAX_REFERENCE_AGE = 1440           # a transaction's reference block is at most this many blocks below the tip
COINBASE_MATURITY = 60
SPEND_MATURITY = 10

HEADER_TAG = b"tenero block header v3"
TX_TAG = b"tenero tx v3"
PRUNABLE_TAG = b"tenero tx prunable v3"
COINBASE_TAG = b"tenero coinbase v3"
GENESIS_TAG = b"tenero genesis v3"
GENESIS_ID_TAG = b"tenero genesis id v3"
MESSAGE_TAG = b"tenero fcmp++ message v3"
GAMMA_LABEL = "tenero gamma network 1"
DEV_LABEL = "tenero development network v3"
TEST_LABEL = "tenero test network v3"


def n_ephemeral_keys(n_outputs):
    """One ephemeral key for a 2-output transaction (both outputs share it), else one per output."""
    return 1 if n_outputs == 2 else n_outputs


# ---------------------------------------------------------------- the objects

def enc_output(o):
    return (w_fixed(o["onetime_address"], 32) + w_fixed(o["amount_commitment"], 32) + w_fixed(o["amount_enc"], 8)
            + w_fixed(o["view_tag"], 3) + w_fixed(o["anchor_enc"], 16))


def dec_output(r):
    return {"onetime_address": r.fixed(32), "amount_commitment": r.fixed(32), "amount_enc": r.fixed(8),
            "view_tag": r.fixed(3), "anchor_enc": r.fixed(16)}


def enc_tx_prefix(t):
    assert len(t["ephemeral_pubkeys"]) == n_ephemeral_keys(len(t["outputs"])), "the ephemeral key count follows the outputs"
    return (w_u16(t["version"]) + w_list(t["inputs"], lambda i: w_fixed(i["key_image"], 32))
            + w_list(t["outputs"], enc_output) + b"".join(w_fixed(k, 32) for k in t["ephemeral_pubkeys"])
            + w_u64(t["fee"]) + w_fixed(t["encrypted_payment_id"], 8))


def dec_tx_prefix(r):
    version = r.u16()
    inputs = [{"key_image": r.fixed(32)} for _ in range(r.count(MIN_INPUTS, INPUT_COUNT_GUARD))]
    outputs = [dec_output(r) for _ in range(r.count(MIN_OUTPUTS, MAX_OUTPUTS))]
    keys = [r.fixed(32) for _ in range(n_ephemeral_keys(len(outputs)))]
    fee = r.u64()
    return {"version": version, "inputs": inputs, "outputs": outputs, "ephemeral_pubkeys": keys, "fee": fee,
            "encrypted_payment_id": r.fixed(8)}


def enc_prunable(t):
    return w_u64(t["reference_height"]) + w_var(t["proof_data"])


def dec_prunable(r):
    return {"reference_height": r.u64(), "proof_data": r.var(MAX_PROOF)}


def enc_tx(t):
    return enc_tx_prefix(t) + enc_prunable(t)


def dec_tx(r):
    start = r.pos
    t = dec_tx_prefix(r)
    t.update(dec_prunable(r))
    if r.pos - start > MAX_TX_SIZE:
        raise DecodeError("length over maximum")
    return t


def prunable_hash(t):
    return sha256(PRUNABLE_TAG, enc_prunable(t))


def enc_pruned_tx(p):
    return enc_tx_prefix(p) + w_fixed(p["prunable_hash"], 32)


def dec_pruned_tx(r):
    p = dec_tx_prefix(r)
    p["prunable_hash"] = r.fixed(32)
    return p


def prune(t):
    p = {k: v for k, v in t.items() if k not in ("reference_height", "proof_data")}
    p["prunable_hash"] = prunable_hash(t).hex()
    return p


# the coinbase, its outputs (each with its own ephemeral key: coinbase keys are always distinct) and the header keep
# their version 2 encodings; only the version number and the tags differ
enc_cb_output, dec_cb_output = v2.enc_cb_output, v2.dec_cb_output
enc_coinbase, dec_coinbase = v2.enc_coinbase, v2.dec_coinbase
enc_header, dec_header = v2.enc_header, v2.dec_header


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
    "output": (enc_output, dec_output), "transaction": (enc_tx, dec_tx),
    "pruned_transaction": (enc_pruned_tx, dec_pruned_tx), "coinbase_output": (enc_cb_output, dec_cb_output),
    "coinbase": (enc_coinbase, dec_coinbase), "header": (enc_header, dec_header), "block": (enc_block, dec_block),
    "pruned_block": (enc_pruned_block, dec_pruned_block),
}


def encode(kind, obj):
    return KINDS[kind][0](obj)


def decode(kind, data):
    r = Reader(data)
    obj = KINDS[kind][1](r)
    r.finish()
    return obj


# ---------------------------------------------------------------- hashes and ids

def header_hash(h):
    return sha256(HEADER_TAG, w_u16(h["version"]), bytes.fromhex(h["prev_id"]), w_u64(h["timestamp"]),
                  bytes.fromhex(h["tx_root"]))


def block_id(h, pow_kind):
    hh = header_hash(h)
    if pow_kind == "matmul":
        return sha256(sha256(hh, w_u64(h["nonce"])), bytes.fromhex(h["mix"]))
    if pow_kind == "sha256":
        return sha256(hh, w_u64(h["nonce"]))
    raise ValueError(pow_kind)


def tx_id(t):
    return sha256(TX_TAG, enc_tx_prefix(t), prunable_hash(t))


def pruned_tx_id(p):
    return sha256(TX_TAG, enc_tx_prefix(p), bytes.fromhex(p["prunable_hash"]))


def coinbase_id(c):
    return sha256(COINBASE_TAG, enc_coinbase(c))


def genesis_header(label):
    return {"version": VERSION, "prev_id": "00" * 32, "timestamp": 0,
            "tx_root": sha256(GENESIS_TAG, label.encode()).hex(), "nonce": 0, "mix": "00" * 64}


def genesis_id(label):
    """The chain id: every signature on the chain is bound to it."""
    return sha256(GENESIS_ID_TAG, enc_header(genesis_header(label)))


def proof_message(chain_id, t, pseudo_outs, range_proof):
    """The 32 bytes the FCMP++ spend-authorisation proofs sign (`signable_tx_hash`): it binds the chain, the whole
    prefix (key images, outputs, ephemeral keys, fee, payment ID), the reference height, the pseudo-outputs and the range
    proof. FCMP++ requires the prefix, the RingCT base and the pseudo-outputs to be bound; the rest is ours."""
    return sha256(MESSAGE_TAG, chain_id, enc_tx_prefix(t), w_u64(t["reference_height"]),
                  b"".join(bytes.fromhex(p) for p in pseudo_outs), range_proof)


# ---------------------------------------------------------------- weight and limits

def tx_weight(t):
    """prefix bytes + ceil(prunable bytes / 4)."""
    return len(enc_tx_prefix(t)) + -(-len(enc_prunable(t)) // PROOF_WEIGHT_DIVISOR)


def block_weight(txs):
    """A block's weight: the sum of its transactions' weights (the coinbase is not counted, as in version 2)."""
    return sum(tx_weight(t) for t in txs)


# the median, the limit and the oversize penalty are version 2's functions, applied to weights instead of sizes


def dynamic_min_fee(size, base_reward, median):
    """Version 2's formula with version 3's FEE_REFERENCE_WEIGHT, applied to the transaction's REAL size and the
    (weight) median: max(1, ceil(base_reward * FEE_REFERENCE_WEIGHT * size / median^2)); None if it does not fit a u64."""
    assert median > 0
    fee = max(1, -(-(base_reward * FEE_REFERENCE_WEIGHT * size) // (median * median)))
    return fee if fee <= U64_MAX else None


block_limit = v2.block_limit


def block_too_large(weight, size, median):
    """Too large: its weight over version 2's limit of the (weight) median, or its real transaction bytes over
    MAX_BLOCK_BYTES."""
    return weight > block_limit(median) or size > MAX_BLOCK_BYTES


oversize_penalty = v2.oversize_penalty
block_median = v2.block_median


# ---------------------------------------------------------------- the shape rules

def shape_error(t):
    """The first shape rule `t` breaks (decoding already fixed the counts), or None. Point validity (key images,
    output keys, commitments) is checked by the proof code, not here."""
    kis = [i["key_image"] for i in t["inputs"]]
    if any(kis[i] >= kis[i + 1] for i in range(len(kis) - 1)):
        return "key images not strictly ascending"
    kos = [o["onetime_address"] for o in t["outputs"]]
    if any(kos[i] >= kos[i + 1] for i in range(len(kos) - 1)):
        return "outputs not strictly ascending by one-time address"
    keys = t["ephemeral_pubkeys"]
    if any(k == "00" * 32 for k in keys):
        return "an ephemeral key is zero"
    if len(keys) > 1 and len(set(keys)) != len(keys):
        return "ephemeral keys repeat"
    return None


def coinbase_shape_error(c):
    """A coinbase's outputs: strictly ascending by one-time address, ephemeral keys non-zero and all different."""
    kos = [o["onetime_address"] for o in c["outputs"]]
    if any(kos[i] >= kos[i + 1] for i in range(len(kos) - 1)):
        return "outputs not strictly ascending by one-time address"
    keys = [o["ephemeral_pubkey"] for o in c["outputs"]]
    if any(k == "00" * 32 for k in keys):
        return "an ephemeral key is zero"
    if len(set(keys)) != len(keys):
        return "ephemeral keys repeat"
    return None


def reference_ok(reference_height, block_height, tree_leaves_at_reference):
    """A transaction in the block at `block_height` may reference a block from MAX_REFERENCE_AGE below up to the block
    just below it, and only one whose tree is not empty."""
    return (block_height - MAX_REFERENCE_AGE <= reference_height <= block_height - 1
            and tree_leaves_at_reference > 0)


# ---------------------------------------------------------------- the curve tree's schedule

def tree_schedule(blocks):
    """`blocks[h]` = (number of coinbase outputs, number of other outputs) of the block at height h. Global output
    indexes run block by block: the coinbase's outputs, then the transactions' outputs (version 2's rule). An output is
    spendable in block B once B >= its height + maturity (60 for a coinbase output, 10 otherwise), and it ENTERS THE TREE
    when the block just before that is applied, so that the tree after block r holds exactly the outputs spendable in
    block r + 1. Returns, for each height, the global indexes of the outputs that enter, in ascending order."""
    first = []
    n = 0
    for cb, other in blocks:
        first.append(n)
        n += cb + other
    entering = []
    for h in range(len(blocks)):
        idx = []
        hc = h + 1 - COINBASE_MATURITY      # a coinbase whose outputs become spendable in block h + 1
        if hc >= 0:
            idx += range(first[hc], first[hc] + blocks[hc][0])
        ho = h + 1 - SPEND_MATURITY
        if ho >= 0:
            idx += range(first[ho] + blocks[ho][0], first[ho] + blocks[ho][0] + blocks[ho][1])
        entering.append(sorted(idx))
    return entering


# ---------------------------------------------------------------- deterministic sample data

def det_bytes(label, n):
    out, i = b"", 0
    while len(out) < n:
        out += hashlib.sha256(f"tenero vectors v3 {label} {i}".encode()).digest()
        i += 1
    return out[:n]


def h(label, n):
    return det_bytes(label, n).hex()


def sample_output(tag):
    return {"onetime_address": h(tag + " ko", 32), "amount_commitment": h(tag + " ca", 32),
            "amount_enc": h(tag + " enc", 8), "view_tag": h(tag + " vt", 3), "anchor_enc": h(tag + " anchor", 16)}


def sample_tx(tag, n_in=2, n_out=2, proof=200, fee=123456, reference_height=777, sort=True):
    inputs = [{"key_image": h(f"{tag} in{i} ki", 32)} for i in range(n_in)]
    outputs = [sample_output(f"{tag} out{i}") for i in range(n_out)]
    if sort:
        inputs.sort(key=lambda i: i["key_image"])
        outputs.sort(key=lambda o: o["onetime_address"])
    return {"version": VERSION, "inputs": inputs, "outputs": outputs,
            "ephemeral_pubkeys": [h(f"{tag} de{i}", 32) for i in range(n_ephemeral_keys(n_out))],
            "fee": fee, "encrypted_payment_id": h(tag + " pid", 8),
            "reference_height": reference_height, "proof_data": h(tag + " proof", proof)}


def sample_coinbase(height, n_out=1, amount=2_000_000_000):
    outs = [v2.sample_cb_output(f"v3 cb {height} {i}", amount + i) for i in range(n_out)]
    outs.sort(key=lambda o: o["onetime_address"])
    return {"version": VERSION, "height": height, "outputs": outs, "extra": h(f"cb {height} extra", 8)}


def sample_header(tag, txs=(), nonce=42, timestamp=1_700_000_060):
    return {"version": VERSION, "prev_id": h(tag + " prev", 32), "timestamp": timestamp,
            "tx_root": merkle_root([bytes.fromhex(x) for x in txs]).hex(), "nonce": nonce, "mix": h(tag + " mix", 64)}


def sized_tx(tag, size, n_in=240):
    """A transaction of exactly `size` bytes; 240 inputs, so that its proof bytes stay under MAX_PROOF."""
    base = len(enc_tx(sample_tx(tag, n_in=n_in, n_out=MAX_OUTPUTS, proof=0)))
    t = sample_tx(tag, n_in=n_in, n_out=MAX_OUTPUTS, proof=size - base)
    assert len(enc_tx(t)) == size
    return t


# ---------------------------------------------------------------- the vector files

def wrap(name, description, body):
    return {"schema": SCHEMA, "name": name, "description": description, **body}


def valid_case(kind, obj, note=""):
    data = encode(kind, obj)
    assert decode(kind, data) == obj, kind
    return {"kind": kind, "note": note, "object": obj, "hex": data.hex()}


def invalid_case(kind, data, error, note):
    try:
        decode(kind, bytes(data))
    except DecodeError as e:
        assert str(e) == error, (note, str(e), error)
    else:
        raise AssertionError(f"decoded: {note}")
    return {"kind": kind, "note": note, "hex": bytes(data).hex(), "error": error}


def serialization_vectors():
    two = sample_tx("two")
    three = sample_tx("three", n_in=1, n_out=3)
    big = sample_tx("sixteen", n_in=3, n_out=MAX_OUTPUTS)
    cb = sample_coinbase(5, n_out=3)
    hdr = sample_header("hdr", txs=[tx_id(two).hex()])
    block = {"header": hdr, "coinbase": cb, "transactions": [two, three]}
    valid = [
        valid_case("output", sample_output("o"), "91 bytes: no ephemeral key"),
        valid_case("transaction", two, "2 outputs: ONE ephemeral key"),
        valid_case("transaction", three, "3 outputs: three ephemeral keys"),
        valid_case("transaction", big, "16 outputs: sixteen ephemeral keys"),
        valid_case("transaction", sample_tx("noproof", proof=0), "empty proof bytes (decodes; the proof check refuses it)"),
        valid_case("transaction", sized_tx("max", MAX_TX_SIZE), "exactly MAX_TX_SIZE"),
        valid_case("pruned_transaction", prune(two), "the pruned form: prefix and prunable hash"),
        valid_case("coinbase", cb),
        valid_case("header", hdr),
        valid_case("block", block),
        valid_case("pruned_block", {"header": hdr, "coinbase": cb, "transactions": [prune(two), prune(three)]}),
    ]
    two_bytes = encode("transaction", two)
    prefix_len = len(enc_tx_prefix(two))
    invalid = [
        invalid_case("transaction", two_bytes[:prefix_len - 1], "short read", "the prefix cut inside the payment ID"),
        invalid_case("transaction", two_bytes[:-1], "short read", "the proof cut"),
        invalid_case("transaction", two_bytes + b"\x00", "trailing bytes", "a byte after the transaction"),
        invalid_case("transaction", w_u16(3) + w_u32(0), "count out of range", "no inputs"),
        invalid_case("transaction", w_u16(3) + w_u32(1) + bytes(32) + w_u32(1), "count out of range", "one output"),
        invalid_case("transaction", w_u16(3) + w_u32(1) + bytes(32) + w_u32(17), "count out of range", "17 outputs"),
        invalid_case("transaction", encode("transaction", sized_tx("over", MAX_TX_SIZE + 1)), "length over maximum",
                     "MAX_TX_SIZE + 1"),
        invalid_case("transaction", two_bytes[:prefix_len] + w_u64(1) + w_u32(MAX_PROOF + 1),
                     "length over maximum", "a proof over MAX_PROOF"),
        # a 2-output prefix read as if it had two keys would run into the fee: the count is implied, never written
        invalid_case("output", encode("output", sample_output("x"))[:90], "short read", "a 90-byte output"),
    ]
    return wrap("v3_serialization", "Version 3 (gamma) encodings: valid objects with their exact bytes, and invalid "
                "encodings with the error a decoder must give. Every object decodes to itself and encodes to its bytes.",
                {"limits": {"OUTPUT_SIZE": OUTPUT_SIZE, "MAX_TX_SIZE": MAX_TX_SIZE, "MAX_PROOF": MAX_PROOF,
                            "MAX_OUTPUTS": MAX_OUTPUTS, "MAX_EXTRA_COINBASE": MAX_EXTRA},
                 "valid": valid, "invalid": invalid})


def ids_vectors():
    chain = genesis_id(GAMMA_LABEL)
    cases = []
    for tag, t in (("2 outputs", sample_tx("id2")), ("4 outputs", sample_tx("id4", n_in=3, n_out=4))):
        pseudo = [h(f"{tag} pseudo {i}", 32) for i in range(len(t["inputs"]))]
        bp = det_bytes(f"{tag} bp", 650)
        cases.append({"note": tag, "transaction": t, "prunable_hash": prunable_hash(t).hex(), "tx_id": tx_id(t).hex(),
                      "pruned_tx_id": pruned_tx_id(prune(t)).hex(),
                      "proof_message": {"chain_id": chain.hex(), "pseudo_outs": pseudo, "range_proof": bp.hex(),
                                        "message": proof_message(chain, t, pseudo, bp).hex()}})
    cb = sample_coinbase(9, n_out=2)
    hdr = sample_header("idh", txs=[coinbase_id(cb).hex()])
    return wrap("v3_ids", "Version 3 ids: the prunable hash, the transaction id (the same from the pruned form), the "
                "message the FCMP++ spend proofs sign, the coinbase id, the header hash and the block id.",
                {"tags": {k: v.decode() for k, v in (("header", HEADER_TAG), ("tx", TX_TAG), ("prunable", PRUNABLE_TAG),
                                                     ("coinbase", COINBASE_TAG), ("message", MESSAGE_TAG))},
                 "transactions": cases,
                 "coinbase": {"coinbase": cb, "coinbase_id": coinbase_id(cb).hex()},
                 "header": {"header": hdr, "header_hash": header_hash(hdr).hex(),
                            "block_id_matmul": block_id(hdr, "matmul").hex(),
                            "block_id_sha256": block_id(hdr, "sha256").hex()}})


def genesis_vectors():
    cases = [{"label": lab, "header": genesis_header(lab), "header_hex": enc_header(genesis_header(lab)).hex(),
              "chain_id": genesis_id(lab).hex()} for lab in (GAMMA_LABEL, DEV_LABEL, TEST_LABEL)]
    assert len({c["chain_id"] for c in cases}) == 3
    return wrap("v3_genesis", "The genesis header and the chain id of the version 3 networks: gamma, dev and test.",
                {"cases": cases})


def weight_vectors():
    txs = [("2 in, 2 out, a 3-layer proof's size", sample_tx("w1", n_in=2, n_out=2, proof=6680)),
           ("1 in, 2 out", sample_tx("w2", n_in=1, n_out=2, proof=5300)),
           ("4 in, 16 out", sample_tx("w3", n_in=4, n_out=16, proof=12_000)),
           ("prunable bytes a multiple of 4 plus 1 (401: weight rounds up)", sample_tx("w4", proof=401 - 12)),
           ("no proof bytes", sample_tx("w5", proof=0))]
    tx_cases = [{"note": n, "size": len(enc_tx(t)), "prefix_size": len(enc_tx_prefix(t)),
                 "prunable_size": len(enc_prunable(t)), "weight": tx_weight(t), "hex": enc_tx(t).hex()} for n, t in txs]
    block_cases = [{"weights": [c["weight"] for c in tx_cases[:k]],
                    "block_weight": sum(c["weight"] for c in tx_cases[:k])} for k in range(len(tx_cases) + 1)]
    # the fee of each transaction at a few medians (real size, median of weights)
    fee_cases = [{"size": c["size"], "base_reward": r, "median": m, "fee": dynamic_min_fee(c["size"], r, m)}
                 for c in tx_cases for r in (20 * v2.UNIT, v2.UNIT // 2) for m in (v2.MIN_BLOCK_MEDIAN, 1_000_000)]
    big = v2.MAX_BLOCK_BODY
    limit_cases = [{"weight": w, "size": z, "median": m, "too_large": block_too_large(w, z, m)}
                   for w, z, m in ((300_000, 1_000_000, 150_000), (300_001, 1_000_000, 150_000),
                                   (big, MAX_BLOCK_BYTES, big), (big + 1, MAX_BLOCK_BYTES, big),
                                   (big, MAX_BLOCK_BYTES + 1, big), (3_000_000, MAX_BLOCK_BYTES + 1, 10 * big))]
    return wrap("v3_weight", "Version 3 weight: prefix bytes + ceil(prunable bytes / 4); a block's weight is the sum of "
                "its transactions'. Block limits, the median and the oversize penalty are version 2's functions of "
                "weight, and a block's real transaction bytes are at most MAX_BLOCK_BYTES; the minimum fee is version "
                "2's formula with FEE_REFERENCE_WEIGHT 1000 (version 2: 3000), of the REAL size and the (weight) median.",
                {"proof_weight_divisor": PROOF_WEIGHT_DIVISOR, "fee_reference_weight": FEE_REFERENCE_WEIGHT,
                 "max_block_bytes": MAX_BLOCK_BYTES, "transactions": tx_cases, "blocks": block_cases,
                 "fees": fee_cases, "limits": limit_cases})


def shape_vectors():
    good2 = sample_tx("s2")
    good4 = sample_tx("s4", n_in=3, n_out=4)

    def variant(base, note, change):
        t = json.loads(json.dumps(base))
        change(t)
        return {"note": note, "hex": enc_tx(t).hex(), "error": shape_error(t)}

    cases = [
        {"note": "a 2-output transaction in order", "hex": enc_tx(good2).hex(), "error": None},
        {"note": "a 4-output transaction in order", "hex": enc_tx(good4).hex(), "error": None},
        variant(good2, "outputs out of order", lambda t: t["outputs"].reverse()),
        variant(good2, "the same one-time address twice",
                lambda t: t["outputs"].__setitem__(1, dict(t["outputs"][0]))),
        variant(good4, "key images out of order", lambda t: t["inputs"].reverse()),
        variant(good4, "a key image twice", lambda t: t["inputs"].__setitem__(1, dict(t["inputs"][0]))),
        variant(good4, "two ephemeral keys the same",
                lambda t: t["ephemeral_pubkeys"].__setitem__(2, t["ephemeral_pubkeys"][0])),
        variant(good2, "a zero ephemeral key", lambda t: t["ephemeral_pubkeys"].__setitem__(0, "00" * 32)),
    ]
    assert [c["error"] is None for c in cases] == [True, True] + [False] * 6
    cb = sample_coinbase(3, n_out=3)
    cb_bad = json.loads(json.dumps(cb))
    cb_bad["outputs"][2]["ephemeral_pubkey"] = cb_bad["outputs"][0]["ephemeral_pubkey"]
    cb_cases = [{"note": "three outputs in order, distinct keys", "hex": enc_coinbase(cb).hex(), "error": None},
                {"note": "a repeated ephemeral key", "hex": enc_coinbase(cb_bad).hex(),
                 "error": coinbase_shape_error(cb_bad)}]
    ref = [{"reference_height": r, "block_height": b, "tree_leaves": n, "ok": reference_ok(r, b, n)}
           for r, b, n in ((99, 100, 5), (100, 100, 5), (0, MAX_REFERENCE_AGE, 5), (0, MAX_REFERENCE_AGE + 1, 5),
                           (0, 100, 5), (99, 100, 0), (0, 1, 1), (0, 0, 1),
                           (2000 - MAX_REFERENCE_AGE, 2000, 9), (2000 - MAX_REFERENCE_AGE - 1, 2000, 9), (1999, 2000, 9))]
    return wrap("v3_shape", "Version 3 shape rules (consensus): key images strictly ascending; outputs strictly ascending "
                "by one-time address; ephemeral keys non-zero and, when there are several, all different; a coinbase's "
                "outputs likewise. And which reference heights a transaction may use (MAX_REFERENCE_AGE, a non-empty tree).",
                {"max_reference_age": MAX_REFERENCE_AGE, "transactions": cases, "coinbases": cb_cases,
                 "reference": ref})


def tree_schedule_vectors():
    # 140 blocks: one to three coinbase outputs, and from block 3 some transactions with outputs
    blocks = [(1 + (h % 3), (0 if h < 3 else (h * 7) % 9)) for h in range(140)]
    entering = tree_schedule(blocks)
    total = 0
    cumulative = []
    for e in entering:
        total += len(e)
        cumulative.append(total)
    return wrap("v3_tree_schedule", "Which outputs enter the FCMP++ curve tree when: applying the block at height h adds "
                "the coinbase outputs of block h + 1 - 60 and the other outputs of block h + 1 - 10, in global output "
                "index order, so the tree after block r holds exactly the outputs spendable in block r + 1.",
                {"coinbase_maturity": COINBASE_MATURITY, "spend_maturity": SPEND_MATURITY,
                 "blocks": [{"coinbase_outputs": c, "other_outputs": o} for c, o in blocks],
                 "entering": entering, "tree_leaves_after": cumulative})


BUILDERS = {"v3_serialization": serialization_vectors, "v3_ids": ids_vectors, "v3_genesis": genesis_vectors,
            "v3_weight": weight_vectors, "v3_shape": shape_vectors, "v3_tree_schedule": tree_schedule_vectors}


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--check", action="store_true", help="compare the committed files with the reference")
    ap.add_argument("--write", action="store_true", help="regenerate the files (a consensus change)")
    args = ap.parse_args(argv)
    if not (args.check or args.write):
        ap.print_help()
        return 1
    bad = 0
    for name in FILES:
        text = v2.jdump(BUILDERS[name]())
        path = os.path.join(VECTOR_DIR, name + ".json")
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
