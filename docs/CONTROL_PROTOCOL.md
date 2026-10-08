# The control protocol (a DRAFT, milestone M8.7)

How a wallet, or any program on the same computer, talks to a running node. It is **not** the peer-to-peer protocol
(`docs/WIRE_PROTOCOL.md`): it is for one machine, it is not encrypted, and it has its own few messages. Decision 4 of
`docs/M8_PLAN.md` asked for "the same protocol with a few extra request messages"; what was built is the same
*style* (length-prefixed frames, strict decoding, golden vectors from an independent Python reference) with its own
message set, because the peer-to-peer messages are about sharing blocks with strangers and the wallet needs to ask
questions. **Experimental and unaudited.** The code is `crates/tenero-app/src/{control,server,client}.rs`; the vectors
are `tests/vectors/control.json`, made by `reference/tools/make_vectors_control.py`, an independent implementation, and checked
by `crates/tenero-app/tests/control_vectors.rs` and `reference/tests/test_vectors_control.py`.

**Version 3 (0.3.0, the `gamma` network: FCMP++ and Carrot).** A spend no longer picks decoys: it proves membership in the
curve tree. So the requests for ring members' outputs (kinds 4, 5 and 15) are **retired** (unknown kinds now), and
`spend_paths` (20) gives the tree paths a spend is proven with. `rules` lost the ring size, the maturities (consensus
constants now) and the input limit, and gained the tree's layers; `block_template` takes the keys of a main address
(a Carrot coinbase output depends on its amount, which only the node knows); a template carries the anchor its output
was made with; a block summary gives its weight instead of its size; a pool entry gives its weight too. The objects inside
are version 3's (`docs/CONSENSUS_V2.md` 15).

## Who may connect, and how

