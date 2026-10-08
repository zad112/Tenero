//! Monero's own FCMP++ test proofs (`tests/vectors/upstream_monero_fcmp_pp.json`, imported verbatim by
//! `reference/tools/import_upstream_fcmp_pp.py` from Monero's FCMP++ stressnet release). They show that the pinned
//! `monero-fcmp-plus-plus` accepts proofs that Monero's code made, and refuses them once anything is changed
//! (`docs/FCMP_CARROT_PLAN.md`, milestone G1). The FCMP++ crates are only partly audited; this checks agreement with
//! Monero, not security.
//!
//! `cargo test --release -p tenero-crypto --test upstream_fcmp_pp -- --ignored --nocapture` prints how long
//! verification takes on this machine.

use std::time::Instant;

use ciphersuite::Ciphersuite;
use dalek_ff_group::Ed25519;
use helioselene::{Helios, Selene};
use monero_fcmp_plus_plus::fcmps::TreeRoot;
use monero_fcmp_plus_plus::{FcmpPlusPlus, HELIOS_FCMP_GENERATORS, SELENE_FCMP_GENERATORS};
use rand_core::OsRng;
use tenero_core::vectors::{hex, load};

#[derive(Clone)]
struct Case {
    inputs: usize,
    n_layers: usize,
    signable_tx_hash: [u8; 32],
    tree_root: [u8; 32],
    pseudo_outs: Vec<[u8; 32]>,
    key_images: Vec<[u8; 32]>,
    proof: Vec<u8>,
}

fn key(v: &str) -> [u8; 32] {
    hex(v).unwrap().try_into().unwrap()
}

fn cases() -> Vec<Case> {
    let file = load("upstream_monero_fcmp_pp").unwrap();
    file["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let field = |n: &str| key(c[n].as_str().unwrap());
            let list = |n: &str| -> Vec<[u8; 32]> {
                c[n].as_array()
                    .unwrap()
                    .iter()
                    .map(|x| key(x.as_str().unwrap()))
                    .collect()
            };
            Case {
                inputs: c["inputs"].as_u64().unwrap() as usize,
                n_layers: c["n_layers"].as_u64().unwrap() as usize,
                signable_tx_hash: field("signable_tx_hash"),
                tree_root: field("tree_root"),
                pseudo_outs: list("pseudo_outs"),
                key_images: list("key_images"),
                proof: hex(c["proof"].as_str().unwrap()).unwrap(),
            }
        })
        .collect()
}

/// The tree root: a Selene point when the number of layers is odd, a Helios point when it is even (as Monero reads it).
fn tree_root(c: &Case) -> Option<TreeRoot<Selene, Helios>> {
    let mut bytes = c.tree_root.as_slice();
    if c.n_layers % 2 == 1 {
        Some(TreeRoot::C1(
            <Selene as Ciphersuite>::read_G(&mut bytes).ok()?,
        ))
    } else {
        Some(TreeRoot::C2(
            <Helios as Ciphersuite>::read_G(&mut bytes).ok()?,
        ))
    }
}

/// Verifies the cases as ONE batch, as a node verifies a block. Anything that does not even parse is a failure.
fn verify_batch(batch: &[Case]) -> bool {
    let n: usize = batch.iter().map(|c| c.inputs).sum();
    let mut ed = multiexp::BatchVerifier::new(n);
    let mut c1 = generalized_bulletproofs::Generators::batch_verifier();
    let mut c2 = generalized_bulletproofs::Generators::batch_verifier();
    for c in batch {
        let mut reader = c.proof.as_slice();
        let Ok(proof) = FcmpPlusPlus::read(&c.pseudo_outs, c.n_layers, &mut reader) else {
            return false;
        };
        if !reader.is_empty() {
            return false;
        }
        let Some(root) = tree_root(c) else {
            return false;
        };
        let mut key_images = Vec::with_capacity(c.key_images.len());
        for k in &c.key_images {
            let Ok(p) = <Ed25519 as Ciphersuite>::read_G(&mut k.as_slice()) else {
                return false;
            };
            key_images.push(p);
        }
        if proof
            .verify(
                &mut OsRng,
                &mut ed,
                &mut c1,
                &mut c2,
                root,
                c.n_layers,
                c.signable_tx_hash,
                key_images,
            )
            .is_err()
        {
            return false;
        }
    }
    ed.verify_vartime()
        && SELENE_FCMP_GENERATORS.generators.verify(c1)
        && HELIOS_FCMP_GENERATORS.generators.verify(c2)
}

#[test]
fn the_file_holds_the_five_proofs_of_monero_s_test() {
    let all = cases();
    let shape: Vec<(usize, usize)> = all.iter().map(|c| (c.inputs, c.n_layers)).collect();
    assert_eq!(shape, [(1, 7), (2, 7), (4, 7), (8, 7), (128, 7)]);
    for c in &all {
        assert_eq!(c.pseudo_outs.len(), c.inputs);
        assert_eq!(c.key_images.len(), c.inputs);
        assert_eq!(
            c.proof.len(),
            FcmpPlusPlus::proof_size(c.inputs, c.n_layers),
            "{} inputs",
            c.inputs
        );
    }
}

