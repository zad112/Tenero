"""The reference for the POOL protocol (docs/POOL_PROTOCOL.md) and its golden vectors. A DRAFT: nothing speaks it yet.

An independent implementation, in Python and the standard library only, of how a miner and a pool turn their messages
into bytes: the frame, the five messages a miner sends, the eight a pool sends, and the order in which a decoder checks a
message. The coinbase, transaction and header encodings inside come from the version 2 data-model reference
(`reference/tools/make_vectors_v2.py`).

    python reference/tools/make_vectors_pool.py --check     do the committed vectors match the reference?
    python reference/tools/make_vectors_pool.py --write     regenerate them (a PROTOCOL CHANGE: explain it in the commit
                                                            and update docs/POOL_PROTOCOL.md)

Nothing here is random and nothing depends on the clock.
"""
import argparse
import json
import os
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import make_vectors_v2 as v2  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
VECTOR_DIR = os.path.join(ROOT, "tests", "vectors")
NAME = "pool"

MAX_FRAME = 4 * 1024 * 1024
MAX_NETWORK = 32
MAX_ADDRESS = 256
MAX_WORKER = 32
MAX_AGENT = 64
MAX_POOL_NAME = 64
MAX_TEXT = 128
MAX_TX_IDS = 8192
MAX_PROVIDED_TXS = 64
MAX_TTL = 3600
MAX_PREFIX_BITS = 32
HEADER_LEN = 146
ERROR = 0xFF

# miner to pool: kinds 1 to 5. pool to miner: 0x80 and up (the answer to request k is k | 0x80 where there is one).
MINER = {"hello": 1, "submit_share": 2, "ping": 3, "declare_job": 4, "provide_txs": 5}
POOL = {"hello_ok": 0x81, "share_result": 0x82, "pong": 0x83, "declare_result": 0x84, "job": 0x90,
        "set_share_target": 0x91, "set_payout": 0x92}

SHARE_REASONS = range(0, 7)          # 0 accepted, 1 stale, 2 duplicate, 3 above the target, 4 bad mix, 5 unknown job, 6 not allowed
DECLARE_REFUSED_REASONS = range(1, 8)
ACCEPTED, REFUSED, MISSING = 0, 1, 2


class PoolError(Exception):
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
    assert len(b) == n, (len(b), n)
    return b


def text(s, maximum):
    b = s.encode("utf-8")
    assert len(b) <= maximum, "text over the maximum"
    return u32(len(b)) + b


def flag(b):
    return u8(1 if b else 0)


def dec_flag(r):
    b = r.u8()
    if b not in (0, 1):
        raise PoolError("malformed")
    return b == 1


def dec_text(r, maximum):
    try:
        return bytes.fromhex(r.var(maximum)).decode("utf-8")
    except UnicodeDecodeError:
        raise PoolError("malformed")


def need(cond):
    if not cond:
        raise PoolError("malformed")


def enc_header_empty(h):
    """A header the pool hands out: the nonce and the mix are for the miner to fill, so they must be empty."""
    assert h["nonce"] == 0 and h["mix"] == "00" * 64
    return v2.enc_header(h)


def dec_header_empty(r):
    h = v2.dec_header(r)
    need(h["nonce"] == 0 and h["mix"] == "00" * 64)
    return h


def target_ok(t):
    return t != "00" * 32 and t != "ff" * 32


# ---- miner to pool ---------------------------------------------------------------------------------------------

