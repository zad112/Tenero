"""The reference for the CONTROL protocol (docs/CONTROL_PROTOCOL.md) and its golden vectors.

An independent implementation, in Python and the standard library only, of how crates/tenero-app turns the requests
a wallet (or a block explorer) makes of a node, and the node's answers, into bytes: the frame, the requests, the answers, and the
order in which a decoder checks a message. The transaction, header and coinbase encodings inside come from the
version 3 (`gamma`) data-model reference (`reference/tools/make_vectors_v3.py`).

Version 3 of the protocol (0.3.0, FCMP++ and Carrot) retired the requests for ring members' outputs (kinds 4, 5 and 15:
unknown now), added `spend_paths` (20: the curve-tree paths a spend is proven with), and changed `rules` (no ring size or
maturities; the tree's layers), `block_template` (the main address's keys and a weight, not a finished output: a Carrot
coinbase output depends on its amount, so the node makes it), the template (the anchor the output was made with), the
block summary (its weight, not its size) and the pool listing (each transaction's weight). A template is COMPACT (the
header, the coinbase and the transaction ids), and `submit_header` (21) hands a block found on one back as its header.

    python reference/tools/make_vectors_control.py --check     do the committed vectors match the reference?
    python reference/tools/make_vectors_control.py --write     regenerate them (a PROTOCOL CHANGE: explain it in the commit
                                                     and update docs/CONTROL_PROTOCOL.md)

Nothing here is random and nothing depends on the clock.
"""
import argparse
import json
import os
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import make_vectors_v3 as v3  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))   # the repository (the tools live in reference/tools)
VECTOR_DIR = os.path.join(ROOT, "tests", "vectors")
NAME = "control"

MAX_FRAME = 16 * 1024 * 1024
MAX_TEXT = 512
MAX_NAME = 64
MAX_BLOCKS_PER_REQUEST = 64
MAX_KEY_IMAGES = 4096
MAX_SPEND_PATHS = 512
MAX_PATH_LAYERS = 32
LEAF_CHUNK = 38          # the curve tree's leaf chunk (FCMP++ LAYER_ONE_LEN): the outputs a path carries
MAX_CHUNK = 38           # the wider of the tree's two branch widths (38 and 18)
MAX_MEMPOOL_LIST = 4096
ANSWER = 0x80
ERROR = 0xFF

REQUESTS = {"auth": 1, "tip": 2, "block": 3, "key_image_spent": 6, "rules": 7,
            "submit_tx": 8, "info": 9, "stop": 10, "blocks": 11, "block_template": 12, "submit_block": 13,
            "key_images_spent": 14, "check_pow": 16, "headers": 17, "mempool": 18, "chain_stats": 19,
            "spend_paths": 20, "submit_header": 21}
RESPONSES = {"authed": 1, "tip": 2, "block": 3, "spent": 6, "rules": 7,
             "tx_accepted": 8, "info": 9, "stopping": 10, "blocks": 11, "template": 12, "block_submitted": 13,
             "spent_many": 14, "pow_checked": 16, "headers": 17, "mempool": 18, "chain_stats": 19,
             "spend_paths": 20, "header_submitted": 21}
RETIRED = (4, 5, 15)     # output, output_count, outputs: the ring members of version 2


class ControlError(Exception):
    def __init__(self, kind):
        super().__init__(kind)
        self.kind = kind


def u8(n):
    return bytes([n])


def u16(n):
    return struct.pack("<H", n)


def u32(n):
    return struct.pack("<I", n)


def u64(n):
    return struct.pack("<Q", n)


def fixed(hex_text, n):
    b = bytes.fromhex(hex_text)
    assert len(b) == n
    return b


def text(s, maximum):
    b = s.encode("utf-8")
    assert len(b) <= maximum, "text over the maximum"
    return u32(len(b)) + b


def flag(b):
    return u8(1 if b else 0)


# ---- scan blocks and spend paths ------------------------------------------------------------------------------

def enc_scan_block(b):
    # the coinbase is written here (not with the consensus encoding): the genesis block has no outputs
    cb = b["coinbase"]
    return (u64(b["height"]) + fixed(b["id"], 32) + u64(b["first_output_index"]) + u64(b["timestamp"])
            + u16(cb["version"]) + u64(cb["height"])
            + v3.w_list(cb["outputs"], v3.enc_cb_output) + v3.w_var(cb["extra"])
            + v3.w_list(b["txs"], v3.enc_tx_prefix))


def dec_scan_block(r):
    height, bid, first, timestamp = r.u64(), r.fixed(32), r.u64(), r.u64()
    version, cb_height = r.u16(), r.u64()
    outputs = [v3.dec_cb_output(r) for _ in range(r.count(0, v3.MAX_COINBASE_OUTPUTS))]
    extra = r.var(v3.MAX_EXTRA)
    txs = [v3.dec_tx_prefix(r) for _ in range(r.count(0, v3.MAX_BLOCK_TXS))]
    return {"height": height, "id": bid, "first_output_index": first, "timestamp": timestamp,
            "coinbase": {"version": version, "height": cb_height, "outputs": outputs, "extra": extra}, "txs": txs}


def enc_path(p):
    """A path in the curve tree: the output's position, its leaf chunk (each output's key and commitment), and one chunk
    for each layer above (each a list of 32-byte points)."""
    assert 1 <= len(p["leaves"]) <= LEAF_CHUNK and len(p["layers"]) <= MAX_PATH_LAYERS
    out = u64(p["position"]) + u32(len(p["leaves"]))
    for o, c in p["leaves"]:
        out += fixed(o, 32) + fixed(c, 32)
    out += u32(len(p["layers"]))
    for chunk in p["layers"]:
        assert 1 <= len(chunk) <= MAX_CHUNK
        out += u32(len(chunk)) + b"".join(fixed(x, 32) for x in chunk)
    return out


