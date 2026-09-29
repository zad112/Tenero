//! ChaCha20 (RFC 8439), written out because the proof of work needs the bare 20-round permutation
//! (with the feed-forward add) on arbitrary 16-word states, which stream-cipher crates do not
//! expose. It is a public, small function and is checked bit for bit against `chacha20.json`
//! (which the Python reference produced and OpenSSL agreed with) and against RFC 8439.
//!
//! Words are little-endian `u32`. Nothing here handles secrets on a data-dependent path: it is
//! add, xor and rotate only. It is used as a mixing function, not as an encryption API.

/// "expand 32-byte k" as four little-endian words.
pub const CONSTANTS: [u32; 4] = [0x6170_7865, 0x3320_646E, 0x7962_2D32, 0x6B20_6574];

#[inline(always)]
fn quarter_round(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

/// The ChaCha20 permutation (20 rounds) followed by adding the original state back.
pub fn core(state: &[u32; 16]) -> [u32; 16] {
    let mut s = *state;
    for _ in 0..10 {
        quarter_round(&mut s, 0, 4, 8, 12);
        quarter_round(&mut s, 1, 5, 9, 13);
        quarter_round(&mut s, 2, 6, 10, 14);
        quarter_round(&mut s, 3, 7, 11, 15);
        quarter_round(&mut s, 0, 5, 10, 15);
        quarter_round(&mut s, 1, 6, 11, 12);
        quarter_round(&mut s, 2, 7, 8, 13);
        quarter_round(&mut s, 3, 4, 9, 14);
    }
    for (w, o) in s.iter_mut().zip(state) {
        *w = w.wrapping_add(*o);
    }
    s
}

/// A 32-byte key as 8 little-endian words.
pub fn key_words(key: &[u8; 32]) -> [u32; 8] {
    let mut w = [0u32; 8];
    for (i, word) in w.iter_mut().enumerate() {
        *word = u32::from_le_bytes([key[4 * i], key[4 * i + 1], key[4 * i + 2], key[4 * i + 3]]);
    }
    w
}

/// The standard ChaCha20 block: 16 words (64 bytes) of keystream.
pub fn block_words(key: &[u32; 8], counter: u32, nonce: [u32; 3]) -> [u32; 16] {
    let mut state = [0u32; 16];
    state[0..4].copy_from_slice(&CONSTANTS);
    state[4..12].copy_from_slice(key);
    state[12] = counter;
    state[13..16].copy_from_slice(&nonce);
    core(&state)
}

/// The standard ChaCha20 block as 64 bytes (each word little-endian).
pub fn block(key: &[u8; 32], counter: u32, nonce: [u32; 3]) -> [u8; 64] {
    words_to_bytes(&block_words(&key_words(key), counter, nonce))
}

/// 16 words as 64 little-endian bytes.
pub fn words_to_bytes(words: &[u32; 16]) -> [u8; 64] {
    let mut out = [0u8; 64];
    for (i, w) in words.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
    }
    out
}

/// `nblocks` consecutive blocks of keystream starting at `start_counter`.
/// The block counter wraps modulo 2^32, exactly as the reference does (`chacha20.json` pins this:
/// one case starts three blocks below 2^32). The proof-of-work parameter rules keep real counters
/// below 2^32, so consensus never relies on the wrap.
pub fn keystream(key: &[u8; 32], nblocks: usize, start_counter: u32, nonce: [u32; 3]) -> Vec<u8> {
    let kw = key_words(key);
    let mut out = Vec::with_capacity(nblocks * 64);
    let mut counter = start_counter;
    for _ in 0..nblocks {
        out.extend_from_slice(&words_to_bytes(&block_words(&kw, counter, nonce)));
        counter = counter.wrapping_add(1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::sha256;
    use crate::vectors::{hex, load};

    fn key32(s: &str) -> [u8; 32] {
        hex(s).unwrap().try_into().unwrap()
    }

    fn nonce_words(bytes: &[u8]) -> [u32; 3] {
        let b: [u8; 12] = bytes.try_into().unwrap();
        [
            u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
        ]
    }

    /// RFC 8439 section 2.3.2, written out here so it does not depend on the vector file.
    #[test]
    fn rfc8439_block_function_example() {
        let key = key32("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f");
        let out = block(
            &key,
            1,
            nonce_words(&hex("000000090000004a00000000").unwrap()),
        );
        assert_eq!(
            out.to_vec(),
            hex(
                "10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4e\
                 d2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e"
            )
            .unwrap()
        );
    }

    #[test]
    fn the_block_function_vectors() {
        let v = load("chacha20").unwrap();
        let cases = v["blocks"].as_array().unwrap();
        assert!(!cases.is_empty());
        for (i, c) in cases.iter().enumerate() {
            let key = key32(c["key"].as_str().unwrap());
            let counter = u32::try_from(c["counter"].as_u64().unwrap()).unwrap();
            let nonce = nonce_words(&hex(c["nonce"].as_str().unwrap()).unwrap());
            let want = hex(c["output"].as_str().unwrap()).unwrap();
            assert_eq!(block(&key, counter, nonce).to_vec(), want, "blocks[{i}]");
        }
    }

    #[test]
    fn the_bare_core_vectors() {
        let v = load("chacha20").unwrap();
        let cases = v["core"].as_array().unwrap();
        assert!(!cases.is_empty());
        let words = |a: &serde_json::Value| -> [u32; 16] {
            let mut w = [0u32; 16];
            let items = a.as_array().unwrap();
            assert_eq!(items.len(), 16);
            for (dst, x) in w.iter_mut().zip(items) {
                *dst = u32::try_from(x.as_u64().unwrap()).unwrap();
            }
            w
        };
        for (i, c) in cases.iter().enumerate() {
            assert_eq!(core(&words(&c["state"])), words(&c["output"]), "core[{i}]");
        }
    }

    #[test]
    fn the_keystream_digest_vectors() {
        let v = load("chacha20").unwrap();
        let cases = v["keystream"].as_array().unwrap();
        assert!(!cases.is_empty());
        for (i, c) in cases.iter().enumerate() {
            let key = key32(c["key"].as_str().unwrap());
            let nblocks = usize::try_from(c["blocks"].as_u64().unwrap()).unwrap();
            let start = u32::try_from(c["start_counter"].as_u64().unwrap()).unwrap();
            let nw = c["nonce_words"].as_array().unwrap();
            let nonce = [0, 1, 2].map(|j| u32::try_from(nw[j].as_u64().unwrap()).unwrap());
            let stream = keystream(&key, nblocks, start, nonce);
            assert_eq!(stream.len(), nblocks * 64);
            assert_eq!(
                sha256(&[&stream]).to_vec(),
                hex(c["sha256"].as_str().unwrap()).unwrap(),
                "keystream[{i}]"
            );
        }
    }

    #[test]
    fn the_counter_wraps_modulo_2_32() {
        let key = [7u8; 32];
        let s = keystream(&key, 2, u32::MAX, [0; 3]);
        assert_eq!(s[..64], block(&key, u32::MAX, [0; 3]));
        assert_eq!(s[64..], block(&key, 0, [0; 3]));
        assert!(keystream(&key, 0, u32::MAX, [0; 3]).is_empty());
    }
}
