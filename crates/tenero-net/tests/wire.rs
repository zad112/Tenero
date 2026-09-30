//! The wire codec against the golden vectors of the independent Python reference
//! (`tests/vectors/v2_wire.json`), plus properties the vectors cannot list: streaming in any chunking, one
//! message one encoding, no panic on any input, and a bounded buffer.

use serde_json::Value;
use tenero_core::v2::{Block, Transaction, Wire};
use tenero_core::vectors::{hex, load};
use tenero_net::message::{MAX_BLOCKS, MAX_IDS, MAX_LOCATOR, MAX_NOT_FOUND, MAX_TXS};
use tenero_net::{
    decode_frame, encode, FrameDecoder, Hello, Limits, Message, WireError, MAX_FRAME,
};

fn h32(v: &Value) -> [u8; 32] {
    hex(v.as_str().unwrap()).unwrap().try_into().unwrap()
}

fn ids(v: &Value) -> Vec<[u8; 32]> {
    v.as_array().unwrap().iter().map(h32).collect()
}

/// A message from its JSON form in the vector file.
fn message(m: &Value) -> Message {
    let n = |k: &str| m[k].as_u64().unwrap();
    match m["kind"].as_str().unwrap() {
        "hello" => Message::Hello(Hello {
            version: n("version") as u32,
            chain_id: h32(&m["chain_id"]),
            tip_height: n("tip_height"),
            cumulative_work: h32(&m["cumulative_work"]),
            tip_id: h32(&m["tip_id"]),
            pruned_below: n("pruned_below"),
        }),
        "ping" => Message::Ping(n("nonce")),
        "pong" => Message::Pong(n("nonce")),
        "get_block_ids" => Message::GetBlockIds {
            locator: ids(&m["locator"]),
        },
        "block_ids" => Message::BlockIds {
            first_height: n("first_height"),
            ids: ids(&m["ids"]),
        },
        "get_blocks" => Message::GetBlocks {
            ids: ids(&m["ids"]),
        },
        "blocks" => Message::Blocks {
            blocks: m["blocks"]
                .as_array()
                .unwrap()
                .iter()
                .map(|b| Block::from_bytes(&hex(b.as_str().unwrap()).unwrap()).unwrap())
                .collect(),
        },
        "not_found" => Message::NotFound {
            ids: ids(&m["ids"]),
        },
        "new_block" => Message::NewBlock {
            id: h32(&m["id"]),
            height: n("height"),
            cumulative_work: h32(&m["cumulative_work"]),
        },
        "new_tx" => Message::NewTx {
            ids: ids(&m["ids"]),
        },
        "get_txs" => Message::GetTxs {
            ids: ids(&m["ids"]),
        },
        "txs" => Message::Txs {
            txs: m["txs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| Transaction::from_bytes(&hex(t.as_str().unwrap()).unwrap()).unwrap())
                .collect(),
        },
        other => panic!("unknown kind {other} in the vector file"),
    }
}

fn vectors() -> Value {
    load("v2_wire").unwrap()
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

// ---- the vectors -----------------------------------------------------------------------------------

#[test]
fn the_limits_in_the_vector_file_are_the_ones_in_the_code() {
    let v = vectors();
    let l = &v["limits"];
    assert_eq!(l["max_frame"].as_u64().unwrap() as usize, MAX_FRAME);
    assert_eq!(l["max_locator"].as_u64().unwrap() as usize, MAX_LOCATOR);
    assert_eq!(l["max_ids"].as_u64().unwrap() as usize, MAX_IDS);
    assert_eq!(l["max_blocks"].as_u64().unwrap() as usize, MAX_BLOCKS);
    assert_eq!(l["max_txs"].as_u64().unwrap() as usize, MAX_TXS);
    assert_eq!(l["max_not_found"].as_u64().unwrap() as usize, MAX_NOT_FOUND);
    // and the engine's defaults are the same ceilings
    let d = Limits::default();
    assert_eq!(
        (d.max_locator, d.max_ids, d.max_blocks, d.max_txs),
        (MAX_LOCATOR, MAX_IDS, MAX_BLOCKS, MAX_TXS)
    );
}

#[test]
fn every_valid_message_encodes_to_the_reference_bytes_and_decodes_back() {
    let v = vectors();
    let cases = v["valid"].as_array().unwrap();
    assert!(cases.len() >= 20, "{} cases", cases.len());
    for c in cases {
        let note = c["note"].as_str().unwrap();
        let msg = message(&c["message"]);
        let frame = hex(c["frame"].as_str().unwrap()).unwrap();
        assert_eq!(encode(&msg).unwrap(), frame, "encoding: {note}");
        assert_eq!(decode_frame(&frame).unwrap(), msg, "decoding: {note}");
    }
}

#[test]
fn every_malformed_frame_is_refused_with_the_reference_error() {
    let v = vectors();
    let cases = v["invalid"].as_array().unwrap();
    assert!(cases.len() >= 30, "{} cases", cases.len());
    for c in cases {
        let note = c["note"].as_str().unwrap();
        let frame = hex(c["frame"].as_str().unwrap()).unwrap();
        let err = decode_frame(&frame).expect_err(note);
        assert_eq!(err.as_str(), c["error"].as_str().unwrap(), "{note}");
    }
}

#[test]
fn a_stream_decoder_fails_early_exactly_where_the_reference_says() {
    let v = vectors();
    for c in v["early"].as_array().unwrap() {
        let note = c["note"].as_str().unwrap();
        let prefix = hex(c["prefix"].as_str().unwrap()).unwrap();
        let mut d = FrameDecoder::new();
        d.push(&prefix);
        match (d.next_message(), c["error"].as_str()) {
            (Err(e), Some(want)) => assert_eq!(e.as_str(), want, "{note}"),
            (Ok(None), None) => {}
            (got, want) => panic!("{note}: got {got:?}, the reference says {want:?}"),
        }
    }
}

// ---- streaming ---------------------------------------------------------------------------------------

fn frames() -> Vec<(Message, Vec<u8>)> {
    vectors()["valid"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                message(&c["message"]),
                hex(c["frame"].as_str().unwrap()).unwrap(),
            )
        })
        .collect()
}

