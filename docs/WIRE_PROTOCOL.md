# The peer-to-peer wire protocol, version 1 (a DRAFT, milestones M8.2, M8.3a and M8.3)

Status: **draft, 2026-09-30, unreviewed.** This is how the messages of `crates/tenero-net` (`Message`) become
bytes. It reuses the version 2 codec of `CONSENSUS_V2.md` section 4: fixed-width little-endian integers, no
varints, strict decoding, and `u32` counts checked **before** any element is read or any memory is reserved. The
independent Python reference is `tools/make_vectors_wire.py`, which makes `tests/vectors/v2_wire.json`; the Rust
code (`crates/tenero-net/src/wire.rs`) must reproduce every vector.

This layer carries no cryptography. The encrypted channel (Noise, M8.4, `crates/tenero-net/src/noise.rs`) carries
these frames as a byte stream: after a handshake bound to the protocol version and the chain id, the stream is cut
into chunks of at most 65,519 plaintext bytes, each sent as `length u16 (big-endian) | ciphertext` (ciphertext =
plaintext + a 16-byte tag, never empty), with the Noise message counter as the nonce. The frames below are what
comes out of that stream, in order, however the chunks fall.

## 1. Frames

```
frame  = length u32      the number of bytes that follow: 1 (the kind) + the body
         kind   u8       which message (section 3)
         body            the message's fields, in the order of section 3
```

- One frame carries one message. **A message has exactly one encoding**: a decoder that accepts bytes must
  produce a message that encodes back to the same bytes (the tests check this on random input).
- `length` is at least 1 and at most the **cap of the kind** (section 2). A receiver checks it as soon as it has
  read the 5 bytes of length and kind, **before** it buffers or reads the body, so a peer cannot make it allocate
  more than one kind's cap.
- A body must be used exactly: bytes left over after the message are an error.

## 2. Limits (the caps a decoder enforces; an encoder refuses to exceed them)

| what | limit |
|---|---|
| ids in a block locator (`get_block_ids`) | 0 to 32 |
| ids in `block_ids` | 0 to 500 |
| ids in a header locator (`get_headers`) | 0 to 32 |
| headers in `headers` | 0 to 500 |
| ids in `get_blocks`, blocks in `blocks` | 0 to 32 |
| ids in `not_found`, `new_tx`, `get_txs`; transactions in `txs` | 0 to 64 |
| addresses in `addrs` | 0 to 100 |
| frame length (kind + body) | per kind, below |

Caps on `length`: `hello` 125, `ping` and `pong` 9, `get_addrs` 1, `addrs` 2605, `new_block` 73, `get_block_ids` and `get_blocks` 1029,
`not_found`, `new_tx` and `get_txs` 2053, `get_headers` 1029, `headers` 73013, `block_ids` 16013, and **`blocks` and `txs` 16,777,216 (16 MiB)**. The
engine's own limits (`Limits`, configurable) may be lower than these; the wire caps are the ceiling a decoder
never exceeds. What a count of zero *means* (an empty locator is a protocol violation, for example) is the
engine's rule, not the codec's.

## 3. The messages

| kind | name | body |
|---|---|---|
| 1 | `hello` | `version` u32, `chain_id` 32, `tip_height` u64, `cumulative_work` 32 (big-endian number), `tip_id` 32, `pruned_below` u64, `nonce` u64 (random per run; equal nonces mean the same node) |
| 2 | `ping` | `nonce` u64 |
| 3 | `pong` | `nonce` u64 |
| 4 | `get_block_ids` | `count` u32, then `count` block ids (32 each): the locator, newest first |
| 5 | `block_ids` | `first_height` u64, `count` u32, then `count` ids, oldest first |
| 6 | `get_blocks` | `count` u32, then ids |
| 7 | `blocks` | `count` u32, then `count` blocks, each in the block wire form of `CONSENSUS_V2.md` 4 |
| 8 | `not_found` | `count` u32, then ids |
| 9 | `new_block` | `id` 32, `height` u64, `cumulative_work` 32 |
| 10 | `new_tx` | `count` u32, then transaction ids |
| 11 | `get_txs` | `count` u32, then transaction ids |
| 12 | `txs` | `count` u32, then `count` transactions, each in the full wire form of `CONSENSUS_V2.md` 6.2 |
| 13 | `get_addrs` | (empty) |
| 14 | `addrs` | `count` u32, then `count` addresses: `ip` 16 (IPv6; IPv4 as `::ffff:a.b.c.d`), `port` u16, `last_seen` u64 (Unix seconds) |
| 15 | `get_headers` | `count` u32, then `count` block ids (32 each): the locator, newest first (the same as `get_block_ids`) |
| 16 | `headers` | `first_height` u64, `count` u32, then `count` block headers, oldest first, each 146 bytes: `version` u16, `prev_id` 32, `timestamp` u64, `tx_root` 32, `nonce` u64, `mix` 64 (the header form of `CONSENSUS_V2.md` 4) |

A header does not carry its own id: the receiver computes it from the header. A pruned node keeps every header, so
it can serve `headers` for its whole chain even where it can no longer serve `blocks`.

Every other kind byte (0 and 17 to 255) is an error. The objects inside `blocks` and `txs` are decoded by the
strict decoders of the data model, so a block or transaction that is malformed *inside* a well-formed frame is
refused with that decoder's error.

## 4. Errors, in the order they are checked

For a whole frame (`decode_frame`):

1. fewer than 4 bytes: `short frame`
2. `length` is 0: `empty frame`
3. fewer than 5 bytes: `short frame`
4. an unknown kind: `unknown kind`
5. `length` over the cap of the kind: `frame too large` (this needs only the first 5 bytes)
6. fewer than `4 + length` bytes: `short frame`; more: `trailing bytes`
7. the body: a count out of range: `count out of range`; a read past the end: `short read`; a block or
   transaction that does not decode: that decoder's error (`short read`, `trailing bytes`,
   `count out of range`, `length over maximum`); bytes left over: `trailing bytes`

A **stream** decoder (bytes arriving in any chunks) applies steps 1 to 5 as soon as the bytes exist, and then waits
for the rest of the frame; steps 2, 4 and 5 therefore fail after at most 5 bytes. After any error the connection
must be closed: the stream cannot be resynchronised.

## 5. What this does not do

No compression, no versioning of individual messages (a new message is a new kind and a new protocol version in
`hello`), no message authentication of its own, and no defence against a peer that sends many small valid
messages: that is the engine's rate limit and score (`docs/M8_PLAN.md`, M8.1).
