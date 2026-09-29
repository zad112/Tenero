//! matmulhash v2 against the golden vectors. The deep and full vectors are slow and big, so like the
//! Python suite they run only with `TENERO_SLOW_VECTORS=1` (the full one also needs 4.3 GiB of RAM).

use serde_json::Value;
use tenero_core::hash::{hex_lower, sha256};
use tenero_core::matmulhash::{self as mh, Dataset, Params};
use tenero_core::u256::U256;
use tenero_core::vectors::{hex, load};

const TWO_POW_256: &str =
    "115792089237316195423570985008687907853269984665640564039457584007913129639936";

fn slow() -> bool {
    std::env::var("TENERO_SLOW_VECTORS").is_ok_and(|v| v == "1")
}

fn bytes32(v: &Value) -> [u8; 32] {
    hex(v.as_str().unwrap()).unwrap().try_into().unwrap()
}

fn params_of(v: &Value) -> Params {
    let f = |k: &str| usize::try_from(v[k].as_u64().unwrap()).unwrap();
    let p = Params {
        m: f("m"),
        k: f("k"),
        nb: f("nb"),
        num_blocks: f("num_blocks"),
    };
    p.validate().unwrap();
    p
}

fn sha_hex(bytes: &[u8]) -> String {
    hex_lower(&sha256(&[bytes]))
}