def enc_miner(m):
    t = m["type"]
    body = u8(MINER[t])
    if t == "hello":
        assert 1 <= m["min_version"] <= m["max_version"]
        body += (u16(m["min_version"]) + u16(m["max_version"]) + u32(m["capabilities"]) + text(m["network"], MAX_NETWORK)
                 + text(m["address"], MAX_ADDRESS) + text(m["worker"], MAX_WORKER) + text(m["agent"], MAX_AGENT))
    elif t == "submit_share":
        body += u64(m["job_id"]) + u64(m["nonce"]) + fixed(m["mix"], 64)
    elif t == "ping":
        body += u64(m["token"])
    elif t == "declare_job":
        assert len(m["tx_ids"]) <= MAX_TX_IDS
        body += (u64(m["decl_id"]) + u64(m["height"]) + fixed(m["prev_id"], 32) + u64(m["timestamp"])
                 + v2.enc_coinbase(m["coinbase"]) + u32(len(m["tx_ids"])) + b"".join(fixed(i, 32) for i in m["tx_ids"]))
    elif t == "provide_txs":
        assert 1 <= len(m["txs"]) <= MAX_PROVIDED_TXS
        body += u64(m["decl_id"]) + v2.w_list(m["txs"], v2.enc_tx)
    return body


def dec_miner(body):
    if len(body) == 0:
        raise PoolError("length")
    r = v2.Reader(body)
    kind = r.u8()
    names = {v: k for k, v in MINER.items()}
    if kind not in names:
        raise PoolError("kind")
    t = names[kind]
    m = {"type": t}
    try:
        if t == "hello":
            m["min_version"], m["max_version"], m["capabilities"] = r.u16(), r.u16(), r.u32()
            need(1 <= m["min_version"] <= m["max_version"])
            m["network"] = dec_text(r, MAX_NETWORK)
            m["address"] = dec_text(r, MAX_ADDRESS)
            m["worker"] = dec_text(r, MAX_WORKER)
            m["agent"] = dec_text(r, MAX_AGENT)
        elif t == "submit_share":
            m["job_id"], m["nonce"], m["mix"] = r.u64(), r.u64(), r.fixed(64)
        elif t == "ping":
            m["token"] = r.u64()
        elif t == "declare_job":
            m["decl_id"], m["height"], m["prev_id"], m["timestamp"] = r.u64(), r.u64(), r.fixed(32), r.u64()
            m["coinbase"] = v2.dec_coinbase(r)
            m["tx_ids"] = [r.fixed(32) for _ in range(r.count(0, MAX_TX_IDS))]
        elif t == "provide_txs":
            m["decl_id"] = r.u64()
            m["txs"] = [v2.dec_tx(r) for _ in range(r.count(1, MAX_PROVIDED_TXS))]
    except v2.DecodeError:
        raise PoolError("malformed")
    try:
        r.finish()
    except v2.DecodeError:
        raise PoolError("trailing")
    return m


# ---- pool to miner ---------------------------------------------------------------------------------------------

def enc_pool(m):
    t = m["type"]
    if t == "error":
        return u8(ERROR) + text(m["message"], MAX_TEXT)
    body = u8(POOL[t])
    if t == "hello_ok":
        assert 1 <= m["version"] and 0 <= m["prefix_bits"] <= MAX_PREFIX_BITS and m["prefix"] < (1 << m["prefix_bits"])
        assert target_ok(m["share_target"])
        body += (u16(m["version"]) + u32(m["capabilities"]) + u64(m["session"]) + u8(m["prefix_bits"]) + u64(m["prefix"])
                 + fixed(m["share_target"], 32) + text(m["pool_name"], MAX_POOL_NAME) + flag(m["pays_pool"]))
    elif t == "share_result":
        assert m["accepted"] == (m["reason"] == 0) and m["reason"] in SHARE_REASONS
        body += u64(m["job_id"]) + flag(m["accepted"]) + u8(m["reason"]) + text(m["text"], MAX_TEXT)
    elif t == "pong":
        body += u64(m["token"])
    elif t == "declare_result":
        body += u64(m["decl_id"])
        s = m["status"]
        if s == "accepted":
            body += u8(ACCEPTED) + u64(m["job_id"]) + enc_header_empty(m["header"])
        elif s == "refused":
            assert m["reason"] in DECLARE_REFUSED_REASONS
            body += u8(REFUSED) + u8(m["reason"]) + text(m["text"], MAX_TEXT)
        else:
            assert 1 <= len(m["missing"]) <= MAX_TX_IDS
            body += u8(MISSING) + u32(len(m["missing"])) + b"".join(fixed(i, 32) for i in m["missing"])
    elif t == "job":
        assert 1 <= m["ttl"] <= MAX_TTL
        body += (u64(m["job_id"]) + u64(m["height"]) + flag(m["clean"]) + enc_header_empty(m["header"])
                 + fixed(m["block_target"], 32) + u32(m["ttl"]))
    elif t == "set_share_target":
        assert target_ok(m["share_target"])
        body += fixed(m["share_target"], 32)
    elif t == "set_payout":
        body += (u64(m["height"]) + fixed(m["onetime_address"], 32) + fixed(m["view_tag"], 3)
                 + fixed(m["ephemeral_pubkey"], 32) + fixed(m["anchor_enc"], 16))
    return body


