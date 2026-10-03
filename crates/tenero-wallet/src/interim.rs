//! The INTERIM output scheme: how a wallet makes an output for a recipient and recognises its own.
//!
//! **This is not Carrot, and it is not private in the Monero sense.** It fills the same 123-byte output
//! format (`docs/CONSENSUS_V2.md` 6.1) with the classic CryptoNote design, so that a wallet works now; Carrot
//! replaces this one module later (decision 6 in `docs/M8_PLAN.md`) and outputs made here stay spendable,
//! because spending needs only the one-time secret and the commitment's mask, which this module recovers.
//! What it lacks compared with Carrot, stated plainly:
//!
//! * **No Janus protection.** The 16-byte `anchor_enc` is random bytes encrypted for the receiver and never
//!   checked: a malicious sender can make two outputs to two addresses of one wallet that look related.
//! * **One address per wallet**: no subaddresses, no integrated addresses or payment ids.
//! * **No outgoing view key**: nothing lets a wallet (or an auditor holding a view key) recover to whom it
//!   paid. Change comes back to the wallet's own address, so scanning finds it like any other output.
//! * The key exchange is Ed25519 (`De = r*G`), not Carrot's X25519, and the hashes are SHA-256 with the labels
//!   below, not Carrot's.
//! * It is built from audited primitives (`curve25519-dalek` and SHA-256) but the composition is ours, written
//!   for this project and **unaudited**.
//!
//! # The construction
//!
//! A wallet has a spend secret `s` and a view secret `v` (both from one seed), and the address is
//! `(K_s, K_v) = (s*G, v*G)`. To pay amount `a` to an address as output number `i` of a transaction whose
//! context is `ctx` (the first key image of the transaction, hashed; for a coinbase, the height, hashed):
//!
//! ```text
//! r      random scalar                       De = r*G                 (ephemeral_pubkey)
//! S      = 8 * r * K_v                       (the receiver gets 8 * v * De: the same point)
//! tag    = SHA-256("viewtag"  || S || ctx)[..3]                       (view_tag)
//! t      = Hs("onetime"       || S || ctx || i)                       (the one-time offset)
//! Ko     = t*G + K_s                                                  (onetime_address)
//! mask   = Hs("mask"          || S || ctx || i)
//! Ca     = mask*G + a*H                                               (amount_commitment)
//! amount_enc = a (little endian) XOR SHA-256("amount" || S || ctx || i)[..8]
//! anchor_enc = anchor XOR SHA-256("anchor" || S || ctx || i)[..16]    (anchor: 16 random bytes)
//! ```
//!
//! `Hs` is two SHA-256 blocks (with a counter) reduced modulo the group order. The one-time secret of the
//! output is `t + s`. A coinbase output has no commitment: its amount is public and its commitment is the fixed
//! `1*G + a*H` (so its mask is 1, `docs/CONSENSUS_V2.md` 7.1), and no `amount_enc`.

use std::fmt;

use curve25519_dalek::constants::ED25519_BASEPOINT_POINT;
use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint};
use curve25519_dalek::scalar::Scalar;
use curve25519_dalek::traits::IsIdentity;
use rand_core::{CryptoRng, RngCore};
use tenero_core::hash::{hex_lower, sha256};
use tenero_core::v2::{CoinbaseOutput, Output};
use tenero_crypto::ringct;
use zeroize::Zeroize;

/// What the program must say wherever it shows an address or a balance.
pub const BANNER: &str =
    "INTERIM output scheme: not Carrot, no Janus protection, one address per wallet. Experimental, unaudited.";

const DOMAIN: &[u8] = b"tenero interim v1";

/// The text form of an address begins with this (and says "interim" in its name so nobody takes it for a final
/// address format).
pub const ADDRESS_PREFIX: &str = "tni1";

/// Hash to a scalar: two SHA-256 blocks, reduced modulo the group order.
pub(crate) fn hs(tag: &[u8], parts: &[&[u8]]) -> Scalar {
    let mut wide = [0u8; 64];
    for (i, half) in wide.chunks_mut(32).enumerate() {
        let counter = [i as u8];
        let mut all: Vec<&[u8]> = vec![DOMAIN, tag, &counter];
        all.extend_from_slice(parts);
        half.copy_from_slice(&sha256(&all));
    }
    Scalar::from_bytes_mod_order_wide(&wide)
}

pub(crate) fn digest(tag: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut all: Vec<&[u8]> = vec![DOMAIN, tag];
    all.extend_from_slice(parts);
    sha256(&all)
}

