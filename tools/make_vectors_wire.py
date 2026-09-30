"""The reference for the peer-to-peer WIRE PROTOCOL (docs/WIRE_PROTOCOL.md) and its golden vectors.

An independent implementation, in Python and the standard library only, of how the messages of
crates/tenero-net become bytes: the frame, the twelve message layouts, the caps, and the order in which a
decoder checks a frame. The transaction and block encodings inside `blocks` and `txs` come from the version 2
data-model reference (`tools/make_vectors_v2.py`), which is itself checked against the Rust code.

    python tools/make_vectors_wire.py --check     do the committed vectors match the reference?
    python tools/make_vectors_wire.py --write     regenerate them (a PROTOCOL CHANGE: explain it in the commit
                                                  and update docs/WIRE_PROTOCOL.md)

Nothing here is random and nothing depends on the clock.
"""
import argparse
import json
import os
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import make_vectors_v2 as v2  # noqa: E402  (the data-model reference)

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
VECTOR_DIR = os.path.join(ROOT, "tests", "vectors")
NAME = "v2_wire"

MAX_FRAME = 16 * 1024 * 1024
MAX_LOCATOR = 32
MAX_IDS = 500
MAX_BLOCKS = 32
MAX_TXS = 64
MAX_NOT_FOUND = 64
MAX_ADDRS = 100

KINDS = {1: "hello", 2: "ping", 3: "pong", 4: "get_block_ids", 5: "block_ids", 6: "get_blocks",
         7: "blocks", 8: "not_found", 9: "new_block", 10: "new_tx", 11: "get_txs", 12: "txs",
         13: "get_addrs", 14: "addrs"}
BY_NAME = {v: k for k, v in KINDS.items()}

# the largest allowed value of `length` (kind byte + body), per kind
CAPS = {
    "hello": 1 + 4 + 32 + 8 + 32 + 32 + 8 + 8,
    "ping": 1 + 8, "pong": 1 + 8,
    "get_block_ids": 1 + 4 + MAX_LOCATOR * 32,
    "block_ids": 1 + 8 + 4 + MAX_IDS * 32,
    "get_blocks": 1 + 4 + MAX_BLOCKS * 32,
    "blocks": MAX_FRAME,
    "not_found": 1 + 4 + MAX_NOT_FOUND * 32,
    "new_block": 1 + 32 + 8 + 32,
    "new_tx": 1 + 4 + MAX_TXS * 32,
    "get_txs": 1 + 4 + MAX_TXS * 32,
    "txs": MAX_FRAME,
    "get_addrs": 1,
    "addrs": 1 + 4 + MAX_ADDRS * 26,
}
# the largest allowed count in a list, per kind
MAX_COUNT = {"get_block_ids": MAX_LOCATOR, "block_ids": MAX_IDS, "get_blocks": MAX_BLOCKS, "blocks": MAX_BLOCKS,
             "not_found": MAX_NOT_FOUND, "new_tx": MAX_TXS, "get_txs": MAX_TXS, "txs": MAX_TXS,
             "addrs": MAX_ADDRS}

ERRORS = ("short frame", "empty frame", "unknown kind", "frame too large", "trailing bytes",
          "count out of range", "short read", "length over maximum")


class WireError(Exception):
    def __init__(self, kind):
        super().__init__(kind)
        self.kind = kind


# ---------------------------------------------------------------- encoding


def ids_hex(items, size=32):
    out = b""
    for x in items:
        b = bytes.fromhex(x)
        assert len(b) == size
        out += b
    return out


def enc_body(m):
    k = m["kind"]
    if k == "hello":
        return (struct.pack("<I", m["version"]) + ids_hex([m["chain_id"]]) + struct.pack("<Q", m["tip_height"])
                + ids_hex([m["cumulative_work"]]) + ids_hex([m["tip_id"]]) + struct.pack("<QQ", m["pruned_below"], m["nonce"]))
    if k in ("ping", "pong"):
        return struct.pack("<Q", m["nonce"])
    if k == "get_block_ids":
        return struct.pack("<I", len(m["locator"])) + ids_hex(m["locator"])
    if k == "block_ids":
        return struct.pack("<QI", m["first_height"], len(m["ids"])) + ids_hex(m["ids"])
    if k in ("get_blocks", "not_found", "new_tx", "get_txs"):
        return struct.pack("<I", len(m["ids"])) + ids_hex(m["ids"])
    if k in ("blocks", "txs"):
        items = m[k]
        return struct.pack("<I", len(items)) + b"".join(bytes.fromhex(x) for x in items)
    if k == "get_addrs":
        return b""
    if k == "addrs":
        return struct.pack("<I", len(m["addrs"])) + b"".join(
            ids_hex([a["ip"]], 16) + struct.pack("<HQ", a["port"], a["last_seen"]) for a in m["addrs"])
    if k == "new_block":
        return ids_hex([m["id"]]) + struct.pack("<Q", m["height"]) + ids_hex([m["cumulative_work"]])
    raise AssertionError(k)


