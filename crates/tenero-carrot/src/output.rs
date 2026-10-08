//! Making outputs (Carrot 7; Monero's `payment_proposal.cpp` and `output_set_finalization.cpp`).
//!
//! * A **payment** ([`PaymentProposal`]) goes to someone's address; its ephemeral key is derived from fresh randomness,
//!   the input context and the address, which is what lets the receiver check it (the Janus protection).
//! * A **self-send** ([`SelfSendProposal`]) comes back to one's own wallet: change, or a payment to oneself. It is made
//!   either *internal* (with the view-balance secret `s_vb`: no exchange at all, invisible to a view-received wallet) or
//!   *special* (with the incoming view key `k_v`, marked with the special Janus anchor).
//! * Every transaction has at least two outputs and at least one self-send; a 2-output transaction's outputs share one
//!   ephemeral key; a larger one's are all different; the outputs are sorted by one-time address.

use curve25519_dalek::edwards::EdwardsPoint;
use curve25519_dalek::scalar::Scalar;
use rand_core::{CryptoRng, RngCore};

use crate::account::Destination;
use crate::derive::*;
use crate::points::in_main_subgroup;
use crate::{
    CarrotError, CoinbaseEnote, Enote, EnoteType, InputContext, JanusAnchor, PaymentId,
    NULL_PAYMENT_ID,
};

const NULL_ANCHOR: JanusAnchor = [0; 16];

/// A payment to an address. `randomness` is `anchor_norm`: 16 fresh random bytes, never zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaymentProposal {
    pub destination: Destination,
    pub amount: u64,
    pub randomness: JanusAnchor,
}

impl PaymentProposal {
    pub fn new(
        destination: Destination,
        amount: u64,
        rng: &mut (impl RngCore + CryptoRng),
    ) -> PaymentProposal {
        PaymentProposal {
            destination,
            amount,
            randomness: random_anchor(rng),
        }
    }
}

/// An output back to one's own wallet. `ephemeral_privkey` is needed only in a transaction of more than two outputs
/// (each output's own key); `internal_message` only for an internal self-send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelfSendProposal {
    pub destination_spend_pubkey: [u8; 32],
    pub is_subaddress: bool,
    pub amount: u64,
    pub enote_type: EnoteType,
    pub ephemeral_privkey: Option<Scalar>,
    pub internal_message: Option<JanusAnchor>,
}

/// An output ready for a transaction, with what the sender must keep to build the range proof and the balance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputProposal {
    pub enote: Enote,
    pub amount: u64,
    pub blinding_factor: Scalar,
}

