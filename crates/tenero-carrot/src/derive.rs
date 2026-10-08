//! Every Carrot derivation, one function each, transcribed from Monero's `carrot_core` (`account_secrets.cpp`,
//! `address_utils.cpp`, `enote_utils.cpp`) and named after it. The domain separators are the specification's, byte
//! for byte. `tests/upstream_convergence.rs` checks each against Monero's own expected values.

use curve25519_dalek::edwards::EdwardsPoint;
use curve25519_dalek::scalar::Scalar;

use crate::hash::Transcript;
use crate::points::{
    compress, decompress, in_main_subgroup, scalar_mult_gt, to_x25519, x25519_mul, x25519_mul_base,
    H, T,
};
use crate::{EnoteType, InputContext, JanusAnchor, PaymentId, ViewTag};

// --- the domain separators (carrot_core/config.h) ---
const SEP_AMOUNT_BLINDING_FACTOR: &str = "Carrot commitment mask";
const SEP_ONETIME_EXTENSION_G_COINBASE: &str = "Carrot coinbase extension G";
const SEP_ONETIME_EXTENSION_T_COINBASE: &str = "Carrot coinbase extension T";
const SEP_ONETIME_EXTENSION_G: &str = "Carrot key extension G";
const SEP_ONETIME_EXTENSION_T: &str = "Carrot key extension T";
const SEP_ENCRYPTION_MASK_ANCHOR: &str = "Carrot encryption mask anchor";
const SEP_ENCRYPTION_MASK_AMOUNT: &str = "Carrot encryption mask a";
const SEP_ENCRYPTION_MASK_PAYMENT_ID: &str = "Carrot encryption mask pid";
const SEP_JANUS_ANCHOR_SPECIAL: &str = "Carrot janus anchor special";
const SEP_EPHEMERAL_PRIVKEY: &str = "Carrot sending key normal";
const SEP_VIEW_TAG: &str = "Carrot view tag";
const SEP_SENDER_RECEIVER_SECRET: &str = "Carrot sender-receiver secret";
const SEP_PROVE_SPEND_KEY: &str = "Carrot prove-spend key";
const SEP_VIEW_BALANCE_SECRET: &str = "Carrot view-balance secret";
const SEP_GENERATE_IMAGE_PREIMAGE: &str = "Carrot generate-image preimage secret";
const SEP_GENERATE_IMAGE_KEY: &str = "Carrot generate-image key";
const SEP_INCOMING_VIEW_KEY: &str = "Carrot incoming view key";
const SEP_GENERATE_ADDRESS_SECRET: &str = "Carrot generate-address secret";
const SEP_ADDRESS_INDEX_PREIMAGE_1: &str = "Carrot address index preimage 1";
const SEP_ADDRESS_INDEX_PREIMAGE_2: &str = "Carrot address index preimage 2";
const SEP_SUBADDRESS_SCALAR: &str = "Carrot subaddress scalar";
const INPUT_CONTEXT_COINBASE: u8 = b'C';
const INPUT_CONTEXT_RINGCT: u8 = b'R';

fn bytes(s: &Scalar) -> [u8; 32] {
    s.to_bytes()
}

// --- the account (account_secrets.cpp) ---

/// `k_ps = H_n[s_m]("Carrot prove-spend key")`.
pub fn make_provespend_key(s_master: &[u8; 32]) -> Scalar {
    Transcript::new(SEP_PROVE_SPEND_KEY).derive_scalar(Some(s_master))
}

/// `K_ps = k_ps * T`.
pub fn make_partial_spend_pubkey(k_prove_spend: &Scalar) -> [u8; 32] {
    compress(&(*T * k_prove_spend))
}

/// `s_vb = H_32[s_m]("Carrot view-balance secret")`.
pub fn make_viewbalance_secret(s_master: &[u8; 32]) -> [u8; 32] {
    Transcript::new(SEP_VIEW_BALANCE_SECRET).derive(Some(s_master))
}

/// `s_gp = H_32[s_vb]("Carrot generate-image preimage secret")`.
pub fn make_generateimage_preimage(s_view_balance: &[u8; 32]) -> [u8; 32] {
    Transcript::new(SEP_GENERATE_IMAGE_PREIMAGE).derive(Some(s_view_balance))
}

/// `k_gi = H_n[s_gp]("Carrot generate-image key" || K_ps)`.
pub fn make_generateimage_key(preimage: &[u8; 32], partial_spend_pubkey: &[u8; 32]) -> Scalar {
    Transcript::new(SEP_GENERATE_IMAGE_KEY)
        .bytes(partial_spend_pubkey)
        .derive_scalar(Some(preimage))
}