def frame(kind_byte, body):
    return struct.pack("<I", 1 + len(body)) + bytes([kind_byte]) + body


def encode(m):
    """A whole frame for the message dict `m`. Refuses what a decoder would refuse."""
    k = m["kind"]
    body = enc_body(m)
    if 1 + len(body) > CAPS[k]:
        raise ValueError(f"{k}: over the cap")
    if k in MAX_COUNT:
        n = struct.unpack("<I", body[:4] if k not in ("block_ids",) else body[8:12])[0]
        if n > MAX_COUNT[k]:
            raise ValueError(f"{k}: count over the cap")
    return frame(BY_NAME[k], body)


# ---------------------------------------------------------------- decoding (strict)


def decode_frame(data):
    data = bytes(data)
    if len(data) < 4:
        raise WireError("short frame")
    (length,) = struct.unpack("<I", data[:4])
    if length == 0:
        raise WireError("empty frame")
    if len(data) < 5:
        raise WireError("short frame")
    kind = KINDS.get(data[4])
    if kind is None:
        raise WireError("unknown kind")
    if length > CAPS[kind]:
        raise WireError("frame too large")
    if len(data) < 4 + length:
        raise WireError("short frame")
    if len(data) > 4 + length:
        raise WireError("trailing bytes")
    return decode_body(kind, data[5:4 + length])


def early_error(prefix):
    """What a STREAM decoder must already say after seeing only `prefix` (None: it must wait for more)."""
    prefix = bytes(prefix)
    if len(prefix) >= 4:
        (length,) = struct.unpack("<I", prefix[:4])
        if length == 0:
            return "empty frame"
        if len(prefix) >= 5:
            kind = KINDS.get(prefix[4])
            if kind is None:
                return "unknown kind"
            if length > CAPS[kind]:
                return "frame too large"
    return None


class Reader:
    def __init__(self, data):
        self.data = bytes(data)
        self.pos = 0

    def take(self, n):
        if n > len(self.data) - self.pos:
            raise WireError("short read")
        out = self.data[self.pos:self.pos + n]
        self.pos += n
        return out

    def u32(self):
        return struct.unpack("<I", self.take(4))[0]

    def u64(self):
        return struct.unpack("<Q", self.take(8))[0]

    def h32(self):
        return self.take(32).hex()

    def count(self, kind):
        n = self.u32()
        if n > MAX_COUNT[kind]:              # checked BEFORE reading the elements
            raise WireError("count out of range")
        return n

    def finish(self):
        if self.pos != len(self.data):
            raise WireError("trailing bytes")


def inner(fn, r):
    """Runs a data-model decoder over the reader; its errors pass through under their own names."""
    sub = v2.Reader(r.data[r.pos:])
    try:
        obj = fn(sub)
    except v2.DecodeError as e:
        raise WireError(e.kind)
    used = sub.pos
    out = r.take(used).hex()
    return out, obj


def decode_body(kind, body):
    r = Reader(body)
    m = {"kind": kind}
    if kind == "hello":
        m.update(version=r.u32(), chain_id=r.h32(), tip_height=r.u64(), cumulative_work=r.h32(),
                 tip_id=r.h32(), pruned_below=r.u64(), nonce=r.u64())
    elif kind in ("ping", "pong"):
        m["nonce"] = r.u64()
    elif kind == "get_block_ids":
        m["locator"] = [r.h32() for _ in range(r.count(kind))]
    elif kind == "block_ids":
        m["first_height"] = r.u64()
        m["ids"] = [r.h32() for _ in range(r.count(kind))]
    elif kind in ("get_blocks", "not_found", "new_tx", "get_txs"):
        m["ids"] = [r.h32() for _ in range(r.count(kind))]
    elif kind == "blocks":
        m["blocks"] = [inner(v2.dec_block, r)[0] for _ in range(r.count(kind))]
    elif kind == "txs":
        m["txs"] = [inner(v2.dec_tx, r)[0] for _ in range(r.count(kind))]
    elif kind == "get_addrs":
        pass
    elif kind == "addrs":
        m["addrs"] = []
        for _ in range(r.count(kind)):
            ip = r.take(16).hex()
            port = struct.unpack("<H", r.take(2))[0]
            m["addrs"].append({"ip": ip, "port": port, "last_seen": r.u64()})
    elif kind == "new_block":
        m.update(id=r.h32(), height=r.u64(), cumulative_work=r.h32())
    r.finish()
    return m