/// Which secret makes the self-sends.
#[derive(Clone, Copy)]
pub enum SelfSendKey<'a> {
    /// `s_vb`: internal self-sends.
    ViewBalance(&'a [u8; 32]),
    /// `k_v`: special self-sends.
    ViewIncoming(&'a Scalar),
}

pub fn random_anchor(rng: &mut (impl RngCore + CryptoRng)) -> JanusAnchor {
    loop {
        let mut a = [0u8; 16];
        rng.fill_bytes(&mut a);
        if a != NULL_ANCHOR {
            return a;
        }
    }
}

fn random_point(rng: &mut (impl RngCore + CryptoRng)) -> [u8; 32] {
    let mut wide = [0u8; 64];
    rng.fill_bytes(&mut wide);
    EdwardsPoint::mul_base(&Scalar::from_bytes_mod_order_wide(&wide))
        .compress()
        .to_bytes()
}

/// A random main address with no owner (`gen_carrot_main_address_v1`), for a dummy output.
pub fn random_main_address(rng: &mut (impl RngCore + CryptoRng)) -> Destination {
    Destination {
        spend_pubkey: random_point(rng),
        view_pubkey: random_point(rng),
        is_subaddress: false,
        payment_id: NULL_PAYMENT_ID,
    }
}

/// `d_e` of a payment.
pub fn ephemeral_privkey(p: &PaymentProposal, input_context: &InputContext) -> Scalar {
    make_enote_ephemeral_privkey(
        &p.randomness,
        input_context,
        &p.destination.spend_pubkey,
        &p.destination.view_pubkey,
        &p.destination.payment_id,
    )
}

/// `D_e` of a payment.
pub fn ephemeral_pubkey(
    p: &PaymentProposal,
    input_context: &InputContext,
) -> Result<[u8; 32], CarrotError> {
    make_enote_ephemeral_pubkey(
        &ephemeral_privkey(p, input_context),
        &p.destination.spend_pubkey,
        p.destination.is_subaddress,
    )
    .ok_or(CarrotError::InvalidPoint)
}

/// The fields every output has, from its contextualised secret (`get_output_proposal_parts`). A coinbase output has
/// blinding factor one and the coinbase one-time address.
struct Parts {
    blinding_factor: Scalar,
    amount_commitment: [u8; 32],
    onetime_address: [u8; 32],
    amount_enc: [u8; 8],
    payment_id_enc: [u8; 8],
}

fn parts(
    ctx: &[u8; 32],
    spend: &[u8; 32],
    payment_id: &PaymentId,
    amount: u64,
    enote_type: EnoteType,
    coinbase: bool,
) -> Result<Parts, CarrotError> {
    let blinding_factor = if coinbase {
        Scalar::ONE
    } else {
        make_amount_blinding_factor(ctx, amount, spend, enote_type)
    };
    let amount_commitment = commit_amount(amount, &blinding_factor);
    let onetime_address = if coinbase {
        make_onetime_address_coinbase(spend, ctx, amount)
    } else {
        make_onetime_address(spend, ctx, &amount_commitment)
    }
    .ok_or(CarrotError::InvalidPoint)?;
    Ok(Parts {
        blinding_factor,
        amount_commitment,
        amount_enc: encrypt_amount(amount, ctx, &onetime_address),
        payment_id_enc: encrypt_payment_id(payment_id, ctx, &onetime_address),
        onetime_address,
    })
}

/// A payment's ephemeral key and the shared secret, from the sender's side.
fn normal_ecdh(
    p: &PaymentProposal,
    input_context: &InputContext,
) -> Result<([u8; 32], [u8; 32]), CarrotError> {
    let d_e = ephemeral_privkey(p, input_context);
    let d_pub = ephemeral_pubkey(p, input_context)?;
    let s_sr = make_shared_key_sender(&d_e, &p.destination.view_pubkey)
        .ok_or(CarrotError::InvalidPoint)?;
    Ok((d_pub, s_sr))
}

/// A coinbase output to a main address (`get_coinbase_enote_v1`). Subaddresses and integrated addresses are refused.
pub fn coinbase_enote(p: &PaymentProposal, block_index: u64) -> Result<CoinbaseEnote, CarrotError> {
    if p.randomness == NULL_ANCHOR {
        return Err(CarrotError::MissingRandomness("the Janus anchor is zero"));
    }
    if p.destination.is_subaddress {
        return Err(CarrotError::BadAddressType(
            "a coinbase output cannot pay a subaddress",
        ));
    }
    if p.destination.payment_id != NULL_PAYMENT_ID {
        return Err(CarrotError::BadAddressType(
            "a coinbase output cannot pay an integrated address",
        ));
    }
    let input_context = make_input_context_coinbase(block_index);
    let (d_pub, s_sr) = normal_ecdh(p, &input_context)?;
    let ctx = make_sender_receiver_secret(&s_sr, &d_pub, &input_context);
    let parts = parts(
        &ctx,
        &p.destination.spend_pubkey,
        &NULL_PAYMENT_ID,
        p.amount,
        EnoteType::Payment,
        true,
    )?;
    Ok(CoinbaseEnote {
        onetime_address: parts.onetime_address,
        amount: p.amount,
        view_tag: make_view_tag(&s_sr, &input_context, &parts.onetime_address),
        ephemeral_pubkey: d_pub,
        anchor_enc: encrypt_anchor(&p.randomness, &ctx, &parts.onetime_address),
        block_index,
    })
}

/// A payment output (`get_output_proposal_normal_v1`), and its encrypted payment ID.
pub fn output_normal(
    p: &PaymentProposal,
    first_key_image: &[u8; 32],
) -> Result<(OutputProposal, [u8; 8]), CarrotError> {
    if p.randomness == NULL_ANCHOR {
        return Err(CarrotError::MissingRandomness("the Janus anchor is zero"));
    }
    let input_context = make_input_context(first_key_image);
    let (d_pub, s_sr) = normal_ecdh(p, &input_context)?;
    let ctx = make_sender_receiver_secret(&s_sr, &d_pub, &input_context);
    let parts = parts(
        &ctx,
        &p.destination.spend_pubkey,
        &p.destination.payment_id,
        p.amount,
        EnoteType::Payment,
        false,
    )?;
    let enote = Enote {
        onetime_address: parts.onetime_address,
        amount_commitment: parts.amount_commitment,
        amount_enc: parts.amount_enc,
        view_tag: make_view_tag(&s_sr, &input_context, &parts.onetime_address),
        ephemeral_pubkey: d_pub,
        anchor_enc: encrypt_anchor(&p.randomness, &ctx, &parts.onetime_address),
        tx_first_key_image: *first_key_image,
    };
    Ok((
        OutputProposal {
            enote,
            amount: p.amount,
            blinding_factor: parts.blinding_factor,
        },
        parts.payment_id_enc,
    ))
}

/// The other output of a 2-output transaction, when there is one.
#[derive(Clone, Copy)]
pub enum Other<'a> {
    None,
    Normal(&'a PaymentProposal),
    SelfSend(&'a SelfSendProposal),
}

/// `d_e` of a self-send: a 2-output transaction's other payment's, or the one given (`get_enote_ephemeral_privkey`).
fn selfsend_ephemeral_privkey(
    p: &SelfSendProposal,
    other: Other<'_>,
    first_key_image: &[u8; 32],
) -> Result<Scalar, CarrotError> {
    match other {
        Other::Normal(n) => {
            if p.ephemeral_privkey.is_some() {
                return Err(CarrotError::BadOutputSet(
                    "a self-send beside a payment in a 2-output transaction may not have its own ephemeral key",
                ));
            }
            Ok(ephemeral_privkey(n, &make_input_context(first_key_image)))
        }
        Other::SelfSend(o) => match (p.ephemeral_privkey, o.ephemeral_privkey) {
            (None, None) => Err(CarrotError::MissingRandomness("no ephemeral key given")),
            (Some(a), Some(b)) if a != b => {
                Err(CarrotError::BadOutputSet("conflicting ephemeral keys"))
            }
            (_, Some(b)) => Ok(b),
            (Some(a), None) => Ok(a),
        },
        Other::None => p
            .ephemeral_privkey
            .ok_or(CarrotError::MissingRandomness("no ephemeral key given")),
    }
}

/// `D_e` of a self-send (`get_enote_ephemeral_pubkey`). In a 2-output transaction of two self-sends, it is made towards
/// the one of type `Payment`.
pub fn selfsend_ephemeral_pubkey(
    p: &SelfSendProposal,
    other: Other<'_>,
    first_key_image: &[u8; 32],
) -> Result<[u8; 32], CarrotError> {
    let (base_spend, base_is_sub) = match other {
        Other::Normal(n) => {
            return ephemeral_pubkey(n, &make_input_context(first_key_image)).and_then(|d| {
                // as C++: a self-send's own key, if it has one, must be the payment's
                match p.ephemeral_privkey {
                    Some(k) if k != ephemeral_privkey(n, &make_input_context(first_key_image)) => {
                        Err(CarrotError::BadOutputSet(
                            "a self-send's ephemeral key conflicts with the payment's",
                        ))
                    }
                    _ => Ok(d),
                }
            });
        }
        Other::None => (p.destination_spend_pubkey, p.is_subaddress),
        Other::SelfSend(o) => {
            if o.enote_type == p.enote_type {
                return Err(CarrotError::BadOutputSet(
                    "two self-sends in a 2-output transaction must be of different types",
                ));
            } else if o.enote_type == EnoteType::Payment {
                (o.destination_spend_pubkey, o.is_subaddress)
            } else {
                (p.destination_spend_pubkey, p.is_subaddress)
            }
        }
    };
    let d_e = selfsend_ephemeral_privkey(p, other, first_key_image)?;
    make_enote_ephemeral_pubkey(&d_e, &base_spend, base_is_sub).ok_or(CarrotError::InvalidPoint)
}

/// A special self-send (`get_output_proposal_special_v1`): the exchange is done with `k_v`, and the anchor is the special
/// Janus anchor.
pub fn output_special(
    p: &SelfSendProposal,
    k_view: &Scalar,
    first_key_image: &[u8; 32],
    ephemeral_pubkey: &[u8; 32],
) -> Result<OutputProposal, CarrotError> {
    if p.internal_message.is_some() {
        return Err(CarrotError::BadOutputSet(
            "an internal message is only for an internal self-send",
        ));
    }
    let input_context = make_input_context(first_key_image);
    let s_sr = make_shared_key_receiver(k_view, ephemeral_pubkey);
    let ctx = make_sender_receiver_secret(&s_sr, ephemeral_pubkey, &input_context);
    let parts = parts(
        &ctx,
        &p.destination_spend_pubkey,
        &NULL_PAYMENT_ID,
        p.amount,
        p.enote_type,
        false,
    )?;
    let anchor = make_janus_anchor_special(
        ephemeral_pubkey,
        &input_context,
        &parts.onetime_address,
        k_view,
    );
    Ok(OutputProposal {
        enote: Enote {
            onetime_address: parts.onetime_address,
            amount_commitment: parts.amount_commitment,
            amount_enc: parts.amount_enc,
            view_tag: make_view_tag(&s_sr, &input_context, &parts.onetime_address),
            ephemeral_pubkey: *ephemeral_pubkey,
            anchor_enc: encrypt_anchor(&anchor, &ctx, &parts.onetime_address),
            tx_first_key_image: *first_key_image,
        },
        amount: p.amount,
        blinding_factor: parts.blinding_factor,
    })
}

/// An internal self-send (`get_output_proposal_internal_v1`): `s_vb` takes the place of the shared secret.
pub fn output_internal(
    p: &SelfSendProposal,
    s_view_balance: &[u8; 32],
    first_key_image: &[u8; 32],
    ephemeral_pubkey: &[u8; 32],
) -> Result<OutputProposal, CarrotError> {
    let input_context = make_input_context(first_key_image);
    let ctx = make_sender_receiver_secret(s_view_balance, ephemeral_pubkey, &input_context);
    let parts = parts(
        &ctx,
        &p.destination_spend_pubkey,
        &NULL_PAYMENT_ID,
        p.amount,
        p.enote_type,
        false,
    )?;
    let anchor = p.internal_message.unwrap_or(NULL_ANCHOR);
    Ok(OutputProposal {
        enote: Enote {
            onetime_address: parts.onetime_address,
            amount_commitment: parts.amount_commitment,
            amount_enc: parts.amount_enc,
            view_tag: make_view_tag(s_view_balance, &input_context, &parts.onetime_address),
            ephemeral_pubkey: *ephemeral_pubkey,
            anchor_enc: encrypt_anchor(&anchor, &ctx, &parts.onetime_address),
            tx_first_key_image: *first_key_image,
        },
        amount: p.amount,
        blinding_factor: parts.blinding_factor,
    })
}

/// What a transaction's output set is missing (`get_additional_output_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdditionalOutputType {
    /// A self-send of type `Payment` sharing the ephemeral key.
    PaymentShared,
    /// Change sharing the ephemeral key (a 2-output transaction).
    ChangeShared,
    /// Change with its own ephemeral key (more than two outputs).
    ChangeUnique,
    /// A zero-amount payment to nobody, to reach two outputs.
    Dummy,
}

pub fn additional_output_type(
    num_outgoing: usize,
    num_selfsend: usize,
    need_change_output: bool,
    have_payment_type_selfsend: bool,
) -> Result<Option<AdditionalOutputType>, CarrotError> {
    let num_outputs = num_outgoing + num_selfsend;
    let already_completed = num_outputs >= 2 && num_selfsend >= 1 && !need_change_output;
    Ok(if num_outputs == 0 {
        return Err(CarrotError::BadOutputSet("the set has no outputs"));
    } else if already_completed {
        None
    } else if num_outputs == 1 {
        if num_selfsend == 0 {
            Some(AdditionalOutputType::ChangeShared)
        } else if !need_change_output {
            Some(AdditionalOutputType::Dummy)
        } else if have_payment_type_selfsend {
            Some(AdditionalOutputType::ChangeShared)
        } else {
            Some(AdditionalOutputType::PaymentShared)
        }
    } else {
        Some(AdditionalOutputType::ChangeUnique)
    })
}

/// The output to add, if any (`get_additional_payment_proposal`). A unique change output gets a fresh ephemeral key here
/// (in C++ the caller supplies it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Additional {
    Payment(PaymentProposal),
    SelfSend(SelfSendProposal),
}