def dec_path(r):
    position = r.u64()
    leaves = [[r.fixed(32), r.fixed(32)] for _ in range(r.count(1, LEAF_CHUNK))]
    layers = [[r.fixed(32) for _ in range(r.count(1, MAX_CHUNK))] for _ in range(r.count(0, MAX_PATH_LAYERS))]
    return {"position": position, "leaves": leaves, "layers": layers}


def enc_tree(t):
    """A block's tree: how many leaves, how many layers, and the root."""
    return u64(t["n_leaves"]) + u8(t["n_layers"]) + fixed(t["root"], 32)


def dec_tree(r):
    return {"n_leaves": r.u64(), "n_layers": r.u8(), "root": r.fixed(32)}


# ---- what a block explorer is told ---------------------------------------------------------------------------

def enc_summary(b):
    return (u64(b["height"]) + fixed(b["id"], 32) + u64(b["timestamp"]) + fixed(b["target"], 32)
            + fixed(b["cumulative_work"], 32) + u64(b["weight"]) + u32(b["tx_count"]) + u64(b["coinbase_total"]))


def dec_summary(r):
    return {"height": r.u64(), "id": r.fixed(32), "timestamp": r.u64(), "target": r.fixed(32),
            "cumulative_work": r.fixed(32), "weight": r.u64(), "tx_count": r.u32(), "coinbase_total": r.u64()}


def enc_pool_entry(t):
    return fixed(t["id"], 32) + u64(t["received"]) + u64(t["fee"]) + u64(t["size"]) + u64(t["weight"])


def dec_pool_entry(r):
    return {"id": r.fixed(32), "received": r.u64(), "fee": r.u64(), "size": r.u64(), "weight": r.u64()}


STATS_FIELDS = ("next_reward", "emitted", "max_supply", "tail_reward", "block_time")


def dec_flag(r):
    b = r.u8()
    if b not in (0, 1):
        raise ControlError("malformed")
    return b == 1


def dec_text(r, maximum):
    try:
        data = bytes.fromhex(r.var(maximum))
        return data.decode("utf-8")
    except UnicodeDecodeError:
        raise ControlError("malformed")


# ---- requests -------------------------------------------------------------------------------------------------

def enc_request(m):
    t = m["type"]
    body = u8(REQUESTS[t])
    if t == "auth":
        body += fixed(m["cookie"], 32)
    elif t == "block":
        body += u64(m["height"])
    elif t == "key_image_spent":
        body += fixed(m["key_image"], 32)
    elif t == "check_pow":
        body += u64(m["height"]) + v3.enc_header(m["header"])
    elif t == "spend_paths":
        assert 1 <= len(m["indexes"]) <= MAX_SPEND_PATHS
        body += u32(len(m["indexes"])) + b"".join(u64(i) for i in m["indexes"])
    elif t == "key_images_spent":
        assert 1 <= len(m["key_images"]) <= MAX_KEY_IMAGES
        body += u32(len(m["key_images"])) + b"".join(fixed(k, 32) for k in m["key_images"])
    elif t == "submit_tx":
        body += v3.enc_tx(m["tx"])
    elif t in ("blocks", "headers"):
        assert 1 <= m["count"] <= MAX_BLOCKS_PER_REQUEST
        body += u64(m["from"]) + u16(m["count"])
    elif t == "block_template":
        body += fixed(m["spend_pubkey"], 32) + fixed(m["view_pubkey"], 32) + u64(m["max_weight"])
    elif t == "submit_block":
        body += v3.enc_block(m["block"])
    elif t == "submit_header":
        body += v3.enc_header(m["header"])
    return body


def dec_request(body):
    if len(body) == 0:
        raise ControlError("length")
    r = v3.Reader(body)
    kind = r.u8()
    names = {v: k for k, v in REQUESTS.items()}
    if kind not in names:
        raise ControlError("kind")
    t = names[kind]
    m = {"type": t}
    try:
        if t == "auth":
            m["cookie"] = r.fixed(32)
        elif t == "block":
            m["height"] = r.u64()
        elif t == "key_image_spent":
            m["key_image"] = r.fixed(32)
        elif t == "check_pow":
            m["height"] = r.u64()
            m["header"] = v3.dec_header(r)
        elif t == "spend_paths":
            m["indexes"] = [r.u64() for _ in range(r.count(1, MAX_SPEND_PATHS))]
        elif t == "key_images_spent":
            m["key_images"] = [r.fixed(32) for _ in range(r.count(1, MAX_KEY_IMAGES))]
        elif t == "submit_tx":
            m["tx"] = v3.dec_tx(r)
        elif t in ("blocks", "headers"):
            m["from"] = r.u64()
            m["count"] = r.u16()
            if not 1 <= m["count"] <= MAX_BLOCKS_PER_REQUEST:
                raise ControlError("malformed")
        elif t == "block_template":
            m["spend_pubkey"] = r.fixed(32)
            m["view_pubkey"] = r.fixed(32)
            m["max_weight"] = r.u64()
        elif t == "submit_block":
            m["block"] = v3.dec_block(r)
        elif t == "submit_header":
            m["header"] = v3.dec_header(r)
    except v3.DecodeError:
        raise ControlError("malformed")
    try:
        r.finish()
    except v3.DecodeError:
        raise ControlError("trailing")
    return m


