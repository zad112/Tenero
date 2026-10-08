//! Finding one's outputs (Carrot 8; Monero's `scan.cpp` and `scan_unsafe.cpp`), and what is needed to spend them.
//!
//! Scanning an output gives the address spend key it was made for, the amount and the commitment's blinding factor,
//! and the two sender extensions `k^o_g`, `k^o_t`. The wallet then looks the address spend key up in its table of
//! addresses; for one of its own, at index `j`, the output's key is `Ko = x G + y T` with
//! `x = k_gi * k^j_subscal + k^o_g` and `y = k_ps * k^j_subscal + k^o_t`, and its key image is `x * Hp²(Ko)`.
//!
//! The Janus checks: an external output must either re-derive its ephemeral key from the decrypted anchor and the
//! address it claims (`normal`), or carry the special anchor that only the owner's `k_v` makes (`special`). An output
//! that fails both is refused, so a sender cannot make two of one's addresses look linked.

use curve25519_dalek::edwards::EdwardsPoint;
use curve25519_dalek::scalar::Scalar;

use crate::account::{AccountPublic, AccountSecrets, AddressIndex, Destination, ViewAll};
use crate::derive::*;
use crate::points::{compress, decompress, T};
use crate::{
    CoinbaseEnote, Enote, EnoteType, InputContext, JanusAnchor, PaymentId, NULL_PAYMENT_ID,
};

/// How an output was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found {
    /// A payment or special self-send, found with `k_v`.
    External,
    /// An internal self-send, found with `s_vb`.
    Internal,
    Coinbase,
}

/// What scanning recovers from an output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Received {
    pub found: Found,
    /// `K^j_s`: the spend key of the address it was sent to.
    pub address_spend_pubkey: [u8; 32],
    pub amount: u64,
    /// The commitment's blinding factor (one for a coinbase output).
    pub blinding_factor: Scalar,
    pub enote_type: EnoteType,
    /// The payment ID (zero unless the output paid an integrated address).
    pub payment_id: PaymentId,
    pub sender_extension_g: Scalar,
    pub sender_extension_t: Scalar,
    /// The internal message of an internal self-send.
    pub internal_message: Option<JanusAnchor>,
}

fn amount_and_type(
    ctx: &[u8; 32],
    enote: &Enote,
    address_spend: &[u8; 32],
) -> Option<(u64, Scalar, EnoteType)> {
    // `try_get_carrot_amount`: the commitment must open, as a payment or as change
    let amount = decrypt_amount(&enote.amount_enc, ctx, &enote.onetime_address);
    for t in [EnoteType::Payment, EnoteType::Change] {
        let k_a = make_amount_blinding_factor(ctx, amount, address_spend, t);
        if commit_amount(amount, &k_a) == enote.amount_commitment {
            return Some((amount, k_a, t));
        }
    }
    None
}

/// `scan_non_coinbase_info`: everything but the Janus check.
fn scan_non_coinbase(
    enote: &Enote,
    encrypted_payment_id: Option<&[u8; 8]>,
    ctx: &[u8; 32],
) -> Option<(Received, JanusAnchor)> {
    let g = make_sender_extension_g(ctx, &enote.amount_commitment);
    let t = make_sender_extension_t(ctx, &enote.amount_commitment);
    let address_spend = recover_address_spend_pubkey(&enote.onetime_address, &g, &t)?;
    let payment_id = encrypted_payment_id
        .map(|e| encrypt_payment_id(e, ctx, &enote.onetime_address))
        .unwrap_or(NULL_PAYMENT_ID);
    let anchor = encrypt_anchor(&enote.anchor_enc, ctx, &enote.onetime_address);
    let (amount, blinding_factor, enote_type) = amount_and_type(ctx, enote, &address_spend)?;
    Some((
        Received {
            found: Found::External,
            address_spend_pubkey: address_spend,
            amount,
            blinding_factor,
            enote_type,
            payment_id,
            sender_extension_g: g,
            sender_extension_t: t,
            internal_message: None,
        },
        anchor,
    ))
}

