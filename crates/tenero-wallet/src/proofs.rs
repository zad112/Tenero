//! Message signatures and payment proofs on Carrot (`docs/WALLET_PROOFS.md`).
//!
//! **The signatures are our own construction, and nobody has reviewed them** (the owner's second exception to rule 3,
//! 2026-10-08: neither Monero's FCMP++ wallet nor the Carrot specification defines signatures or payment proofs for Carrot
//! addresses). They are built from textbook parts only: a Schnorr proof of knowledge of the two secrets of an address's
//! spend key `K = a G + b T`, made non-interactive with Carrot's own hash. The payment CHECK is not ours: it is Carrot's
//! sender scan ([`tenero_carrot::scan::scan_external_as_sender`], Monero's `try_scan_carrot_enote_external_sender`), which
//! re-derives the whole output from the payment's Janus anchor.
//!
//! * A [`Signature`] of a message by an address: whoever holds the wallet of that address signed exactly this message.
//! * A [`PaymentProof`]: an output in a block pays an address an amount. It carries the payment's anchor, so whoever holds
//!   the proof can check that one output (the anchor of one payment: like Monero's "tx key"). The sender kept the anchor;
//!   the receiver decrypts it from the output. A proof with a signature ([`PaymentProof::signature`]) is a RECEIVED proof:
//!   the receiving address also signed the output and a message, which shows the prover holds that address.
//!
//! Nothing here is a legal or financial proof of anything.

use curve25519_dalek::edwards::EdwardsPoint;
use curve25519_dalek::scalar::Scalar;
use rand_core::{CryptoRng, RngCore};
use tenero_carrot::points::{compress, decompress, in_main_subgroup, T};
use tenero_carrot::scan::{
    anchor_coinbase, anchor_external, scan_external_as_sender, shared_secret,
};
use tenero_carrot::{CoinbaseEnote, Enote, JanusAnchor};
use zeroize::Zeroizing;

use crate::address::{b58decode, b58encode, Address, Kind};
use crate::chain::{ChainView, ScanBlock};

/// The text form of a [`Signature`]: this, then the 96 bytes in Monero's block base58.
pub const SIGNATURE_PREFIX: &str = "TENsig1";
/// The text form of a [`PaymentProof`]: this, then its bytes in Monero's block base58.
pub const PROOF_PREFIX: &str = "TENpay1";
/// The format version of a payment proof's bytes.
pub const PROOF_VERSION: u8 = 1;
/// The longest message a payment proof's signature may bind (a message signature has no limit).
pub const MAX_PROOF_MESSAGE: usize = 4096;
/// How far a check of an anchor alone ([`check_anchor`]) searches, in blocks.
pub const MAX_ANCHOR_SEARCH: u64 = 20_000;

const MESSAGE_DOMAIN: &str = "Tenero message signature v1";
const PAYMENT_DOMAIN: &str = "Tenero payment proof signature v1";
const NONCE_DOMAIN: &str = "Tenero signature nonce v1";

/// Why a signature or a proof was refused, or could not be made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProofError {
    /// Not the text or bytes of one (with what is wrong).
    Format(&'static str),
    /// The signature does not verify for this address and message.
    BadSignature,
    /// The block at the proof's height is not in the node's chain.
    NoBlock(u64),
    /// No output with the proof's one-time address in that block.
    NoOutput,
    /// The output is not the one the anchor makes for that address: it does not pay it (or the proof is wrong).
    NotThisAddress,
    /// The output is the wallet's own change: a proof that the wallet paid itself proves nothing.
    Change,
    /// The wallet holds nothing about that output or payment (or forgot its anchor).
    Unknown,
    /// The node could not be read.
    Chain(String),
}