# ---- responses ------------------------------------------------------------------------------------------------

def enc_response(m):
    t = m["type"]
    if t == "error":
        return u8(ERROR) + text(m["message"], MAX_TEXT)
    body = u8(RESPONSES[t] | ANSWER)
    if t == "tip":
        body += u64(m["height"]) + fixed(m["id"], 32)
    elif t == "block":
        body += flag(m["block"] is not None) + (enc_scan_block(m["block"]) if m["block"] is not None else b"")
    elif t == "spent":
        body += flag(m["spent"])
    elif t == "pow_checked":
        body += flag(m["ok"])
    elif t == "spend_paths":
        assert 1 <= len(m["paths"]) <= MAX_SPEND_PATHS
        body += u64(m["reference_height"]) + enc_tree(m["tree"]) + u32(len(m["paths"])) + b"".join(
            flag(p is not None) + (enc_path(p) if p is not None else b"") for p in m["paths"])
    elif t == "spent_many":
        assert 1 <= len(m["spent"]) <= MAX_KEY_IMAGES
        body += u32(len(m["spent"])) + b"".join(flag(b) for b in m["spent"])
    elif t == "rules":
        assert m["tree_layers"] <= MAX_PATH_LAYERS
        body += (fixed(m["chain_id"], 32) + u64(m["next_height"]) + u64(m["reward"]) + u64(m["median"])
                 + u8(m["tree_layers"]))
    elif t == "tx_accepted":
        body += fixed(m["id"], 32)
    elif t == "info":
        body += (u64(m["height"]) + fixed(m["tip_id"], 32) + u32(m["peers"]) + u32(m["inbound"]) + u64(m["pruned_below"])
                 + u32(m["mempool_txs"]) + flag(m["syncing"]) + u8({"archive": 0, "pruned": 1}[m["kind"]])
                 + text(m["network"], MAX_NAME) + text(m["version"], MAX_NAME))
    elif t == "blocks":
        assert len(m["blocks"]) <= MAX_BLOCKS_PER_REQUEST
        body += u32(len(m["blocks"])) + b"".join(enc_scan_block(b) for b in m["blocks"])
    elif t == "template":
        assert len(m["tx_ids"]) <= v3.MAX_BLOCK_TXS
        body += (u64(m["height"]) + fixed(m["target"], 32) + fixed(m["anchor"], 16) + v3.enc_header(m["header"])
                 + v3.enc_coinbase(m["coinbase"]) + u32(len(m["tx_ids"])) + b"".join(fixed(i, 32) for i in m["tx_ids"]))
    elif t in ("block_submitted", "header_submitted"):
        body += fixed(m["id"], 32) + flag(m["in_chain"])
    elif t == "headers":
        assert len(m["blocks"]) <= MAX_BLOCKS_PER_REQUEST
        body += u32(len(m["blocks"])) + b"".join(enc_summary(b) for b in m["blocks"])
    elif t == "mempool":
        assert len(m["txs"]) <= min(MAX_MEMPOOL_LIST, m["total"])
        body += u32(m["total"]) + u32(len(m["txs"])) + b"".join(enc_pool_entry(t) for t in m["txs"])
    elif t == "chain_stats":
        body += (u64(m["height"]) + fixed(m["next_target"], 32) + fixed(m["cumulative_work"], 32)
                 + b"".join(u64(m[k]) for k in STATS_FIELDS))
    return body


def dec_response(body):
    if len(body) == 0:
        raise ControlError("length")
    r = v3.Reader(body)
    kind = r.u8()
    names = {v | ANSWER: k for k, v in RESPONSES.items()}
    try:
        if kind == ERROR:
            m = {"type": "error", "message": dec_text(r, MAX_TEXT)}
        elif kind in names:
            t = names[kind]
            m = {"type": t}
            if t == "tip":
                m["height"] = r.u64()
                m["id"] = r.fixed(32)
            elif t == "block":
                m["block"] = dec_scan_block(r) if dec_flag(r) else None
            elif t == "spent":
                m["spent"] = dec_flag(r)
            elif t == "pow_checked":
                m["ok"] = dec_flag(r)
            elif t == "spend_paths":
                m["reference_height"] = r.u64()
                m["tree"] = dec_tree(r)
                m["paths"] = [dec_path(r) if dec_flag(r) else None for _ in range(r.count(1, MAX_SPEND_PATHS))]
            elif t == "spent_many":
                m["spent"] = [dec_flag(r) for _ in range(r.count(1, MAX_KEY_IMAGES))]
            elif t == "rules":
                m.update({"chain_id": r.fixed(32), "next_height": r.u64(), "reward": r.u64(), "median": r.u64(),
                          "tree_layers": r.u8()})
                if m["tree_layers"] > MAX_PATH_LAYERS:
                    raise ControlError("malformed")
            elif t == "tx_accepted":
                m["id"] = r.fixed(32)
            elif t == "info":
                m.update({"height": r.u64(), "tip_id": r.fixed(32), "peers": r.u32(), "inbound": r.u32(),
                          "pruned_below": r.u64(), "mempool_txs": r.u32(), "syncing": dec_flag(r)})
                k = r.u8()
                if k not in (0, 1):
                    raise ControlError("malformed")
                m["kind"] = ["archive", "pruned"][k]
                m["network"] = dec_text(r, MAX_NAME)
                m["version"] = dec_text(r, MAX_NAME)
            elif t == "blocks":
                m["blocks"] = [dec_scan_block(r) for _ in range(r.count(0, MAX_BLOCKS_PER_REQUEST))]
            elif t == "template":
                m["height"] = r.u64()
                m["target"] = r.fixed(32)
                m["anchor"] = r.fixed(16)
                m["header"] = v3.dec_header(r)
                m["coinbase"] = v3.dec_coinbase(r)
                m["tx_ids"] = [r.fixed(32) for _ in range(r.count(0, v3.MAX_BLOCK_TXS))]
            elif t in ("block_submitted", "header_submitted"):
                m["id"] = r.fixed(32)
                m["in_chain"] = dec_flag(r)
            elif t == "headers":
                m["blocks"] = [dec_summary(r) for _ in range(r.count(0, MAX_BLOCKS_PER_REQUEST))]
            elif t == "mempool":
                m["total"] = r.u32()
                # a list longer than the pool it lists is not an answer
                m["txs"] = [dec_pool_entry(r) for _ in range(r.count(0, min(MAX_MEMPOOL_LIST, m["total"])))]
            elif t == "chain_stats":
                m["height"] = r.u64()
                m["next_target"] = r.fixed(32)
                m["cumulative_work"] = r.fixed(32)
                for k in STATS_FIELDS:
                    m[k] = r.u64()
        else:
            raise ControlError("kind")
    except v3.DecodeError:
        raise ControlError("malformed")
    try:
        r.finish()
    except v3.DecodeError:
        raise ControlError("trailing")
    return m