/// A point that must be canonical, of prime order and not the identity.
pub(crate) fn strict_point(bytes: &[u8; 32]) -> Option<EdwardsPoint> {
    let p = CompressedEdwardsY(*bytes).decompress()?;
    (p.is_torsion_free() && !p.is_identity()).then_some(p)
}

pub(crate) fn compress(p: &EdwardsPoint) -> [u8; 32] {
    p.compress().to_bytes()
}

/// The shared secret as bytes: the Diffie-Hellman point multiplied by the cofactor, so a small-order component
/// in a sender-chosen `De` cannot change what the receiver computes.
pub(crate) fn shared_bytes(dh: &EdwardsPoint) -> [u8; 32] {
    compress(&dh.mul_by_cofactor())
}

/// The context of an ordinary transaction: its first key image.
pub fn tx_context(first_key_image: &[u8; 32]) -> [u8; 32] {
    digest(b"tx context", &[first_key_image])
}

/// The context of the coinbase of the block at `height`.
pub fn coinbase_context(height: u64) -> [u8; 32] {
    digest(b"coinbase context", &[&height.to_le_bytes()])
}

// ------------------------------------------------------------------------------------------------
// Keys and addresses
// ------------------------------------------------------------------------------------------------

/// A wallet's address: the public spend key and the public view key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Address {
    pub spend: [u8; 32],
    pub view: [u8; 32],
}

#[derive(Debug, PartialEq, Eq)]
pub enum AddressError {
    /// Not `tni1` followed by 136 hexadecimal digits.
    Format,
    /// The checksum does not match (a typing mistake).
    Checksum,
    /// A key is not a valid prime-order point.
    BadKey,
}

impl fmt::Display for AddressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AddressError::Format => write!(f, "not an interim Tenero address"),
            AddressError::Checksum => write!(f, "the address checksum does not match"),
            AddressError::BadKey => write!(f, "the address holds an invalid key"),
        }
    }
}

impl std::error::Error for AddressError {}

impl Address {
    fn checksum(&self) -> [u8; 4] {
        let d = digest(b"address checksum", &[&self.spend, &self.view]);
        [d[0], d[1], d[2], d[3]]
    }

    /// `tni1` + 68 bytes in hexadecimal: both keys and a 4-byte checksum.
    pub fn to_text(&self) -> String {
        let mut bytes = Vec::with_capacity(68);
        bytes.extend_from_slice(&self.spend);
        bytes.extend_from_slice(&self.view);
        bytes.extend_from_slice(&self.checksum());
        format!("{ADDRESS_PREFIX}{}", hex_lower(&bytes))
    }

    pub fn from_text(text: &str) -> Result<Address, AddressError> {
        let hex = text
            .strip_prefix(ADDRESS_PREFIX)
            .ok_or(AddressError::Format)?;
        if hex.len() != 136 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(AddressError::Format);
        }
        let mut bytes = [0u8; 68];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b =
                u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).map_err(|_| AddressError::Format)?;
        }
        let mut spend = [0u8; 32];
        let mut view = [0u8; 32];
        spend.copy_from_slice(&bytes[..32]);
        view.copy_from_slice(&bytes[32..64]);
        let address = Address { spend, view };
        if address.checksum() != bytes[64..] {
            return Err(AddressError::Checksum);
        }
        if strict_point(&spend).is_none() || strict_point(&view).is_none() {
            return Err(AddressError::BadKey);
        }
        Ok(address)
    }
}

/// The secrets of a wallet. Zeroed when dropped; never printed.
pub struct Keys {
    spend: Scalar,
    view: Scalar,
}

impl fmt::Debug for Keys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Keys(..)")
    }
}

impl Drop for Keys {
    fn drop(&mut self) {
        self.spend.zeroize();
        self.view.zeroize();
    }
}

impl Keys {
    /// The keys of a 32-byte seed (the seed is the wallet's whole secret; keep it safe).
    pub fn from_seed(seed: &[u8; 32]) -> Keys {
        let spend = hs(b"spend key", &[seed]);
        let view = hs(b"view key", &[&spend.to_bytes()]);
        Keys { spend, view }
    }

    /// The spend scalar, for signing (`proofs.rs`).
    pub(crate) fn spend_scalar(&self) -> &Scalar {
        &self.spend
    }

    pub fn address(&self) -> Address {
        Address {
            spend: compress(&(ED25519_BASEPOINT_POINT * self.spend)),
            view: compress(&(ED25519_BASEPOINT_POINT * self.view)),
        }
    }