/// `try_scan_carrot_enote_external_no_janus`.
fn scan_external_no_janus(
    enote: &Enote,
    encrypted_payment_id: Option<&[u8; 8]>,
    s_sender_receiver: &[u8; 32],
) -> Option<(Received, JanusAnchor)> {
    let input_context = make_input_context(&enote.tx_first_key_image);
    if make_view_tag(s_sender_receiver, &input_context, &enote.onetime_address) != enote.view_tag {
        return None;
    }
    let ctx =
        make_sender_receiver_secret(s_sender_receiver, &enote.ephemeral_pubkey, &input_context);
    scan_non_coinbase(enote, encrypted_payment_id, &ctx)
}

/// `verify_carrot_normal_janus_protection` for one payment ID: the anchor re-derives the ephemeral key.
fn normal_janus(
    anchor: &JanusAnchor,
    input_context: &InputContext,
    address_spend: &[u8; 32],
    address_view: &[u8; 32],
    is_subaddress: bool,
    payment_id: &PaymentId,
    ephemeral_pubkey: &[u8; 32],
) -> bool {
    let d_e = make_enote_ephemeral_privkey(
        anchor,
        input_context,
        address_spend,
        address_view,
        payment_id,
    );
    make_enote_ephemeral_pubkey(&d_e, address_spend, is_subaddress).as_ref()
        == Some(ephemeral_pubkey)
}

/// The normal Janus check with the decrypted payment ID, then with none (an output that paid a non-integrated
/// address carries a random encrypted payment ID). Returns the payment ID that passed.
fn normal_janus_any_pid(
    anchor: &JanusAnchor,
    input_context: &InputContext,
    address_spend: &[u8; 32],
    address_view: &[u8; 32],
    is_subaddress: bool,
    payment_id: PaymentId,
    ephemeral_pubkey: &[u8; 32],
) -> Option<PaymentId> {
    if normal_janus(
        anchor,
        input_context,
        address_spend,
        address_view,
        is_subaddress,
        &payment_id,
        ephemeral_pubkey,
    ) {
        return Some(payment_id);
    }
    normal_janus(
        anchor,
        input_context,
        address_spend,
        address_view,
        is_subaddress,
        &NULL_PAYMENT_ID,
        ephemeral_pubkey,
    )
    .then_some(NULL_PAYMENT_ID)
}

/// The shared secret of a transaction's ephemeral key, for the receiver: `k_v * D_e` (compute it once per key).
pub fn shared_secret(k_view: &Scalar, ephemeral_pubkey: &[u8; 32]) -> [u8; 32] {
    make_shared_key_receiver(k_view, ephemeral_pubkey)
}

/// An external output (a payment, or a special self-send) for the receiver (`try_scan_carrot_enote_external_receiver`).
/// `main_spend_pubkeys` are the account's main address spend keys (one for a Carrot account). The result's address spend
/// key may be any address: the caller must look it up among its own.
pub fn scan_external(
    enote: &Enote,
    encrypted_payment_id: Option<&[u8; 8]>,
    s_sender_receiver: &[u8; 32],
    main_spend_pubkeys: &[[u8; 32]],
    k_view: &Scalar,
) -> Option<Received> {
    let (mut r, anchor) = scan_external_no_janus(enote, encrypted_payment_id, s_sender_receiver)?;
    // `recover_address_view_pubkey`: a subaddress's view key is k_v K^j_s, the main address's k_v G
    let is_subaddress = !main_spend_pubkeys.contains(&r.address_spend_pubkey);
    let view = if is_subaddress {
        compress(&(decompress(&r.address_spend_pubkey)? * k_view))
    } else {
        compress(&EdwardsPoint::mul_base(k_view))
    };
    let input_context = make_input_context(&enote.tx_first_key_image);
    match normal_janus_any_pid(
        &anchor,
        &input_context,
        &r.address_spend_pubkey,
        &view,
        is_subaddress,
        r.payment_id,
        &enote.ephemeral_pubkey,
    ) {
        Some(pid) => {
            r.payment_id = pid;
            Some(r)
        }
        None => {
            // `verify_carrot_special_janus_protection`. As in C++, a special self-send has no payment ID: the failed
            // normal check has already set it to zero (`verify_carrot_normal_janus_protection` resets it before its
            // second try), so the decrypted bytes, which mean nothing here, are not reported.
            let special = make_janus_anchor_special(
                &enote.ephemeral_pubkey,
                &input_context,
                &enote.onetime_address,
                k_view,
            );
            r.payment_id = NULL_PAYMENT_ID;
            (special == anchor).then_some(r)
        }
    }
}

