//! Message signatures and payment proofs, like Monero's `sign`/`verify` and `get_tx_proof`/`check_tx_proof`, for the INTERIM
//! output scheme. The byte-level definition is in `docs/WALLET_PROOFS.md`, and `reference/tools/make_vectors_proofs.py` is an independent
//! implementation that the vectors (`tests/vectors/wallet_proofs.json`) come from.
//!
//! **Unaudited, and home-made in the sense of CLAUDE.md rule 3**: a Schnorr signature and a Chaum-Pedersen proof (two points
//! have the same discrete logarithm), both standard, composed here from `curve25519-dalek` and SHA-256 with the owner's
//! approval (2026-10-03). They are not a legal or financial proof of anything.
//!
//! * **A message signature** shows that whoever holds an address's spend key signed these bytes. It is bound to the whole address
//!   and to a label, so it cannot be taken for anything else.
//! * **A payment proof** shows that one OUTPUT of the chain (named by its block height and its global index: the wallet's view of
//!   a block carries no transaction ids) is addressed to an address and holds an amount. Three kinds:
//!   * *received*: made by the receiver with the view key, without giving the view key away;
//!   * *sent*: made by the sender with the output's secret `r` (kept in the wallet file from the first payment made by a
//!     wallet that has this feature; **a payment sent before that cannot be proved by its sender**);
//!   * *key*: the secret `r` itself, which anyone can check (it reveals only this output).
//!
//! What they do NOT show: who sent a payment, that it is final, or anything about the transaction's other outputs. The interim
//! scheme has no Janus protection: a proof says the output is addressed to the address, not that the sender meant it for that
//! wallet's owner. A proof reveals the amount and the link to the address to whoever holds it.

use curve25519_dalek::constants::ED25519_BASEPOINT_POINT as G;
use curve25519_dalek::scalar::Scalar;
use rand_core::{CryptoRng, RngCore};
use tenero_core::hash::hex_lower;
use tenero_crypto::ringct;
use zeroize::Zeroize;

use crate::chain::ChainView;
use crate::interim::{
    coinbase_context, compress, digest, hs, shared_bytes, strict_point, tx_context, xor, Address,
    TxSecret, ViewKeys,
};

pub const SIGNATURE_PREFIX: &str = "tnsig1";
pub const PROOF_PREFIX: &str = "tnpay1";

/// Longest proof text accepted (a real one is under 400 characters).
const MAX_PROOF_TEXT: usize = 1000;

#[derive(Debug, PartialEq, Eq)]
pub enum ProofError {
    /// Not a signature or proof at all (the reason).
    Format(&'static str),
    /// The address holds an invalid key.
    BadAddress,
    /// The signature does not verify for this address and message.
    BadSignature,
    /// The proof does not verify (it was not made by someone who knows the secret, or it is about another output or address).
    BadProof,
    /// The output is not addressed to the address in the proof.
    NotAddressed,
    /// The output's amount does not match what the shared secret decrypts to.
    AmountMismatch,
    /// The chain has no such output.
    NoSuchOutput,
    /// This payment's secret was not kept (it was sent before the wallet kept them).
    NoTxSecret,
    /// The payment is not in the chain yet.
    NotInChain,
    /// No output made with this key was found in the blocks searched (they are named).
    NotFound { from: u64, to: u64 },
    /// The node said no.
    Chain(String),
}

impl std::fmt::Display for ProofError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProofError::Format(why) => write!(f, "not a valid signature or proof: {why}"),
            ProofError::BadAddress => write!(f, "the address holds an invalid key"),
            ProofError::BadSignature => write!(f, "the signature is NOT valid for this address and message"),
            ProofError::BadProof => write!(f, "the proof is NOT valid"),
            ProofError::NotAddressed => write!(f, "the output is not addressed to that address"),
            ProofError::AmountMismatch => write!(f, "the amount does not match the output"),
            ProofError::NoSuchOutput => write!(f, "the chain has no such output"),
            ProofError::NoTxSecret => write!(
                f,
                "the secret of this payment was not kept (it was sent before the wallet kept them), so the sender cannot prove it"
            ),
            ProofError::NotFound { from, to } => write!(
                f,
                "no output made with that key was found in blocks {from} to {to}: check the key, or search from an earlier block (an output can only be in a block after the payment was sent)"
            ),
            ProofError::NotInChain => write!(f, "the payment is not in the chain yet: wait until a block takes it in"),
            ProofError::Chain(e) => write!(f, "the node said: {e}"),
        }
    }
}

