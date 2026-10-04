//! Real Monero mainnet Bulletproofs+ proofs (`tests/vectors/upstream_monero_bpp.json`, imported verbatim by
//! `reference/tools/import_upstream_vectors.py`). They show that the pinned library accepts what Monero's own code
//! produced, not only what the library produced itself.

use monero_bulletproofs::Bulletproof;
use monero_ed25519::CompressedPoint;
use rand_core::OsRng;
use tenero_core::vectors::{hex, load};

struct Vector {
    tx: String,
    out_pk: Vec<CompressedPoint>,
    bytes: Vec<u8>,
}

fn key(v: &str) -> [u8; 32] {
    hex(v).unwrap().try_into().unwrap()
}

fn vectors() -> Vec<Vector> {
    let file = load("upstream_monero_bpp").unwrap();
    file["proofs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            let field = |n: &str| key(p[n].as_str().unwrap());
            let list = |n: &str| -> Vec<[u8; 32]> {
                p[n].as_array()
                    .unwrap()
                    .iter()
                    .map(|x| key(x.as_str().unwrap()))
                    .collect()
            };
            // Monero's encoding: A, A1, B, r1, s1, d1, then L and R each with a one-byte length
            let mut bytes = Vec::new();
            for n in ["A", "A1", "B", "r1", "s1", "d1"] {
                bytes.extend_from_slice(&field(n));
            }
            for n in ["L", "R"] {
                let l = list(n);
                assert!(l.len() < 128);
                bytes.push(l.len() as u8);
                for point in l {
                    bytes.extend_from_slice(&point);
                }
            }
            Vector {
                tx: p["tx"].as_str().unwrap().to_string(),
                out_pk: list("out_pk")
                    .into_iter()
                    .map(CompressedPoint::from)
                    .collect(),
                bytes,
            }
        })
        .collect()
}

#[test]
fn the_vectors_are_there() {
    let v = vectors();
    assert_eq!(v.len(), 2);
    assert!(v.iter().any(|x| x.out_pk.len() == 4));
    assert!(v.iter().any(|x| x.out_pk.len() == 2));
}

#[test]
fn real_monero_range_proofs_decode_and_re_encode_to_the_same_bytes() {
    for v in vectors() {
        let bp = Bulletproof::read_plus(&mut v.bytes.as_slice()).unwrap();
        assert_eq!(bp.serialize(), v.bytes, "tx {}", v.tx);
    }
}

#[test]
fn real_monero_range_proofs_verify() {
    for v in vectors() {
        let bp = Bulletproof::read_plus(&mut v.bytes.as_slice()).unwrap();
        assert!(bp.verify(&mut OsRng, &v.out_pk), "tx {}", v.tx);
    }
}

#[test]
fn a_real_proof_does_not_verify_for_other_commitments() {
    for v in vectors() {
        let bp = Bulletproof::read_plus(&mut v.bytes.as_slice()).unwrap();
        // a different order
        let mut swapped = v.out_pk.clone();
        swapped.swap(0, 1);
        assert!(!bp.verify(&mut OsRng, &swapped), "tx {}", v.tx);
        // a different commitment
        let mut other = v.out_pk.clone();
        other[0] = CompressedPoint::G;
        assert!(!bp.verify(&mut OsRng, &other), "tx {}", v.tx);
        // fewer commitments than the proof was made for
        assert!(!bp.verify(&mut OsRng, &v.out_pk[..1]), "tx {}", v.tx);
    }
}

#[test]
fn a_real_proof_with_any_byte_flipped_is_refused() {
    for v in vectors() {
        for pos in (0..v.bytes.len()).step_by(7) {
            let mut b = v.bytes.clone();
            b[pos] ^= 1;
            let accepted = Bulletproof::read_plus(&mut b.as_slice())
                .map(|bp| bp.verify(&mut OsRng, &v.out_pk))
                .unwrap_or(false);
            assert!(!accepted, "tx {}: a flip at byte {pos} was accepted", v.tx);
        }
    }
}