# ---------------------------------------------------------------- the vectors


def h(label, n=32):
    return v2.h("wire " + label, n)


def work(n):
    return f"{n:064x}"


def sample_block(tag, ntx=1):
    txs = [v2.sample_tx(f"{tag} tx{i}", proof=64 + i, ring_size=16) for i in range(ntx)]
    tx_ids = [v2.tx_id(t).hex() for t in txs]
    return {"header": v2.sample_header(tag, txs=tx_ids), "coinbase": v2.sample_coinbase(7), "transactions": txs}


def block_hex(tag, ntx=1):
    return v2.enc_block(sample_block(tag, ntx)).hex()


def tx_hex(tag, **kw):
    return v2.enc_tx(v2.sample_tx(tag, **kw)).hex()


def valid_messages():
    hello = {"kind": "hello", "version": 1, "chain_id": h("chain"), "tip_height": 123456,
             "cumulative_work": work(2 ** 70 + 5), "tip_id": h("tip"), "pruned_below": 1000,
             "nonce": 0x1122334455667788}
    out = [
        ("a handshake", hello),
        ("a handshake with every number at its extreme", {**hello, "version": 0xFFFFFFFF, "tip_height": 2 ** 64 - 1,
                                                          "cumulative_work": "ff" * 32, "pruned_below": 2 ** 64 - 1,
                                                          "nonce": 2 ** 64 - 1}),
        ("a ping", {"kind": "ping", "nonce": 0x0102030405060708}),
        ("a pong", {"kind": "pong", "nonce": 0}),
        ("a locator of one id (the genesis block)", {"kind": "get_block_ids", "locator": [h("genesis")]}),
        ("an empty locator (well formed; the engine refuses its meaning)", {"kind": "get_block_ids", "locator": []}),
        ("a locator at the cap of 32", {"kind": "get_block_ids", "locator": [h(f"loc{i}") for i in range(32)]}),
        ("block ids", {"kind": "block_ids", "first_height": 90, "ids": [h(f"id{i}") for i in range(3)]}),
        ("no block ids", {"kind": "block_ids", "first_height": 1, "ids": []}),
        ("block ids at the cap of 500", {"kind": "block_ids", "first_height": 2 ** 64 - 501,
                                         "ids": [h(f"bulk{i}") for i in range(500)]}),
        ("a request for blocks", {"kind": "get_blocks", "ids": [h("a"), h("b")]}),
        ("a request for 32 blocks", {"kind": "get_blocks", "ids": [h(f"r{i}") for i in range(32)]}),
        ("two blocks", {"kind": "blocks", "blocks": [block_hex("first", 1), block_hex("second", 2)]}),
        ("no blocks", {"kind": "blocks", "blocks": []}),
        ("not found", {"kind": "not_found", "ids": [h("gone")]}),
        ("not found at the cap of 64", {"kind": "not_found", "ids": [h(f"nf{i}") for i in range(64)]}),
        ("a block announcement", {"kind": "new_block", "id": h("new"), "height": 42, "cumulative_work": work(99)}),
        ("a transaction announcement", {"kind": "new_tx", "ids": [h("t1"), h("t2"), h("t3")]}),
        ("a request for transactions", {"kind": "get_txs", "ids": [h("t1")]}),
        ("a request for 64 transactions", {"kind": "get_txs", "ids": [h(f"q{i}") for i in range(64)]}),
        ("two transactions", {"kind": "txs", "txs": [tx_hex("x", n_in=1), tx_hex("y", n_in=2, n_out=3)]}),
        ("no transactions", {"kind": "txs", "txs": []}),
        ("a request for addresses", {"kind": "get_addrs"}),
        ("two addresses", {"kind": "addrs", "addrs": [
            {"ip": "00" * 10 + "ffff" + "0a000102", "port": 8333, "last_seen": 1_700_000_000},
            {"ip": "20010db8" + "00" * 8 + "00000001", "port": 65535, "last_seen": 2 ** 64 - 1}]}),
        ("no addresses", {"kind": "addrs", "addrs": []}),
        ("addresses at the cap of 100", {"kind": "addrs", "addrs": [
            {"ip": "00" * 10 + "ffff" + f"0a00{i // 256:02x}{i % 256:02x}", "port": 8000 + i, "last_seen": 1_700_000_000 + i}
            for i in range(100)]}),
    ]
    return out