/// `k_v = H_n[s_vb]("Carrot incoming view key")`.
pub fn make_viewincoming_key(s_view_balance: &[u8; 32]) -> Scalar {
    Transcript::new(SEP_INCOMING_VIEW_KEY).derive_scalar(Some(s_view_balance))
}

/// `s_ga = H_32[s_vb]("Carrot generate-address secret")`.
pub fn make_generateaddress_secret(s_view_balance: &[u8; 32]) -> [u8; 32] {
    Transcript::new(SEP_GENERATE_ADDRESS_SECRET).derive(Some(s_view_balance))
}

/// `K_s = k_gi * G + k_ps * T`.
pub fn make_spend_pubkey(k_generate_image: &Scalar, k_prove_spend: &Scalar) -> [u8; 32] {
    compress(&scalar_mult_gt(k_generate_image, k_prove_spend))
}

// --- addresses (address_utils.cpp) ---

/// `s^j_gen = H_32[s_ga]("Carrot address index preimage 1" || j_major || j_minor)`.
pub fn make_address_index_preimage_1(
    s_generate_address: &[u8; 32],
    major: u32,
    minor: u32,
) -> [u8; 32] {
    Transcript::new(SEP_ADDRESS_INDEX_PREIMAGE_1)
        .u32(major)
        .u32(minor)
        .derive(Some(s_generate_address))
}

/// `s^j_2 = H_32[s^j_gen]("Carrot address index preimage 2" || j_major || j_minor || K_s || K_v)`.
pub fn make_address_index_preimage_2(
    preimage_1: &[u8; 32],
    major: u32,
    minor: u32,
    account_spend_pubkey: &[u8; 32],
    account_view_pubkey: &[u8; 32],
) -> [u8; 32] {
    Transcript::new(SEP_ADDRESS_INDEX_PREIMAGE_2)
        .u32(major)
        .u32(minor)
        .bytes(account_spend_pubkey)
        .bytes(account_view_pubkey)
        .derive(Some(preimage_1))
}

/// `k^j_subscal = H_n[s^j_2]("Carrot subaddress scalar" || K_s)`.
pub fn make_subaddress_scalar(preimage_2: &[u8; 32], account_spend_pubkey: &[u8; 32]) -> Scalar {
    Transcript::new(SEP_SUBADDRESS_SCALAR)
        .bytes(account_spend_pubkey)
        .derive_scalar(Some(preimage_2))
}

// --- enotes (enote_utils.cpp) ---

/// `d_e = H_n(anchor_norm || input_context || K^j_s || K^j_v || pid)`: unkeyed.
pub fn make_enote_ephemeral_privkey(
    anchor_norm: &JanusAnchor,
    input_context: &InputContext,
    address_spend_pubkey: &[u8; 32],
    address_view_pubkey: &[u8; 32],
    payment_id: &PaymentId,
) -> Scalar {
    Transcript::new(SEP_EPHEMERAL_PRIVKEY)
        .bytes(anchor_norm)
        .bytes(&input_context.0)
        .bytes(address_spend_pubkey)
        .bytes(address_view_pubkey)
        .bytes(payment_id)
        .derive_scalar(None)
}

/// `D_e = d_e * B` on X25519 (to a main address).
pub fn make_enote_ephemeral_pubkey_cryptonote(d_e: &Scalar) -> [u8; 32] {
    x25519_mul_base(d_e)
}

/// `D_e = ConvertPointE(d_e * K^j_s)` (to a subaddress). `None` if the spend key is not a point.
pub fn make_enote_ephemeral_pubkey_subaddress(
    d_e: &Scalar,
    address_spend_pubkey: &[u8; 32],
) -> Option<[u8; 32]> {
    Some(to_x25519(&(decompress(address_spend_pubkey)? * d_e)))
}

pub fn make_enote_ephemeral_pubkey(
    d_e: &Scalar,
    address_spend_pubkey: &[u8; 32],
    is_subaddress: bool,
) -> Option<[u8; 32]> {
    if is_subaddress {
        make_enote_ephemeral_pubkey_subaddress(d_e, address_spend_pubkey)
    } else {
        Some(make_enote_ephemeral_pubkey_cryptonote(d_e))
    }
}

/// `s_sr = k_v * D_e` (the receiver's side of the exchange).
pub fn make_shared_key_receiver(k_view: &Scalar, enote_ephemeral_pubkey: &[u8; 32]) -> [u8; 32] {
    x25519_mul(k_view, enote_ephemeral_pubkey)
}