    /// What is needed to find and read one's outputs, but not to spend them.
    pub fn view_keys(&self) -> ViewKeys {
        ViewKeys {
            view: self.view,
            address: self.address(),
        }
    }

    /// The secret of an output the wallet recognised: `offset + spend`. Feed it to the prover.
    pub fn onetime_secret(&self, offset: &[u8; 32]) -> Option<[u8; 32]> {
        let t = Option::<Scalar>::from(Scalar::from_canonical_bytes(*offset))?;
        Some((t + self.spend).to_bytes())
    }
}

/// The view secret and the address: enough to recognise and read outputs, not to spend them.
pub struct ViewKeys {
    view: Scalar,
    address: Address,
}

impl fmt::Debug for ViewKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ViewKeys(..)")
    }
}

impl Drop for ViewKeys {
    fn drop(&mut self) {
        self.view.zeroize();
    }
}

impl ViewKeys {
    pub fn address(&self) -> Address {
        self.address
    }

    /// The view scalar, for proving receipt (`proofs.rs`).
    pub(crate) fn view_scalar(&self) -> &Scalar {
        &self.view
    }
}

// ------------------------------------------------------------------------------------------------
// Making an output
// ------------------------------------------------------------------------------------------------

/// The secret `r` of one output (the sender's half of its key exchange: `De = r*G`). It is what proves a payment (`proofs.rs`).
/// Whoever holds it can show that THIS output paid this address and how much, and nothing else. Zeroed when dropped; its `Debug`
/// never prints it.
#[derive(Clone, PartialEq, Eq)]
pub struct TxSecret(zeroize::Zeroizing<[u8; 32]>);

impl TxSecret {
    pub fn new(bytes: [u8; 32]) -> TxSecret {
        TxSecret(zeroize::Zeroizing::new(bytes))
    }

    /// The bytes. Show them only when the person asks.
    pub fn expose(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for TxSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TxSecret(..)")
    }
}

/// An output being made for a recipient: the fields of the output and what the SENDER keeps (the mask and the secret).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Enote {
    pub onetime_address: [u8; 32],
    /// `mask*G + amount*H`; for a coinbase output, the fixed public-amount commitment (nothing is stored).
    pub amount_commitment: [u8; 32],
    pub amount_enc: [u8; 8],
    pub view_tag: [u8; 3],
    pub ephemeral_pubkey: [u8; 32],
    pub anchor_enc: [u8; 16],
    /// The commitment's mask (1 for a coinbase output). The range proof needs it.
    pub mask: [u8; 32],
    /// The secret `r` that made this output's ephemeral key: kept so that a payment can be proved later.
    pub tx_secret: TxSecret,
}

impl Enote {
    pub fn to_output(&self) -> Output {
        Output {
            onetime_address: self.onetime_address,
            amount_commitment: self.amount_commitment,
            amount_enc: self.amount_enc,
            view_tag: self.view_tag,
            ephemeral_pubkey: self.ephemeral_pubkey,
            anchor_enc: self.anchor_enc,
        }
    }

    pub fn to_coinbase_output(&self, amount: u64) -> CoinbaseOutput {
        CoinbaseOutput {
            onetime_address: self.onetime_address,
            amount,
            view_tag: self.view_tag,
            ephemeral_pubkey: self.ephemeral_pubkey,
            anchor_enc: self.anchor_enc,
        }
    }
}

pub(crate) fn xor<const N: usize>(a: &[u8; N], key: &[u8; 32]) -> [u8; N] {
    let mut out = [0u8; N];
    for i in 0..N {
        out[i] = a[i] ^ key[i];
    }
    out
}