/// An internal self-send, with the view-balance secret (`try_scan_carrot_enote_internal_receiver`).
pub fn scan_internal(enote: &Enote, s_view_balance: &[u8; 32]) -> Option<Received> {
    let input_context = make_input_context(&enote.tx_first_key_image);
    if make_view_tag(s_view_balance, &input_context, &enote.onetime_address) != enote.view_tag {
        return None;
    }
    let ctx = make_sender_receiver_secret(s_view_balance, &enote.ephemeral_pubkey, &input_context);
    let (mut r, message) = scan_non_coinbase(enote, None, &ctx)?;
    r.found = Found::Internal;
    r.internal_message = Some(message);
    Some(r)
}

/// A coinbase output for the receiver (`try_scan_carrot_coinbase_enote_receiver`): only to a main address.
pub fn scan_coinbase(
    enote: &CoinbaseEnote,
    s_sender_receiver: &[u8; 32],
    main_spend_pubkeys: &[[u8; 32]],
    main_view_pubkey: &[u8; 32],
) -> Option<Received> {
    let input_context = make_input_context_coinbase(enote.block_index);
    if make_view_tag(s_sender_receiver, &input_context, &enote.onetime_address) != enote.view_tag {
        return None;
    }
    let ctx =
        make_sender_receiver_secret(s_sender_receiver, &enote.ephemeral_pubkey, &input_context);
    let mut found = None;
    for main in main_spend_pubkeys {
        let g = make_sender_extension_g_coinbase(&ctx, enote.amount, main);
        let t = make_sender_extension_t_coinbase(&ctx, enote.amount, main);
        // as C++: an output key that is not a point ends the scan
        let recovered = recover_address_spend_pubkey(&enote.onetime_address, &g, &t)?;
        if recovered == *main {
            found = Some((recovered, g, t));
            break;
        }
    }
    let (spend, g, t) = found?;
    let anchor = encrypt_anchor(&enote.anchor_enc, &ctx, &enote.onetime_address);
    if !normal_janus(
        &anchor,
        &input_context,
        &spend,
        main_view_pubkey,
        false,
        &NULL_PAYMENT_ID,
        &enote.ephemeral_pubkey,
    ) {
        return None;
    }
    Some(Received {
        found: Found::Coinbase,
        address_spend_pubkey: spend,
        amount: enote.amount,
        blinding_factor: Scalar::ONE,
        enote_type: EnoteType::Payment,
        payment_id: NULL_PAYMENT_ID,
        sender_extension_g: g,
        sender_extension_t: t,
        internal_message: None,
    })
}

/// The sender's check of an output it made to `destination` (`try_scan_carrot_enote_external_sender`), from the anchor
/// it chose: proves which address and amount an output paid (for payment proofs). `check_payment_id`: the payment ID
/// must be the destination's.
pub fn scan_external_as_sender(
    enote: &Enote,
    encrypted_payment_id: Option<&[u8; 8]>,
    destination: &Destination,
    anchor_norm: &JanusAnchor,
    check_payment_id: bool,
) -> Option<Received> {
    let input_context = make_input_context(&enote.tx_first_key_image);
    let d_e = make_enote_ephemeral_privkey(
        anchor_norm,
        &input_context,
        &destination.spend_pubkey,
        &destination.view_pubkey,
        &destination.payment_id,
    );
    let s_sr = make_shared_key_sender(&d_e, &destination.view_pubkey)?;
    let (mut r, anchor) = scan_external_no_janus(enote, encrypted_payment_id, &s_sr)?;
    if r.address_spend_pubkey != destination.spend_pubkey {
        return None;
    }
    let pid = normal_janus_any_pid(
        &anchor,
        &input_context,
        &destination.spend_pubkey,
        &destination.view_pubkey,
        destination.is_subaddress,
        r.payment_id,
        &enote.ephemeral_pubkey,
    )?;
    if check_payment_id && pid != destination.payment_id {
        return None;
    }
    r.payment_id = pid;
    Some(r)
}