/// `s_sr = d_e * ConvertPointE(K^j_v)` (the sender's side). `None` unless the view key is in the prime-order subgroup.
pub fn make_shared_key_sender(d_e: &Scalar, address_view_pubkey: &[u8; 32]) -> Option<[u8; 32]> {
    if !in_main_subgroup(address_view_pubkey) {
        return None;
    }
    Some(x25519_mul(
        d_e,
        &to_x25519(&decompress(address_view_pubkey)?),
    ))
}

/// `vt = H_3[s_sr](input_context || Ko)`.
pub fn make_view_tag(
    s_sender_receiver: &[u8; 32],
    input_context: &InputContext,
    onetime_address: &[u8; 32],
) -> ViewTag {
    Transcript::new(SEP_VIEW_TAG)
        .bytes(&input_context.0)
        .bytes(onetime_address)
        .derive(Some(s_sender_receiver))
}

/// `"C" || block_index (8 bytes, little endian) || 24 zero bytes`.
pub fn make_input_context_coinbase(block_index: u64) -> InputContext {
    let mut c = [0u8; 33];
    c[0] = INPUT_CONTEXT_COINBASE;
    c[1..9].copy_from_slice(&block_index.to_le_bytes());
    InputContext(c)
}

/// `"R" || the transaction's first key image`.
pub fn make_input_context(first_key_image: &[u8; 32]) -> InputContext {
    let mut c = [0u8; 33];
    c[0] = INPUT_CONTEXT_RINGCT;
    c[1..].copy_from_slice(first_key_image);
    InputContext(c)
}

/// `s^ctx_sr = H_32[s_sr](D_e || input_context)`.
pub fn make_sender_receiver_secret(
    s_sender_receiver: &[u8; 32],
    enote_ephemeral_pubkey: &[u8; 32],
    input_context: &InputContext,
) -> [u8; 32] {
    Transcript::new(SEP_SENDER_RECEIVER_SECRET)
        .bytes(enote_ephemeral_pubkey)
        .bytes(&input_context.0)
        .derive(Some(s_sender_receiver))
}

pub fn make_sender_extension_g_coinbase(
    ctx: &[u8; 32],
    amount: u64,
    main_spend: &[u8; 32],
) -> Scalar {
    Transcript::new(SEP_ONETIME_EXTENSION_G_COINBASE)
        .u64(amount)
        .bytes(main_spend)
        .derive_scalar(Some(ctx))
}

pub fn make_sender_extension_t_coinbase(
    ctx: &[u8; 32],
    amount: u64,
    main_spend: &[u8; 32],
) -> Scalar {
    Transcript::new(SEP_ONETIME_EXTENSION_T_COINBASE)
        .u64(amount)
        .bytes(main_spend)
        .derive_scalar(Some(ctx))
}

pub fn make_sender_extension_g(ctx: &[u8; 32], amount_commitment: &[u8; 32]) -> Scalar {
    Transcript::new(SEP_ONETIME_EXTENSION_G)
        .bytes(amount_commitment)
        .derive_scalar(Some(ctx))
}

pub fn make_sender_extension_t(ctx: &[u8; 32], amount_commitment: &[u8; 32]) -> Scalar {
    Transcript::new(SEP_ONETIME_EXTENSION_T)
        .bytes(amount_commitment)
        .derive_scalar(Some(ctx))
}

fn add_to(spend: &[u8; 32], ext: EdwardsPoint) -> Option<[u8; 32]> {
    Some(compress(&(decompress(spend)? + ext)))
}

/// `Ko = K^0_s + k^o_g G + k^o_t T` with the coinbase extensions. `None` if the spend key is not a point.
pub fn make_onetime_address_coinbase(
    main_spend: &[u8; 32],
    ctx: &[u8; 32],
    amount: u64,
) -> Option<[u8; 32]> {
    let g = make_sender_extension_g_coinbase(ctx, amount, main_spend);
    let t = make_sender_extension_t_coinbase(ctx, amount, main_spend);
    add_to(main_spend, scalar_mult_gt(&g, &t))
}

/// `Ko = K^j_s + k^o_g G + k^o_t T`. `None` if the spend key is not a point.
pub fn make_onetime_address(
    address_spend: &[u8; 32],
    ctx: &[u8; 32],
    amount_commitment: &[u8; 32],
) -> Option<[u8; 32]> {
    let g = make_sender_extension_g(ctx, amount_commitment);
    let t = make_sender_extension_t(ctx, amount_commitment);
    add_to(address_spend, scalar_mult_gt(&g, &t))
}