fn drain(d: &mut FrameDecoder) -> Vec<Message> {
    let mut out = Vec::new();
    while let Some(m) = d.next_message().expect("no error expected") {
        out.push(m);
    }
    out
}

#[test]
fn a_stream_yields_the_same_messages_however_the_bytes_are_chunked() {
    let all = frames();
    let wire: Vec<u8> = all.iter().flat_map(|(_, f)| f.clone()).collect();
    let want: Vec<Message> = all.iter().map(|(m, _)| m.clone()).collect();
    for chunk in [1usize, 2, 3, 5, 7, 64, 1000, 65_536, wire.len()] {
        let mut d = FrameDecoder::new();
        let mut got = Vec::new();
        for piece in wire.chunks(chunk) {
            d.push(piece);
            got.extend(drain(&mut d));
        }
        assert_eq!(got, want, "chunks of {chunk}");
        assert_eq!(d.buffered(), 0);
    }
    // random chunk sizes
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for _ in 0..20 {
        let mut d = FrameDecoder::new();
        let mut got = Vec::new();
        let mut at = 0;
        while at < wire.len() {
            let n = (1 + rng.below(3000)).min(wire.len() - at);
            d.push(&wire[at..at + n]);
            at += n;
            got.extend(drain(&mut d));
        }
        assert_eq!(got, want);
    }
}

#[test]
fn a_stream_decoder_waits_for_an_incomplete_frame_and_never_yields_half_a_message() {
    for (m, f) in frames() {
        if f.len() > 5000 {
            continue;
        }
        let mut d = FrameDecoder::new();
        for cut in 0..f.len() {
            let mut probe = FrameDecoder::new();
            probe.push(&f[..cut]);
            assert_eq!(
                probe.next_message().unwrap(),
                None,
                "{cut} of {} bytes",
                f.len()
            );
        }
        d.push(&f);
        assert_eq!(d.next_message().unwrap(), Some(m));
        assert_eq!(d.next_message().unwrap(), None);
    }
}

#[test]
fn after_an_error_the_decoder_stays_failed_and_holds_nothing() {
    let mut d = FrameDecoder::new();
    d.push(&[0, 0, 0, 0, 2]); // a zero length
    assert_eq!(d.next_message(), Err(WireError::EmptyFrame));
    assert_eq!(d.buffered(), 0);
    // later bytes, even a valid frame, cannot revive it
    let ping = encode(&Message::Ping(1)).unwrap();
    d.push(&ping);
    assert_eq!(d.next_message(), Err(WireError::EmptyFrame));
    assert_eq!(d.buffered(), 0);
}

#[test]
fn a_declared_size_over_the_cap_is_refused_before_any_body_is_buffered() {
    // 4 GiB declared for a ping, and only the 5 header bytes sent: refused at once
    let mut d = FrameDecoder::new();
    d.push(&[0xff, 0xff, 0xff, 0xff, 2]);
    assert!(matches!(
        d.next_message(),
        Err(WireError::FrameTooLarge { .. })
    ));
    assert_eq!(d.buffered(), 0);
    // a block list may declare up to 16 MiB: it waits, and holds only what has arrived
    let mut d = FrameDecoder::new();
    let mut head = ((MAX_FRAME) as u32).to_le_bytes().to_vec();
    head.push(7);
    d.push(&head);
    d.push(&[0u8; 1000]);
    assert_eq!(d.next_message().unwrap(), None);
    assert_eq!(d.buffered(), 1005);
}