def valid_cases():
    cases = []
    for note, m in valid_messages():
        f = encode(m)
        assert decode_frame(f) == m, note
        cases.append({"note": note, "message": m, "frame": f.hex()})
    return cases


def bad(note, data, error):
    data = bytes(data)
    try:
        decode_frame(data)
    except WireError as e:
        assert e.kind == error, (note, e.kind, error)
    else:
        raise AssertionError(f"the reference accepted an invalid frame: {note}")
    return {"note": note, "frame": data.hex(), "error": error}


def u32(n):
    return struct.pack("<I", n)


def invalid_cases():
    ping = encode({"kind": "ping", "nonce": 7})
    getbi = encode({"kind": "get_block_ids", "locator": [h("a")]})
    hello = encode(valid_messages()[0][1])
    cases = [
        bad("nothing at all", b"", "short frame"),
        bad("three bytes of a length", b"\x09\x00\x00", "short frame"),
        bad("a length and no kind", u32(9), "short frame"),
        bad("a length of zero", u32(0) + b"\x02", "empty frame"),
        bad("a length of zero and nothing else", u32(0), "empty frame"),
        bad("kind 0", u32(9) + b"\x00" + b"\x00" * 8, "unknown kind"),
        bad("kind 15", u32(9) + b"\x0f" + b"\x00" * 8, "unknown kind"),
        bad("kind 255", u32(1) + b"\xff", "unknown kind"),
        bad("an unknown kind is reported before a too-large length", u32(0xFFFFFFFF) + b"\x00", "unknown kind"),
        bad("a ping declaring one byte too many", u32(10) + b"\x02" + b"\x00" * 9, "frame too large"),
        bad("a ping declaring 4 GiB, header only", u32(0xFFFFFFFF) + b"\x02", "frame too large"),
        bad("a handshake one byte over its cap", u32(126) + b"\x01", "frame too large"),
        bad("block ids one byte over the cap, header only", u32(16014) + b"\x05", "frame too large"),
        bad("a block list over 16 MiB, header only", u32(MAX_FRAME + 1) + b"\x07", "frame too large"),
        bad("a transaction list over 16 MiB, header only", u32(MAX_FRAME + 1) + b"\x0c", "frame too large"),
        bad("a valid ping cut one byte short", ping[:-1], "short frame"),
        bad("a valid handshake cut in the middle", hello[:60], "short frame"),
        bad("a valid ping and one more byte", ping + b"\x00", "trailing bytes"),
        bad("a valid ping and a whole second ping", ping + ping, "trailing bytes"),
        bad("a handshake whose body is one byte short", u32(1 + 123) + b"\x01" + hello[5:5 + 123], "short read"),
        bad("a ping whose body is 4 bytes", u32(1 + 4) + b"\x02" + b"\x00" * 4, "short read"),
        bad("a locator claiming 33 ids", u32(1 + 4) + b"\x04" + u32(33), "count out of range"),
        bad("a locator claiming 4 billion ids", u32(1 + 4) + b"\x04" + u32(0xFFFFFFFF), "count out of range"),
        bad("block ids claiming 501", u32(1 + 8 + 4) + b"\x05" + struct.pack("<Q", 1) + u32(501), "count out of range"),
        bad("a block request claiming 33", u32(1 + 4) + b"\x06" + u32(33), "count out of range"),
        bad("blocks claiming 33", u32(1 + 4) + b"\x07" + u32(33), "count out of range"),
        bad("not found claiming 65", u32(1 + 4) + b"\x08" + u32(65), "count out of range"),
        bad("a transaction announcement claiming 65", u32(1 + 4) + b"\x0a" + u32(65), "count out of range"),
        bad("a transaction request claiming 65", u32(1 + 4) + b"\x0b" + u32(65), "count out of range"),
        bad("transactions claiming 65", u32(1 + 4) + b"\x0c" + u32(65), "count out of range"),
        bad("a locator claiming 2 ids and holding 1", u32(1 + 4 + 32) + b"\x04" + u32(2) + b"\x00" * 32, "short read"),
        bad("a locator claiming 1 id and holding 2", u32(1 + 4 + 64) + b"\x04" + u32(1) + b"\x01" * 64,
            "trailing bytes"),
        bad("no block ids but bytes after the count", u32(1 + 8 + 4 + 32) + b"\x05" + struct.pack("<Q", 1) + u32(0)
            + b"\x02" * 32, "trailing bytes"),
        bad("blocks claiming one block and holding ten bytes", u32(1 + 4 + 10) + b"\x07" + u32(1) + b"\x00" * 10,
            "short read"),
        bad("transactions claiming one and holding ten zero bytes (a transaction needs at least one input)",
            u32(1 + 4 + 10) + b"\x0c" + u32(1) + b"\x00" * 10, "count out of range"),
        bad("a transaction with a ring of 17 inside a well-formed frame",
            (lambda body: u32(1 + len(body)) + b"\x0c" + body)(u32(1) + v2.enc_tx(v2.sample_tx("bad", ring_size=17))),
            "count out of range"),
        bad("a transaction whose proof is one byte over the limit inside a well-formed frame",
            (lambda body: u32(1 + len(body)) + b"\x0c" + body)(
                u32(1) + v2.enc_tx(v2.sample_tx("big", proof=v2.MAX_PROOF + 1))),
            "length over maximum"),
        bad("a valid block with a byte missing inside a well-formed frame",
            (lambda body: u32(1 + len(body)) + b"\x07" + body)(u32(1) + bytes.fromhex(block_hex("cut"))[:-1]),
            "short read"),
        bad("a valid block followed by a stray byte inside the frame",
            (lambda body: u32(1 + len(body)) + b"\x07" + body)(u32(1) + bytes.fromhex(block_hex("stray")) + b"\x00"),
            "trailing bytes"),
        bad("a request for addresses with a body", u32(2) + b"\x0d" + b"\x00", "frame too large"),
        bad("addresses claiming 101", u32(1 + 4) + b"\x0e" + u32(101), "count out of range"),
        bad("addresses claiming 2 and holding one", u32(1 + 4 + 26) + b"\x0e" + u32(2) + b"\x00" * 26, "short read"),
        bad("addresses claiming 1 and holding 27 bytes", u32(1 + 4 + 27) + b"\x0e" + u32(1) + b"\x00" * 27,
            "trailing bytes"),
        bad("a message announcing a block with a body one byte long", u32(2) + b"\x09" + b"\x00", "short read"),
    ]
    # every kind with a cap below 16 MiB: one byte over the cap is refused from the header alone
    covered = {"hello", "ping", "block_ids"}
    for kind_name, cap in CAPS.items():
        if cap < MAX_FRAME and kind_name not in covered:
            cases.append(bad(f"{kind_name} declaring one byte over its cap, header only",
                             u32(cap + 1) + bytes([BY_NAME[kind_name]]), "frame too large"))
    return cases