pub fn additional_payment_proposal(
    num_outgoing: usize,
    num_selfsend: usize,
    needed_change_amount: u64,
    have_payment_type_selfsend: bool,
    change_spend_pubkey: &[u8; 32],
    change_is_subaddress: bool,
    rng: &mut (impl RngCore + CryptoRng),
) -> Result<Option<Additional>, CarrotError> {
    let kind = additional_output_type(
        num_outgoing,
        num_selfsend,
        needed_change_amount != 0,
        have_payment_type_selfsend,
    )?;
    let selfsend = |enote_type, ephemeral_privkey| {
        Additional::SelfSend(SelfSendProposal {
            destination_spend_pubkey: *change_spend_pubkey,
            is_subaddress: change_is_subaddress,
            amount: needed_change_amount,
            enote_type,
            ephemeral_privkey,
            internal_message: None,
        })
    };
    Ok(kind.map(|k| match k {
        AdditionalOutputType::PaymentShared => selfsend(EnoteType::Payment, None),
        AdditionalOutputType::ChangeShared => selfsend(EnoteType::Change, None),
        AdditionalOutputType::ChangeUnique => {
            let mut wide = [0u8; 64];
            rng.fill_bytes(&mut wide);
            selfsend(
                EnoteType::Change,
                Some(Scalar::from_bytes_mod_order_wide(&wide)),
            )
        }
        AdditionalOutputType::Dummy => Additional::Payment(PaymentProposal {
            destination: random_main_address(rng),
            amount: 0,
            randomness: random_anchor(rng),
        }),
    }))
}