def dec_pool(body):
    if len(body) == 0:
        raise PoolError("length")
    r = v2.Reader(body)
    kind = r.u8()
    names = {v: k for k, v in POOL.items()}
    try:
        if kind == ERROR:
            m = {"type": "error", "message": dec_text(r, MAX_TEXT)}
        elif kind in names:
            t = names[kind]
            m = {"type": t}
            if t == "hello_ok":
                m["version"], m["capabilities"], m["session"] = r.u16(), r.u32(), r.u64()
                need(m["version"] >= 1)
                m["prefix_bits"], m["prefix"] = r.u8(), r.u64()
                need(m["prefix_bits"] <= MAX_PREFIX_BITS and m["prefix"] < (1 << m["prefix_bits"]))
                m["share_target"] = r.fixed(32)
                need(target_ok(m["share_target"]))
                m["pool_name"] = dec_text(r, MAX_POOL_NAME)
                m["pays_pool"] = dec_flag(r)
            elif t == "share_result":
                m["job_id"] = r.u64()
                m["accepted"] = dec_flag(r)
                m["reason"] = r.u8()
                need(m["reason"] in SHARE_REASONS and m["accepted"] == (m["reason"] == 0))
                m["text"] = dec_text(r, MAX_TEXT)
            elif t == "pong":
                m["token"] = r.u64()
            elif t == "declare_result":
                m["decl_id"] = r.u64()
                s = r.u8()
                if s == ACCEPTED:
                    m.update({"status": "accepted", "job_id": r.u64(), "header": dec_header_empty(r)})
                elif s == REFUSED:
                    m["status"] = "refused"
                    m["reason"] = r.u8()
                    need(m["reason"] in DECLARE_REFUSED_REASONS)
                    m["text"] = dec_text(r, MAX_TEXT)
                elif s == MISSING:
                    m["status"] = "missing"
                    m["missing"] = [r.fixed(32) for _ in range(r.count(1, MAX_TX_IDS))]
                else:
                    raise PoolError("malformed")
            elif t == "job":
                m["job_id"], m["height"] = r.u64(), r.u64()
                m["clean"] = dec_flag(r)
                m["header"] = dec_header_empty(r)
                m["block_target"] = r.fixed(32)
                m["ttl"] = r.u32()
                need(1 <= m["ttl"] <= MAX_TTL)
            elif t == "set_share_target":
                m["share_target"] = r.fixed(32)
                need(target_ok(m["share_target"]))
            elif t == "set_payout":
                m.update({"height": r.u64(), "onetime_address": r.fixed(32), "view_tag": r.fixed(3),
                          "ephemeral_pubkey": r.fixed(32), "anchor_enc": r.fixed(16)})
        else:
            raise PoolError("kind")
    except v2.DecodeError:
        raise PoolError("malformed")
    try:
        r.finish()
    except v2.DecodeError:
        raise PoolError("trailing")
    return m


def frame(body):
    if not 1 <= len(body) <= MAX_FRAME:
        raise PoolError("length")
    return u32(len(body)) + body


# ---- the cases -------------------------------------------------------------------------------------------------

def h(c, n=32):
    """`n` copies of the byte `c` (two hex digits), as hex."""
    return c * n


def sample_coinbase(height=7):
    return {"version": 2, "height": height,
            "outputs": [{"onetime_address": h("01"), "amount": 5, "view_tag": h("02", 3), "ephemeral_pubkey": h("03"),
                         "anchor_enc": h("04", 16)}], "extra": ""}