def frame(body):
    if not 1 <= len(body) <= MAX_FRAME:
        raise ControlError("length")
    return u32(len(body)) + body


# ---- the cases ------------------------------------------------------------------------------------------------

def h(c, n=32):
    """`n` copies of the byte `c` (two hex digits), as hex."""
    return c * n


def sample_tx(n_in=2):
    return v3.sample_tx("control", n_in=n_in, proof=40)


def sample_prefix():
    t = sample_tx()
    return {k: t[k] for k in ("version", "inputs", "outputs", "ephemeral_pubkeys", "fee", "encrypted_payment_id")}


def sample_coinbase_output():
    return {"onetime_address": h("01"), "amount": 5, "view_tag": h("02", 3), "ephemeral_pubkey": h("03"),
            "anchor_enc": h("04", 16)}


def sample_scan_block(height=77, txs=1, outputs=1):
    return {"height": height, "id": "ab" * 32, "first_output_index": 1000, "timestamp": 1_700_000_000 + 60 * height,
            "coinbase": {"version": 3, "height": height, "outputs": [sample_coinbase_output() for _ in range(outputs)],
                         "extra": ""},
            "txs": [sample_prefix() for _ in range(txs)]}


def genesis_scan_block():
    """The genesis block as the wallet sees it: no coinbase outputs and no transactions."""
    return {"height": 0, "id": "cd" * 32, "first_output_index": 0, "timestamp": 1_700_000_000,
            "coinbase": {"version": 3, "height": 0, "outputs": [], "extra": ""}, "txs": []}


def sample_template(height, n_ids):
    """A compact template: the header, the coinbase and `n_ids` transaction ids (the codec does not check the root)."""
    return {"type": "template", "height": height, "target": "00" * 4 + "ff" * 28, "anchor": h("5a", 16),
            "header": v3.sample_header(f"template {height}", nonce=0),
            "coinbase": {"version": 3, "height": height, "outputs": [sample_coinbase_output()], "extra": ""},
            "tx_ids": [v3.h(f"template {height} tx {i}", 32) for i in range(n_ids)]}


def sample_block(height=7, txs=1):
    return {"header": {"version": 3, "prev_id": "11" * 32, "timestamp": 1700000000, "tx_root": "22" * 32,
                       "nonce": 0, "mix": "00" * 64},
            "coinbase": {"version": 3, "height": height, "outputs": [sample_coinbase_output()], "extra": ""},
            "transactions": [sample_tx() for _ in range(txs)]}


def sample_template_request(max_weight=3 * 1024 * 1024):
    return {"type": "block_template", "spend_pubkey": h("0a"), "view_pubkey": h("0b"), "max_weight": max_weight}


def sample_path(position=77, leaves=3, layers=2):
    """A path: `leaves` outputs in its leaf chunk and a chunk for each of `layers` layers, alternating the two widths' worth
    of points (none of it is a real tree: the codec does not look inside)."""
    return {"position": position,
            "leaves": [[v3.h(f"path {position} leaf {i} O", 32), v3.h(f"path {position} leaf {i} C", 32)]
                       for i in range(leaves)],
            "layers": [[v3.h(f"path {position} layer {k} {j}", 32) for j in range(1 + (k * 7) % MAX_CHUNK)]
                       for k in range(layers)]}


def sample_tree(n_leaves=1234, n_layers=3):
    return {"n_leaves": n_leaves, "n_layers": n_layers, "root": v3.h(f"root {n_leaves}", 32)}


def info(**kw):
    m = {"type": "info", "height": 9, "tip_id": "05" * 32, "peers": 3, "inbound": 1, "pruned_below": 4,
         "mempool_txs": 2, "syncing": True, "kind": "pruned", "network": "test", "version": "0.0.0"}
    m.update(kw)
    return m


def sample_summary(height=12, txs=2):
    return {"height": height, "id": "%02x" % (height % 256) * 32, "timestamp": 1700000000 + 60 * height,
            "target": "00" * 3 + "0f" + "ff" * 28, "cumulative_work": "00" * 30 + "%04x" % (height * 7),
            "weight": 1500 * txs, "tx_count": txs, "coinbase_total": 2000012345}