/// Where each output of a finished set came from: `(is a self-send, its index in its list)`.
pub type ProposalOrder = Vec<(bool, usize)>;

/// A transaction's whole output set (`get_output_enote_proposals`): the outputs sorted by one-time address, the
/// transaction's encrypted payment ID (the integrated address's, or `dummy_encrypted_payment_id`), and where each output
/// came from. Every rule of the C++ function is checked.
pub fn output_set(
    normal: &[PaymentProposal],
    selfsend: &[SelfSendProposal],
    dummy_encrypted_payment_id: Option<[u8; 8]>,
    key: SelfSendKey<'_>,
    first_key_image: &[u8; 32],
) -> Result<(Vec<OutputProposal>, [u8; 8], ProposalOrder), CarrotError> {
    let n = normal.len() + selfsend.len();
    if n < 2 {
        return Err(CarrotError::BadOutputSet("fewer than two outputs"));
    }
    if selfsend.is_empty() {
        return Err(CarrotError::BadOutputSet("no self-send"));
    }
    let integrated = normal
        .iter()
        .filter(|p| p.destination.payment_id != NULL_PAYMENT_ID)
        .count();
    if integrated > 1 {
        return Err(CarrotError::BadAddressType(
            "more than one integrated address in one transaction",
        ));
    }
    if normal.iter().any(|p| p.randomness == NULL_ANCHOR) {
        return Err(CarrotError::MissingRandomness(
            "a payment has a zero Janus anchor",
        ));
    }
    let mut anchors: Vec<_> = normal.iter().map(|p| p.randomness).collect();
    anchors.sort_unstable();
    anchors.dedup();
    if anchors.len() != normal.len() {
        return Err(CarrotError::MissingRandomness(
            "two payments have the same Janus anchor",
        ));
    }

    let mut entries: Vec<(OutputProposal, (bool, usize))> = Vec::with_capacity(n);
    let mut encrypted_payment_id = [0u8; 8];
    for (i, p) in normal.iter().enumerate() {
        let (o, pid_enc) = output_normal(p, first_key_image)?;
        if p.destination.payment_id != NULL_PAYMENT_ID {
            encrypted_payment_id = pid_enc;
        }
        entries.push((o, (false, i)));
    }
    if integrated == 0 {
        encrypted_payment_id = dummy_encrypted_payment_id.ok_or(CarrotError::BadOutputSet(
            "no integrated address and no dummy encrypted payment ID",
        ))?;
    }

    let two_out_normal = n == 2 && normal.len() == 1;
    let two_out_selfsend = n == 2 && selfsend.len() == 2;
    for (i, p) in selfsend.iter().enumerate() {
        let other = if two_out_normal {
            Other::Normal(&normal[0])
        } else if two_out_selfsend {
            Other::SelfSend(&selfsend[1 - i])
        } else {
            Other::None
        };
        let d_pub = selfsend_ephemeral_pubkey(p, other, first_key_image)?;
        let o = match key {
            SelfSendKey::ViewBalance(s_vb) => output_internal(p, s_vb, first_key_image, &d_pub)?,
            SelfSendKey::ViewIncoming(k_v) => output_special(p, k_v, first_key_image, &d_pub)?,
        };
        entries.push((o, (true, i)));
    }

    entries.sort_by_key(|e| e.0.enote.onetime_address);

    if entries
        .iter()
        .any(|(o, _)| o.enote.ephemeral_pubkey == [0u8; 32])
    {
        return Err(CarrotError::MissingRandomness(
            "an ephemeral key with u = 0",
        ));
    }
    let mut keys: Vec<_> = entries
        .iter()
        .map(|(o, _)| o.enote.ephemeral_pubkey)
        .collect();
    keys.sort_unstable();
    keys.dedup();
    let unique_keys = keys.len() == n;
    if n == 2 && unique_keys {
        return Err(CarrotError::BadOutputSet(
            "a 2-output set must share its ephemeral key",
        ));
    }
    if n != 2 && !unique_keys {
        return Err(CarrotError::MissingRandomness(
            "a set of more than two outputs repeats an ephemeral key",
        ));
    }
    if entries
        .windows(2)
        .any(|w| w[0].0.enote.onetime_address == w[1].0.enote.onetime_address)
    {
        return Err(CarrotError::BadOutputSet(
            "two outputs have the same one-time address",
        ));
    }
    if entries
        .iter()
        .any(|(o, _)| !in_main_subgroup(&o.enote.onetime_address))
    {
        return Err(CarrotError::InvalidPoint);
    }
    if entries
        .iter()
        .any(|(o, _)| o.blinding_factor == Scalar::ZERO)
    {
        return Err(CarrotError::MissingRandomness("a zero blinding factor"));
    }
    let mut factors: Vec<_> = entries
        .iter()
        .map(|(o, _)| o.blinding_factor.to_bytes())
        .collect();
    factors.sort_unstable();
    factors.dedup();
    if factors.len() != n {
        return Err(CarrotError::MissingRandomness(
            "two outputs have the same blinding factor",
        ));
    }

    let order = entries.iter().map(|(_, w)| *w).collect();
    Ok((
        entries.into_iter().map(|(o, _)| o).collect(),
        encrypted_payment_id,
        order,
    ))
}

