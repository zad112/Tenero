//! Consensus arithmetic against the golden vectors: units, emission, difficulty, fees and size.

use serde_json::Value;
use tenero_core::difficulty::{self, DifficultyParams};
use tenero_core::emission::Emission;
use tenero_core::fees;
use tenero_core::u256::U256;
use tenero_core::units;
use tenero_core::vectors::load;

fn u(v: &Value) -> u64 {
    v.as_u64().unwrap()
}

// ------------------------------------------------------------------ units.json

/// Inputs the Python reference accepts and this implementation rejects on purpose (`to_units` docs).
const REJECTED_ON_PURPOSE: [&str; 4] = [" 3 ", "1e2", "1e-4", "+2"];

#[test]
fn units_parse() {
    let v = load("units").unwrap();
    for c in v["parse"].as_array().unwrap() {
        let text = c["text"].as_str().unwrap();
        let got = units::to_units(text);
        if c["error"].as_bool() == Some(true) {
            assert!(got.is_err(), "{text:?} should be an error, got {got:?}");
        } else if REJECTED_ON_PURPOSE.contains(&text) {
            assert!(got.is_err(), "{text:?} is rejected on purpose, got {got:?}");
        } else {
            assert_eq!(got, Ok(c["units"].as_i64().unwrap()), "{text:?}");
        }
    }
    // every deliberate rejection really is one of the vector's accepted inputs
    let accepted: Vec<&str> = v["parse"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["error"].as_bool() != Some(true))
        .map(|c| c["text"].as_str().unwrap())
        .collect();
    for text in REJECTED_ON_PURPOSE {
        assert!(accepted.contains(&text), "{text:?} is not in the vector");
    }
}

#[test]
fn units_format() {
    let v = load("units").unwrap();
    assert_eq!(u(&v["decimals"]) as usize, units::DECIMALS);
    for c in v["format"].as_array().unwrap() {
        let n = c["units"].as_i64().unwrap();
        assert_eq!(units::fmt(n), c["text"].as_str().unwrap(), "{n}");
        // what we print we can read back
        assert_eq!(units::to_units(&units::fmt(n)), Ok(n), "round trip of {n}");
    }
    assert_eq!(units::fmt(i64::MIN), "-922337203685477.5808");
    assert_eq!(units::to_units("922337203685477.5807"), Ok(i64::MAX));
    assert!(units::to_units("922337203685477.5808").is_err());
    assert_eq!(units::to_units("-922337203685477.5808"), Ok(i64::MIN));
}

#[test]
fn units_reject_malformed_text() {
    for bad in [
        "",
        "-",
        ".",
        "1.",
        ".5",
        "--1",
        "1..2",
        "1 .5",
        "0x1",
        "١",
        "1_0",
        "99999999999999999999999999",
    ] {
        assert!(units::to_units(bad).is_err(), "{bad:?}");
    }
}

// ------------------------------------------------------------------ emission.json

#[test]
fn emission_schedules() {
    let v = load("emission").unwrap();
    for (name, set) in v["sets"].as_object().unwrap() {
        let p = &set["params"];
        let e = Emission {
            initial_reward: u(&p["initial_reward"]),
            halving_interval: u(&p["halving_interval"]),
            max_supply: u(&p["max_supply"]),
            tail_reward: u(&p["tail_reward"]),
        };
        e.validate().unwrap();
        let rows = set["rows"].as_array().unwrap();
        assert!(rows.len() > 10, "{name}");
        for r in rows {
            let h = u(&r["height"]);
            let what = format!("{name} height {h}");
            assert_eq!(
                e.scheduled_reward(h),
                u(&r["scheduled"]),
                "{what}: scheduled"
            );
            assert_eq!(
                e.issued_before(h),
                u(&r["issued_before"]),
                "{what}: issued_before"
            );
            assert_eq!(
                e.main_reward_at(h),
                u(&r["main_reward"]),
                "{what}: main_reward"
            );
            assert_eq!(e.reward_at(h), u(&r["reward"]), "{what}: reward");
            assert_eq!(
                e.in_tail(h),
                r["in_tail"].as_bool().unwrap(),
                "{what}: in_tail"
            );
        }
        assert_eq!(
            e.main_emission_end(1),
            Some(u(&set["main_emission_end_from_1"])),
            "{name}: end of main emission"
        );
    }
}

