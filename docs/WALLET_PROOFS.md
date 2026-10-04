# Message signatures and payment proofs (M10.4; the wallet's INTERIM scheme)

**Unaudited. Home-made in the sense of CLAUDE.md rule 3**: a Schnorr signature and a Chaum-Pedersen proof, both standard, composed
from `curve25519-dalek` and SHA-256 with the owner's approval (2026-10-03), because the interim output scheme is already ours. An
independent Python implementation (`reference/tools/make_vectors_proofs.py`, its own Ed25519 arithmetic) makes the vectors
(`tests/vectors/wallet_proofs.json`); the Rust code (`crates/tenero-wallet/src/proofs.rs`) matches them bit for bit. **Nothing
here is a legal or financial proof.** It all goes when Carrot replaces the interim scheme (the format will change).

Notation: `G` the Ed25519 base point, `L` the group order, points are 32-byte compressed encodings and must be canonical, of
prime order and not the identity ("strict"), scalars are 32 bytes little-endian and must be below `L` ("canonical").
`Hs(tag, parts...)` is the interim scheme's hash to a scalar (two SHA-256 blocks of `"tenero interim v1" || tag || counter ||
parts`, reduced mod `L`); `digest(tag, parts...)` is one SHA-256 of `"tenero interim v1" || tag || parts`.

## What they do, and what they do not

* **A signature** shows that whoever holds the spend key of an address signed these bytes. Nothing about when or where.
* **A payment proof** shows that one OUTPUT of the chain is addressed to an address and holds an amount. The output is named by
  its **block height and global output index** (the wallet's view of a block carries no transaction ids, only outputs and key
  images), and a checker reads the output from a node. A proof that checks is about an output that IS in the node's chain.
* **They do not show:** who sent a payment (the interim scheme has no sender identity), that it is final (only how many blocks lie
  on top of it), or anything about the transaction's other outputs. The interim scheme has no Janus protection, so a proof says the
  output is addressed to the address, not that the sender meant it for that wallet's owner.
* **A proof reveals** the amount and the link between the output and the address to whoever holds it. The app warns before making
  one, and never puts one on the clipboard without a click.
* **A payment's secret `r`** (the sender's half of the key exchange, one per output) is kept in the wallet file for payments sent
  by a wallet that has this feature, and shown only on a click. **A payment sent before that, or whose secret has been deleted, can
  never be proved by its sender**; a receiver can always prove receipt (it needs only the view key). Losing the wallet loses the
  secrets: that is intended.

## Message signature (`tnsig1` + 128 hex digits)

```text
h   = digest("message", spend_pub || view_pub || len(message) as u64 LE || message)
k   = Hs("sig nonce", s_bytes || h || rnd)          s = the spend scalar, rnd = 32 fresh random bytes
R   = k*G                       c = Hs("sig challenge", R || spend_pub || h)        z = k + c*s mod L
signature = R (32) || z (32)
verify: spend_pub and view_pub strict, R strict, z canonical, and  z*G == R + c*spend_pub
```

## Payment proof (`tnpay1` + hex of the bytes below)

```text
kind u8 | height u64 LE | global_index u64 LE | spend_pub 32 | view_pub 32 | body
body for kind 1 (received) and 2 (sent): D 32 | c 32 | z 32          (96 bytes)
body for kind 3 (key):                   r 32                         (32 bytes)
```

The output's fields (`onetime_address`, `ephemeral_pubkey` `De`, the commitment, the encrypted amount) and its context and number
`i` in the transaction come from the chain. The shared secret is `S = compress(8*D)` where `D` is the Diffie-Hellman point:

* **kind 1, received (made with the view key `v`):** `D = v*De`. Proves `K_view = v*G` and `D = v*De` with the same `v`.
* **kind 2, sent (made with the output's secret `r`):** `D = r*K_view`. Proves `De = r*G` and `D = r*K_view` with the same `r`.
* **kind 3, key:** the body is `r`; the checker needs `r*G == De`, and `D = r*K_view`. (It reveals this output's `r`, which lets anyone
  decode this output and nothing else.)

The proof of kinds 1 and 2 (Chaum-Pedersen, Fiat-Shamir): with `P1 = x*G`, `P2 = x*B2` (kind 1: `x=v, P1=K_view, B2=De, P2=D`; kind 2:
`x=r, P1=De, B2=K_view, P2=D`) and `bind = kind || height || global_index || spend_pub || view_pub || De`:

```text
k  = Hs("pay nonce", x_bytes || bind || rnd)           rnd = 32 fresh random bytes
A1 = k*G     A2 = k*B2      c = Hs("pay challenge", bind || D || A1 || A2)      z = k + c*x mod L
verify: D strict, c and z canonical;  A1' = z*G - c*P1,  A2' = z*B2 - c*D,  and  c == Hs("pay challenge", bind || D || A1' || A2')
```

Then what `S` says about the output is recomputed, as the receiver's scan does:
`t = Hs("onetime", S || ctx || i)`; the output's one-time address must equal `t*G + spend_pub` (this is what binds the address). A block
reward's amount is public. Any other output's amount is `amount_enc XOR digest("amount", S || ctx || i)[..8]`, and the commitment
`mask*G + amount*H` with `mask = Hs("mask", S || ctx || i)` must equal the output's commitment.

## Checking a bare transaction key

A key alone does not say which output it made. `proofs::check_key(chain, key, address, from_height)` computes `De = r*G`, reads blocks from
`from_height` on (at most `MAX_KEY_SEARCH_BLOCKS` = 50,000) until an output with that ephemeral key is found, and checks it as a key proof
(kind 3). It says "not found" with the blocks it read if there is none (the key is wrong, or the start is after the payment's block), and
"not addressed" if the output was not paid to the address given.

## Where it is used

`tenero-wallet`: `proofs.rs` (the functions), `Purse::sign_message`, `prove_received`, `prove_sent`, `tx_secret`,
`forget_tx_secret`; the wallet file version 3 (`purse.rs`) stores each sent payment's secret and its output's one-time address.
`tenero-gui`: see `docs/RUNNING.md`.