impl ProofError {
    pub fn as_str(&self) -> String {
        match self {
            ProofError::Format(w) => format!("not a valid proof or signature: {w}"),
            ProofError::BadSignature => {
                "the signature does not match this address and message".into()
            }
            ProofError::NoBlock(h) => format!("the node's chain has no block at height {h}"),
            ProofError::NoOutput => "that block has no such output".into(),
            ProofError::NotThisAddress => {
                "the output does not pay that address (or the proof is wrong)".into()
            }
            ProofError::Change => {
                "that output is this wallet's own change: there is nothing to prove".into()
            }
            ProofError::Unknown => "this wallet has nothing to prove that with".into(),
            ProofError::Chain(e) => format!("cannot read the node: {e}"),
        }
    }
}

impl std::fmt::Display for ProofError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_str())
    }
}

impl std::error::Error for ProofError {}

// ---- signatures ------------------------------------------------------------------------------------------------------------

/// A signature by an address: `(c, s_g, s_t)`, each a canonical scalar. With `K` the address's spend key and `V` its view
/// key, it verifies when `c = H_n(domain, K, V, R, bound...)` for `R = s_g G + s_t T + c K` (a Schnorr proof of knowledge of
/// `a, b` with `K = a G + b T`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signature {
    pub c: [u8; 32],
    pub s_g: [u8; 32],
    pub s_t: [u8; 32],
}

impl Signature {
    pub fn to_bytes(&self) -> [u8; 96] {
        let mut out = [0u8; 96];
        out[..32].copy_from_slice(&self.c);
        out[32..64].copy_from_slice(&self.s_g);
        out[64..].copy_from_slice(&self.s_t);
        out
    }

    pub fn from_bytes(b: &[u8]) -> Result<Signature, ProofError> {
        if b.len() != 96 {
            return Err(ProofError::Format("a signature is 96 bytes"));
        }
        let part = |i: usize| -> [u8; 32] { b[i..i + 32].try_into().expect("32") };
        Ok(Signature {
            c: part(0),
            s_g: part(32),
            s_t: part(64),
        })
    }

    pub fn to_text(&self) -> String {
        format!("{SIGNATURE_PREFIX}{}", b58encode(&self.to_bytes()))
    }

    pub fn from_text(text: &str) -> Result<Signature, ProofError> {
        let body = text
            .trim()
            .strip_prefix(SIGNATURE_PREFIX)
            .ok_or(ProofError::Format("a signature starts with TENsig1"))?;
        let bytes = b58decode(body).map_err(|_| ProofError::Format("not base58"))?;
        Signature::from_bytes(&bytes)
    }
}

fn challenge(
    domain: &str,
    spend: &[u8; 32],
    view: &[u8; 32],
    r: &[u8; 32],
    bound: &[&[u8]],
) -> Scalar {
    let mut fields: Vec<&[u8]> = vec![spend, view, r];
    fields.extend_from_slice(bound);
    tenero_carrot::hash_to_scalar(domain, &fields)
}

/// Signs `bound` under `domain` with the secrets `a, b` of the spend key `spend` (`spend = a G + b T`). The nonces are
/// derived from the secrets, everything signed and 32 fresh random bytes, so a broken random source cannot repeat one.
fn sign_raw(
    domain: &str,
    a: &Scalar,
    b: &Scalar,
    spend: &[u8; 32],
    view: &[u8; 32],
    bound: &[&[u8]],
    rng: &mut (impl RngCore + CryptoRng),
) -> Signature {
    let mut rnd = Zeroizing::new([0u8; 32]);
    rng.fill_bytes(&mut *rnd);
    let nonce = |which: &[u8]| {
        let mut fields: Vec<&[u8]> = vec![
            which,
            a.as_bytes(),
            b.as_bytes(),
            spend,
            view,
            domain.as_bytes(),
        ];
        fields.extend_from_slice(bound);
        fields.push(&*rnd);
        tenero_carrot::hash_to_scalar(NONCE_DOMAIN, &fields)
    };
    let (r_g, r_t) = (Zeroizing::new(nonce(b"G")), Zeroizing::new(nonce(b"T")));
    let r = EdwardsPoint::mul_base(&r_g) + *T * *r_t;
    let c = challenge(domain, spend, view, &compress(&r), bound);
    Signature {
        c: c.to_bytes(),
        s_g: (*r_g - c * a).to_bytes(),
        s_t: (*r_t - c * b).to_bytes(),
    }
}