def genesis_summary():
    """The genesis block: no target, no coinbase, no transactions; its size is its header."""
    return {"height": 0, "id": "cd" * 32, "timestamp": 1700000000, "target": "00" * 32, "cumulative_work": "00" * 32,
            "weight": 0, "tx_count": 0, "coinbase_total": 0}


def sample_pool_entry(i, received=1700000123):
    return {"id": "%02x" % (i + 0x40) * 32, "received": received, "fee": 1000000 * (i + 1), "size": 4500 + i,
            "weight": 1500 + i}


def chain_stats(**kw):
    m = {"type": "chain_stats", "height": 812, "next_target": "00" * 2 + "3a" + "ff" * 29,
         "cumulative_work": "00" * 28 + "01020304", "next_reward": 2000000000, "emitted": 1624000000000,
         "max_supply": 2000000000000000, "tail_reward": 50000000, "block_time": 60}
    m.update(kw)
    return m


def valid_requests():
    return [
        ("authenticate", {"type": "auth", "cookie": "09" * 32}),
        ("tip", {"type": "tip"}),
        ("a block, the largest height", {"type": "block", "height": 2 ** 64 - 1}),
        ("is a key image spent", {"type": "key_image_spent", "key_image": "08" * 32}),
        ("the rules", {"type": "rules"}),
        ("check pow: a header", {"type": "check_pow", "height": 77, "header": v3.sample_header("cp")}),
        ("check pow: the largest height and nonce", {"type": "check_pow", "height": 2 ** 64 - 1,
                                                     "header": dict(v3.sample_header("cp2", nonce=2 ** 64 - 1),
                                                                    mix="ff" * 64)}),
        ("spend paths: one", {"type": "spend_paths", "indexes": [12]}),
        ("spend paths: the largest index and a repeat", {"type": "spend_paths", "indexes": [2 ** 64 - 1, 5, 5, 0]}),
        ("spend paths: the most at once", {"type": "spend_paths", "indexes": list(range(1000, 1000 + MAX_SPEND_PATHS))}),
        ("are many key images spent: one", {"type": "key_images_spent", "key_images": ["08" * 32]}),
        ("are many key images spent: sixty-four", {"type": "key_images_spent",
                                                     "key_images": ["%02x" % (i + 1) * 32 for i in range(64)]}),
        ("submit a transaction", {"type": "submit_tx", "tx": sample_tx()}),
        ("submit a transaction with three inputs", {"type": "submit_tx", "tx": sample_tx(3)}),
        ("submit a transaction with one input and three outputs (a key for each)",
         {"type": "submit_tx", "tx": v3.sample_tx("control three", n_in=1, n_out=3, proof=40)}),
        ("the node's status", {"type": "info"}),
        ("stop", {"type": "stop"}),
        ("one block", {"type": "blocks", "from": 5, "count": 1}),
        ("the most blocks at once", {"type": "blocks", "from": 0, "count": 64}),
        ("a block template", sample_template_request()),
        ("a block template, no transactions wanted", sample_template_request(0)),
        ("a block template, any weight", sample_template_request(2 ** 64 - 1)),
        ("a mined block", {"type": "submit_block", "block": sample_block(txs=0)}),
        ("a mined block with two transactions", {"type": "submit_block", "block": sample_block(txs=2)}),
        ("a block found on a template, as its header", {"type": "submit_header",
                                                        "header": dict(v3.sample_header("found", nonce=77),
                                                                       mix="5e" * 64)}),
        ("one block summary", {"type": "headers", "from": 7, "count": 1}),
        ("the most block summaries at once, from the largest height", {"type": "headers", "from": 2 ** 64 - 1, "count": 64}),
        ("the pool", {"type": "mempool"}),
        ("the chain's numbers", {"type": "chain_stats"}),
    ]


