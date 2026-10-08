# Message signatures and payment proofs (0.3.0, Carrot)

How a Tenero wallet signs a message as one of its addresses, and proves that an output paid an address. The code is
`crates/tenero-wallet/src/proofs.rs`; the independent reference is `reference/tools/make_vectors_proofs.py` (Python, the
standard library only), whose vectors `tests/vectors/wallet_proofs.json` the Rust reproduces byte for byte
(`crates/tenero-wallet/tests/proofs.rs`, `reference/tests/test_vectors_proofs.py`).

**Read this first.** Neither Monero nor the Carrot specification defines message signatures or payment proofs for Carrot
addresses (checked 2026-10-08: Monero's FCMP++ wallet, `seraphis-migration/monero` branch `fcmp++-stage`, still has the
pre-Carrot `wallet2::sign` and `get_tx_proof`; `jeffro256/carrot` has no proofs section). The owner chose to have them in
0.3.0-gamma.1 anyway (decision F16, a second exception to rule 3). So:

* **The signatures are this project's own construction**, from textbook parts (a Schnorr proof of knowledge of a
  representation). **Nobody has reviewed them.**
* **The payment check is not ours**: it is Carrot's sender scan (Monero's `try_scan_carrot_enote_external_sender`,
  transcribed in `tenero-carrot` and checked against Monero's C++ vectors), which makes the whole output again from the
  payment's anchor.
* Nothing here is a legal or financial proof of anything. The 0.2.0 programs' signatures and proofs (on the interim scheme)
  are a different design: they do not carry over.

## Notation

`G` is Ed25519's base point, `T` the FCMP++ generator (an output key is `x G + y T`; the encoding is in the vectors),
`l` the group order. `H_n(domain, fields...)` is Carrot's hash to a scalar: Blake2b-512 personalised `"Monero"` over the
transcript `len(domain) as one byte | domain | the fields as raw bytes`, read little-endian and reduced modulo `l`. Integers
are little-endian; a variable-length field is preceded by its length as a `u64`.

A Carrot address has a spend key `K` and a view key `V`. Its spend key has two secrets: `K = a G + b T`, with `a = s_j k_gi`
and `b = s_j k_ps` (`k_gi` the generate-image key, `k_ps` the prove-spend key, `s_j` the subaddress scalar, one for the main
address). An integrated address has its main address's keys.

## Signatures

To sign the fields `bound` under a domain, as the address `(K, V)` with secrets `a, b`:

```
rnd   = 32 fresh random bytes
r_g   = H_n("Tenero signature nonce v1", "G", a, b, K, V, domain, bound..., rnd)
r_t   = H_n("Tenero signature nonce v1", "T", a, b, K, V, domain, bound..., rnd)
R     = r_g G + r_t T
c     = H_n(domain, K, V, R, bound...)
s_g   = r_g - c a          s_t = r_t - c b          (mod l)
signature = c | s_g | s_t  (96 bytes)
```

To verify: `c`, `s_g` and `s_t` must be canonical scalars; `K` must decode, lie in the prime-order group and not be the
identity; `V` must decode. Then `R' = s_g G + s_t T + c K`, and the signature holds when `H_n(domain, K, V, R', bound...) = c`.
The nonces are derived from the secrets, everything signed and fresh randomness, so a broken random source cannot make two
signatures share a nonce. Both keys of the address are in the challenge, so a signature holds for that address only.

* **A message signature:** the domain `"Tenero message signature v1"`, `bound = len(message) | message`. Text:
  `TENsig1` and the 96 bytes in Monero's block base58 (the alphabet and blocks of the address text).

## Payment proofs

A payment proof says: the output with one-time address `Ko`, in the block at height `h`, pays the address `A`. It carries the
payment's **Janus anchor** (Carrot's 16 bytes of randomness for the output), from which the checker makes the output again.

```
bytes = version (1) | n (u8) | A's text (n bytes) | h (u64) | Ko (32) | anchor (16) | flag (u8) | signature (96, if flag = 1)
text  = "TENpay1" + base58(bytes)
```

Decoding is strict (one proof has one encoding): the version is 1, the address is a valid address of some network, written
exactly in its own text, the flag is 0 or 1, and nothing follows.

**Checking** (against the checker's own node):

1. The node's chain has a block at `h`, and it has an output `Ko`.
2. A block reward: `A` is a main address, and Carrot's coinbase output for `A`, the height, the output's public amount and
   the anchor is exactly the output (one-time address, view tag, ephemeral key, encrypted anchor).
3. Any other output: Carrot's sender scan with `A` and the anchor succeeds: the ephemeral key re-derives from the anchor,
   the transaction's first key image and `A` (the Janus check), the shared secret opens the output to `A`'s spend key and
   its amount commitment, and for an integrated address the payment ID is `A`'s. The amount is what the scan opened.
4. A signature, if there is one, holds for `A` under the domain `"Tenero payment proof signature v1"` with
   `bound = h | Ko | anchor | len(message) | message`.

What a proof shows, and what it does not:

* **A payment proof** (no signature): this output, in this block, pays `A` that amount. The sender makes it from the anchor
  it kept; **the receiver can make the same one** (it decrypts the anchor from the output), so it does not say who sent the
  payment.
* **A received proof** (with a signature): the same, and the holder of `A` signed this output and a message: the prover
  holds the receiving address. The wallet app signs an empty message.
* **The anchor is a secret of one payment.** Whoever holds a proof can see that one output's amount and that it paid `A`,
  as with Monero's "tx key". It cannot spend anything. The anchor alone, with the address, is a **payment key**: the checker
  looks for the output it makes in the blocks from a height on (at most 20,000 blocks).
* A proof of the wallet's own change is refused: there is nothing to prove.
* **Spend proofs and reserve proofs** (proving that an output was spent, or that a wallet holds an amount) have no published
  FCMP++ design, and are not built.

## Where the wallet keeps what it needs

A sent payment keeps its anchor and its output's one-time address in the wallet file (`SentRecord`; the app can forget it).
A received output's anchor is not stored: it is decrypted again from the chain with the view key when a proof is asked for.