fn verify_raw(
    domain: &str,
    spend: &[u8; 32],
    view: &[u8; 32],
    bound: &[&[u8]],
    sig: &Signature,
) -> bool {
    let scalar = |b: &[u8; 32]| Option::<Scalar>::from(Scalar::from_canonical_bytes(*b));
    let (Some(c), Some(s_g), Some(s_t)) = (scalar(&sig.c), scalar(&sig.s_g), scalar(&sig.s_t))
    else {
        return false;
    };
    // the spend key must be a point of the prime-order group (a key with a small-order part has no known secrets, and
    // would let one signature have several forms)
    if !in_main_subgroup(spend) || decompress(view).is_none() {
        return false;
    }
    let Some(k) = decompress(spend) else {
        return false;
    };
    // the identity has no secrets to know: anyone could "sign" for it
    if k == EdwardsPoint::default() {
        return false;
    }
    let r = EdwardsPoint::mul_base(&s_g) + *T * s_t + k * c;
    challenge(domain, spend, view, &compress(&r), bound) == c
}

fn message_bound(message: &[u8]) -> [u8; 8] {
    (message.len() as u64).to_le_bytes()
}

/// Signs `message` as `address` with the secrets of its spend key.
pub fn sign_message(
    a: &Scalar,
    b: &Scalar,
    address: &Address,
    message: &[u8],
    rng: &mut (impl RngCore + CryptoRng),
) -> Signature {
    let len = message_bound(message);
    sign_raw(
        MESSAGE_DOMAIN,
        a,
        b,
        &address.spend_pubkey,
        &address.view_pubkey,
        &[&len, message],
        rng,
    )
}

/// Whether `sig` is `address`'s signature of exactly `message`. (An integrated address signs as its main address: the
/// keys are the same.)
pub fn verify_message(address: &Address, message: &[u8], sig: &Signature) -> bool {
    let len = message_bound(message);
    verify_raw(
        MESSAGE_DOMAIN,
        &address.spend_pubkey,
        &address.view_pubkey,
        &[&len, message],
        sig,
    )
}

// ---- payment proofs -------------------------------------------------------------------------------------------------------

/// That the output with one-time address `onetime_address`, in the block at `height`, pays `address`: the payment's Janus
/// anchor, from which the output is made again. With a signature, a RECEIVED proof (see the module documentation).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentProof {
    pub address: Address,
    pub height: u64,
    pub onetime_address: [u8; 32],
    pub anchor: JanusAnchor,
    pub signature: Option<Signature>,
}

fn proof_bound<'a>(
    height: &'a [u8; 8],
    onetime_address: &'a [u8; 32],
    anchor: &'a JanusAnchor,
    len: &'a [u8; 8],
    message: &'a [u8],
) -> [&'a [u8]; 5] {
    [height, onetime_address, anchor, len, message]
}

impl PaymentProof {
    /// `version | address text length u8 | address text | height u64 | one-time address 32 | anchor 16 | flag u8 |
    /// signature 96 if the flag is 1`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let addr = self.address.to_text();
        let mut out = Vec::with_capacity(1 + 1 + addr.len() + 8 + 32 + 16 + 1 + 96);
        out.push(PROOF_VERSION);
        out.push(addr.len() as u8);
        out.extend_from_slice(addr.as_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.extend_from_slice(&self.onetime_address);
        out.extend_from_slice(&self.anchor);
        match &self.signature {
            Some(s) => {
                out.push(1);
                out.extend_from_slice(&s.to_bytes());
            }
            None => out.push(0),
        }
        out
    }

