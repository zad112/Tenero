//! Points, generators and the Edwards/Montgomery conversions Carrot uses.
//!
//! Decoding is monero-oxide's `CompressedPoint::decompress`, which refuses non-canonical encodings and "negative
//! zero". Monero's C++ `ge_frombytes_vartime` is more lenient there; for every point an honest party makes the two
//! agree, and on `gamma` consensus already refuses non-canonical output points (`strict_point`). For malformed
//! addresses this implementation is deliberately the stricter one.

use std::sync::LazyLock;

use curve25519_dalek::edwards::EdwardsPoint;
use curve25519_dalek::montgomery::MontgomeryPoint;
use curve25519_dalek::scalar::Scalar;
use curve25519_dalek::traits::IsIdentity;
use monero_ed25519::CompressedPoint;

/// Monero's second generator for amounts, `H`.
pub static H: LazyLock<EdwardsPoint> = LazyLock::new(|| {
    CompressedPoint::H
        .decompress()
        .expect("H is a point")
        .into()
});

/// The FCMP++ generator `T` (an output key is `x*G + y*T`).
pub static T: LazyLock<EdwardsPoint> = LazyLock::new(|| {
    CompressedPoint::T
        .decompress()
        .expect("T is a point")
        .into()
});

/// Decodes a canonically encoded Ed25519 point (any of the `8*l` points; the subgroup is not checked).
pub fn decompress(bytes: &[u8; 32]) -> Option<EdwardsPoint> {
    CompressedPoint::from(*bytes).decompress().map(|p| p.into())
}

pub fn compress(p: &EdwardsPoint) -> [u8; 32] {
    p.compress().to_bytes()
}

/// `verify_point_is_in_main_subgroup`: decodes, and `l*P` is the identity (so the identity itself passes, as in C++).
pub fn in_main_subgroup(bytes: &[u8; 32]) -> bool {
    decompress(bytes).is_some_and(|p| p.is_torsion_free())
}

/// `x*G + y*T`.
pub fn scalar_mult_gt(x: &Scalar, y: &Scalar) -> EdwardsPoint {
    EdwardsPoint::mul_base(x) + *T * y
}

/// An X25519 public key (the Montgomery `u` coordinate) of an Edwards point.
pub fn to_x25519(p: &EdwardsPoint) -> [u8; 32] {
    p.to_montgomery().to_bytes()
}

/// Unclamped X25519 multiplication `s * U` (`mx25519_scmul_key_unclamped`). The scalar is used as it is, not clamped
/// as RFC 7748's X25519 does: Carrot needs the plain product.
pub fn x25519_mul(s: &Scalar, u: &[u8; 32]) -> [u8; 32] {
    (MontgomeryPoint(*u) * s).to_bytes()
}

/// Unclamped X25519 multiplication by the base point (`mx25519_scmul_base_unclamped`).
pub fn x25519_mul_base(s: &Scalar) -> [u8; 32] {
    EdwardsPoint::mul_base(s).to_montgomery().to_bytes()
}

pub fn is_identity(p: &EdwardsPoint) -> bool {
    p.is_identity()
}
