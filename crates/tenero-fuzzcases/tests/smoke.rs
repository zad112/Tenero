//! The fuzz bodies run on the seeds and on pseudo-random bytes, in every ordinary `cargo test`, so that a body that no longer compiles or no
//! longer holds on honest input is found without a fuzzer. A crash a fuzzer finds becomes a case in `regressions.rs`.

use tenero_fuzzcases::{
    control_bodies, decode_v2, engine_messages, fixture, op, record, seeds, wire_stream,
};

fn bytes(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

#[test]
fn every_seed_runs_through_its_target() {
    let all = seeds();
    for target in ["decode_v2", "wire_stream", "engine_messages"] {
        assert!(
            all.iter().any(|(t, _, _)| *t == target),
            "no seed for {target}"
        );
    }
    for (target, name, data) in &all {
        match *target {
            "decode_v2" => {
                decode_v2(data);
                control_bodies(data);
            }
            "wire_stream" => wire_stream(data),
            "engine_messages" => {
                engine_messages(data);
            }
            other => panic!("{other} {name}"),
        }
    }
}

#[test]
fn the_decoders_and_the_stream_survive_pseudo_random_bytes() {
    for seed in 1..=400u64 {
        let n = (seed as usize * 7) % 900;
        let b = bytes(seed, n);
        decode_v2(&b);
        control_bodies(&b);
        wire_stream(&b);
    }
}

#[test]
fn the_engine_survives_pseudo_random_records() {
    for seed in 1..=60u64 {
        let _ = engine_messages(&bytes(seed, 40 + (seed as usize % 7) * 90));
    }
}

#[test]
fn the_engine_target_reaches_a_sync_from_its_seed() {
    // an honest peer, an honest hello and the chain's ids and blocks: the engine must have asked and applied (this checks that the seed is
    // good: a fuzzer that starts from a seed that does nothing starts from nothing)
    let fx = fixture();
    let seed = seeds()
        .into_iter()
        .find(|(t, n, _)| *t == "engine_messages" && n == "an honest sync")
        .unwrap()
        .2;
    let stats = engine_messages(&seed);
    eprintln!("{stats:?}");
    assert!(
        stats.blocks_applied >= 1,
        "the seed applied no block: {stats:?}"
    );
    assert_eq!(fx.ids.len(), 15);
    // and the records are what the doc says
    let r = record(op::FRAME, 3, &[1, 2, 3]);
    assert_eq!(r, vec![1, 3, 3, 0, 1, 2, 3]);
}

#[test]
fn an_input_cut_short_is_not_a_panic() {
    let _ = engine_messages(&[]);
    let _ = engine_messages(&[0]);
    let _ = engine_messages(&[0, 1, 2]);
    let _ = engine_messages(&[0, op::FRAME, 0, 255, 255, 1]); // a length that runs past the end
    wire_stream(&[]);
    wire_stream(&[3]);
    decode_v2(&[]);
    control_bodies(&[]);
}