def valid_responses():
    return [
        ("authenticated", {"type": "authed"}),
        ("the tip", {"type": "tip", "height": 5, "id": "01" * 32}),
        ("no such block", {"type": "block", "block": None}),
        ("a block", {"type": "block", "block": sample_scan_block()}),
        ("the genesis block: a coinbase with no outputs", {"type": "block", "block": genesis_scan_block()}),
        ("spent", {"type": "spent", "spent": True}),
        ("not spent", {"type": "spent", "spent": False}),
        ("the mix is right", {"type": "pow_checked", "ok": True}),
        ("the mix is wrong", {"type": "pow_checked", "ok": False}),
        ("spend paths: an output not in the tree", {"type": "spend_paths", "reference_height": 70, "tree": sample_tree(),
                                                    "paths": [None]}),
        ("spend paths: one", {"type": "spend_paths", "reference_height": 70, "tree": sample_tree(),
                              "paths": [sample_path()]}),
        ("spend paths: a mix", {"type": "spend_paths", "reference_height": 2 ** 64 - 1,
                                "tree": sample_tree(2 ** 64 - 1, MAX_PATH_LAYERS),
                                "paths": [sample_path(1, 1, 0), None, sample_path(2 ** 64 - 1, LEAF_CHUNK, 5), None]}),
        ("spend paths: the widest chunks and the most layers",
         {"type": "spend_paths", "reference_height": 9, "tree": sample_tree(10 ** 9, MAX_PATH_LAYERS),
          "paths": [dict(sample_path(5, LEAF_CHUNK, MAX_PATH_LAYERS),
                         layers=[[v3.h(f"wide {k} {j}", 32) for j in range(MAX_CHUNK if k == 0 else 1)]
                                 for k in range(MAX_PATH_LAYERS)])]}),
        ("spend paths: an empty tree", {"type": "spend_paths", "reference_height": 0,
                                        "tree": {"n_leaves": 0, "n_layers": 0, "root": "00" * 32}, "paths": [None, None]}),
        ("many: one answer", {"type": "spent_many", "spent": [True]}),
        ("many: a mix of answers", {"type": "spent_many", "spent": [True, False, False, True, False]}),
        ("many: sixty-four answers", {"type": "spent_many", "spent": [i % 3 == 0 for i in range(64)]}),
        ("the rules", {"type": "rules", "chain_id": "07" * 32, "next_height": 11, "reward": 2000000000,
                       "median": 150000, "tree_layers": 3}),
        ("the rules of an empty tree", {"type": "rules", "chain_id": "08" * 32, "next_height": 1, "reward": 2000000000,
                                        "median": 150000, "tree_layers": 0}),
        ("the rules with the most layers", {"type": "rules", "chain_id": "09" * 32, "next_height": 2 ** 64 - 1,
                                            "reward": 2 ** 64 - 1, "median": 2 ** 64 - 1, "tree_layers": MAX_PATH_LAYERS}),
        ("a transaction was accepted", {"type": "tx_accepted", "id": "03" * 32}),
        ("a pruned node's status", info()),
        ("an archive node's status", info(kind="archive", syncing=False)),
        ("the longest names", info(network="n" * MAX_NAME, version="v" * MAX_NAME)),
        ("stopping", {"type": "stopping"}),
        ("no blocks", {"type": "blocks", "blocks": []}),
        ("two blocks", {"type": "blocks", "blocks": [sample_scan_block(1), sample_scan_block(2, txs=2, outputs=2)]}),
        ("genesis and the block after it", {"type": "blocks", "blocks": [genesis_scan_block(), sample_scan_block(1)]}),
        ("a template", sample_template(7, 3)),
        ("a template with no transactions", dict(sample_template(1, 0), target="7f" + "ff" * 31, anchor=h("01", 16))),
        ("a template of 200 ids", sample_template(9, 200)),
        ("the block is in the chain", {"type": "block_submitted", "id": "33" * 32, "in_chain": True}),
        ("the block lost a race", {"type": "block_submitted", "id": "44" * 32, "in_chain": False}),
        ("the header's block is in the chain", {"type": "header_submitted", "id": "35" * 32, "in_chain": True}),
        ("the header's block lost a race", {"type": "header_submitted", "id": "46" * 32, "in_chain": False}),
        ("no block summaries", {"type": "headers", "blocks": []}),
        ("the genesis summary and the next two", {"type": "headers",
                                                  "blocks": [genesis_summary(), sample_summary(1, 0), sample_summary(2)]}),
        ("a summary with the largest numbers", {"type": "headers", "blocks": [
            {"height": 2 ** 64 - 1, "id": "ff" * 32, "timestamp": 2 ** 64 - 1, "target": "ff" * 32,
             "cumulative_work": "ff" * 32, "weight": 2 ** 64 - 1, "tx_count": 2 ** 32 - 1, "coinbase_total": 2 ** 64 - 1}]}),
        ("an empty pool", {"type": "mempool", "total": 0, "txs": []}),
        ("a pool of three", {"type": "mempool", "total": 3,
                             "txs": [sample_pool_entry(0), sample_pool_entry(1, received=0), sample_pool_entry(2)]}),
        ("a pool larger than its list", {"type": "mempool", "total": 9000, "txs": [sample_pool_entry(5)]}),
        ("the chain's numbers", chain_stats()),
        ("the chain's numbers at genesis", chain_stats(height=0, cumulative_work="00" * 32, emitted=0)),
        ("an error", {"type": "error", "message": "no"}),
        ("an error with accents", {"type": "error", "message": "fée trop basse: пять"}),
        ("the longest error", {"type": "error", "message": "x" * MAX_TEXT}),
    ]


def valid_cases():
    cases = []
    for note, m in valid_requests():
        body = enc_request(m)
        assert dec_request(body) == m, note
        cases.append({"direction": "request", "note": note, "message": m, "body": body.hex(),
                      "frame": frame(body).hex()})
    for note, m in valid_responses():
        body = enc_response(m)
        assert dec_response(body) == m, note
        cases.append({"direction": "response", "note": note, "message": m, "body": body.hex(),
                      "frame": frame(body).hex()})
    return cases


def bad(direction, note, body, error):
    body = bytes(body)
    dec = dec_request if direction == "request" else dec_response
    try:
        dec(body)
    except ControlError as e:
        assert e.kind == error, (note, e.kind, error)
    else:
        raise AssertionError(f"the reference accepted an invalid message: {note}")
    return {"direction": direction, "note": note, "body": body.hex(), "error": error}


