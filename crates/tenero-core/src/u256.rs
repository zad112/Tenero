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

impl U256 {
    /// 2^256 - 1, the largest target.
    pub const MAX: U256 = U256([u64::MAX; 4]);
    pub const ONE: U256 = U256([1, 0, 0, 0]);

    /// The value divided by a small number, rounding down. `d` must not be zero.
    pub fn div_u64(&self, d: u64) -> U256 {
        U320::from_u256(self)
            .div_u64(d)
            .to_u256()
            .expect("a quotient is not larger than its dividend")
    }
}

/// A 320-bit unsigned integer: room for a 256-bit target times a 64-bit factor, or for the sum of
/// up to 2^64 targets, which the difficulty adjustment needs. Only what it needs: add, multiply and
/// divide by a 64-bit number, compare.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct U320([u64; 5]);

impl U320 {
    pub const ZERO: U320 = U320([0; 5]);

    pub fn from_u256(x: &U256) -> U320 {
        U320([x.0[0], x.0[1], x.0[2], x.0[3], 0])
    }

    /// `None` when the value is 2^256 or more.
    pub fn to_u256(&self) -> Option<U256> {
        (self.0[4] == 0).then(|| U256([self.0[0], self.0[1], self.0[2], self.0[3]]))
    }

    /// `None` on overflow past 320 bits.
    pub fn checked_add(&self, o: &U320) -> Option<U320> {
        let mut out = [0u64; 5];
        let mut carry = false;
        for (slot, (a, b)) in out.iter_mut().zip(self.0.iter().zip(&o.0)) {
            let (s1, c1) = a.overflowing_add(*b);
            let (s2, c2) = s1.overflowing_add(u64::from(carry));
            *slot = s2;
            carry = c1 || c2;
        }
        (!carry).then_some(U320(out))
    }

    /// `None` on overflow past 320 bits.
    pub fn checked_mul_u64(&self, m: u64) -> Option<U320> {
        let mut out = [0u64; 5];
        let mut carry = 0u128;
        for (slot, limb) in out.iter_mut().zip(&self.0) {
            let t = u128::from(*limb) * u128::from(m) + carry;
            *slot = t as u64; // the low 64 bits; the high bits carry on
            carry = t >> 64;
        }
        (carry == 0).then_some(U320(out))
    }

    /// Divided by `d`, rounding down. `d` must not be zero.
    pub fn div_u64(&self, d: u64) -> U320 {
        assert!(d != 0, "division by zero");
        let mut out = [0u64; 5];
        let mut rem = 0u128;
        for i in (0..5).rev() {
            let cur = (rem << 64) | u128::from(self.0[i]);
            out[i] = (cur / u128::from(d)) as u64; // below 2^64 because rem < d
            rem = cur % u128::from(d);
        }
        U320(out)
    }
}

impl Ord for U320 {
    fn cmp(&self, other: &U320) -> Ordering {
        for i in (0..5).rev() {
            match self.0[i].cmp(&other.0[i]) {
                Ordering::Equal => {}
                o => return o,
            }
        }
        Ordering::Equal
    }
}

impl PartialOrd for U320 {
    fn partial_cmp(&self, other: &U320) -> Option<Ordering> {
        Some(self.cmp(other))
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
    fn wide_arithmetic() {
        let max = U320::from_u256(&U256::MAX);
        // (2^256 - 1) * 2^64 - 1 does not fit in 256 bits but does in 320
        let big = max.checked_mul_u64(u64::MAX).unwrap();
        assert_eq!(big.to_u256(), None);
        assert_eq!(big.div_u64(u64::MAX), max);
        // 2^256 - 1 plus itself, then halved
        let two = max.checked_add(&max).unwrap();
        assert_eq!(two.div_u64(2), max);
        assert_eq!(two.to_u256(), None);
        // a sum of 2^64 - 1 maximal targets (`huge`) still fits, and a 320-bit overflow is reported
        let huge = max.checked_mul_u64(u64::MAX).unwrap();
        assert!(huge.checked_add(&max).is_some());
        assert!(huge.checked_add(&huge).is_none());
        assert!(huge.checked_mul_u64(u64::MAX).is_none());
        assert!(U320([u64::MAX; 5])
            .checked_add(&U320([1, 0, 0, 0, 0]))
            .is_none());
        // division rounds down, with a remainder carried across limbs
        let x = U256::from_dec_str("100000000000000000000000000000000000000000").unwrap();
        assert_eq!(
            x.div_u64(3),
            U256::from_dec_str("33333333333333333333333333333333333333333").unwrap()
        );
        assert_eq!(x.div_u64(1), x);
        assert!(U320::from_u256(&x) < big && big > U320::ZERO);
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