* **Only this computer.** The node listens on a loopback address (default `127.0.0.1:38352` on `gamma`,
  `127.0.0.1:18332` on the test network, `127.0.0.1:28332` on the dev network). Any other address is refused when the node starts (and by the `control`
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

**A miner on another computer does not use this port.** It uses the node's separate miner service (`docs/REMOTE_MINING_PLAN.md`, `crates/tenero-app/src/miner_service.rs`), which answers only `info` (trimmed), `block_template` and `submit_block`, with the same encodings as here, over an encrypted channel and behind a pre-shared key. Everything on this page about loopback and the cookie is unchanged.

**What this does not give:** it is not encrypted (the bytes stay on the machine); it cannot tell two programs of the same
user apart; and anything holding the cookie may read the whole chain, send transactions and ask the node to stop.

## Frames

```
frame  = length u32 little-endian | body            1 <= length <= 16,777,216 (16 MiB)
body   = kind u8 | payload
```

Integers are little-endian and fixed-width. A *count* is a `u32`; a *var* (bytes or text) is a `u32` length and then the
bytes; a *flag* is one byte, 0 or 1 (nothing else); text is UTF-8. The transaction, transaction prefix, header and coinbase
encodings are version 3's, `docs/CONSENSUS_V2.md` section 15 (`tenero-core`'s `v3` module).

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
| 4, 5 | (`output`, `output_count`: version 2's ring members; **retired** in 0.3.0, unknown kinds now) | | |
| 6 | `key_image_spent` | key image (32) | `spent`: flag |
| 7 | `rules` | | `rules`: chain id (32), next height u64, reward u64, median u64 (the next block's reward and the weight median it is judged by: the inputs of the minimum fee), **tree layers u8** (0 to 32: the layers of the curve tree at the tip, the size of a membership proof made now). **Changed in 0.3.0**: the ring size, the maturities (consensus constants, 60 and 10 blocks) and the input limit are gone |
| 8 | `submit_tx` | a whole transaction | `tx_accepted`: id (32), or an error saying why not |
| 9 | `info` | | `info`: height u64, tip id (32), peers u32, inbound u32, pruned-below u64, mempool transactions u32, syncing flag, node kind u8 (0 archive, 1 pruned), network (text, at most 64 bytes), version (text, at most 64 bytes) |
| 10 | `stop` | | `stopping`: asks the node to shut down cleanly |
| 11 | `blocks` | from u64, count u16 (1 to 64) | `blocks`: count u32 (0 to 64), then that many scan blocks |
| 12 | `block_template` | the keys of the main address the reward goes to (spend key 32, view key 32) and the most transaction weight wanted u64 | `template`: height u64, target (32, big-endian), the **anchor** the coinbase output was made with (16), then a whole block with its nonce and mix empty; **an error if the node is syncing, or if the keys make no output** (not points). **Changed in 0.3.0**: it took a finished output |
| 13 | `submit_block` | a whole block | `block_submitted`: id (32) and a flag, 1 if the block is in the node's chain, 0 if it is valid but on a side branch (it lost a race); **an error if the node refuses it** |
| 14 | `key_images_spent` | a count u32 (1 to 4096) and that many key images (32 each) | `spent_many`: a count u32 and one flag for each key image, in order, 1 if the key image is in the chain. **Added 2026-10-07:** a wallet that has mined thousands of blocks owns thousands of coins, and asking `key_image_spent` about each in turn (about 15 ms a round trip, measured) took twenty seconds every time the tip moved. A client with more than 4096 splits the list |
| 15 | (`outputs`: version 2's ring members, many at once; **retired** in 0.3.0) | | |
| 16 | `check_pow` | height u64, then a block header (146 bytes) | `pow_checked`: a flag, 1 when the header's mix is what the proof of work gives for its nonce (`PowCheck::check_full`, with the dataset the node already holds). **Added 2026-10-07** for the mining pool: a pool checks every share, and a dataset is 4 GiB, so the pool asks its own node instead of holding one. It says nothing about any target: the asker compares the id itself. Costs the node one proof-of-work attempt each time, so it is on the control interface only, **never in the miner service's allowlist** |
| 17 | `headers` | from u64, count u16 (1 to 64) | `headers`: count u32 (0 to 64), then that many *block summaries*. **Added 2026-10-07** for the block explorer (`tenero-explorer`) |
| 18 | `mempool` | | `mempool`: total u32 (the transactions the pool holds), count u32 (0 to 4096, and never more than total), then that many *pool entries*, best fee rate first. **Added 2026-10-07** for the block explorer |
| 19 | `chain_stats` | | `chain_stats`: tip height u64, the next block's target (32, big-endian), the tip's cumulative work (32, big-endian), the next block's reward u64, emitted u64, max supply u64, tail reward u64, block time u64 (seconds). **Added 2026-10-07** for the block explorer |
| 20 | `spend_paths` | a count u32 (1 to 512) and that many global indexes (u64) | `spend_paths`: the reference height u64 (the tip), its *tree* (leaves u64, layers u8, root 32), a count u32, then for each index a flag and (if 1) a *path*, in order (0 for an output not in that tree). **Added in 0.3.0**: what a wallet proves a spend with. Every path is in the tree of that ONE block; a client with more than 512 splits the list and asks again if the pieces came from different tips. **The node learns which outputs are about to be spent**: harmless for a node on the wallet's own machine, which is the only kind this interface reaches (a remote wallet needs another way, `docs/FCMP_CARROT_PLAN.md` 7) |

* A **scan block** is what a wallet needs: height u64, block id (32), the global index of its first output u64, the
  coinbase (version u16, height u64, a count of coinbase outputs from **0** to 16 and the outputs, extra as a var), and a
  count of transaction **prefixes** (0 to 8192) and the prefixes. It carries no rings and no proofs, so a pruned node can
  serve it. **The genesis block has no coinbase outputs** (the consensus coinbase encoding requires at least one), which
  is why a scan block writes the coinbase itself.
* A **block summary** (132 bytes): height u64, block id (32), the header's timestamp u64, the target it met (32, big-endian; all
  zeros for the genesis block), the chain's cumulative work up to and including it (32, big-endian), its **weight** u64 (its
  transactions' weight, what the block limit counts, `CONSENSUS_V2.md` 15.4; 0 for a block with none; it was the size in
  bytes before 0.3.0, which the index no longer keeps), its transaction count u32 (besides the coinbase) and what its coinbase paid u64 (its outputs added up: the reward and the fees,
  less any penalty). The node reads the index record and the coinbase only, so a pruned node answers it too and it costs
  no transaction reads.
* A **pool entry** (64 bytes): transaction id (32), when this node received it u64 (Unix seconds by the node's own clock; 0 if not
  known), its fee u64, its size u64 in bytes and its weight u64 (**added in 0.3.0**). Nothing a wallet's privacy rests on: no
  keys or amounts.
* `chain_stats`'s **emitted** is the schedule's total, the base rewards of blocks 1 to the tip added up
  (`Emission::paid_through`), not what coinbases paid: an oversize penalty, which creates fewer coins, is not subtracted. It
  passes **max supply** in the tail, which goes on for ever. The **network hash rate is not in any answer**: nobody can
  measure it. The explorer estimates it from the summaries (the work of the last blocks over the time their timestamps say
  they took) and says that it is an estimate.
* `headers`, `mempool` and `chain_stats` only read, and like everything on this page they are **not in the miner
  service's allowlist**.
* A **path** (`tenero_tree::PathBytes`): the output's position u64 in the tree, its leaf chunk (a count u32, 1 to 38, and that
  many outputs, each its one-time address and amount commitment, 32 + 32), and a chunk for each layer above (a count u32, 0 to
  32, and for each a count u32, 1 to 38, and that many points of 32 bytes). The codec does not look inside: the wallet turns it
  into a prover's path and refuses one that does not decode (`tenero_tree::path_from_bytes`).