def sample_output(seed):
    return {"onetime_address": h(seed), "amount_commitment": h("03"), "amount_enc": h("04", 8),
            "view_tag": h("05", 3), "ephemeral_pubkey": h("06"), "anchor_enc": h("07", 16)}


def sample_tx(n_in=1):
    return {"version": 2, "inputs": [{"key_image": "%02x" % (i + 1) * 32} for i in range(n_in)],
            "outputs": [sample_output("02"), sample_output("08")], "fee": 12345, "extra": "0909",
            "rings": [[1, 2] for _ in range(n_in)], "proof_data": "08" * 10}


def sample_header(nonce=0, mix="00" * 64):
    return {"version": 2, "prev_id": h("11"), "timestamp": 1700000000, "tx_root": h("22"), "nonce": nonce, "mix": mix}


def hello(**kw):
    m = {"type": "hello", "min_version": 1, "max_version": 1, "capabilities": 0, "network": "alpha",
         "address": "tni1examplepayoutaddress", "worker": "rig1", "agent": "tenero-miner/0.1"}
    m.update(kw)
    return m


def hello_ok(**kw):
    m = {"type": "hello_ok", "version": 1, "capabilities": 0, "session": 99, "prefix_bits": 16, "prefix": 0xABCD,
         "share_target": "0000ffff" + "ff" * 28, "pool_name": "test pool", "pays_pool": True}
    m.update(kw)
    return m


def job(**kw):
    m = {"type": "job", "job_id": 5, "height": 7, "clean": True, "header": sample_header(),
         "block_target": "00000000" + "ff" * 28, "ttl": 60}
    m.update(kw)
    return m


def valid_miner():
    return [
        ("hello", hello()),
        ("hello, job declaration offered, the longest strings",
         hello(capabilities=1, network="n" * MAX_NETWORK, address="a" * MAX_ADDRESS, worker="w" * MAX_WORKER,
               agent="g" * MAX_AGENT)),
        ("hello, versions 1 to 9, unknown capability bits", hello(min_version=1, max_version=9, capabilities=0xFFFFFFFF)),
        ("hello with accents", hello(worker="équipe", agent="прыклад")),
        ("a share", {"type": "submit_share", "job_id": 5, "nonce": 2 ** 64 - 1, "mix": "ab" * 64}),
        ("a ping", {"type": "ping", "token": 12345}),
        ("declare a job with no transactions", {"type": "declare_job", "decl_id": 1, "height": 7, "prev_id": h("11"),
                                                "timestamp": 1700000000, "coinbase": sample_coinbase(),
                                                "tx_ids": []}),
        ("declare a job with three transactions", {"type": "declare_job", "decl_id": 2, "height": 7,
                                                   "prev_id": h("11"), "timestamp": 1700000060,
                                                   "coinbase": sample_coinbase(), "tx_ids": [h("aa"), h("bb"), h("cc")]}),
        ("provide one transaction", {"type": "provide_txs", "decl_id": 2, "txs": [sample_tx()]}),
        ("provide two transactions", {"type": "provide_txs", "decl_id": 2, "txs": [sample_tx(), sample_tx(3)]}),
    ]