impl std::error::Error for ProofError {}

fn canonical(b: &[u8; 32]) -> Option<Scalar> {
    Option::<Scalar>::from(Scalar::from_canonical_bytes(*b))
}

fn parse_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != 2 * N || !text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let mut out = [0u8; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

// ------------------------------------------------------------------------------------------------
// messages
// ------------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageSignature(pub [u8; 64]);

impl MessageSignature {
    /// `tnsig1` and 128 lower-case hexadecimal digits.
    pub fn to_text(&self) -> String {
        format!("{SIGNATURE_PREFIX}{}", hex_lower(&self.0))
    }

    pub fn from_text(text: &str) -> Result<MessageSignature, ProofError> {
        let hex = text
            .trim()
            .strip_prefix(SIGNATURE_PREFIX)
            .ok_or(ProofError::Format("a signature starts with tnsig1"))?;
        parse_hex::<64>(hex)
            .map(MessageSignature)
            .ok_or(ProofError::Format(
                "a signature is tnsig1 and 128 hexadecimal digits",
            ))
    }
}

fn message_hash(address: &Address, message: &[u8]) -> [u8; 32] {
    digest(
        b"message",
        &[
            &address.spend,
            &address.view,
            &(message.len() as u64).to_le_bytes(),
            message,
        ],
    )
}

/// Signs `message` with the spend key of `keys`.
pub fn sign_message(
    keys: &crate::interim::Keys,
    rng: &mut (impl RngCore + CryptoRng),
    message: &[u8],
) -> MessageSignature {
    let address = keys.address();
    let h = message_hash(&address, message);
    let mut rnd = [0u8; 32];
    rng.fill_bytes(&mut rnd);
    let s = *keys.spend_scalar();
    let mut sb = s.to_bytes();
    let k = hs(b"sig nonce", &[&sb, &h, &rnd]);
    sb.zeroize();
    rnd.zeroize();
    let r_pub = compress(&(G * k));
    let c = hs(b"sig challenge", &[&r_pub, &address.spend, &h]);
    let z = k + c * s;
    let mut out = [0u8; 64];
    out[..32].copy_from_slice(&r_pub);
    out[32..].copy_from_slice(&z.to_bytes());
    MessageSignature(out)
}

/// Does `signature` show that the holder of `address`'s spend key signed `message`?
pub fn verify_message(
    address: &Address,
    message: &[u8],
    signature: &MessageSignature,
) -> Result<(), ProofError> {
    let k_spend = strict_point(&address.spend).ok_or(ProofError::BadAddress)?;
    strict_point(&address.view).ok_or(ProofError::BadAddress)?;
    let mut rb = [0u8; 32];
    rb.copy_from_slice(&signature.0[..32]);
    let mut zb = [0u8; 32];
    zb.copy_from_slice(&signature.0[32..]);
    let r_pt = strict_point(&rb).ok_or(ProofError::BadSignature)?;
    let z = canonical(&zb).ok_or(ProofError::BadSignature)?;
    let h = message_hash(address, message);
    let c = hs(b"sig challenge", &[&rb, &address.spend, &h]);
    if G * z == r_pt + k_spend * c {
        Ok(())
    } else {
        Err(ProofError::BadSignature)
    }
}

// ------------------------------------------------------------------------------------------------
// payment proofs
// ------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProofKind {
    Received = 1,
    Sent = 2,
    Key = 3,
}

impl ProofKind {
    pub fn name(self) -> &'static str {
        match self {
            ProofKind::Received => "received",
            ProofKind::Sent => "sent",
            ProofKind::Key => "key",
        }
    }
}