// ---- properties -------------------------------------------------------------------------------------

#[test]
fn the_encoder_refuses_what_a_decoder_would_refuse() {
    let id = [1u8; 32];
    for (name, msg) in [
        (
            "locator",
            Message::GetBlockIds {
                locator: vec![id; MAX_LOCATOR + 1],
            },
        ),
        (
            "block ids",
            Message::BlockIds {
                first_height: 1,
                ids: vec![id; MAX_IDS + 1],
            },
        ),
        (
            "get blocks",
            Message::GetBlocks {
                ids: vec![id; MAX_BLOCKS + 1],
            },
        ),
        (
            "not found",
            Message::NotFound {
                ids: vec![id; MAX_NOT_FOUND + 1],
            },
        ),
        (
            "new tx",
            Message::NewTx {
                ids: vec![id; MAX_TXS + 1],
            },
        ),
        (
            "get txs",
            Message::GetTxs {
                ids: vec![id; MAX_TXS + 1],
            },
        ),
    ] {
        assert!(
            matches!(encode(&msg), Err(WireError::Encode(_))),
            "{name} over its cap must not encode"
        );
    }
    // exactly at the caps is fine and round-trips
    for msg in [
        Message::GetBlockIds {
            locator: vec![id; MAX_LOCATOR],
        },
        Message::BlockIds {
            first_height: 1,
            ids: vec![id; MAX_IDS],
        },
        Message::NotFound {
            ids: vec![id; MAX_NOT_FOUND],
        },
    ] {
        let f = encode(&msg).unwrap();
        assert_eq!(decode_frame(&f).unwrap(), msg);
    }
}

#[test]
fn one_message_has_one_encoding_and_no_input_panics() {
    // Take real frames, damage them in random ways, and check that the decoder either refuses or yields a
    // message that encodes back to EXACTLY the bytes it was given (so a message cannot have two encodings), and
    // that nothing panics.
    let mut rng = Rng(0x0dd_ba11_c0ff_ee01);
    let corpus: Vec<Vec<u8>> = frames().into_iter().map(|(_, f)| f).collect();
    let mut accepted = 0;
    for round in 0..40_000 {
        let mut f = corpus[rng.below(corpus.len())].clone();
        if f.len() > 20_000 {
            continue;
        }
        match rng.below(5) {
            0 => {
                let i = rng.below(f.len());
                f[i] ^= 1 << rng.below(8);
            }
            1 => {
                let i = rng.below(f.len());
                f[i] = rng.next() as u8;
            }
            2 => f.truncate(rng.below(f.len() + 1)),
            3 => {
                for _ in 0..1 + rng.below(4) {
                    f.push(rng.next() as u8);
                }
            }
            _ => {
                let i = rng.below(f.len());
                f.remove(i);
            }
        }
        if let Ok(m) = decode_frame(&f) {
            accepted += 1;
            assert_eq!(
                encode(&m).unwrap(),
                f,
                "round {round}: two encodings of one message"
            );
        }
    }
    // damage that lands in a hash or a number still gives a well-formed message: the property was exercised
    assert!(accepted > 1000, "only {accepted} damaged frames decoded");
}

#[test]
fn random_bytes_never_panic_the_decoders() {
    let mut rng = Rng(0xfeed_face_cafe_beef);
    for _ in 0..30_000 {
        let n = rng.below(300);
        let mut data: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
        // give some a plausible header so the body code is reached
        if n >= 5 && rng.below(2) == 0 {
            let kind = 1 + rng.below(12) as u8;
            let len = (n - 4) as u32;
            data[..4].copy_from_slice(&len.to_le_bytes());
            data[4] = kind;
        }
        let _ = decode_frame(&data);
        let mut d = FrameDecoder::new();
        d.push(&data);
        while let Ok(Some(_)) = d.next_message() {}
    }
}

#[test]
fn every_kind_byte_but_one_to_twelve_is_unknown() {
    for kind in 0u8..=255 {
        let frame = [1u8, 0, 0, 0, kind];
        let r = decode_frame(&frame);
        if (1..=12).contains(&kind) {
            assert!(!matches!(r, Err(WireError::UnknownKind(_))), "kind {kind}");
        } else {
            assert_eq!(r, Err(WireError::UnknownKind(kind)));
        }
    }
}