/// `k_a = H_n[s^ctx_sr](a || K^j_s || enote_type)`.
pub fn make_amount_blinding_factor(
    ctx: &[u8; 32],
    amount: u64,
    address_spend: &[u8; 32],
    enote_type: EnoteType,
) -> Scalar {
    Transcript::new(SEP_AMOUNT_BLINDING_FACTOR)
        .u64(amount)
        .bytes(address_spend)
        .u8(enote_type as u8)
        .derive_scalar(Some(ctx))
}

/// `C_a = k_a G + a H`.
pub fn commit_amount(amount: u64, blinding_factor: &Scalar) -> [u8; 32] {
    compress(&(EdwardsPoint::mul_base(blinding_factor) + *H * Scalar::from(amount)))
}

/// The commitment of a coinbase output: `1*G + a*H` (its blinding factor is one).
pub fn commit_coinbase_amount(amount: u64) -> [u8; 32] {
    commit_amount(amount, &Scalar::ONE)
}

pub fn make_anchor_encryption_mask(ctx: &[u8; 32], onetime_address: &[u8; 32]) -> JanusAnchor {
    Transcript::new(SEP_ENCRYPTION_MASK_ANCHOR)
        .bytes(onetime_address)
        .derive(Some(ctx))
}

pub fn make_amount_encryption_mask(ctx: &[u8; 32], onetime_address: &[u8; 32]) -> [u8; 8] {
    Transcript::new(SEP_ENCRYPTION_MASK_AMOUNT)
        .bytes(onetime_address)
        .derive(Some(ctx))
}

pub fn make_payment_id_encryption_mask(ctx: &[u8; 32], onetime_address: &[u8; 32]) -> [u8; 8] {
    Transcript::new(SEP_ENCRYPTION_MASK_PAYMENT_ID)
        .bytes(onetime_address)
        .derive(Some(ctx))
}

fn xor<const N: usize>(a: &[u8; N], b: &[u8; N]) -> [u8; N] {
    let mut out = [0u8; N];
    for i in 0..N {
        out[i] = a[i] ^ b[i];
    }
    out
}

/// Encrypting and decrypting are the same XOR.
pub fn encrypt_anchor(
    anchor: &JanusAnchor,
    ctx: &[u8; 32],
    onetime_address: &[u8; 32],
) -> JanusAnchor {
    xor(anchor, &make_anchor_encryption_mask(ctx, onetime_address))
}

/// The amount as 8 bytes little endian, XOR the mask.
pub fn encrypt_amount(amount: u64, ctx: &[u8; 32], onetime_address: &[u8; 32]) -> [u8; 8] {
    xor(
        &amount.to_le_bytes(),
        &make_amount_encryption_mask(ctx, onetime_address),
    )
}

pub fn decrypt_amount(amount_enc: &[u8; 8], ctx: &[u8; 32], onetime_address: &[u8; 32]) -> u64 {
    u64::from_le_bytes(xor(
        amount_enc,
        &make_amount_encryption_mask(ctx, onetime_address),
    ))
}

pub fn encrypt_payment_id(
    payment_id: &PaymentId,
    ctx: &[u8; 32],
    onetime_address: &[u8; 32],
) -> [u8; 8] {
    xor(
        payment_id,
        &make_payment_id_encryption_mask(ctx, onetime_address),
    )
}

/// `anchor_sp = H_16[k_v](D_e || input_context || Ko)`.
pub fn make_janus_anchor_special(
    enote_ephemeral_pubkey: &[u8; 32],
    input_context: &InputContext,
    onetime_address: &[u8; 32],
    k_view: &Scalar,
) -> JanusAnchor {
    Transcript::new(SEP_JANUS_ANCHOR_SPECIAL)
        .bytes(enote_ephemeral_pubkey)
        .bytes(&input_context.0)
        .bytes(onetime_address)
        .derive(Some(&bytes(k_view)))
}

/// `K^j_s = Ko - (k^o_g G + k^o_t T)`. `None` if `Ko` is not a point.
pub fn recover_address_spend_pubkey(
    onetime_address: &[u8; 32],
    ext_g: &Scalar,
    ext_t: &Scalar,
) -> Option<[u8; 32]> {
    Some(compress(
        &(decompress(onetime_address)? - scalar_mult_gt(ext_g, ext_t)),
    ))
}

/// The key-image generator of an output: Carrot's unbiased `Hp²(Ko)` (monero-oxide's `Point::hash`). `gamma` has only
/// Carrot outputs, so the biased hash of older Monero outputs never applies.
pub fn key_image_generator(onetime_address: &[u8; 32]) -> EdwardsPoint {
    monero_ed25519::Point::hash(*onetime_address).into()
}