/// The fields of one output that a proof is checked against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputFields {
    pub onetime_address: [u8; 32],
    pub ephemeral_pubkey: [u8; 32],
    pub amount_commitment: [u8; 32],
    pub amount_enc: [u8; 8],
    /// The transaction's (or coinbase's) context, and the output's number in it.
    pub ctx: [u8; 32],
    pub index: u32,
    /// `Some` for a block reward, whose amount is public.
    pub public_amount: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentProof {
    pub kind: ProofKind,
    pub height: u64,
    pub global_index: u64,
    pub address: Address,
    body: Vec<u8>,
}

/// What a valid proof shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checked {
    pub kind: ProofKind,
    pub address: Address,
    pub amount: u64,
    pub height: u64,
    pub global_index: u64,
    /// A block reward (its amount is public to everyone).
    pub block_reward: bool,
}

fn binding(kind: ProofKind, height: u64, gi: u64, address: &Address, de: &[u8; 32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(1 + 8 + 8 + 96);
    b.push(kind as u8);
    b.extend_from_slice(&height.to_le_bytes());
    b.extend_from_slice(&gi.to_le_bytes());
    b.extend_from_slice(&address.spend);
    b.extend_from_slice(&address.view);
    b.extend_from_slice(de);
    b
}

impl PaymentProof {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(81 + self.body.len());
        v.push(self.kind as u8);
        v.extend_from_slice(&self.height.to_le_bytes());
        v.extend_from_slice(&self.global_index.to_le_bytes());
        v.extend_from_slice(&self.address.spend);
        v.extend_from_slice(&self.address.view);
        v.extend_from_slice(&self.body);
        v
    }

    pub fn from_bytes(b: &[u8]) -> Result<PaymentProof, ProofError> {
        if b.len() < 81 {
            return Err(ProofError::Format("too short"));
        }
        let kind = match b[0] {
            1 => ProofKind::Received,
            2 => ProofKind::Sent,
            3 => ProofKind::Key,
            _ => return Err(ProofError::Format("unknown kind")),
        };
        let body = &b[81..];
        let want = if kind == ProofKind::Key { 32 } else { 96 };
        if body.len() != want {
            return Err(ProofError::Format("wrong length"));
        }
        let mut spend = [0u8; 32];
        let mut view = [0u8; 32];
        spend.copy_from_slice(&b[17..49]);
        view.copy_from_slice(&b[49..81]);
        Ok(PaymentProof {
            kind,
            height: u64::from_le_bytes(b[1..9].try_into().expect("8 bytes")),
            global_index: u64::from_le_bytes(b[9..17].try_into().expect("8 bytes")),
            address: Address { spend, view },
            body: body.to_vec(),
        })
    }

    /// `tnpay1` and the proof in hexadecimal.
    pub fn to_text(&self) -> String {
        format!("{PROOF_PREFIX}{}", hex_lower(&self.to_bytes()))
    }

    pub fn from_text(text: &str) -> Result<PaymentProof, ProofError> {
        let t = text.trim();
        if t.len() > MAX_PROOF_TEXT {
            return Err(ProofError::Format("too long"));
        }
        let hex = t
            .strip_prefix(PROOF_PREFIX)
            .ok_or(ProofError::Format("a payment proof starts with tnpay1"))?;
        if hex.len() % 2 != 0 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(ProofError::Format("not lower-case hexadecimal"));
        }
        let bytes: Vec<u8> = (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("checked"))
            .collect();
        PaymentProof::from_bytes(&bytes)
    }
}