    /// Strict: one proof has one encoding, and nothing may follow it.
    pub fn from_bytes(b: &[u8]) -> Result<PaymentProof, ProofError> {
        let bad = ProofError::Format;
        let (&version, rest) = b.split_first().ok_or(bad("empty"))?;
        if version != PROOF_VERSION {
            return Err(bad("an unknown version"));
        }
        let (&n, rest) = rest.split_first().ok_or(bad("cut short"))?;
        let n = usize::from(n);
        if rest.len() < n + 8 + 32 + 16 + 1 {
            return Err(bad("cut short"));
        }
        let text =
            std::str::from_utf8(&rest[..n]).map_err(|_| bad("an address that is not text"))?;
        let address = Address::parse_any(text).map_err(|_| bad("not a valid address"))?;
        // only the address's own text: no spaces or other forms
        if address.to_text() != text {
            return Err(bad("not a valid address"));
        }
        let rest = &rest[n..];
        let height = u64::from_le_bytes(rest[..8].try_into().expect("8"));
        let onetime_address: [u8; 32] = rest[8..40].try_into().expect("32");
        let anchor: JanusAnchor = rest[40..56].try_into().expect("16");
        let signature = match (rest[56], &rest[57..]) {
            (0, []) => None,
            (1, s) if s.len() == 96 => Some(Signature::from_bytes(s)?),
            (0 | 1, _) => return Err(bad("the wrong length")),
            _ => return Err(bad("an unknown flag")),
        };
        Ok(PaymentProof {
            address,
            height,
            onetime_address,
            anchor,
            signature,
        })
    }

    pub fn to_text(&self) -> String {
        format!("{PROOF_PREFIX}{}", b58encode(&self.to_bytes()))
    }

    pub fn from_text(text: &str) -> Result<PaymentProof, ProofError> {
        let body = text
            .trim()
            .strip_prefix(PROOF_PREFIX)
            .ok_or(ProofError::Format("a payment proof starts with TENpay1"))?;
        let bytes = b58decode(body).map_err(|_| ProofError::Format("not base58"))?;
        PaymentProof::from_bytes(&bytes)
    }

    /// Adds the receiving address's signature over the output, the anchor and `message`: a RECEIVED proof.
    pub fn sign(
        &mut self,
        a: &Scalar,
        b: &Scalar,
        message: &[u8],
        rng: &mut (impl RngCore + CryptoRng),
    ) -> Result<(), ProofError> {
        if message.len() > MAX_PROOF_MESSAGE {
            return Err(ProofError::Format("the message is too long"));
        }
        let (h, len) = (self.height.to_le_bytes(), message_bound(message));
        let bound = proof_bound(&h, &self.onetime_address, &self.anchor, &len, message);
        self.signature = Some(sign_raw(
            PAYMENT_DOMAIN,
            a,
            b,
            &self.address.spend_pubkey,
            &self.address.view_pubkey,
            &bound,
            rng,
        ));
        Ok(())
    }

    /// Whether the proof's signature (if any) is the address's over this output and `message`.
    fn signature_holds(&self, message: &[u8]) -> bool {
        let Some(sig) = &self.signature else {
            return true;
        };
        let (h, len) = (self.height.to_le_bytes(), message_bound(message));
        let bound = proof_bound(&h, &self.onetime_address, &self.anchor, &len, message);
        verify_raw(
            PAYMENT_DOMAIN,
            &self.address.spend_pubkey,
            &self.address.view_pubkey,
            &bound,
            sig,
        )
    }
}

/// What a checked proof shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checked {
    pub address: Address,
    pub amount: u64,
    pub height: u64,
    pub global_index: u64,
    pub coinbase: bool,
    /// The payment ID the output carried (zero but for an integrated address).
    pub payment_id: [u8; 8],
    /// Blocks on top of it and its own (1 for the tip).
    pub confirmations: u64,
    /// Whether the receiving address signed it (a received proof).
    pub signed: bool,
}