/// Makes output number `index` of the transaction with context `ctx` for `to`.
///
/// `coinbase` outputs carry a public amount (nothing is encrypted and the mask is 1); the others carry a
/// hidden one. Returns `None` if the address holds an invalid key or the commitment cannot be made.
pub fn create_enote(
    rng: &mut (impl RngCore + CryptoRng),
    to: &Address,
    amount: u64,
    ctx: &[u8; 32],
    index: u32,
    coinbase: bool,
) -> Option<Enote> {
    let k_spend = strict_point(&to.spend)?;
    let k_view = strict_point(&to.view)?;
    let mut wide = [0u8; 64];
    rng.fill_bytes(&mut wide);
    let r = Scalar::from_bytes_mod_order_wide(&wide);
    wide.zeroize();
    let tx_secret = TxSecret::new(r.to_bytes());
    let mut anchor = [0u8; 16];
    rng.fill_bytes(&mut anchor);
    let de = ED25519_BASEPOINT_POINT * r;
    let s = shared_bytes(&(k_view * r));
    let i = index.to_le_bytes();
    let t = hs(b"onetime", &[&s, ctx, &i]);
    let onetime = compress(&(ED25519_BASEPOINT_POINT * t + k_spend));
    let tag = digest(b"viewtag", &[&s, ctx]);
    let (mask, amount_commitment, amount_enc) = if coinbase {
        (
            Scalar::ONE.to_bytes(),
            ringct::public_amount_commitment(amount),
            [0u8; 8],
        )
    } else {
        let mask = hs(b"mask", &[&s, ctx, &i]).to_bytes();
        let key = digest(b"amount", &[&s, ctx, &i]);
        (
            mask,
            ringct::commit(&mask, amount)?,
            xor(&amount.to_le_bytes(), &key),
        )
    };
    let anchor_enc = xor(&anchor, &digest(b"anchor", &[&s, ctx, &i]));
    Some(Enote {
        onetime_address: onetime,
        amount_commitment,
        amount_enc,
        view_tag: [tag[0], tag[1], tag[2]],
        ephemeral_pubkey: compress(&de),
        anchor_enc,
        mask,
        tx_secret,
    })
}

// ------------------------------------------------------------------------------------------------
// Recognising an output
// ------------------------------------------------------------------------------------------------

/// An output that belongs to the wallet, read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recognised {
    pub amount: u64,
    /// The commitment's mask (1 for a coinbase output).
    pub mask: [u8; 32],
    /// The one-time offset `t`; the output's secret is `t + spend` ([`Keys::onetime_secret`]).
    pub offset: [u8; 32],
}

/// The fields of an output that the recogniser needs, whichever kind it is.
struct Fields<'a> {
    onetime_address: &'a [u8; 32],
    ephemeral_pubkey: &'a [u8; 32],
    view_tag: &'a [u8; 3],
}

fn recognise(
    keys: &ViewKeys,
    f: &Fields<'_>,
    ctx: &[u8; 32],
    index: u32,
) -> Option<([u8; 32], Scalar, [u8; 32])> {
    let de = strict_point(f.ephemeral_pubkey)?;
    let s = shared_bytes(&(de * keys.view));
    // the cheap test first: 255 in 256 (here, all but one in 16 million) stop here
    let tag = digest(b"viewtag", &[&s, ctx]);
    if tag[..3] != f.view_tag[..] {
        return None;
    }
    let i = index.to_le_bytes();
    let t = hs(b"onetime", &[&s, ctx, &i]);
    let k_spend = strict_point(&keys.address.spend)?;
    let ko = compress(&(ED25519_BASEPOINT_POINT * t + k_spend));
    if &ko != f.onetime_address {
        return None;
    }
    Some((s, t, ko))
}

/// Is output number `index` of the transaction with context `ctx` ours? If so, its amount, mask and offset.
pub fn scan_output(
    keys: &ViewKeys,
    output: &Output,
    ctx: &[u8; 32],
    index: u32,
) -> Option<Recognised> {
    let fields = Fields {
        onetime_address: &output.onetime_address,
        ephemeral_pubkey: &output.ephemeral_pubkey,
        view_tag: &output.view_tag,
    };
    let (s, t, _) = recognise(keys, &fields, ctx, index)?;
    let i = index.to_le_bytes();
    let amount = u64::from_le_bytes(xor(&output.amount_enc, &digest(b"amount", &[&s, ctx, &i])));
    let mask = hs(b"mask", &[&s, ctx, &i]).to_bytes();
    // the commitment must be the one these secrets give: a sender who gets this wrong would hand over
    // coins that cannot be spent
    if ringct::commit(&mask, amount)? != output.amount_commitment {
        return None;
    }
    Some(Recognised {
        amount,
        mask,
        offset: t.to_bytes(),
    })
}

/// Is output number `index` of the coinbase at `height` ours?
pub fn scan_coinbase_output(
    keys: &ViewKeys,
    output: &CoinbaseOutput,
    height: u64,
    index: u32,
) -> Option<Recognised> {
    let fields = Fields {
        onetime_address: &output.onetime_address,
        ephemeral_pubkey: &output.ephemeral_pubkey,
        view_tag: &output.view_tag,
    };
    let (_, t, _) = recognise(keys, &fields, &coinbase_context(height), index)?;
    Some(Recognised {
        amount: output.amount,
        mask: Scalar::ONE.to_bytes(),
        offset: t.to_bytes(),
    })
}
