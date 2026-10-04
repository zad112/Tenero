//! The fuzz bodies run on the seeds and on pseudo-random bytes, in every ordinary `cargo test`, so that a body that no longer compiles or no
//! longer holds on honest input is found without a fuzzer. A crash a fuzzer finds becomes a case in `regressions.rs`.

use tenero_fuzzcases::{
    control_bodies, decode_v2, engine_messages, fixture, noise_handshake, op, record, seeds,
    wallet_proofs, wire_stream,
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
    for target in [
        "decode_v2",
        "wire_stream",
        "engine_messages",
        "wallet_proofs",
        "noise_handshake",
    ] {
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
            "wallet_proofs" => wallet_proofs(data),
            "engine_messages" => {
                engine_messages(data);
            }
            "noise_handshake" => noise_handshake(data),
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
        wallet_proofs(&b);
    }
}

#[test]
fn the_proof_checker_survives_every_single_byte_change_of_an_honest_proof() {
    let f = tenero_fuzzcases::proof_fixture();
    for p in &f.proofs {
        let good = p.to_bytes();
        for i in 0..good.len() {
            for flip in [1u8, 0x80, 0xff] {
                let mut bad = good.clone();
                bad[i] ^= flip;
                wallet_proofs(&bad);
            }
        }
        // and an honest one is checked, not just survived
        assert!(tenero_wallet::proofs::check(p, &f.out).is_ok());
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

/// The engine target keeps one store from input to input (it was 22 ms an input on Windows to make one). That is only sound if an input can
/// never depend on the ones before it: every seed, run in a thread that has already run all the others (in a shuffled order, so that blocks
/// were applied and wound back many times), must give exactly what it gives in a fresh thread with a fresh store.
#[test]
fn the_engine_target_gives_the_same_answer_after_other_inputs_as_alone() {
    let seeds: Vec<Vec<u8>> = seeds()
        .into_iter()
        .filter(|(t, _, _)| *t == "engine_messages")
        .map(|(_, _, d)| d)
        .collect();
    assert!(seeds.len() >= 4);
    let alone: Vec<String> = seeds
        .iter()
        .map(|d| {
            let d = d.clone();
            std::thread::spawn(move || format!("{:?}", engine_messages(&d)))
                .join()
                .unwrap()
        })
        .collect();
    let after_others = {
        let seeds = seeds.clone();
        std::thread::spawn(move || {
            // pseudo-random garbage, too, between the seeds
            let mut out = vec![String::new(); seeds.len()];
            for round in 0..6usize {
                for k in 0..seeds.len() {
                    let i = (k * 3 + round * 5) % seeds.len();
                    out[i] = format!("{:?}", engine_messages(&seeds[i]));
                    engine_messages(&bytes((round * 31 + k) as u64 + 1, 200 + k * 40));
                }
            }
            out
        })
        .join()
        .unwrap()
    };
    assert_eq!(alone, after_others);
}

/// The channel target on every kind of tampering, with payloads of many sizes (the body panics when a damaged chunk is accepted or an honest one is
/// refused), and on pseudo-random bytes in every mode.
#[test]
fn the_noise_target_refuses_every_tampering_of_an_honest_chunk_and_every_made_up_message() {
    for op in 0..6u8 {
        for (i, n) in [1usize, 2, 15, 16, 17, 100, 1000, 2999, 3000, 5000]
            .into_iter()
            .enumerate()
        {
            for (a, b) in [(0u8, 0u8), (1, 0), (7, 3), (255, 255), (16, 1)] {
                let mut v = vec![3, op, a, b];
                v.extend(bytes((op as u64) * 100 + i as u64 + 1, n));
                noise_handshake(&v);
            }
        }
    }
    for seed in 1..=300u64 {
        let mut v = vec![(seed % 4) as u8];
        v.extend(bytes(seed, (seed as usize * 5) % 400));
        noise_handshake(&v);
    }
    noise_handshake(&[]);
    noise_handshake(&[3]);
    noise_handshake(&[3, 0, 0]);
}