* `blocks` is the way to scan a chain: up to 64 blocks from a height, in order; fewer at the tip, and **fewer if they would
  not fit in 8 MiB** (the node always sends at least one, so a client always makes progress and asks again from where it
  stopped). One block after another is what a wallet asks for; a client that gets a block other than the one it asked for
  must treat it as an error.
* `submit_tx`: the node first checks the transaction against the tip as a mempool would (and says why not if it fails,
  or that it already has it); if it passes it is handed to the node's engine, which keeps it and tells the peers, and the
  answer comes at the next look, once the pool has been seen to keep it. **"Accepted" means in this node's pool, not in a
  block.**

* `block_template` is how a miner in another process gets work (`tenero-miner`, `docs/RUNNING.md`). The node builds the
  block exactly as its own miner would (the pool's best transactions within the weight asked for, never more than **3 MiB of
  weight**, a quarter of the largest block, so that a template, whose bytes are at most four times its weight, and the mined
  block always fit one 16 MiB frame; the block limit stays below that until the chain's median passes 1.5 MiB), and a
  coinbase paying the whole reward to the address whose keys were given. **A Carrot coinbase output depends on its
  amount**, which only the node knows once it has chosen the transactions, so the node makes the output itself, with a fresh
  Janus anchor (randomness) each time, and hands the anchor back: the miner makes the same output from its address, the
  height, the amount and the anchor (`tenero_wallet::coinbase_payout_to_keys`), and mines nothing that does not pay it
  (`remote_miner::check_template`). The node therefore knows which of its templates' outputs are this address's, as the
  node a miner mines through always did. The output also binds the HEIGHT, so a template for another height (the tip moved
  between the miner's two questions) is discarded. A node that is syncing has no tip worth building on and answers with an
  error.
* `submit_block`: the block is handed to the node's engine as a local block, which **validates it completely, proof of work
  and every transaction proof included**, and tells the peers; the answer comes at the next look. The miner reports what the
  node said and trusts nothing it found itself.

## What the answers do not say

The node answers what it is asked about **its own chain**. A client that wants to know it is on the best chain asks
`info` (is the node `syncing`?), and nothing stops a node from lying to a wallet that trusts it: the wallet trusts the
node it is pointed at. That is a property of this design, not something the protocol checks.

## Vectors

`tests/vectors/control.json`: 71 valid messages (both directions), 212 malformed bodies with the error class a decoder
must give (the retired kinds 4, 5 and 15 among them, as unknown kinds) (`length`, `kind`, `trailing`, `malformed`), and the frame length rule. Every valid message encodes to exactly
the reference's bytes in Rust and decodes back. The reference also checks, over every valid message and every
single-bit change of it, that the result is refused or decodes to a message that encodes back to the same bytes.