/// The proof that `p1 = x*G` and `p2 = x*b2` for a secret `x`: `D = p2`, the challenge and the response.
fn prove_dleq(
    x: &Scalar,
    b2: &curve25519_dalek::edwards::EdwardsPoint,
    p2: &curve25519_dalek::edwards::EdwardsPoint,
    bind: &[u8],
    rng: &mut (impl RngCore + CryptoRng),
) -> Vec<u8> {
    let mut rnd = [0u8; 32];
    rng.fill_bytes(&mut rnd);
    let mut xb = x.to_bytes();
    let k = hs(b"pay nonce", &[&xb, bind, &rnd]);
    xb.zeroize();
    rnd.zeroize();
    let a1 = compress(&(G * k));
    let a2 = compress(&(b2 * k));
    let d = compress(p2);
    let c = hs(b"pay challenge", &[bind, &d, &a1, &a2]);
    let z = k + c * x;
    let mut body = Vec::with_capacity(96);
    body.extend_from_slice(&d);
    body.extend_from_slice(&c.to_bytes());
    body.extend_from_slice(&z.to_bytes());
    body
}

/// Checks a proof against the output it names, and says what it shows.
pub fn check(proof: &PaymentProof, out: &OutputFields) -> Result<Checked, ProofError> {
    let k_spend = strict_point(&proof.address.spend).ok_or(ProofError::BadAddress)?;
    let k_view = strict_point(&proof.address.view).ok_or(ProofError::BadAddress)?;
    let de = strict_point(&out.ephemeral_pubkey).ok_or(ProofError::NoSuchOutput)?;
    let s_bytes = match proof.kind {
        ProofKind::Received | ProofKind::Sent => {
            let take = |from: usize| -> [u8; 32] {
                proof.body[from..from + 32].try_into().expect("32 bytes")
            };
            let (db, cb, zb) = (take(0), take(32), take(64));
            let d = strict_point(&db).ok_or(ProofError::BadProof)?;
            let c = canonical(&cb).ok_or(ProofError::BadProof)?;
            let z = canonical(&zb).ok_or(ProofError::BadProof)?;
            // received: K_view = v*G and D = v*De; sent: De = r*G and D = r*K_view
            let (p1, b2) = if proof.kind == ProofKind::Received {
                (k_view, de)
            } else {
                (de, k_view)
            };
            let bind = binding(
                proof.kind,
                proof.height,
                proof.global_index,
                &proof.address,
                &out.ephemeral_pubkey,
            );
            let a1 = G * z - p1 * c;
            let a2 = b2 * z - d * c;
            let c2 = hs(
                b"pay challenge",
                &[&bind, &db, &compress(&a1), &compress(&a2)],
            );
            if c2 != c {
                return Err(ProofError::BadProof);
            }
            shared_bytes(&d)
        }
        ProofKind::Key => {
            let rb: [u8; 32] = proof.body[..32].try_into().expect("32 bytes");
            let r = canonical(&rb).ok_or(ProofError::BadProof)?;
            if compress(&(G * r)) != out.ephemeral_pubkey {
                return Err(ProofError::BadProof);
            }
            shared_bytes(&(k_view * r))
        }
    };
    // what the shared secret says about the output
    let i = out.index.to_le_bytes();
    let t = hs(b"onetime", &[&s_bytes, &out.ctx, &i]);
    if compress(&(G * t + k_spend)) != out.onetime_address {
        return Err(ProofError::NotAddressed);
    }
    let (amount, block_reward) = match out.public_amount {
        Some(a) => (a, true),
        None => {
            let mask = hs(b"mask", &[&s_bytes, &out.ctx, &i]).to_bytes();
            let amount = u64::from_le_bytes(xor(
                &out.amount_enc,
                &digest(b"amount", &[&s_bytes, &out.ctx, &i]),
            ));
            if ringct::commit(&mask, amount) != Some(out.amount_commitment) {
                return Err(ProofError::AmountMismatch);
            }
            (amount, false)
        }
    };
    Ok(Checked {
        kind: proof.kind,
        address: proof.address,
        amount,
        height: proof.height,
        global_index: proof.global_index,
        block_reward,
    })
}

