//! Carrot's hash functions and its fixed transcript (`carrot_core/hash_functions.cpp`, `transcript_fixed.h`).
//!
//! Every Carrot hash is Blake2b with the personalisation `"Monero"`, an output of 3 to 64 bytes, and optionally a
//! 32-byte key (fed as a zero-padded first block, the standard Blake2b keying): exactly monero-oxide's `Blake2bMonero`.
//! The data hashed is a transcript: one byte giving the length of the domain separator, the separator itself, then the
//! fields in order (unsigned integers little endian, everything else as raw bytes).

use blake2::digest::Update;
use curve25519_dalek::scalar::Scalar;
use monero_primitives::Blake2bMonero;
use zeroize::Zeroize;

/// A transcript under construction. It holds secrets (keys go into some), so it is wiped when dropped.
pub(crate) struct Transcript(Vec<u8>);

impl Transcript {
    pub(crate) fn new(domain_separator: &str) -> Transcript {
        let sep = domain_separator.as_bytes();
        let len = u8::try_from(sep.len()).expect("a domain separator is under 256 bytes");
        let mut t = Vec::with_capacity(1 + sep.len() + 160);
        t.push(len);
        t.extend_from_slice(sep);
        Transcript(t)
    }

    pub(crate) fn bytes(mut self, b: &[u8]) -> Transcript {
        self.0.extend_from_slice(b);
        self
    }

    pub(crate) fn u8(mut self, v: u8) -> Transcript {
        self.0.push(v);
        self
    }

    pub(crate) fn u32(mut self, v: u32) -> Transcript {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub(crate) fn u64(mut self, v: u64) -> Transcript {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// `H_N(key, transcript)`: `N` bytes (`derive_bytes_3` ... `derive_bytes_64`).
    pub(crate) fn derive<const N: usize>(&self, key: Option<&[u8; 32]>) -> [u8; N] {
        let mut h = match key {
            Some(k) => Blake2bMonero::<N>::new_with_key(k),
            None => Blake2bMonero::<N>::new(),
        };
        h.update(&self.0);
        h.finalize()
    }

    /// `H_n(key, transcript)`: 64 bytes reduced modulo the group order (`derive_scalar`).
    pub(crate) fn derive_scalar(&self, key: Option<&[u8; 32]>) -> Scalar {
        let mut wide = self.derive::<64>(key);
        let s = Scalar::from_bytes_mod_order_wide(&wide);
        wide.zeroize();
        s
    }
}

impl Drop for Transcript {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_transcript_is_a_length_prefixed_separator_then_the_fields() {
        let t = Transcript::new("abc")
            .u8(7)
            .u32(0x0102_0304)
            .u64(5)
            .bytes(&[9, 9]);
        assert_eq!(
            t.0,
            [3, b'a', b'b', b'c', 7, 4, 3, 2, 1, 5, 0, 0, 0, 0, 0, 0, 0, 9, 9]
        );
    }

    #[test]
    fn keyed_and_unkeyed_and_lengths_all_differ() {
        let t = Transcript::new("x");
        let a: [u8; 32] = t.derive(None);
        let b: [u8; 32] = t.derive(Some(&[0; 32]));
        let c: [u8; 16] = t.derive(None);
        assert_ne!(a, b);
        // Blake2b's output length is a parameter, so a short output is not a prefix of a long one
        assert_ne!(a[..16], c);
    }
}
