//! A 256-bit unsigned integer, only what targets need so far: parse, compare, powers of two.
//! (Multiplication and division arrive with the difficulty adjustment, together with the vectors
//! that test them.)

use std::cmp::Ordering;

/// Four little-endian 64-bit limbs.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct U256([u64; 4]);

impl U256 {
    pub const ZERO: U256 = U256([0; 4]);

    /// From 32 big-endian bytes (how a hash is compared with a target).
    pub fn from_be_bytes(b: &[u8; 32]) -> U256 {
        let mut limbs = [0u64; 4];
        for (i, limb) in limbs.iter_mut().enumerate() {
            let at = 24 - 8 * i;
            let mut eight = [0u8; 8];
            eight.copy_from_slice(&b[at..at + 8]);
            *limb = u64::from_be_bytes(eight);
        }
        U256(limbs)
    }

    /// From decimal digits only (no sign, no spaces, not empty). `None` on anything else or when
    /// the value does not fit in 256 bits.
    pub fn from_dec_str(s: &str) -> Option<U256> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let mut limbs = [0u64; 4];
        for digit in s.bytes() {
            let mut carry = u128::from(digit - b'0');
            for limb in limbs.iter_mut() {
                let t = u128::from(*limb) * 10 + carry;
                *limb = t as u64; // the low 64 bits; the high bits carry on
                carry = t >> 64;
            }
            if carry != 0 {
                return None;
            }
        }
        Some(U256(limbs))
    }

    /// 2^bit, or `None` when `bit >= 256`.
    pub fn pow2(bit: u32) -> Option<U256> {
        if bit >= 256 {
            return None;
        }
        let mut limbs = [0u64; 4];
        limbs[(bit / 64) as usize] = 1u64 << (bit % 64);
        Some(U256(limbs))
    }
}

impl Ord for U256 {
    fn cmp(&self, other: &U256) -> Ordering {
        for i in (0..4).rev() {
            match self.0[i].cmp(&other.0[i]) {
                Ordering::Equal => {}
                o => return o,
            }
        }
        Ordering::Equal
    }
}

impl PartialOrd for U256 {
    fn partial_cmp(&self, other: &U256) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_DEC: &str =
        "115792089237316195423570985008687907853269984665640564039457584007913129639935";

    #[test]
    fn decimal_parsing() {
        assert_eq!(U256::from_dec_str("0"), Some(U256::ZERO));
        assert_eq!(U256::from_dec_str("1"), U256::pow2(0));
        assert_eq!(U256::from_dec_str("18446744073709551616"), U256::pow2(64));
        assert_eq!(
            U256::from_dec_str(
                "57896044618658097711785492504343953926634992332820282019728792003956564819968"
            ),
            U256::pow2(255)
        );
        // 2^256 - 1 fits, 2^256 does not
        assert_eq!(
            U256::from_dec_str(MAX_DEC),
            Some(U256::from_be_bytes(&[0xff; 32]))
        );
        assert_eq!(
            U256::from_dec_str(
                "115792089237316195423570985008687907853269984665640564039457584007913129639936"
            ),
            None
        );
        for bad in ["", "-1", "+1", " 1", "1 ", "0x10", "1e2", "1.0", "٣"] {
            assert_eq!(U256::from_dec_str(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn ordering_is_numeric_and_big_endian_bytes_compare_the_same_way() {
        let mut lo = [0u8; 32];
        let mut hi = [0u8; 32];
        lo[31] = 255; // 255
        hi[0] = 1; // 2^248
        assert!(U256::from_be_bytes(&lo) < U256::from_be_bytes(&hi));
        assert!(U256::pow2(64).unwrap() > U256::from_dec_str("18446744073709551615").unwrap());
        assert!(U256::pow2(255).unwrap() > U256::pow2(254).unwrap());
        assert_eq!(U256::from_be_bytes(&hi), U256::pow2(248).unwrap());
        assert_eq!(U256::pow2(256), None);
    }
}