def valid_pool():
    return [
        ("hello_ok", hello_ok()),
        ("hello_ok, no prefix (a pool of one miner)", hello_ok(prefix_bits=0, prefix=0)),
        ("hello_ok, the widest prefix, job declaration supported",
         hello_ok(prefix_bits=32, prefix=2 ** 32 - 1, capabilities=1)),
        ("hello_ok, the hardest and the easiest allowed target",
         hello_ok(share_target="00" * 31 + "01")),
        ("hello_ok, an easy target", hello_ok(share_target="7f" + "ff" * 30 + "fe")),
        ("a share was accepted", {"type": "share_result", "job_id": 5, "accepted": True, "reason": 0, "text": ""}),
        ("a share was stale", {"type": "share_result", "job_id": 5, "accepted": False, "reason": 1, "text": "tip moved"}),
        ("a share was a duplicate", {"type": "share_result", "job_id": 5, "accepted": False, "reason": 2, "text": ""}),
        ("a share was above the target", {"type": "share_result", "job_id": 5, "accepted": False, "reason": 3,
                                          "text": "x" * MAX_TEXT}),
        ("a share had the wrong mix", {"type": "share_result", "job_id": 5, "accepted": False, "reason": 4, "text": ""}),
        ("a share for an unknown job", {"type": "share_result", "job_id": 5, "accepted": False, "reason": 5, "text": ""}),
        ("a share from a miner that is not allowed", {"type": "share_result", "job_id": 5, "accepted": False,
                                                      "reason": 6, "text": "too many bad shares"}),
        ("a pong", {"type": "pong", "token": 12345}),
        ("a declaration was accepted", {"type": "declare_result", "decl_id": 1, "status": "accepted", "job_id": 8,
                                        "header": sample_header()}),
        ("a declaration was refused", {"type": "declare_result", "decl_id": 1, "status": "refused", "reason": 2,
                                       "text": "the coinbase does not pay the pool"}),
        ("a declaration was refused for every reason", {"type": "declare_result", "decl_id": 1, "status": "refused",
                                                        "reason": 7, "text": ""}),
        ("a declaration lacks one transaction", {"type": "declare_result", "decl_id": 2, "status": "missing",
                                                 "missing": [h("bb")]}),
        ("a declaration lacks two transactions", {"type": "declare_result", "decl_id": 2, "status": "missing",
                                                  "missing": [h("aa"), h("cc")]}),
        ("a job", job()),
        ("a job that makes every earlier one dead", job(clean=True, job_id=6)),
        ("a job that does not", job(clean=False, job_id=7, ttl=3600)),
        ("the share target changes", {"type": "set_share_target", "share_target": "0001" + "ff" * 30}),
        ("the pool's payout for a height", {"type": "set_payout", "height": 8, "onetime_address": h("0a"),
                                            "view_tag": h("0b", 3), "ephemeral_pubkey": h("0c"),
                                            "anchor_enc": h("0d", 16)}),
        ("an error", {"type": "error", "message": "no"}),
        ("the longest error", {"type": "error", "message": "x" * MAX_TEXT}),
    ]


def valid_cases():
    cases = []
    for note, m in valid_miner():
        body = enc_miner(m)
        assert dec_miner(body) == m, note
        cases.append({"direction": "miner", "note": note, "message": m, "body": body.hex(), "frame": frame(body).hex()})
    for note, m in valid_pool():
        body = enc_pool(m)
        assert dec_pool(body) == m, note
        cases.append({"direction": "pool", "note": note, "message": m, "body": body.hex(), "frame": frame(body).hex()})
    return cases


def bad(direction, note, body, error):
    body = bytes(body)
    dec = dec_miner if direction == "miner" else dec_pool
    try:
        dec(body)
    except PoolError as e:
        assert e.kind == error, (note, e.kind, error)
    else:
        raise AssertionError(f"the reference accepted an invalid message: {note}")
    return {"direction": direction, "note": note, "body": body.hex(), "error": error}