#[test]
fn a_frame_over_sixteen_mebibytes_is_refused_by_the_encoder_and_a_smaller_one_is_not() {
    use tenero_core::v2::*;
    let tx = |proof: usize| Transaction {
        prefix: TxPrefix {
            version: VERSION,
            inputs: vec![Input { key_image: [1; 32] }],
            outputs: vec![
                Output {
                    onetime_address: [2; 32],
                    amount_commitment: [3; 32],
                    amount_enc: [4; 8],
                    view_tag: [5; 3],
                    ephemeral_pubkey: [6; 32],
                    anchor_enc: [7; 16],
                };
                2
            ],
            fee: 1,
            extra: vec![],
        },
        prunable: Prunable {
            rings: vec![(0..16).collect()],
            proof_data: vec![9; proof],
        },
    };
    let block = |txs: Vec<Transaction>| Block {
        header: BlockHeader {
            version: VERSION,
            prev_id: [1; 32],
            timestamp: 1,
            tx_root: [2; 32],
            nonce: 3,
            mix: [4; 64],
        },
        coinbase: Coinbase {
            version: VERSION,
            height: 1,
            outputs: vec![CoinbaseOutput {
                onetime_address: [1; 32],
                amount: 1,
                view_tag: [1; 3],
                ephemeral_pubkey: [1; 32],
                anchor_enc: [1; 16],
            }],
            extra: vec![],
        },
        transactions: txs,
    };
    // one transaction of about 32 KiB, so 520 of them exceed 16 MiB and 400 do not
    let one = tx(32 * 1024).to_bytes().unwrap().len();
    let too_many = (MAX_FRAME / one) + 2;
    let big = Message::Blocks {
        blocks: vec![block(vec![tx(32 * 1024); too_many])],
    };
    assert!(
        matches!(encode(&big), Err(WireError::Encode(_))),
        "a {} byte block list must not encode",
        too_many * one
    );
    let fits = Message::Blocks {
        blocks: vec![block(vec![tx(32 * 1024); too_many - 40])],
    };
    let frame = encode(&fits).unwrap();
    assert!(frame.len() < MAX_FRAME + 4);
    assert_eq!(decode_frame(&frame).unwrap(), fits);
}

#[test]
fn the_encoder_accepts_a_frame_of_exactly_sixteen_mebibytes_and_refuses_one_byte_more() {
    use tenero_core::v2::*;
    let tx = |proof: usize| Transaction {
        prefix: TxPrefix {
            version: VERSION,
            inputs: vec![Input { key_image: [1; 32] }],
            outputs: vec![
                Output {
                    onetime_address: [2; 32],
                    amount_commitment: [3; 32],
                    amount_enc: [4; 8],
                    view_tag: [5; 3],
                    ephemeral_pubkey: [6; 32],
                    anchor_enc: [7; 16],
                };
                2
            ],
            fee: 1,
            extra: vec![],
        },
        prunable: Prunable {
            rings: vec![(0..16).collect()],
            proof_data: vec![9; proof],
        },
    };
    let block = |txs: Vec<Transaction>| Block {
        header: BlockHeader {
            version: VERSION,
            prev_id: [1; 32],
            timestamp: 1,
            tx_root: [2; 32],
            nonce: 3,
            mix: [4; 64],
        },
        coinbase: Coinbase {
            version: VERSION,
            height: 1,
            outputs: vec![CoinbaseOutput {
                onetime_address: [1; 32],
                amount: 1,
                view_tag: [1; 3],
                ephemeral_pubkey: [1; 32],
                anchor_enc: [1; 16],
            }],
            extra: vec![],
        },
        transactions: txs,
    };
    // frame length = kind byte + count (4) + the block's bytes
    let frame_len = |txs: &[Transaction]| 1 + 4 + block(txs.to_vec()).to_bytes().unwrap().len();
    let full = tx(32 * 1024);
    let base = tx(0).to_bytes().unwrap().len();
    let one = full.to_bytes().unwrap().len();
    // n full transactions, then one more whose proof is sized to land exactly on the cap
    let with_last = |n: usize, p: usize| {
        let mut txs = vec![full.clone(); n];
        txs.push(tx(p));
        txs
    };
    let (n, p) = (MAX_FRAME / one - 2..=MAX_FRAME / one)
        .find_map(|n| {
            let rem = MAX_FRAME as i64 - frame_len(&with_last(n, 0)) as i64;
            (0..=32 * 1024).contains(&rem).then_some((n, rem as usize))
        })
        .unwrap_or_else(|| panic!("no sizing lands inside one proof (base {base})"));
    let exact = with_last(n, p);
    assert_eq!(frame_len(&exact), MAX_FRAME);
    let msg = Message::Blocks {
        blocks: vec![block(exact)],
    };
    let frame = encode(&msg).expect("a frame of exactly the cap must encode");
    assert_eq!(frame.len(), MAX_FRAME + 4);
    assert_eq!(decode_frame(&frame).unwrap(), msg);
    let over = Message::Blocks {
        blocks: vec![block(with_last(n, p + 1))],
    };
    assert!(
        matches!(encode(&over), Err(WireError::Encode(_))),
        "one byte over the cap"
    );
}
