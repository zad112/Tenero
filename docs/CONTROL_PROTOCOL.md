# The control protocol (a DRAFT, milestone M8.7)

How a wallet, or any program on the same computer, talks to a running node. It is **not** the peer-to-peer protocol
(`docs/WIRE_PROTOCOL.md`): it is for one machine, it is not encrypted, and it has its own few messages. Decision 4 of
`docs/M8_PLAN.md` asked for "the same protocol with a few extra request messages"; what was built is the same
*style* (length-prefixed frames, strict decoding, golden vectors from an independent Python reference) with its own
message set, because the peer-to-peer messages are about sharing blocks with strangers and the wallet needs to ask
questions. **Experimental and unaudited.** The code is `crates/tenero-app/src/{control,server,client}.rs`; the vectors
are `tests/vectors/control.json`, made by `tools/make_vectors_control.py`, an independent implementation, and checked
by `crates/tenero-app/tests/control_vectors.rs` and `tests/test_vectors_control.py`.

## Who may connect, and how

* **Only this computer.** The node listens on a loopback address (default `127.0.0.1:18332` on the test network,
  `127.0.0.1:28332` on the dev network). Any other address is refused when the node starts (and by the `control`
  setting). Every accepted connection's peer address is checked again. The client library refuses to connect anywhere
  else, so a cookie never leaves the machine.
* **Only a program that can read the node's data directory.** At every start the node makes a new 32-byte random
  **cookie** and writes it, as 64 lower-case hexadecimal digits, to `control.cookie` in its data directory. The first
  message of a connection must be `auth` with that cookie. Anything else (another request first, a wrong cookie, a
  malformed frame) closes the connection with no answer. This keeps another user of the same computer, or a web page that
  makes a browser send bytes to a local port, from reading the chain through the node, sending transactions or stopping it.
  **The file has the default permissions of the data directory; protecting that directory is the operator's job.**
  The cookie is compared without stopping at the first difference.
* **Limits:** at most 8 connections at once (more are closed at once); a connection that has not authenticated in 10 s,
  or is silent for 10 minutes, is closed; at most 64 requests wait for the node (past that the answer is "the node is
  busy"); the node answers at most 32 requests each time it looks, so a flood cannot starve the network code.

**What this does not give:** it is not encrypted (the bytes stay on the machine); it cannot tell two programs of the same
user apart; and anything holding the cookie may read the whole chain, send transactions and ask the node to stop.

## Frames

```
frame  = length u32 little-endian | body            1 <= length <= 16,777,216 (16 MiB)
body   = kind u8 | payload
```

Integers are little-endian and fixed-width. A *count* is a `u32`; a *var* (bytes or text) is a `u32` length and then the
bytes; a *flag* is one byte, 0 or 1 (nothing else); text is UTF-8. The transaction, transaction prefix and coinbase
output encodings are those of `docs/CONSENSUS_V2.md` section 4 (`tenero-core`'s `v2` module).

**Decoding is strict.** A frame of length 0 or over the limit, an unknown kind, a payload that is too short, a byte after
the end of the message, a count over its limit (checked before any element is read), a flag that is not 0 or 1, a node
kind that is not 0 or 1, and text that is not UTF-8 are all errors, and one message has one encoding. A request that
cannot be decoded is answered with an error (once) and the connection is closed.

## Requests and answers

The answer to request `k` has kind `k | 0x80`. Any request may instead be answered with an **error** (kind `0xFF`:
`message` as text of at most 512 bytes).

| kind | request | payload | answer |
|---|---|---|---|
| 1 | `auth` | cookie (32) | `authed` (kind 0x81), no payload |
| 2 | `tip` | | `tip`: height u64, id (32) |
| 3 | `block` | height u64 | `block`: flag, then (if 1) a *scan block* |
| 4 | `output` | global index u64 | `output`: flag, then (if 1) a *stored output* |
| 5 | `output_count` | | `output_count`: u64 |
| 6 | `key_image_spent` | key image (32) | `spent`: flag |
| 7 | `rules` | | `rules`: chain id (32), ring size u32, coinbase maturity u64, spend maturity u64, next height u64, reward u64, median u64 |
| 8 | `submit_tx` | a whole transaction | `tx_accepted`: id (32), or an error saying why not |
| 9 | `info` | | `info`: height u64, tip id (32), peers u32, inbound u32, pruned-below u64, mempool transactions u32, syncing flag, node kind u8 (0 archive, 1 pruned), network (text, at most 64 bytes), version (text, at most 64 bytes) |
| 10 | `stop` | | `stopping`: asks the node to shut down cleanly |
| 11 | `blocks` | from u64, count u16 (1 to 64) | `blocks`: count u32 (0 to 64), then that many scan blocks |

* A **scan block** is what a wallet needs: height u64, block id (32), the global index of its first output u64, the
  coinbase (version u16, height u64, a count of coinbase outputs from **0** to 16 and the outputs, extra as a var), and a
  count of transaction **prefixes** (0 to 8192) and the prefixes. It carries no rings and no proofs, so a pruned node can
  serve it. **The genesis block has no coinbase outputs** (the consensus coinbase encoding requires at least one), which
  is why a scan block writes the coinbase itself.
* A **stored output**: one-time address (32), amount commitment (32; all zeros for a coinbase output), public amount u64
  (0 for an ordinary output), height u64, coinbase flag.
* `blocks` is the way to scan a chain: up to 64 blocks from a height, in order; fewer at the tip, and **fewer if they would
  not fit in 8 MiB** (the node always sends at least one, so a client always makes progress and asks again from where it
  stopped). One block after another is what a wallet asks for; a client that gets a block other than the one it asked for
  must treat it as an error.
* `submit_tx`: the node first checks the transaction against the tip as a mempool would (and says why not if it fails,
  or that it already has it); if it passes it is handed to the node's engine, which keeps it and tells the peers, and the
  answer comes at the next look, once the pool has been seen to keep it. **"Accepted" means in this node's pool, not in a
  block.**

## What the answers do not say

The node answers what it is asked about **its own chain**. A client that wants to know it is on the best chain asks
`info` (is the node `syncing`?), and nothing stops a node from lying to a wallet that trusts it: the wallet trusts the
node it is pointed at. That is a property of this design, not something the protocol checks.

## Vectors

`tests/vectors/control.json`: 36 valid messages (both directions), 90 malformed bodies with the error class a decoder
must give (`length`, `kind`, `trailing`, `malformed`), and the frame length rule. Every valid message encodes to exactly
the reference's bytes in Rust and decodes back. The reference also checks, over every valid message and every
single-bit change of it, that the result is refused or decodes to a message that encodes back to the same bytes.