/// The tail starts when the main reward would pay LESS than the tail, not when it equals it. No
/// vector row has `main_reward == tail_reward`, so this pins the boundary (found by mutation testing).
#[test]
fn the_tail_starts_only_when_the_main_reward_is_strictly_below_it() {
    let e = Emission {
        initial_reward: 8,
        halving_interval: 2,
        max_supply: 1_000_000,
        tail_reward: 2,
    };
    let rows: Vec<(u64, u64, bool)> = (1..=8)
        .map(|h| (e.main_reward_at(h), e.reward_at(h), e.in_tail(h)))
        .collect();
    assert_eq!(
        rows,
        [
            (8, 8, false),
            (8, 8, false),
            (4, 4, false),
            (4, 4, false),
            (2, 2, false), // equal to the tail: still the main emission
            (2, 2, false),
            (1, 2, true), // below the tail: the tail pays
            (1, 2, true),
        ]
    );
    // a tail of 0 is never "in the tail"
    let none = Emission {
        tail_reward: 0,
        ..e
    };
    assert!((1..=20).all(|h| !none.in_tail(h)));
}

#[test]
fn emission_default_schedule_facts() {
    // the facts CONSENSUS.md section 5 states in words
    let e = Emission {
        initial_reward: 200_000,
        halving_interval: 525_600,
        max_supply: 200_000_000_000,
        tail_reward: 5_000,
    };
    assert_eq!(e.main_emission_end(1), Some(2_334_401));
    assert_eq!(e.reward_at(2_334_400), 12_500); // the last main block pays a full 1.25
    assert_eq!(e.reward_at(2_334_401), 5_000); // the tail
    assert!(!e.in_tail(2_334_400) && e.in_tail(2_334_401));
    assert_eq!(e.issued_before(2_334_401), 200_000_000_000); // exactly the cap
                                                             // far in the future: the shift runs out, the tail stays, nothing overflows
    assert_eq!(e.scheduled_reward(u64::MAX), 0);
    assert_eq!(e.reward_at(u64::MAX), 5_000);
    assert_eq!(e.issued_before(u64::MAX), 200_000_000_000);
    assert_eq!(e.scheduled_reward(1), 200_000);
    assert_eq!(e.scheduled_reward(525_600), 200_000);
    assert_eq!(e.scheduled_reward(525_601), 100_000);
}

// ------------------------------------------------------------------ difficulty.json

fn i64s(v: &Value) -> Vec<i64> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_i64().unwrap())
        .collect()
}

fn target(v: &Value) -> U256 {
    U256::from_dec_str(v.as_str().unwrap()).unwrap()
}

#[test]
fn difficulty_scenarios() {
    let v = load("difficulty").unwrap();
    let scenarios = v["scenarios"].as_array().unwrap();
    assert!(scenarios.len() >= 17);
    for s in scenarios {
        let name = s["name"].as_str().unwrap();
        let p = DifficultyParams {
            block_time: u(&s["params"]["block_time"]),
            window: u(&s["params"]["window"]),
            start_target: target(&s["params"]["start_target"]),
        };
        let timestamps = i64s(&s["timestamps"]);
        let want: Vec<U256> = s["required_targets"]
            .as_array()
            .unwrap()
            .iter()
            .map(target)
            .collect();
        let got = difficulty::required_targets(&p, &timestamps).unwrap();
        assert_eq!(got.len(), timestamps.len() + 1, "{name}");
        for (i, (g, w)) in got.iter().zip(&want).enumerate() {
            assert_eq!(g, w, "{name}: required target of block {}", i + 1);
        }
        assert_eq!(got, want, "{name}");
        // the timestamp rule applies only when the adjustment is on
        if p.window > 0 {
            assert_eq!(
                difficulty::median_times(&timestamps),
                i64s(&s["median_times"]),
                "{name}: median times"
            );
        }
    }
}

#[test]
fn difficulty_edges() {
    let p = DifficultyParams {
        block_time: 60,
        window: 30,
        start_target: U256::pow2(240).unwrap(),
    };
    // no blocks yet: the next block (1) and the one after (2) use the starting target
    assert_eq!(
        difficulty::required_targets(&p, &[]).unwrap(),
        vec![p.start_target]
    );
    assert_eq!(
        difficulty::required_targets(&p, &[100]).unwrap(),
        vec![p.start_target; 2]
    );
    // fixed difficulty ignores timestamps altogether
    let fixed = DifficultyParams { window: 0, ..p };
    assert_eq!(
        difficulty::required_targets(&fixed, &[5, 1, 999_999]).unwrap(),
        vec![p.start_target; 4]
    );
    // the target never goes below 1 or above 2^256 - 1, however fast or slow the blocks are
    let tiny = DifficultyParams {
        start_target: U256::ONE,
        ..p
    };
    let fast: Vec<i64> = (1..=40).map(|i| 1_700_000_000 + i).collect();
    assert!(difficulty::required_targets(&tiny, &fast)
        .unwrap()
        .iter()
        .all(|t| *t >= U256::ONE));
    let huge = DifficultyParams {
        start_target: U256::MAX,
        ..p
    };
    let slow: Vec<i64> = (1..=40).map(|i| 1_700_000_000 + i * 100_000).collect();
    assert!(difficulty::required_targets(&huge, &slow)
        .unwrap()
        .iter()
        .all(|t| *t <= U256::MAX));
    // a zero block time is an error, not a division by zero
    assert!(difficulty::required_targets(&DifficultyParams { block_time: 0, ..p }, &fast).is_err());
}