def early_cases():
    """Prefixes on which a stream decoder must fail without waiting for more bytes, or must wait."""
    out = []
    for note, prefix in [
        ("a zero length is refused after 4 bytes", u32(0)),
        ("an unknown kind is refused after 5 bytes", u32(9) + b"\x00"),
        ("an oversized ping is refused after 5 bytes", u32(10) + b"\x02"),
        ("an oversized block list is refused after 5 bytes", u32(MAX_FRAME + 1) + b"\x07"),
        ("a maximum-size block list is not refused (it must wait)", u32(MAX_FRAME) + b"\x07"),
        ("a ping length is not refused (it must wait)", u32(9) + b"\x02"),
        ("three bytes: it must wait", b"\x09\x00\x00"),
        ("four bytes of a valid length: it must wait", u32(9)),
    ]:
        out.append({"note": note, "prefix": prefix.hex(), "error": early_error(prefix)})
    return out


def build():
    return {
        "schema": 1, "name": NAME,
        "description": "The peer-to-peer wire protocol (docs/WIRE_PROTOCOL.md): every message as a whole frame, "
                       "malformed frames with the error a decoder must give, and the prefixes on which a stream "
                       "decoder must already fail. Made by tools/make_vectors_wire.py. `blocks` and `txs` hold "
                       "the wire form (CONSENSUS_V2.md) of each block and transaction as hex.",
        "limits": {"max_frame": MAX_FRAME, "max_locator": MAX_LOCATOR, "max_ids": MAX_IDS, "max_blocks": MAX_BLOCKS,
                   "max_txs": MAX_TXS, "max_not_found": MAX_NOT_FOUND, "max_addrs": MAX_ADDRS},
        "kinds": {str(k): v for k, v in KINDS.items()},
        "caps": CAPS,
        "valid": valid_cases(),
        "invalid": invalid_cases(),
        "early": early_cases(),
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