/// The Janus anchor of an external output, for its receiver (or anyone with the shared secret `s_sr`): the anchor the sender
/// chose, which with the transaction and the address re-derives the output ([`scan_external_as_sender`]). `None` if the
/// view tag or the amount does not open.
pub fn anchor_external(enote: &Enote, s_sender_receiver: &[u8; 32]) -> Option<JanusAnchor> {
    scan_external_no_janus(enote, None, s_sender_receiver).map(|(_, anchor)| anchor)
}

/// The Janus anchor of a coinbase output, for its receiver: as [`anchor_external`].
pub fn anchor_coinbase(enote: &CoinbaseEnote, s_sender_receiver: &[u8; 32]) -> JanusAnchor {
    let input_context = make_input_context_coinbase(enote.block_index);
    let ctx =
        make_sender_receiver_secret(s_sender_receiver, &enote.ephemeral_pubkey, &input_context);
    encrypt_anchor(&enote.anchor_enc, &ctx, &enote.onetime_address)
}

/// Scans an output with everything a view-all wallet has: an internal self-send first, then an external output.
/// `lookup` maps an address spend key to the index of one of the wallet's own addresses; outputs to other addresses are
/// not the wallet's and give `None`.
pub fn scan_as_view_all(
    view_all: &ViewAll,
    enote: &Enote,
    encrypted_payment_id: Option<&[u8; 8]>,
    lookup: impl Fn(&[u8; 32]) -> Option<AddressIndex>,
) -> Option<(Received, AddressIndex)> {
    let found = scan_internal(enote, &view_all.s_view_balance).or_else(|| {
        let vr = view_all.view_received();
        let s_sr = shared_secret(&vr.k_view_incoming, &enote.ephemeral_pubkey);
        scan_external(
            enote,
            encrypted_payment_id,
            &s_sr,
            &[view_all.public.spend_pubkey],
            &vr.k_view_incoming,
        )
    })?;
    let index = lookup(&found.address_spend_pubkey)?;
    Some((found, index))
}

/// The two secrets of an output's key, `Ko = x G + y T` (Carrot 8.2): needed to spend it.
pub struct SpendKeys {
    pub x: Scalar,
    pub y: Scalar,
}

impl Drop for SpendKeys {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.x.zeroize();
        self.y.zeroize();
    }
}

/// `x = k_gi k^j_subscal + k^o_g`, `y = k_ps k^j_subscal + k^o_t`, checked against the output's key: `None` if they do
/// not open it (the output is not this account's at this index).
pub fn spend_keys(
    account: &AccountSecrets,
    index: AddressIndex,
    received: &Received,
    onetime_address: &[u8; 32],
) -> Option<SpendKeys> {
    let s = crate::account::subaddress_scalar(&account.public, &account.s_generate_address, index);
    let keys = SpendKeys {
        x: account.k_generate_image * s + received.sender_extension_g,
        y: account.k_prove_spend * s + received.sender_extension_t,
    };
    let ko = EdwardsPoint::mul_base(&keys.x) + *T * keys.y;
    (compress(&ko) == *onetime_address).then_some(keys)
}

/// The key image `x * Hp²(Ko)`.
pub fn key_image(x: &Scalar, onetime_address: &[u8; 32]) -> [u8; 32] {
    compress(&(key_image_generator(onetime_address) * x))
}

/// The key image as a view-all wallet computes it (no spend key needed): `(k_gi k^j_subscal + k^o_g) * Hp²(Ko)`.
pub fn key_image_view_all(
    view_all: &ViewAll,
    s_generate_address: &[u8; 32],
    index: AddressIndex,
    received: &Received,
    onetime_address: &[u8; 32],
) -> [u8; 32] {
    let public: &AccountPublic = &view_all.public;
    let s = crate::account::subaddress_scalar(public, s_generate_address, index);
    key_image(
        &(view_all.k_generate_image() * s + received.sender_extension_g),
        onetime_address,
    )
}