/// An output of a scanned block, found by its one-time address.
enum Found<'a> {
    Coinbase(&'a tenero_core::v3::CoinbaseOutput, u64),
    Tx(Enote, &'a [u8; 8], u64),
}

fn find<'a>(block: &'a ScanBlock, onetime_address: &[u8; 32]) -> Option<Found<'a>> {
    let mut index = block.first_output_index;
    for o in &block.coinbase.outputs {
        if &o.onetime_address == onetime_address {
            return Some(Found::Coinbase(o, index));
        }
        index += 1;
    }
    for t in &block.txs {
        for (j, o) in t.outputs.iter().enumerate() {
            if &o.onetime_address == onetime_address {
                let k = if t.ephemeral_pubkeys.len() == 1 { 0 } else { j };
                let (first, d_e) = (t.inputs.first()?, t.ephemeral_pubkeys.get(k)?);
                let enote = Enote {
                    onetime_address: o.onetime_address,
                    amount_commitment: o.amount_commitment,
                    amount_enc: o.amount_enc,
                    view_tag: o.view_tag,
                    ephemeral_pubkey: *d_e,
                    anchor_enc: o.anchor_enc,
                    tx_first_key_image: first.key_image,
                };
                return Some(Found::Tx(enote, &t.encrypted_payment_id, index));
            }
            index += 1;
        }
    }
    None
}

/// The amount, global index, coinbase flag and payment ID of the output `found` if `anchor` makes it for `address`.
fn rebuild(
    found: &Found<'_>,
    address: &Address,
    anchor: &JanusAnchor,
    height: u64,
) -> Result<(u64, u64, bool, [u8; 8]), ProofError> {
    match found {
        Found::Coinbase(o, index) => {
            // a block reward pays a main address, and its output is made from the anchor and the public amount
            let made = (address.kind == Kind::Main)
                .then(|| {
                    crate::coinbase_payout_to_keys(
                        &address.spend_pubkey,
                        &address.view_pubkey,
                        height,
                        o.amount,
                        anchor,
                    )
                })
                .flatten()
                .ok_or(ProofError::NotThisAddress)?;
            let same = made.onetime_address == o.onetime_address
                && made.view_tag == o.view_tag
                && made.ephemeral_pubkey == o.ephemeral_pubkey
                && made.anchor_enc == o.anchor_enc;
            if !same {
                return Err(ProofError::NotThisAddress);
            }
            Ok((o.amount, *index, true, [0; 8]))
        }
        Found::Tx(enote, pid_enc, index) => {
            let r = scan_external_as_sender(
                enote,
                Some(pid_enc),
                &address.destination(),
                anchor,
                address.kind == Kind::Integrated,
            )
            .ok_or(ProofError::NotThisAddress)?;
            Ok((r.amount, *index, false, r.payment_id))
        }
    }
}

fn confirmations(chain: &impl ChainView, height: u64) -> Result<u64, ProofError> {
    let (tip, _) = chain.tip().map_err(ProofError::Chain)?;
    Ok(tip.saturating_add(1).saturating_sub(height))
}

/// Checks `proof` against the node's chain: the output is in the block it names, the anchor makes it for the proof's
/// address, and a signature (if there is one) is that address's over the output and `message`.
pub fn check_payment(
    chain: &impl ChainView,
    proof: &PaymentProof,
    message: &[u8],
) -> Result<Checked, ProofError> {
    let block = chain
        .block(proof.height)
        .map_err(ProofError::Chain)?
        .ok_or(ProofError::NoBlock(proof.height))?;
    let found = find(&block, &proof.onetime_address).ok_or(ProofError::NoOutput)?;
    let (amount, global_index, coinbase, payment_id) =
        rebuild(&found, &proof.address, &proof.anchor, proof.height)?;
    if !proof.signature_holds(message) {
        return Err(ProofError::BadSignature);
    }
    Ok(Checked {
        address: proof.address,
        amount,
        height: proof.height,
        global_index,
        coinbase,
        payment_id,
        confirmations: confirmations(chain, proof.height)?,
        signed: proof.signature.is_some(),
    })
}