def invalid_cases():
    out = []
    for d in ("miner", "pool"):
        out.append(bad(d, "an empty body", b"", "length"))
    # kinds that do not exist, and kinds of the other direction
    for k in (0, 6, 0x7F, 0x80, 0x81, 0x90, 0xFF):
        out.append(bad("miner", f"unknown miner kind {k}", bytes([k]), "kind"))
    for k in (0, 1, 5, 0x7F, 0x80, 0x85, 0x8F, 0x93, 0xFE):
        out.append(bad("pool", f"unknown pool kind {k}", bytes([k]), "kind"))
    # every message cut short and with a byte too many
    for direction, valid, enc in (("miner", valid_miner(), enc_miner), ("pool", valid_pool(), enc_pool)):
        for note, m in valid:
            body = enc(m)
            if len(body) > 2:
                out.append(bad(direction, f"{note}: cut in the middle", body[: len(body) // 2], "malformed"))
            out.append(bad(direction, f"{note}: with a trailing byte", body + b"\0", "trailing"))
    # hello
    base = enc_miner(hello())
    out.append(bad("miner", "hello: the lowest version is 0", bytes([1]) + u16(0) + u16(1) + base[5:], "malformed"))
    out.append(bad("miner", "hello: the lowest version is above the highest", bytes([1]) + u16(2) + u16(1) + base[5:],
                   "malformed"))
    for field, cap in (("network", MAX_NETWORK), ("address", MAX_ADDRESS), ("worker", MAX_WORKER), ("agent", MAX_AGENT)):
        m = hello()
        body = bytearray(bytes([1]) + u16(1) + u16(1) + u32(0))
        for f, c in (("network", MAX_NETWORK), ("address", MAX_ADDRESS), ("worker", MAX_WORKER), ("agent", MAX_AGENT)):
            n = c + 1 if f == field else len(m[f])
            body += u32(n) + b"x" * n
        out.append(bad("miner", f"hello: {field} over the cap", bytes(body), "malformed"))
    out.append(bad("miner", "hello: a network name that is not UTF-8",
                   bytes([1]) + u16(1) + u16(1) + u32(0) + u32(2) + b"\xff\xfe" + u32(0) + u32(0) + u32(0), "malformed"))
    # declarations
    out.append(bad("miner", "declare_job: 8193 transaction ids",
                   bytes(enc_miner({"type": "declare_job", "decl_id": 1, "height": 7, "prev_id": h("11"),
                                    "timestamp": 1, "coinbase": sample_coinbase(), "tx_ids": []})[:-4]) + u32(MAX_TX_IDS + 1),
                   "malformed"))
    pt = enc_miner({"type": "provide_txs", "decl_id": 2, "txs": [sample_tx()]})
    out.append(bad("miner", "provide_txs: no transactions", pt[:1 + 8] + u32(0), "malformed"))
    out.append(bad("miner", "provide_txs: 65 transactions", pt[:1 + 8] + u32(MAX_PROVIDED_TXS + 1), "malformed"))
    # hello_ok
    ok = enc_pool(hello_ok())
    out.append(bad("pool", "hello_ok: version 0", ok[:1] + u16(0) + ok[3:], "malformed"))
    # offsets: kind 1, version 2, capabilities 4, session 8, prefix_bits 1, prefix 8, target 32
    off_bits = 1 + 2 + 4 + 8
    out.append(bad("pool", "hello_ok: a prefix of 33 bits", ok[:off_bits] + u8(33) + ok[off_bits + 1:], "malformed"))
    out.append(bad("pool", "hello_ok: a prefix too big for its bits",
                   ok[:off_bits] + u8(16) + u64(0x10000) + ok[off_bits + 9:], "malformed"))
    out.append(bad("pool", "hello_ok: no bits and a prefix", ok[:off_bits] + u8(0) + u64(1) + ok[off_bits + 9:],
                   "malformed"))
    off_target = off_bits + 1 + 8
    out.append(bad("pool", "hello_ok: a share target of zero", ok[:off_target] + bytes(32) + ok[off_target + 32:],
                   "malformed"))
    out.append(bad("pool", "hello_ok: a share target of all ones", ok[:off_target] + b"\xff" * 32 + ok[off_target + 32:],
                   "malformed"))
    out.append(bad("pool", "hello_ok: the pays-the-pool flag is 2", ok[:-1] + u8(2), "malformed"))
    out.append(bad("pool", "hello_ok: a pool name over the cap",
                   ok[:off_target + 32] + u32(MAX_POOL_NAME + 1) + b"x" * (MAX_POOL_NAME + 1) + u8(1), "malformed"))
    # share_result
    sr = lambda acc, reason: u8(0x82) + u64(5) + u8(acc) + u8(reason) + u32(0)  # noqa: E731
    out.append(bad("pool", "share_result: accepted with a reason", sr(1, 3), "malformed"))
    out.append(bad("pool", "share_result: refused with reason 0", sr(0, 0), "malformed"))
    out.append(bad("pool", "share_result: reason 7", sr(0, 7), "malformed"))
    out.append(bad("pool", "share_result: the accepted flag is 2", sr(2, 0), "malformed"))
    out.append(bad("pool", "share_result: text over the cap",
                   u8(0x82) + u64(5) + u8(0) + u8(0) + u32(MAX_TEXT + 1) + b"x" * (MAX_TEXT + 1), "malformed"))
    # declare_result
    out.append(bad("pool", "declare_result: status 3", u8(0x84) + u64(1) + u8(3), "malformed"))
    out.append(bad("pool", "declare_result: refused with reason 0", u8(0x84) + u64(1) + u8(1) + u8(0) + u32(0), "malformed"))
    out.append(bad("pool", "declare_result: refused with reason 8", u8(0x84) + u64(1) + u8(1) + u8(8) + u32(0), "malformed"))
    out.append(bad("pool", "declare_result: missing, none", u8(0x84) + u64(1) + u8(2) + u32(0), "malformed"))
    out.append(bad("pool", "declare_result: missing 8193", u8(0x84) + u64(1) + u8(2) + u32(MAX_TX_IDS + 1), "malformed"))
    dr = enc_pool({"type": "declare_result", "decl_id": 1, "status": "accepted", "job_id": 8, "header": sample_header()})
    out.append(bad("pool", "declare_result: an accepted header that already has a nonce",
                   dr[:1 + 8 + 1 + 8 + 2 + 32 + 8 + 32] + u64(1) + dr[1 + 8 + 1 + 8 + 2 + 32 + 8 + 32 + 8:], "malformed"))
    # job
    jb = enc_pool(job())
    out.append(bad("pool", "job: the clean flag is 2", jb[:1 + 8 + 8] + u8(2) + jb[1 + 8 + 8 + 1:], "malformed"))
    off_nonce = 1 + 8 + 8 + 1 + 2 + 32 + 8 + 32
    out.append(bad("pool", "job: a header that already has a nonce", jb[:off_nonce] + u64(1) + jb[off_nonce + 8:],
                   "malformed"))
    out.append(bad("pool", "job: a header that already has a mix",
                   jb[:off_nonce + 8] + b"\x01" + jb[off_nonce + 9:], "malformed"))
    out.append(bad("pool", "job: a time to live of 0", jb[:-4] + u32(0), "malformed"))
    out.append(bad("pool", "job: a time to live of 3601", jb[:-4] + u32(MAX_TTL + 1), "malformed"))
    # set_share_target
    out.append(bad("pool", "set_share_target: zero", u8(0x91) + bytes(32), "malformed"))
    out.append(bad("pool", "set_share_target: all ones", u8(0x91) + b"\xff" * 32, "malformed"))
    # error text
    out.append(bad("pool", "an error that is not UTF-8", u8(ERROR) + u32(2) + b"\xff\xfe", "malformed"))
    out.append(bad("pool", "an error over the cap", u8(ERROR) + u32(MAX_TEXT + 1) + b"x" * (MAX_TEXT + 1), "malformed"))
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
        "description": "The pool protocol between a miner and a mining pool (docs/POOL_PROTOCOL.md, a DRAFT): every message "
                       "as a body and a frame, malformed bodies with the error a decoder must give, and the frame length "
                       "rule. An independent Python implementation (reference/tools/make_vectors_pool.py).",
        "limits": {"max_frame": MAX_FRAME, "max_network": MAX_NETWORK, "max_address": MAX_ADDRESS,
                   "max_worker": MAX_WORKER, "max_agent": MAX_AGENT, "max_pool_name": MAX_POOL_NAME,
                   "max_text": MAX_TEXT, "max_tx_ids": MAX_TX_IDS, "max_provided_txs": MAX_PROVIDED_TXS,
                   "max_ttl": MAX_TTL, "max_prefix_bits": MAX_PREFIX_BITS, "header_len": HEADER_LEN},
        "kinds": {"miner": MINER, "pool": POOL, "error": ERROR},
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
