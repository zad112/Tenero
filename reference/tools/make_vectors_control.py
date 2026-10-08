"""The reference for the CONTROL protocol (docs/CONTROL_PROTOCOL.md) and its golden vectors.

An independent implementation, in Python and the standard library only, of how crates/tenero-app turns the requests
a wallet (or a block explorer) makes of a node, and the node's answers, into bytes: the frame, the requests, the answers, and the
order in which a decoder checks a message. The transaction and coinbase encodings inside come from the version 2
data-model reference (`reference/tools/make_vectors_v2.py`).

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
import make_vectors_v2 as v2  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))   # the repository (the tools live in reference/tools)
VECTOR_DIR = os.path.join(ROOT, "tests", "vectors")
NAME = "control"

MAX_FRAME = 16 * 1024 * 1024
MAX_TEXT = 512
MAX_NAME = 64
MAX_BLOCKS_PER_REQUEST = 64
MAX_KEY_IMAGES = 4096
MAX_OUTPUTS_PER_REQUEST = 1024
MAX_MEMPOOL_LIST = 4096
ANSWER = 0x80
ERROR = 0xFF

REQUESTS = {"auth": 1, "tip": 2, "block": 3, "output": 4, "output_count": 5, "key_image_spent": 6, "rules": 7,
            "submit_tx": 8, "info": 9, "stop": 10, "blocks": 11, "block_template": 12, "submit_block": 13,
            "key_images_spent": 14, "outputs": 15, "check_pow": 16, "headers": 17, "mempool": 18, "chain_stats": 19}
RESPONSES = {"authed": 1, "tip": 2, "block": 3, "output": 4, "output_count": 5, "spent": 6, "rules": 7,
             "tx_accepted": 8, "info": 9, "stopping": 10, "blocks": 11, "template": 12, "block_submitted": 13,
             "spent_many": 14, "outputs_many": 15, "pow_checked": 16, "headers": 17, "mempool": 18, "chain_stats": 19}


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


# ---- scan blocks and stored outputs --------------------------------------------------------------------------

def enc_scan_block(b):
    # the coinbase is written here (not with the consensus encoding): the genesis block has no outputs
    cb = b["coinbase"]
    return (u64(b["height"]) + fixed(b["id"], 32) + u64(b["first_output_index"]) + u16(cb["version"]) + u64(cb["height"])
            + v2.w_list(cb["outputs"], v2.enc_cb_output) + v2.w_var(cb["extra"])
            + v2.w_list(b["txs"], v2.enc_tx_prefix))


def dec_scan_block(r):
    height, bid, first = r.u64(), r.fixed(32), r.u64()
    version, cb_height = r.u16(), r.u64()
    outputs = [v2.dec_cb_output(r) for _ in range(r.count(0, v2.MAX_COINBASE_OUTPUTS))]
    extra = r.var(v2.MAX_EXTRA)
    txs = [v2.dec_tx_prefix(r) for _ in range(r.count(0, v2.MAX_BLOCK_TXS))]
    return {"height": height, "id": bid, "first_output_index": first,
            "coinbase": {"version": version, "height": cb_height, "outputs": outputs, "extra": extra}, "txs": txs}


def enc_stored_output(o):
    return (fixed(o["onetime_address"], 32) + fixed(o["amount_commitment"], 32) + u64(o["public_amount"])
            + u64(o["height"]) + flag(o["coinbase"]))


def dec_stored_output(r):
    return {"onetime_address": r.fixed(32), "amount_commitment": r.fixed(32), "public_amount": r.u64(),
            "height": r.u64(), "coinbase": dec_flag(r)}


# ---- what a block explorer is told ---------------------------------------------------------------------------

def enc_summary(b):
    return (u64(b["height"]) + fixed(b["id"], 32) + u64(b["timestamp"]) + fixed(b["target"], 32)
            + fixed(b["cumulative_work"], 32) + u64(b["size"]) + u32(b["tx_count"]) + u64(b["coinbase_total"]))


def dec_summary(r):
    return {"height": r.u64(), "id": r.fixed(32), "timestamp": r.u64(), "target": r.fixed(32),
            "cumulative_work": r.fixed(32), "size": r.u64(), "tx_count": r.u32(), "coinbase_total": r.u64()}


def enc_pool_entry(t):
    return fixed(t["id"], 32) + u64(t["received"]) + u64(t["fee"]) + u64(t["size"])


def dec_pool_entry(r):
    return {"id": r.fixed(32), "received": r.u64(), "fee": r.u64(), "size": r.u64()}


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
    elif t == "output":
        body += u64(m["index"])
    elif t == "key_image_spent":
        body += fixed(m["key_image"], 32)
    elif t == "check_pow":
        body += u64(m["height"]) + v2.enc_header(m["header"])
    elif t == "outputs":
        assert 1 <= len(m["indexes"]) <= MAX_OUTPUTS_PER_REQUEST
        body += u32(len(m["indexes"])) + b"".join(u64(i) for i in m["indexes"])
    elif t == "key_images_spent":
        assert 1 <= len(m["key_images"]) <= MAX_KEY_IMAGES
        body += u32(len(m["key_images"])) + b"".join(fixed(k, 32) for k in m["key_images"])
    elif t == "submit_tx":
        body += v2.enc_tx(m["tx"])
    elif t in ("blocks", "headers"):
        assert 1 <= m["count"] <= MAX_BLOCKS_PER_REQUEST
        body += u64(m["from"]) + u16(m["count"])
    elif t == "block_template":
        pl = m["payout"]
        body += (fixed(pl["onetime_address"], 32) + fixed(pl["view_tag"], 3) + fixed(pl["ephemeral_pubkey"], 32)
                 + fixed(pl["anchor_enc"], 16) + u32(m["max_body_bytes"]))
    elif t == "submit_block":
        body += v2.enc_block(m["block"])
    return body


def dec_request(body):
    if len(body) == 0:
        raise ControlError("length")
    r = v2.Reader(body)
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
        elif t == "output":
            m["index"] = r.u64()
        elif t == "key_image_spent":
            m["key_image"] = r.fixed(32)
        elif t == "check_pow":
            m["height"] = r.u64()
            m["header"] = v2.dec_header(r)
        elif t == "outputs":
            m["indexes"] = [r.u64() for _ in range(r.count(1, MAX_OUTPUTS_PER_REQUEST))]
        elif t == "key_images_spent":
            m["key_images"] = [r.fixed(32) for _ in range(r.count(1, MAX_KEY_IMAGES))]
        elif t == "submit_tx":
            m["tx"] = v2.dec_tx(r)
        elif t in ("blocks", "headers"):
            m["from"] = r.u64()
            m["count"] = r.u16()
            if not 1 <= m["count"] <= MAX_BLOCKS_PER_REQUEST:
                raise ControlError("malformed")
        elif t == "block_template":
            m["payout"] = {"onetime_address": r.fixed(32), "view_tag": r.fixed(3), "ephemeral_pubkey": r.fixed(32),
                           "anchor_enc": r.fixed(16)}
            m["max_body_bytes"] = r.u32()
        elif t == "submit_block":
            m["block"] = v2.dec_block(r)
    except v2.DecodeError:
        raise ControlError("malformed")
    try:
        r.finish()
    except v2.DecodeError:
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
    elif t == "output":
        body += flag(m["output"] is not None) + (enc_stored_output(m["output"]) if m["output"] is not None else b"")
    elif t == "output_count":
        body += u64(m["count"])
    elif t == "spent":
        body += flag(m["spent"])
    elif t == "pow_checked":
        body += flag(m["ok"])
    elif t == "outputs_many":
        assert 1 <= len(m["outputs"]) <= MAX_OUTPUTS_PER_REQUEST
        body += u32(len(m["outputs"])) + b"".join(
            flag(o is not None) + (enc_stored_output(o) if o is not None else b"") for o in m["outputs"])
    elif t == "spent_many":
        assert 1 <= len(m["spent"]) <= MAX_KEY_IMAGES
        body += u32(len(m["spent"])) + b"".join(flag(b) for b in m["spent"])
    elif t == "rules":
        body += (fixed(m["chain_id"], 32) + u32(m["ring_size"]) + u64(m["coinbase_maturity"]) + u64(m["spend_maturity"])
                 + u64(m["next_height"]) + u64(m["reward"]) + u64(m["median"]) + u32(m["max_inputs"]))
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
        body += u64(m["height"]) + fixed(m["target"], 32) + v2.enc_block(m["block"])
    elif t == "block_submitted":
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
    r = v2.Reader(body)
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
            elif t == "output":
                m["output"] = dec_stored_output(r) if dec_flag(r) else None
            elif t == "output_count":
                m["count"] = r.u64()
            elif t == "spent":
                m["spent"] = dec_flag(r)
            elif t == "pow_checked":
                m["ok"] = dec_flag(r)
            elif t == "outputs_many":
                m["outputs"] = [dec_stored_output(r) if dec_flag(r) else None
                                for _ in range(r.count(1, MAX_OUTPUTS_PER_REQUEST))]
            elif t == "spent_many":
                m["spent"] = [dec_flag(r) for _ in range(r.count(1, MAX_KEY_IMAGES))]
            elif t == "rules":
                m.update({"chain_id": r.fixed(32), "ring_size": r.u32(), "coinbase_maturity": r.u64(),
                          "spend_maturity": r.u64(), "next_height": r.u64(), "reward": r.u64(), "median": r.u64(),
                          "max_inputs": r.u32()})
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
                m["block"] = v2.dec_block(r)
            elif t == "block_submitted":
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
    except v2.DecodeError:
        raise ControlError("malformed")
    try:
        r.finish()
    except v2.DecodeError:
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


def sample_output(seed):
    return {"onetime_address": h(seed), "amount_commitment": h("03"), "amount_enc": h("04", 8),
            "view_tag": h("05", 3), "ephemeral_pubkey": h("06"), "anchor_enc": h("07", 16)}


def sample_tx(n_in=1):
    return {"version": 2, "inputs": [{"key_image": "%02x" % (i + 1) * 32} for i in range(n_in)],
            "outputs": [sample_output("02"), sample_output("08")], "fee": 12345, "extra": "0909",
            "rings": [[1, 2] for _ in range(n_in)], "proof_data": "08" * 10}


def sample_prefix():
    t = sample_tx()
    return {k: t[k] for k in ("version", "inputs", "outputs", "fee", "extra")}


def sample_coinbase_output():
    return {"onetime_address": h("01"), "amount": 5, "view_tag": h("02", 3), "ephemeral_pubkey": h("03"),
            "anchor_enc": h("04", 16)}


def sample_scan_block(height=77, txs=1, outputs=1):
    return {"height": height, "id": "ab" * 32, "first_output_index": 1000,
            "coinbase": {"version": 2, "height": height, "outputs": [sample_coinbase_output() for _ in range(outputs)],
                         "extra": ""},
            "txs": [sample_prefix() for _ in range(txs)]}


def genesis_scan_block():
    """The genesis block as the wallet sees it: no coinbase outputs and no transactions."""
    return {"height": 0, "id": "cd" * 32, "first_output_index": 0,
            "coinbase": {"version": 2, "height": 0, "outputs": [], "extra": ""}, "txs": []}


def sample_block(height=7, txs=1):
    return {"header": {"version": 2, "prev_id": "11" * 32, "timestamp": 1700000000, "tx_root": "22" * 32,
                       "nonce": 0, "mix": "00" * 64},
            "coinbase": {"version": 2, "height": height, "outputs": [sample_coinbase_output()], "extra": ""},
            "transactions": [sample_tx() for _ in range(txs)]}


def sample_payout():
    return {"onetime_address": h("0a"), "view_tag": h("0b", 3), "ephemeral_pubkey": h("0c"), "anchor_enc": h("0d", 16)}


def info(**kw):
    m = {"type": "info", "height": 9, "tip_id": "05" * 32, "peers": 3, "inbound": 1, "pruned_below": 4,
         "mempool_txs": 2, "syncing": True, "kind": "pruned", "network": "test", "version": "0.0.0"}
    m.update(kw)
    return m


def sample_summary(height=12, txs=2):
    return {"height": height, "id": "%02x" % (height % 256) * 32, "timestamp": 1700000000 + 60 * height,
            "target": "00" * 3 + "0f" + "ff" * 28, "cumulative_work": "00" * 30 + "%04x" % (height * 7),
            "size": 300 + 2000 * txs, "tx_count": txs, "coinbase_total": 2000012345}


def genesis_summary():
    """The genesis block: no target, no coinbase, no transactions; its size is its header."""
    return {"height": 0, "id": "cd" * 32, "timestamp": 1700000000, "target": "00" * 32, "cumulative_work": "00" * 32,
            "size": 146, "tx_count": 0, "coinbase_total": 0}


def sample_pool_entry(i, received=1700000123):
    return {"id": "%02x" % (i + 0x40) * 32, "received": received, "fee": 1000000 * (i + 1), "size": 1500 + i}


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
        ("an output", {"type": "output", "index": 12}),
        ("the number of outputs", {"type": "output_count"}),
        ("is a key image spent", {"type": "key_image_spent", "key_image": "08" * 32}),
        ("the rules", {"type": "rules"}),
        ("check pow: a header", {"type": "check_pow", "height": 77, "header": v2.sample_header("cp")}),
        ("check pow: the largest height and nonce", {"type": "check_pow", "height": 2 ** 64 - 1, "header": v2.sample_header("cp2", nonce=2 ** 64 - 1, mix="ff" * 64)}),
        ("many outputs: one", {"type": "outputs", "indexes": [12]}),
        ("many outputs: the largest index and a repeat", {"type": "outputs", "indexes": [2 ** 64 - 1, 5, 5, 0]}),
        ("many outputs: sixty-four", {"type": "outputs", "indexes": list(range(100, 164))}),
        ("are many key images spent: one", {"type": "key_images_spent", "key_images": ["08" * 32]}),
        ("are many key images spent: sixty-four", {"type": "key_images_spent",
                                                     "key_images": ["%02x" % (i + 1) * 32 for i in range(64)]}),
        ("submit a transaction", {"type": "submit_tx", "tx": sample_tx()}),
        ("submit a transaction with three inputs", {"type": "submit_tx", "tx": sample_tx(3)}),
        ("the node's status", {"type": "info"}),
        ("stop", {"type": "stop"}),
        ("one block", {"type": "blocks", "from": 5, "count": 1}),
        ("the most blocks at once", {"type": "blocks", "from": 0, "count": 64}),
        ("a block template", {"type": "block_template", "payout": sample_payout(), "max_body_bytes": 1000000}),
        ("a block template, no transactions wanted", {"type": "block_template", "payout": sample_payout(),
                                                      "max_body_bytes": 0}),
        ("a mined block", {"type": "submit_block", "block": sample_block(txs=0)}),
        ("a mined block with two transactions", {"type": "submit_block", "block": sample_block(txs=2)}),
        ("one block summary", {"type": "headers", "from": 7, "count": 1}),
        ("the most block summaries at once, from the largest height", {"type": "headers", "from": 2 ** 64 - 1, "count": 64}),
        ("the pool", {"type": "mempool"}),
        ("the chain's numbers", {"type": "chain_stats"}),
    ]


def valid_responses():
    out_rec = {"onetime_address": h("01"), "amount_commitment": h("02"), "public_amount": 3, "height": 4,
               "coinbase": True}
    return [
        ("authenticated", {"type": "authed"}),
        ("the tip", {"type": "tip", "height": 5, "id": "01" * 32}),
        ("no such block", {"type": "block", "block": None}),
        ("a block", {"type": "block", "block": sample_scan_block()}),
        ("the genesis block: a coinbase with no outputs", {"type": "block", "block": genesis_scan_block()}),
        ("no such output", {"type": "output", "output": None}),
        ("a coinbase output", {"type": "output", "output": out_rec}),
        ("an ordinary output", {"type": "output", "output": dict(out_rec, coinbase=False, public_amount=0)}),
        ("the number of outputs", {"type": "output_count", "count": 99}),
        ("spent", {"type": "spent", "spent": True}),
        ("not spent", {"type": "spent", "spent": False}),
        ("the mix is right", {"type": "pow_checked", "ok": True}),
        ("the mix is wrong", {"type": "pow_checked", "ok": False}),
        ("many outputs: none of them exists", {"type": "outputs_many", "outputs": [None]}),
        ("many outputs: a mix", {"type": "outputs_many", "outputs": [out_rec, None, dict(out_rec, coinbase=False, public_amount=0), None]}),
        ("many outputs: thirty-two", {"type": "outputs_many", "outputs": [dict(out_rec, height=i) for i in range(32)]}),
        ("many: one answer", {"type": "spent_many", "spent": [True]}),
        ("many: a mix of answers", {"type": "spent_many", "spent": [True, False, False, True, False]}),
        ("many: sixty-four answers", {"type": "spent_many", "spent": [i % 3 == 0 for i in range(64)]}),
        ("the rules", {"type": "rules", "chain_id": "07" * 32, "ring_size": 16, "coinbase_maturity": 60,
                       "spend_maturity": 10, "next_height": 11, "reward": 2000000000, "median": 150000, "max_inputs": 0}),
        ("the rules of a network with a limit on inputs", {"type": "rules", "chain_id": "08" * 32, "ring_size": 16, "coinbase_maturity": 60,
                       "spend_maturity": 10, "next_height": 11, "reward": 2000000000, "median": 150000, "max_inputs": 32}),
        ("a transaction was accepted", {"type": "tx_accepted", "id": "03" * 32}),
        ("a pruned node's status", info()),
        ("an archive node's status", info(kind="archive", syncing=False)),
        ("the longest names", info(network="n" * MAX_NAME, version="v" * MAX_NAME)),
        ("stopping", {"type": "stopping"}),
        ("no blocks", {"type": "blocks", "blocks": []}),
        ("two blocks", {"type": "blocks", "blocks": [sample_scan_block(1), sample_scan_block(2, txs=2, outputs=2)]}),
        ("genesis and the block after it", {"type": "blocks", "blocks": [genesis_scan_block(), sample_scan_block(1)]}),
        ("a template", {"type": "template", "height": 7, "target": "00" * 4 + "ff" * 28, "block": sample_block()}),
        ("a template with no transactions", {"type": "template", "height": 1, "target": "7f" + "ff" * 31,
                                             "block": sample_block(1, txs=0)}),
        ("the block is in the chain", {"type": "block_submitted", "id": "33" * 32, "in_chain": True}),
        ("the block lost a race", {"type": "block_submitted", "id": "44" * 32, "in_chain": False}),
        ("no block summaries", {"type": "headers", "blocks": []}),
        ("the genesis summary and the next two", {"type": "headers",
                                                  "blocks": [genesis_summary(), sample_summary(1, 0), sample_summary(2)]}),
        ("a summary with the largest numbers", {"type": "headers", "blocks": [
            {"height": 2 ** 64 - 1, "id": "ff" * 32, "timestamp": 2 ** 64 - 1, "target": "ff" * 32,
             "cumulative_work": "ff" * 32, "size": 2 ** 64 - 1, "tx_count": 2 ** 32 - 1, "coinbase_total": 2 ** 64 - 1}]}),
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
    for k in (0, 20, 0x80, 0x8F, 0xFE):
        out.append(bad("request", f"unknown request kind {k}", bytes([k]), "kind"))
    for k in (0, 1, 11, 12, 13, 14, 15, 17, 0x94, 0xFE):
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
    for k, name in ((3, "block"), (4, "output"), (6, "spent")):
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
    # many outputs: how many, and the shape
    for count in (0, MAX_OUTPUTS_PER_REQUEST + 1, 2 ** 32 - 1):
        out.append(bad("request", f"outputs: a count of {count}", u8(15) + u32(count), "malformed"))
        out.append(bad("response", f"outputs_many: a count of {count}", u8(15 | ANSWER) + u32(count), "malformed"))
    out.append(bad("request", "outputs: fewer indexes than the count", u8(15) + u32(2) + u64(1), "malformed"))
    out.append(bad("response", "outputs_many: an entry flag of 2", u8(15 | ANSWER) + u32(1) + bytes([2]), "malformed"))
    out.append(bad("response", "outputs_many: an output cut short", u8(15 | ANSWER) + u32(1) + bytes([1]) + bytes(10), "malformed"))
    out.append(bad("response", "outputs_many: an output that is not a coinbase flag 0 or 1",
                   u8(15 | ANSWER) + u32(1) + bytes([1]) + enc_stored_output({"onetime_address": "01" * 32, "amount_commitment": "02" * 32, "public_amount": 3, "height": 4, "coinbase": True})[:-1] + bytes([2]), "malformed"))
    # check_pow: a header cut short, a trailing byte, a flag that is not 0 or 1
    good = u8(16) + u64(5) + v2.enc_header(v2.sample_header("cp"))
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
                   bytes(big[: 1 + 1 + 8 + 32 + 8 + 2 + 8]) + u32(17), "malformed"))
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
                   "max_outputs_per_request": MAX_OUTPUTS_PER_REQUEST, "max_mempool_list": MAX_MEMPOOL_LIST},
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