/// A proof of receipt, made with the view key, for output `global_index` of the block at `height`. Refuses (with
/// [`ProofError::NotAddressed`]) an output that is not this wallet's.
pub fn prove_received(
    view: &ViewKeys,
    rng: &mut (impl RngCore + CryptoRng),
    height: u64,
    global_index: u64,
    out: &OutputFields,
) -> Result<PaymentProof, ProofError> {
    let address = view.address();
    let de = strict_point(&out.ephemeral_pubkey).ok_or(ProofError::NoSuchOutput)?;
    let v = view.view_scalar();
    let bind = binding(
        ProofKind::Received,
        height,
        global_index,
        &address,
        &out.ephemeral_pubkey,
    );
    let body = prove_dleq(v, &de, &(de * v), &bind, rng);
    let proof = PaymentProof {
        kind: ProofKind::Received,
        height,
        global_index,
        address,
        body,
    };
    // never hand out a proof that does not check
    check(&proof, out)?;
    Ok(proof)
}

/// A proof of payment, made by the sender from the output's secret `r`, to `to`.
pub fn prove_sent(
    r: &TxSecret,
    to: &Address,
    rng: &mut (impl RngCore + CryptoRng),
    height: u64,
    global_index: u64,
    out: &OutputFields,
) -> Result<PaymentProof, ProofError> {
    let rs = canonical(r.expose()).ok_or(ProofError::BadProof)?;
    let k_view = strict_point(&to.view).ok_or(ProofError::BadAddress)?;
    let bind = binding(
        ProofKind::Sent,
        height,
        global_index,
        to,
        &out.ephemeral_pubkey,
    );
    let body = prove_dleq(&rs, &k_view, &(k_view * rs), &bind, rng);
    let proof = PaymentProof {
        kind: ProofKind::Sent,
        height,
        global_index,
        address: *to,
        body,
    };
    check(&proof, out)?;
    Ok(proof)
}

/// The secret itself as a proof: anyone can check it, and it shows only this output.
pub fn key_proof(
    r: &TxSecret,
    to: &Address,
    height: u64,
    global_index: u64,
    out: &OutputFields,
) -> Result<PaymentProof, ProofError> {
    let proof = PaymentProof {
        kind: ProofKind::Key,
        height,
        global_index,
        address: *to,
        body: r.expose().to_vec(),
    };
    check(&proof, out)?;
    Ok(proof)
}

// ------------------------------------------------------------------------------------------------
// against the chain
// ------------------------------------------------------------------------------------------------

fn chain_err(e: String) -> ProofError {
    ProofError::Chain(e)
}

/// The output with global index `global_index`, which must be in the block at `height`.
pub fn output_at(
    chain: &impl ChainView,
    height: u64,
    global_index: u64,
) -> Result<OutputFields, ProofError> {
    let block = chain
        .block(height)
        .map_err(chain_err)?
        .ok_or(ProofError::NoSuchOutput)?;
    output_in(&block, global_index).ok_or(ProofError::NoSuchOutput)
}

fn output_in(block: &crate::chain::ScanBlock, global_index: u64) -> Option<OutputFields> {
    let mut index = block.first_output_index;
    for (j, o) in block.coinbase.outputs.iter().enumerate() {
        if index == global_index {
            return Some(OutputFields {
                onetime_address: o.onetime_address,
                ephemeral_pubkey: o.ephemeral_pubkey,
                amount_commitment: ringct::public_amount_commitment(o.amount),
                amount_enc: [0u8; 8],
                ctx: coinbase_context(block.height),
                index: j as u32,
                public_amount: Some(o.amount),
            });
        }
        index += 1;
    }
    for t in &block.txs {
        let ctx = t.inputs.first().map(|i| tx_context(&i.key_image));
        for (j, o) in t.outputs.iter().enumerate() {
            if index == global_index {
                return Some(OutputFields {
                    onetime_address: o.onetime_address,
                    ephemeral_pubkey: o.ephemeral_pubkey,
                    amount_commitment: o.amount_commitment,
                    amount_enc: o.amount_enc,
                    ctx: ctx?,
                    index: j as u32,
                    public_amount: None,
                });
            }
            index += 1;
        }
    }
    None
}

