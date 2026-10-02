//! Property tests on the control interface's decoders (M9 groundwork; proptest, a dev-dependency). The interface is
//! loopback only and needs the cookie, but a program that holds the cookie, or a bug in one, must still not be able to
//! crash the node. Decoding must not panic or allocate wildly, and what decodes must re-encode to the same bytes.

use proptest::prelude::*;
use std::io::Cursor;
use tenero_app::control::{frame_len, read_frame, Request, Response, MAX_FRAME};
use tenero_core::vectors::{hex, load};

fn valid_bodies() -> Vec<Vec<u8>> {
    let v = load("control").unwrap();
    v["valid"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| hex(c["body"].as_str().unwrap()).unwrap())
        .collect()
}

fn mutate(mut b: Vec<u8>, edits: &[(usize, u8, u8)]) -> Vec<u8> {
    for &(p, v, how) in edits {
        if b.is_empty() {
            break;
        }
        let n = b.len();
        match how % 6 {
            0 => b[p % n] ^= 1 << (v % 8),
            1 => b[p % n] = v,
            2 => b.insert(p % n, v),
            3 => {
                b.remove(p % n);
            }
            4 => b.truncate(p % n),
            _ => {
                // a count or length made large
                for i in 0..4 {
                    if p % n + i < b.len() {
                        b[p % n + i] = 0xFF;
                    }
                }
            }
        }
    }
    b
}

fn edits() -> impl Strategy<Value = Vec<(usize, u8, u8)>> {
    proptest::collection::vec((any::<usize>(), any::<u8>(), any::<u8>()), 1..5)
}

fn check(body: &[u8]) -> Result<(), TestCaseError> {
    if let Ok(r) = Request::from_body(body) {
        let back = r
            .to_body()
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;
        prop_assert_eq!(back.as_slice(), body, "a second encoding of a request");
    }
    if let Ok(r) = Response::from_body(body) {
        let back = r
            .to_body()
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;
        prop_assert_eq!(back.as_slice(), body, "a second encoding of a response");
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    #[test]
    fn random_bodies_never_break_the_decoders(bytes in proptest::collection::vec(any::<u8>(), 0..3000)) {
        check(&bytes)?;
    }

    #[test]
    fn a_valid_body_with_a_few_edits_never_breaks_the_decoders(which in any::<usize>(), e in edits()) {
        let bodies = valid_bodies();
        check(&mutate(bodies[which % bodies.len()].clone(), &e))?;
    }

    #[test]
    fn a_frame_header_is_accepted_only_when_the_length_is_within_the_limit(h in any::<[u8; 4]>()) {
        if let Ok(n) = frame_len(h) {
            prop_assert!((1..=MAX_FRAME).contains(&n));
        }
    }

    #[test]
    fn a_frame_header_with_a_small_length_is_judged_exactly(n in 0u32..64) {
        // the interesting edge: nothing is a frame of length zero
        prop_assert_eq!(frame_len(n.to_le_bytes()).is_ok(), n >= 1);
    }

    #[test]
    fn reading_a_frame_from_any_bytes_returns_at_most_what_arrived(
        bytes in proptest::collection::vec(any::<u8>(), 0..2000),
    ) {
        if let Ok(body) = read_frame(&mut Cursor::new(&bytes)) {
            prop_assert!(body.len() + 4 <= bytes.len());
        }
    }
}