#[test]
fn every_proof_of_monero_s_verifies_alone() {
    for c in cases() {
        assert!(
            verify_batch(std::slice::from_ref(&c)),
            "the {}-input proof must verify",
            c.inputs
        );
    }
}

#[test]
fn all_the_proofs_verify_as_one_batch() {
    assert!(verify_batch(&cases()));
}

#[test]
fn a_changed_byte_anywhere_in_the_proof_is_refused() {
    // the 1-input proof: a byte in each part (the input tuple, the spend-authorisation proof, the membership proof)
    let c = cases().remove(0);
    let len = c.proof.len();
    for at in [0, 31, 64, 100, 200, 500, 1000, len / 2, len - 33, len - 1] {
        let mut bad = c.clone();
        bad.proof[at] ^= 1;
        assert!(
            !verify_batch(&[bad]),
            "a flipped bit at byte {at} of {len} must be refused"
        );
    }
}

#[test]
fn a_bad_proof_spoils_a_batch_of_good_ones() {
    let mut all = cases();
    all[1].proof[40] ^= 0x80;
    assert!(!verify_batch(&all));
}

#[test]
fn the_proof_is_bound_to_the_transaction_the_tree_the_key_images_and_the_pseudo_outputs() {
    let all = cases();
    let one = &all[0];
    let two = &all[1];

    let mut c = one.clone();
    c.signable_tx_hash[0] ^= 1;
    assert!(!verify_batch(&[c]), "another transaction");

    let mut c = one.clone();
    c.tree_root = two.tree_root;
    if one.tree_root != two.tree_root {
        assert!(!verify_batch(&[c]), "another tree");
    }

    let mut c = two.clone();
    c.key_images.swap(0, 1);
    assert!(!verify_batch(&[c]), "the key images in another order");

    // Monero's test files spend some of the same outputs (the 1-input proof's key image is also the 2-input proof's
    // first), so take a key image that the 2-input proof does not have
    let other = all
        .iter()
        .flat_map(|x| x.key_images.iter())
        .find(|k| !two.key_images.contains(k))
        .unwrap();
    let mut c = two.clone();
    c.key_images[0] = *other;
    assert!(!verify_batch(&[c]), "another key image");

    let mut c = two.clone();
    c.pseudo_outs.swap(0, 1);
    assert!(!verify_batch(&[c]), "the pseudo-outputs in another order");

    let mut c = one.clone();
    c.n_layers -= 1;
    assert!(!verify_batch(&[c]), "a wrong number of layers");
}

/// Times verification (build with `--release`; the number means nothing in a debug build). Rule 5: a measured number from
/// the machine that ran it, nothing more.
#[test]
#[ignore]
fn time_verification() {
    let all = cases();
    // the generators are built on first use (a one-time cost of a process, not of a proof)
    let start = Instant::now();
    assert!(verify_batch(&all[..1]));
    println!(
        "first verification of the process (builds the generators): {:.2?}",
        start.elapsed()
    );
    let runs = 5;
    for c in &all {
        let start = Instant::now();
        for _ in 0..runs {
            assert!(verify_batch(std::slice::from_ref(c)));
        }
        let per = start.elapsed() / runs;
        println!(
            "FCMP++ verify, {:>3} inputs, {} layers, {:>6} bytes: {:>9.2?} per proof, {:>8.2?} per input",
            c.inputs,
            c.n_layers,
            c.proof.len(),
            per,
            per / c.inputs as u32
        );
    }
    let ones: Vec<Case> = std::iter::repeat_n(all[0].clone(), 16).collect();
    let start = Instant::now();
    assert!(verify_batch(&ones));
    let per = start.elapsed() / 16;
    println!("FCMP++ verify, 16 one-input proofs as one batch: {per:.2?} per proof");
    println!("(threads: one; batch verification as a node does it for a block)");
}

/// The exact size of an FCMP++ proof by inputs and tree layers (`FcmpPlusPlus::proof_size`), and the most inputs that fit in
/// today's limits: `MAX_PROOF` (64 KiB, which must also hold the pseudo-outputs and the range proof) and `MAX_TX_SIZE`.
/// Printed for `docs/FCMP_CARROT_PLAN.md` 4.2; the version 3 limits are set from it in G4.
#[test]
#[ignore]
fn print_proof_sizes() {
    use tenero_core::v2::types::{MAX_PROOF, MAX_TX_SIZE};
    println!("layers | 1 input | 2 inputs | 4 inputs | 8 inputs | 16 inputs | most inputs under MAX_PROOF ({MAX_PROOF}) by the FCMP++ proof alone");
    for layers in 1..=12 {
        let size = |n| FcmpPlusPlus::proof_size(n, layers);
        let most = (1..=1024)
            .take_while(|&n| size(n) <= MAX_PROOF)
            .last()
            .unwrap_or(0);
        println!(
            "{layers:>6} | {:>7} | {:>8} | {:>8} | {:>8} | {:>9} | {most}",
            size(1),
            size(2),
            size(4),
            size(8),
            size(16)
        );
    }
    println!("(MAX_TX_SIZE is {MAX_TX_SIZE} bytes for the whole transaction)");
}