/// A block's coinbase outputs (`get_coinbase_output_enotes`): main addresses only, fresh anchors, unique ephemeral keys,
/// sorted by one-time address.
pub fn coinbase_enotes(
    normal: &[PaymentProposal],
    block_index: u64,
) -> Result<Vec<CoinbaseEnote>, CarrotError> {
    if normal
        .iter()
        .any(|p| p.destination.payment_id != NULL_PAYMENT_ID || p.destination.is_subaddress)
    {
        return Err(CarrotError::BadAddressType(
            "coinbase outputs pay main addresses only (no subaddresses, no integrated addresses)",
        ));
    }
    if normal.iter().any(|p| p.randomness == NULL_ANCHOR) {
        return Err(CarrotError::MissingRandomness(
            "a payment has a zero Janus anchor",
        ));
    }
    let mut anchors: Vec<_> = normal.iter().map(|p| p.randomness).collect();
    anchors.sort_unstable();
    anchors.dedup();
    if anchors.len() != normal.len() {
        return Err(CarrotError::MissingRandomness(
            "two payments have the same Janus anchor",
        ));
    }
    let mut out = normal
        .iter()
        .map(|p| coinbase_enote(p, block_index))
        .collect::<Result<Vec<_>, _>>()?;
    if out.iter().any(|e| e.ephemeral_pubkey == [0u8; 32]) {
        return Err(CarrotError::MissingRandomness(
            "an ephemeral key with u = 0",
        ));
    }
    let mut keys: Vec<_> = out.iter().map(|e| e.ephemeral_pubkey).collect();
    keys.sort_unstable();
    keys.dedup();
    if keys.len() != out.len() {
        return Err(CarrotError::MissingRandomness(
            "coinbase outputs repeat an ephemeral key",
        ));
    }
    out.sort_by_key(|e| e.onetime_address);
    if out
        .windows(2)
        .any(|w| w[0].onetime_address == w[1].onetime_address)
    {
        return Err(CarrotError::BadOutputSet(
            "two outputs have the same one-time address",
        ));
    }
    if out.iter().any(|e| !in_main_subgroup(&e.onetime_address)) {
        return Err(CarrotError::InvalidPoint);
    }
    Ok(out)
}