/// Checks a proof against the chain: finds the output it names and says what it shows, and how many blocks are on top of it
/// (1 = it is in the newest block). **A proof that checks is about an output that IS in the node's chain now.**
pub fn check_on_chain(
    chain: &impl ChainView,
    proof: &PaymentProof,
) -> Result<(Checked, u64), ProofError> {
    let out = output_at(chain, proof.height, proof.global_index)?;
    let checked = check(proof, &out)?;
    let (tip, _) = chain.tip().map_err(chain_err)?;
    Ok((checked, tip.saturating_sub(proof.height) + 1))
}

/// Looks for the output with this one-time address in the blocks from `from_height` on (at most the next 5,000): where the
/// sender finds the output a payment became. `None` if it is not there (yet).
pub fn find_output(
    chain: &impl ChainView,
    from_height: u64,
    onetime_address: &[u8; 32],
) -> Result<Option<(u64, u64)>, ProofError> {
    let (tip, _) = chain.tip().map_err(chain_err)?;
    let end = tip.min(from_height.saturating_add(5_000));
    let mut h = from_height;
    while h <= end {
        let batch = chain.blocks(h, 64).map_err(chain_err)?;
        if batch.is_empty() {
            break;
        }
        for block in &batch {
            let n = block.coinbase.outputs.len() as u64
                + block
                    .txs
                    .iter()
                    .map(|t| t.outputs.len() as u64)
                    .sum::<u64>();
            for k in 0..n {
                let gi = block.first_output_index + k;
                if output_in(block, gi).is_some_and(|o| &o.onetime_address == onetime_address) {
                    return Ok(Some((block.height, gi)));
                }
            }
            h = block.height + 1;
        }
    }
    Ok(None)
}

/// The most blocks one search for a key reads (a key does not say where its output is, so the chain is read from a starting height
/// until the output is found).
pub const MAX_KEY_SEARCH_BLOCKS: u64 = 50_000;

/// Checks a transaction key (the secret `r` of one output) and an address against the chain, the way Monero's `check_tx_key` does:
/// the output made with `r` is found by its ephemeral key `r*G`, in the blocks from `from_height` on, and then checked like a key
/// proof. Returns what it shows and how many blocks lie on top of it.
pub fn check_key(
    chain: &impl ChainView,
    key: &[u8; 32],
    address: &Address,
    from_height: u64,
) -> Result<(Checked, u64), ProofError> {
    let r = canonical(key).ok_or(ProofError::Format(
        "a transaction key is 64 hexadecimal digits below the group order",
    ))?;
    strict_point(&address.spend).ok_or(ProofError::BadAddress)?;
    strict_point(&address.view).ok_or(ProofError::BadAddress)?;
    let de = compress(&(G * r));
    let (tip, _) = chain.tip().map_err(chain_err)?;
    let end = tip.min(from_height.saturating_add(MAX_KEY_SEARCH_BLOCKS - 1));
    let mut h = from_height;
    let mut found: Option<(u64, u64)> = None;
    'search: while h <= end {
        let batch = chain.blocks(h, 64).map_err(chain_err)?;
        if batch.is_empty() {
            break;
        }
        for block in &batch {
            if block.height > end {
                break 'search;
            }
            let mut index = block.first_output_index;
            for o in &block.coinbase.outputs {
                if o.ephemeral_pubkey == de {
                    found = Some((block.height, index));
                    break 'search;
                }
                index += 1;
            }
            for t in &block.txs {
                for o in &t.outputs {
                    if o.ephemeral_pubkey == de {
                        found = Some((block.height, index));
                        break 'search;
                    }
                    index += 1;
                }
            }
            h = block.height + 1;
        }
    }
    let (height, gi) = found.ok_or(ProofError::NotFound {
        from: from_height,
        to: end,
    })?;
    let out = output_at(chain, height, gi)?;
    let proof = PaymentProof {
        kind: ProofKind::Key,
        height,
        global_index: gi,
        address: *address,
        body: key.to_vec(),
    };
    let checked = check(&proof, &out)?;
    Ok((checked, tip.saturating_sub(height) + 1))
}