def invalid_cases():
    out = []
    # nothing, and kinds that do not exist
    for d in ("request", "response"):
        out.append(bad(d, "an empty body", b"", "length"))
    for k in (0, 22, 0x80, 0x8F, 0xFE):
        out.append(bad("request", f"unknown request kind {k}", bytes([k]), "kind"))
    for k in RETIRED:
        out.append(bad("request", f"the retired request kind {k} (version 2's ring members)", bytes([k]) + u64(1), "kind"))
        out.append(bad("response", f"the retired answer kind {k | ANSWER}", bytes([k | ANSWER, 0]), "kind"))
    for k in (0, 1, 11, 12, 13, 14, 15, 17, 0x96, 0xFE):
        out.append(bad("response", f"unknown response kind {k}", bytes([k]), "kind"))
    # every message cut short and with a byte too many
    for note, m in valid_requests():
        body = enc_request(m)
        out.append(bad("request", f"{note}: cut in the middle" if len(body) > 2 else f"{note}: with a trailing byte",
                       body[: len(body) // 2] if len(body) > 2 else body + b"\0",
                       "malformed" if len(body) > 2 else "trailing"))
        out.append(bad("request", f"{note}: with a trailing byte", body + b"\0", "trailing"))
    for note, m in valid_responses():
        body = enc_response(m)
        if len(body) > 2:
            out.append(bad("response", f"{note}: cut in the middle", body[: len(body) // 2], "malformed"))
        out.append(bad("response", f"{note}: with a trailing byte", body + b"\0", "trailing"))
    # flags are 0 or 1
    out.append(bad("response", "the in-chain flag is 2", bytes([13 | ANSWER]) + bytes(32) + bytes([2]), "malformed"))
    for k, name in ((3, "block"), (6, "spent")):
        out.append(bad("response", f"the {name} flag is 2", bytes([k | ANSWER, 2]), "malformed"))
    # a node kind is 0 or 1
    info_body = bytearray(enc_response(info()))
    info_body[1 + 8 + 32 + 4 + 4 + 8 + 4 + 1] = 2
    out.append(bad("response", "the node kind is 2", bytes(info_body), "malformed"))
    # text: UTF-8 and the caps
    out.append(bad("response", "an error that is not UTF-8", bytes([ERROR]) + u32(2) + b"\xff\xfe", "malformed"))
    out.append(bad("response", "an error over the cap", bytes([ERROR]) + u32(MAX_TEXT + 1) + b"x" * (MAX_TEXT + 1),
                   "malformed"))
    out.append(bad("response", "a network name over the cap",
                   bytes(enc_response(info())[: 1 + 8 + 32 + 4 + 4 + 8 + 4 + 1 + 1]) + u32(MAX_NAME + 1) + b"x" * (MAX_NAME + 1)
                   + u32(1) + b"v", "malformed"))
    # many key images: how many, and the flags
    for count in (0, MAX_KEY_IMAGES + 1, 2 ** 32 - 1):
        out.append(bad("request", f"key_images_spent: a count of {count}", u8(14) + u32(count), "malformed"))
        out.append(bad("response", f"spent_many: a count of {count}", u8(14 | ANSWER) + u32(count), "malformed"))
    out.append(bad("response", "spent_many: a flag of 2", u8(14 | ANSWER) + u32(2) + bytes([1, 2]), "malformed"))
    out.append(bad("response", "spent_many: fewer flags than the count", u8(14 | ANSWER) + u32(3) + bytes([1, 0]), "malformed"))
    out.append(bad("request", "key_images_spent: fewer ids than the count", u8(14) + u32(2) + bytes(32), "malformed"))
    # spend paths: how many, and the shape of a path
    for count in (0, MAX_SPEND_PATHS + 1, 2 ** 32 - 1):
        out.append(bad("request", f"spend_paths: a count of {count}", u8(20) + u32(count), "malformed"))
    head = u8(20 | ANSWER) + u64(5) + enc_tree(sample_tree())
    for count in (0, MAX_SPEND_PATHS + 1, 2 ** 32 - 1):
        out.append(bad("response", f"spend_paths answer: a count of {count}", head + u32(count), "malformed"))
    out.append(bad("request", "spend_paths: fewer indexes than the count", u8(20) + u32(2) + u64(1), "malformed"))
    out.append(bad("response", "spend_paths answer: an entry flag of 2", head + u32(1) + bytes([2]), "malformed"))
    out.append(bad("response", "spend_paths answer: fewer entries than the count", head + u32(2) + bytes([0]), "malformed"))
    p = sample_path()
    out.append(bad("response", "a path with no leaves", head + u32(1) + bytes([1]) + u64(1) + u32(0) + u32(0), "malformed"))
    out.append(bad("response", f"a path of {LEAF_CHUNK + 1} leaves, every one there",
                   head + u32(1) + bytes([1]) + u64(1) + u32(LEAF_CHUNK + 1) + bytes(64 * (LEAF_CHUNK + 1)) + u32(0),
                   "malformed"))
    out.append(bad("response", f"a path of {MAX_PATH_LAYERS + 1} layers",
                   head + u32(1) + bytes([1]) + u64(1) + u32(1) + bytes(64) + u32(MAX_PATH_LAYERS + 1)
                   + (u32(1) + bytes(32)) * (MAX_PATH_LAYERS + 1), "malformed"))
    out.append(bad("response", "a layer chunk with nothing in it",
                   head + u32(1) + bytes([1]) + u64(1) + u32(1) + bytes(64) + u32(1) + u32(0), "malformed"))
    out.append(bad("response", f"a layer chunk of {MAX_CHUNK + 1}, every one there",
                   head + u32(1) + bytes([1]) + u64(1) + u32(1) + bytes(64) + u32(1) + u32(MAX_CHUNK + 1)
                   + bytes(32 * (MAX_CHUNK + 1)), "malformed"))
    out.append(bad("response", "a path cut short", head + u32(1) + bytes([1]) + enc_path(p)[:-1], "malformed"))
    out.append(bad("response", "spend_paths answer: no tree", u8(20 | ANSWER) + u64(5), "malformed"))
    # the rules: at most MAX_PATH_LAYERS layers
    rules = bytearray(enc_response({"type": "rules", "chain_id": "07" * 32, "next_height": 1, "reward": 2,
                                    "median": 3, "tree_layers": 0}))
    rules[-1] = MAX_PATH_LAYERS + 1
    out.append(bad("response", f"the rules with {MAX_PATH_LAYERS + 1} layers", bytes(rules), "malformed"))
    # check_pow: a header cut short, a trailing byte, a flag that is not 0 or 1
    good = u8(16) + u64(5) + v3.enc_header(v3.sample_header("cp"))
    out.append(bad("request", "check_pow: a header one byte short", good[:-1], "malformed"))
    out.append(bad("request", "check_pow: no height", u8(16), "malformed"))
    out.append(bad("response", "pow_checked: a flag of 2", u8(16 | ANSWER) + bytes([2]), "malformed"))
    out.append(bad("response", "pow_checked: no flag", u8(16 | ANSWER), "malformed"))
    # headers: how many, asked and answered
    for count in (0, 65, 65535):
        out.append(bad("request", f"headers: a count of {count}", u8(17) + u64(1) + u16(count), "malformed"))
    out.append(bad("response", "headers: 65 of them, every one there",
                   u8(17 | ANSWER) + u32(65) + b"".join(enc_summary(sample_summary(i)) for i in range(65)), "malformed"))
    out.append(bad("response", "headers: a summary one byte short",
                   u8(17 | ANSWER) + u32(1) + enc_summary(sample_summary())[:-1], "malformed"))
    # the pool: a list longer than the pool, or over the cap
    out.append(bad("response", "mempool: more listed than the pool holds",
                   u8(18 | ANSWER) + u32(1) + u32(2) + enc_pool_entry(sample_pool_entry(0)) + enc_pool_entry(sample_pool_entry(1)),
                   "malformed"))
    out.append(bad("response", "mempool: a list over the cap",
                   u8(18 | ANSWER) + u32(10000) + u32(MAX_MEMPOOL_LIST + 1), "malformed"))
    out.append(bad("response", "mempool: fewer entries than the count",
                   u8(18 | ANSWER) + u32(5) + u32(2) + enc_pool_entry(sample_pool_entry(0)), "malformed"))
    out.append(bad("response", "mempool: no total", u8(18 | ANSWER), "malformed"))
    out.append(bad("response", "chain_stats: one byte short", enc_response(chain_stats())[:-1], "malformed"))
    # Blocks: how many
    for count in (0, 65, 65535):
        out.append(bad("request", f"blocks: a count of {count}", u8(11) + u64(1) + u16(count), "malformed"))
    out.append(bad("response", "blocks: 65 of them", u8(11 | ANSWER) + u32(65), "malformed"))
    out.append(bad("response", "blocks: 65 of them, every one there",
                   u8(11 | ANSWER) + u32(65) + b"".join(enc_scan_block(genesis_scan_block()) for _ in range(65)),
                   "malformed"))
    # a coinbase with too many outputs, and a transaction with no inputs
    big = bytearray(enc_response({"type": "block", "block": genesis_scan_block()}))
    out.append(bad("response", "a coinbase of 17 outputs",
                   bytes(big[: 1 + 1 + 8 + 32 + 8 + 8 + 2 + 8]) + u32(17), "malformed"))
    blk = sample_scan_block()
    cb17 = dict(blk["coinbase"], outputs=[sample_coinbase_output() for _ in range(17)])
    forged = enc_scan_block(dict(blk, coinbase=cb17))
    out.append(bad("response", "a coinbase of 17 outputs, every one there", u8(3 | ANSWER) + u8(1) + forged, "malformed"))
    return out


def frame_cases():
    """The length prefix on its own: what makes a frame acceptable before any body is read."""
    return [
        {"note": "zero length", "length": 0, "ok": False},
        {"note": "one byte", "length": 1, "ok": True},
        {"note": "the largest", "length": MAX_FRAME, "ok": True},
        {"note": "one over", "length": MAX_FRAME + 1, "ok": False},
        {"note": "all ones", "length": 2 ** 32 - 1, "ok": False},
    ]


def build():
    return {
        "schema": 1,
        "name": NAME,
        "description": "The control protocol between a wallet and a node on one machine (docs/CONTROL_PROTOCOL.md): "
                       "every request and answer as a body and a frame, malformed bodies with the error a decoder "
                       "must give, and the frame length rule. An independent Python implementation "
                       "(reference/tools/make_vectors_control.py).",
        "limits": {"max_frame": MAX_FRAME, "max_text": MAX_TEXT, "max_name": MAX_NAME,
                   "max_blocks_per_request": MAX_BLOCKS_PER_REQUEST, "max_key_images": MAX_KEY_IMAGES,
                   "max_spend_paths": MAX_SPEND_PATHS, "max_path_layers": MAX_PATH_LAYERS, "leaf_chunk": LEAF_CHUNK,
                   "max_chunk": MAX_CHUNK, "max_mempool_list": MAX_MEMPOOL_LIST},
        "retired": list(RETIRED),
        "kinds": {"requests": REQUESTS, "responses": {k: v | ANSWER for k, v in RESPONSES.items()}, "error": ERROR},
        "valid": valid_cases(),
        "invalid": invalid_cases(),
        "frames": frame_cases(),
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
    text_ = jdump(build())
    path = path_of()
    if args.check:
        same = os.path.exists(path) and open(path).read() == text_
        print(f"{'ok      ' if same else 'DIFFERS '}{NAME}")
        return 0 if same else 1
    os.makedirs(VECTOR_DIR, exist_ok=True)
    with open(path, "w", newline="\n") as f:
        f.write(text_)
    print(f"wrote {os.path.relpath(path, ROOT)}  ({len(text_) / 1024:.0f} KiB)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