/// Checks every step of one recorded attempt.
fn check_attempt(data: &Dataset, a: &Value, what: &str) {
    let header_hash = bytes32(&a["header_hash"]);
    let nonce = a["nonce"].as_u64().unwrap();
    let got = mh::compute_attempt(data, &header_hash, nonce).unwrap();
    assert_eq!(
        hex_lower(&got.seed),
        a["seed"].as_str().unwrap(),
        "{what}: seed"
    );
    assert_eq!(
        got.slice_index as u64,
        a["slice_index"].as_u64().unwrap(),
        "{what}: slice"
    );
    let p = data.params();
    let x = mh::make_x(&got.seed, p);
    let x_bytes: Vec<u8> = x.iter().map(|&v| v as u8).collect();
    assert_eq!(
        sha_hex(&x_bytes),
        a["x_sha256"].as_str().unwrap(),
        "{what}: X"
    );
    let c = mh::product(&x, data.slice(got.slice_index).unwrap(), p);
    let c_bytes: Vec<u8> = c.iter().flat_map(|v| v.to_le_bytes()).collect();
    assert_eq!(
        sha_hex(&c_bytes),
        a["c_sha256"].as_str().unwrap(),
        "{what}: C"
    );
    let sums: Vec<String> = got.sums.iter().map(|s| format!("{s:016x}")).collect();
    let want: Vec<&str> = a["sums"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert_eq!(sums, want, "{what}: fold sums");
    assert_eq!(
        hex_lower(&got.mix),
        a["mix"].as_str().unwrap(),
        "{what}: mix"
    );
    assert_eq!(
        hex_lower(&got.digest),
        a["digest"].as_str().unwrap(),
        "{what}: digest"
    );
}

#[test]
fn small_datasets_and_attempts() {
    let v = load("matmulhash_small").unwrap();
    for (n, case) in v["cases"].as_array().unwrap().iter().enumerate() {
        let p = params_of(&case["params"]);
        let seed = bytes32(&case["epoch_seed"]);
        assert_eq!(
            hex_lower(&mh::dataset_key(&seed)),
            case["dataset_key"].as_str().unwrap()
        );
        // one thread and several threads must give the same bytes
        let one = Dataset::build(&p, &seed, p.num_blocks, 1).unwrap();
        let many = Dataset::build(&p, &seed, p.num_blocks, 3).unwrap();
        assert_eq!(
            one.bytes(),
            many.bytes(),
            "case {n}: threads change the result"
        );
        let want = case["slice_sha256"].as_array().unwrap();
        assert_eq!(want.len(), p.num_blocks);
        for (j, w) in want.iter().enumerate() {
            assert_eq!(
                sha_hex(one.slice(j).unwrap()),
                w.as_str().unwrap(),
                "case {n} slice {j}"
            );
        }
        assert_eq!(
            sha_hex(one.bytes()),
            case["dataset_sha256"].as_str().unwrap(),
            "case {n}: dataset"
        );
        let attempts = case["attempts"].as_array().unwrap();
        assert!(!attempts.is_empty());
        for (i, a) in attempts.iter().enumerate() {
            check_attempt(&one, a, &format!("case {n} attempt {i}"));
        }
    }
}

#[test]
fn the_fold_on_its_own() {
    let v = load("matmulhash_small").unwrap();
    for (i, f) in v["fold"].as_array().unwrap().iter().enumerate() {
        let c: Vec<i32> = f["c"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| i32::try_from(x.as_i64().unwrap()).unwrap())
            .collect();
        let sums: Vec<String> = mh::fold_sums(&c)
            .iter()
            .map(|s| format!("{s:016x}"))
            .collect();
        let want: Vec<&str> = f["sums"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        assert_eq!(sums, want, "fold[{i}]");
    }
}

#[test]
fn real_parameters_first_eight_slices() {
    let v = load("matmulhash_real").unwrap();
    let p = params_of(&v["params"]);
    assert_eq!(p, Params::DEFAULT);
    let seed = bytes32(&v["epoch_seed"]);
    assert_eq!(
        hex_lower(&mh::dataset_key(&seed)),
        v["dataset_key"].as_str().unwrap()
    );
    let built = usize::try_from(v["slices_built"].as_u64().unwrap()).unwrap();
    let data = Dataset::build(&p, &seed, built, 4).unwrap();
    for (j, w) in v["slice_sha256"].as_object().unwrap() {
        let j: usize = j.parse().unwrap();
        assert_eq!(
            sha_hex(data.slice(j).unwrap()),
            w.as_str().unwrap(),
            "slice {j}"
        );
    }
    for (i, a) in v["attempts"].as_array().unwrap().iter().enumerate() {
        check_attempt(&data, a, &format!("real attempt {i}"));
    }
}

#[test]
fn deep_slices_up_to_77() {
    if !slow() {
        eprintln!("skipped: set TENERO_SLOW_VECTORS=1 (needs about 1.3 GiB of RAM)");
        return;
    }
    let v = load("matmulhash_deep").unwrap();
    let p = params_of(&v["params"]);
    let seed = bytes32(&v["epoch_seed"]);
    let built = usize::try_from(v["slices_built"].as_u64().unwrap()).unwrap();
    let data = Dataset::build(&p, &seed, built, 4).unwrap();
    for (j, w) in v["slice_sha256"].as_object().unwrap() {
        let j: usize = j.parse().unwrap();
        assert_eq!(
            sha_hex(data.slice(j).unwrap()),
            w.as_str().unwrap(),
            "slice {j}"
        );
    }
    for (i, a) in v["attempts"].as_array().unwrap().iter().enumerate() {
        check_attempt(&data, a, &format!("deep attempt {i}"));
    }
}

#[test]
fn full_dataset_every_slice() {
    if !slow() {
        eprintln!("skipped: set TENERO_SLOW_VECTORS=1 (needs about 4.3 GiB of RAM)");
        return;
    }
    let v = load("matmulhash_full").unwrap();
    let p = params_of(&v["params"]);
    let seed = bytes32(&v["epoch_seed"]);
    let data = Dataset::build(&p, &seed, p.num_blocks, 4).unwrap();
    let want = v["slice_sha256"].as_array().unwrap();
    assert_eq!(want.len(), p.num_blocks);
    for (j, w) in want.iter().enumerate() {
        assert_eq!(
            sha_hex(data.slice(j).unwrap()),
            w.as_str().unwrap(),
            "slice {j}"
        );
    }
    assert_eq!(sha_hex(data.bytes()), v["dataset_sha256"].as_str().unwrap());
}

// ------------------------------------------------------------------ pow_misc.json

#[test]
fn epoch_seeds() {
    let v = load("pow_misc").unwrap();
    assert_eq!(
        hex_lower(&sha256(&[v["epoch_seed_0_label"]
            .as_str()
            .unwrap()
            .as_bytes()])),
        hex_lower(&mh::epoch_seed(0))
    );
    for (e, s) in v["epoch_seeds"].as_object().unwrap() {
        let e: u64 = e.parse().unwrap();
        assert_eq!(
            hex_lower(&mh::epoch_seed(e)),
            s.as_str().unwrap(),
            "epoch {e}"
        );
    }
}

#[test]
fn epoch_numbering() {
    let v = load("pow_misc").unwrap();
    for c in v["epoch_of"].as_array().unwrap() {
        let (idx, eb) = (
            c["index"].as_u64().unwrap(),
            c["epoch_blocks"].as_u64().unwrap(),
        );
        assert_eq!(
            mh::epoch_of(idx, eb),
            c["epoch"].as_u64(),
            "index {idx}, epoch_blocks {eb}"
        );
    }
    assert_eq!(mh::epoch_of(0, 100), None);
    assert_eq!(mh::epoch_of(5, 0), None);
}

#[test]
fn bits_to_target() {
    let v = load("pow_misc").unwrap();
    for c in v["bits_to_target"].as_array().unwrap() {
        let bits = u32::try_from(c["bits"].as_u64().unwrap()).unwrap();
        let want = U256::from_dec_str(c["target"].as_str().unwrap()).unwrap();
        assert_eq!(mh::bits_to_target(bits), Some(want), "bits {bits}");
    }
    assert_eq!(mh::bits_to_target(0), None);
    assert_eq!(mh::bits_to_target(257), None);
    assert_eq!(mh::bits_to_target(256), U256::pow2(0));
}

#[test]
fn the_cheap_precheck() {
    let v = load("pow_misc").unwrap();
    let cases = v["precheck"].as_array().unwrap();
    assert!(cases.len() >= 11);
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let header_hash = bytes32(&c["header_hash"]);
        let mix = hex(c["mix"].as_str().unwrap()).unwrap();
        // The reference's "no limit" target is exactly 2^256, which is not a U256 (see `precheck`).
        let target_text = c["target"].as_str().unwrap();
        let target = if target_text == TWO_POW_256 {
            None
        } else {
            Some(U256::from_dec_str(target_text).unwrap())
        };
        // A nonce outside 0..2^64 cannot be represented, and is invalid by that rule.
        let got = match c["nonce"].as_u64() {
            Some(nonce) => mh::precheck(
                &header_hash,
                nonce,
                &mix,
                c["hash"].as_str().unwrap(),
                target.as_ref(),
            ),
            None => false,
        };
        assert_eq!(got, c["expect"].as_bool().unwrap(), "{name}");
    }
}
