//! Property tests on the peer-to-peer wire decoders (M9 groundwork; proptest, a dev-dependency). A peer is a
//! stranger: whatever it sends, decoding must not panic, hang or allocate wildly, a decoder that has failed must stay
//! failed, and (the decoders being strict) a frame that decodes must re-encode to exactly the same bytes.

use proptest::prelude::*;
use tenero_core::vectors::{hex, load};
use tenero_net::{decode_frame, encode, FrameDecoder, MAX_FRAME};

fn valid_frames() -> Vec<Vec<u8>> {
    let v = load("v3_wire").unwrap();
    v["valid"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| hex(c["frame"].as_str().unwrap()).unwrap())
        .collect()
}

fn mutate(mut b: Vec<u8>, edits: &[(usize, u8, u8)]) -> Vec<u8> {
    for &(p, v, how) in edits {
        if b.is_empty() {
            break;
        }
        let n = b.len();
        match how % 5 {
            0 => b[p % n] ^= 1 << (v % 8),
            1 => b[p % n] = v,
            2 => b.insert(p % n, v),
            3 => {
                b.remove(p % n);
            }
            _ => b.truncate(p % n),
        }
    }
    b
}

fn edits() -> impl Strategy<Value = Vec<(usize, u8, u8)>> {
    proptest::collection::vec((any::<usize>(), any::<u8>(), any::<u8>()), 1..5)
}

fn check_frame(frame: &[u8]) -> Result<(), TestCaseError> {
    if let Ok(msg) = decode_frame(frame) {
        let back = encode(&msg).map_err(|e| TestCaseError::fail(format!("{e}")))?;
        prop_assert_eq!(back.as_slice(), frame, "a second encoding of a frame");
    }
    Ok(())
}

/// Feeds `data` to a streaming decoder in the given chunk sizes and checks what must always hold.
fn stream(data: &[u8], sizes: &[usize]) -> Result<(), TestCaseError> {
    let mut d = FrameDecoder::new();
    let (mut at, mut i, mut failed) = (0, 0, false);
    while at < data.len() {
        let n = sizes[i % sizes.len()].max(1).min(data.len() - at);
        i += 1;
        d.push(&data[at..at + n]);
        at += n;
        loop {
            match d.next_message() {
                Ok(Some(_)) => prop_assert!(!failed, "a message after a failure"),
                Ok(None) => break,
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }
        // never holds more than one frame's worth
        prop_assert!(d.buffered() <= MAX_FRAME + 4 + n);
    }
    if failed {
        // a failure is final: it stays an error however much more arrives
        let held = d.buffered();
        d.push(&[0; 16]);
        prop_assert!(d.next_message().is_err());
        prop_assert_eq!(
            d.buffered(),
            held,
            "a failed decoder keeps buffering what arrives"
        );
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    #[test]
    fn random_bytes_never_break_the_frame_decoder(bytes in proptest::collection::vec(any::<u8>(), 0..3000)) {
        check_frame(&bytes)?;
    }

    #[test]
    fn a_valid_frame_with_a_few_edits_never_breaks_the_decoder(which in any::<usize>(), e in edits()) {
        let frames = valid_frames();
        check_frame(&mutate(frames[which % frames.len()].clone(), &e))?;
    }

    #[test]
    fn a_stream_of_random_bytes_in_random_chunks_is_safe(
        bytes in proptest::collection::vec(any::<u8>(), 0..3000),
        sizes in proptest::collection::vec(1usize..400, 1..8),
    ) {
        stream(&bytes, &sizes)?;
    }

    #[test]
    fn a_stream_of_edited_valid_frames_in_random_chunks_is_safe(
        picks in proptest::collection::vec(any::<usize>(), 1..6),
        e in edits(),
        sizes in proptest::collection::vec(1usize..400, 1..8),
    ) {
        let frames = valid_frames();
        let mut data = Vec::new();
        for p in picks {
            data.extend_from_slice(&frames[p % frames.len()]);
        }
        stream(&mutate(data, &e), &sizes)?;
    }

    #[test]
    fn a_huge_declared_length_does_not_make_the_decoder_hold_what_never_came(
        len in (MAX_FRAME as u32 / 2)..u32::MAX,
        tail in proptest::collection::vec(any::<u8>(), 0..64),
    ) {
        let mut d = FrameDecoder::new();
        let mut data = len.to_le_bytes().to_vec();
        data.extend_from_slice(&tail);
        d.push(&data);
        let _ = d.next_message(); // refused at once (too long), or waiting: either way, bounded
        prop_assert!(d.buffered() <= data.len());
    }
}