/// Looks for the output a payment's anchor made for `address`, in the blocks from `from_height` to the tip (at most
/// [`MAX_ANCHOR_SEARCH`] of them): what a person given only the "payment key" (the anchor) and an address can check.
pub fn check_anchor(
    chain: &impl ChainView,
    anchor: &JanusAnchor,
    address: &Address,
    from_height: u64,
) -> Result<Checked, ProofError> {
    let (tip, _) = chain.tip().map_err(ProofError::Chain)?;
    let end = tip.min(from_height.saturating_add(MAX_ANCHOR_SEARCH));
    let mut h = from_height;
    while h <= end {
        let blocks = chain.blocks(h, end - h + 1).map_err(ProofError::Chain)?;
        if blocks.is_empty() {
            break;
        }
        for block in &blocks {
            let outputs = block
                .coinbase
                .outputs
                .iter()
                .map(|o| o.onetime_address)
                .chain(
                    block
                        .txs
                        .iter()
                        .flat_map(|t| t.outputs.iter().map(|o| o.onetime_address)),
                );
            for ko in outputs {
                let found = find(block, &ko).expect("an output of this block");
                if let Ok((amount, global_index, coinbase, payment_id)) =
                    rebuild(&found, address, anchor, block.height)
                {
                    return Ok(Checked {
                        address: *address,
                        amount,
                        height: block.height,
                        global_index,
                        coinbase,
                        payment_id,
                        confirmations: confirmations(chain, block.height)?,
                        signed: false,
                    });
                }
            }
        }
        h = blocks.last().map_or(end, |b| b.height) + 1;
    }
    Err(ProofError::NotThisAddress)
}

/// The anchor of an output a wallet RECEIVED, from its view key: what the sender chose, decrypted from the output.
pub(crate) fn received_anchor(
    k_view: &Scalar,
    block: &ScanBlock,
    onetime_address: &[u8; 32],
) -> Result<JanusAnchor, ProofError> {
    match find(block, onetime_address).ok_or(ProofError::NoOutput)? {
        Found::Coinbase(o, _) => {
            let enote = CoinbaseEnote {
                onetime_address: o.onetime_address,
                amount: o.amount,
                view_tag: o.view_tag,
                ephemeral_pubkey: o.ephemeral_pubkey,
                anchor_enc: o.anchor_enc,
                block_index: block.height,
            };
            Ok(anchor_coinbase(
                &enote,
                &shared_secret(k_view, &o.ephemeral_pubkey),
            ))
        }
        Found::Tx(enote, _, _) => {
            anchor_external(&enote, &shared_secret(k_view, &enote.ephemeral_pubkey))
                .ok_or(ProofError::NotThisAddress)
        }
    }
}

/// The block from `from` on (at most `within` blocks) that holds the output with this one-time address.
pub(crate) fn block_with(
    chain: &impl ChainView,
    onetime_address: &[u8; 32],
    from: u64,
    within: u64,
) -> Result<ScanBlock, ProofError> {
    let (tip, _) = chain.tip().map_err(ProofError::Chain)?;
    let end = tip.min(from.saturating_add(within));
    let mut h = from;
    while h <= end {
        let blocks = chain.blocks(h, end - h + 1).map_err(ProofError::Chain)?;
        if blocks.is_empty() {
            break;
        }
        for b in &blocks {
            if find(b, onetime_address).is_some() {
                return Ok(b.clone());
            }
        }
        h = blocks.last().map_or(end, |b| b.height) + 1;
    }
    Err(ProofError::NoOutput)
}