#[test]
fn median_time_window() {
    assert_eq!(difficulty::median_time(&[0], 1), 0); // nothing before block 1 but the genesis
    assert_eq!(difficulty::median_times(&[]), vec![0]);
    // upper median: of [10, 20] the element at index 1
    assert_eq!(difficulty::median_time(&[0, 10, 20], 3), 20);
    // only the last 11 blocks count
    let mut ts = vec![0i64];
    ts.extend((1..=30).map(|i| i * 10));
    assert_eq!(difficulty::median_time(&ts, 31), 250); // blocks 20..=30: median block 25
}

// ------------------------------------------------------------------ fees_and_size.json

#[test]
fn fee_constants_match_the_vector() {
    let v = load("fees_and_size").unwrap();
    let c = &v["constants"];
    assert_eq!(u(&c["default_min_block_median"]), fees::MIN_BLOCK_MEDIAN);
    assert_eq!(u(&c["median_window"]) as usize, fees::MEDIAN_WINDOW);
    assert_eq!(
        u(&c["min_fee_rate_units_per_1000_bytes"]),
        fees::MIN_FEE_RATE_UNITS_PER_1000_BYTES
    );
}

#[test]
fn minimum_fee() {
    let v = load("fees_and_size").unwrap();
    for c in v["min_fee"].as_array().unwrap() {
        let size = u(&c["size"]);
        assert_eq!(
            fees::min_fee(size, fees::MIN_FEE_RATE_UNITS_PER_1000_BYTES),
            Some(u(&c["fee"])),
            "size {size}"
        );
    }
    assert_eq!(fees::min_fee(u64::MAX, 100).map(|f| f > 1), Some(true));
    assert_eq!(fees::min_fee(u64::MAX, u64::MAX), None); // does not fit, so an error, not a wrap
}

#[test]
fn oversize_penalty() {
    let v = load("fees_and_size").unwrap();
    for c in v["penalty"].as_array().unwrap() {
        let (base, size, median) = (u(&c["base"]), u(&c["size"]), u(&c["median"]));
        assert_eq!(
            fees::penalty(base, size, median),
            Ok(u(&c["penalty"])),
            "base {base} size {size} median {median}"
        );
    }
    // exactly the whole reward at the hard limit, and the hard limit itself
    assert_eq!(fees::penalty(200_000, 600_000, 300_000), Ok(200_000));
    assert!(!fees::over_hard_limit(600_000, 300_000));
    assert!(fees::over_hard_limit(600_001, 300_000));
    // no room for base * over^2 in 128 bits: an error, not a wrap
    assert!(fees::penalty(u64::MAX, u64::MAX, 1).is_err());
    assert!(fees::penalty(1, 5, 0).is_err());
}

#[test]
fn block_size_median() {
    let v = load("fees_and_size").unwrap();
    for c in v["median"].as_array().unwrap() {
        let sizes: Vec<u64> = c["sizes"].as_array().unwrap().iter().map(u).collect();
        assert_eq!(
            fees::median(&sizes, u(&c["floor"])),
            u(&c["median"]),
            "{sizes:?}"
        );
    }
    let h = &v["median_history"];
    let sizes: Vec<u64> = h["sizes_by_position_0_is_genesis"]
        .as_array()
        .unwrap()
        .iter()
        .map(u)
        .collect();
    for c in h["windowed"].as_array().unwrap() {
        let pos = u(&c["pos"]) as usize;
        assert_eq!(
            fees::median_at(&sizes, pos, fees::MIN_BLOCK_MEDIAN),
            u(&c["median"]),
            "pos {pos}"
        );
    }
    assert_eq!(fees::median_at(&[], 1, 300_000), 300_000);
}
