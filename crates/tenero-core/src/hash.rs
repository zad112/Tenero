//! SHA-256, from the audited `sha2` crate (RustCrypto). Nothing here is home-made.

use sha2::{Digest, Sha256};

/// SHA-256 of the concatenation of `parts`.
pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// Lower-case hexadecimal.
pub fn hex_lower(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(DIGITS[usize::from(b >> 4)]));
        s.push(char::from(DIGITS[usize::from(b & 15)]));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vectors::hex;

    #[test]
    fn hex_round_trip() {
        assert_eq!(hex_lower(&[0, 1, 0xab, 0xff]), "0001abff");
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(hex(&hex_lower(&all)).unwrap(), all);
    }

    #[test]
    fn known_answers() {
        // FIPS 180-4 examples: "" and "abc".
        assert_eq!(
            sha256(&[]).to_vec(),
            hex("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855").unwrap()
        );
        assert_eq!(
            sha256(&[b"abc"]).to_vec(),
            hex("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad").unwrap()
        );
    }

    #[test]
    fn parts_are_concatenated() {
        assert_eq!(sha256(&[b"a", b"bc"]), sha256(&[b"abc"]));
    }
}
