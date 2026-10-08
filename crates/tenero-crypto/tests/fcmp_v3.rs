//! Version 3 transaction proofs end to end (`tenero_crypto::fcmp`): a transaction proven against a real curve tree
//! verifies, alone and in a batch with others, and is refused when anything the proofs bind is changed.

use curve25519_dalek::edwards::EdwardsPoint;
use curve25519_dalek::scalar::Scalar;
use monero_ed25519::CompressedPoint;
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use tenero_core::v3::{Input, Output, Prunable, Transaction, TxPrefix, VERSION};
use tenero_crypto::curve_tree::{CurveTree, Leaf};
use tenero_crypto::fcmp::{
    key_image, proof_data_size, prove, verify_tx, Batch, OutputSecret, ProofError, Spend,
};

fn t() -> EdwardsPoint {
    CompressedPoint::T.decompress().unwrap().into()
}

fn h() -> EdwardsPoint {
    CompressedPoint::H.decompress().unwrap().into()
}

fn scalar(rng: &mut ChaCha20Rng) -> Scalar {
    let mut w = [0u8; 64];
    rng.fill_bytes(&mut w);
    Scalar::from_bytes_mod_order_wide(&w)
}

fn point(rng: &mut ChaCha20Rng) -> [u8; 32] {
    EdwardsPoint::mul_base(&scalar(rng)).compress().to_bytes()
}

/// An output we own: key `x G + y T`, commitment `z G + a H`.
struct Owned {
    x: Scalar,
    y: Scalar,
    z: Scalar,
    a: u64,
    leaf: Leaf,
    ko: [u8; 32],
}

fn owned(rng: &mut ChaCha20Rng, a: u64) -> Owned {
    let (x, y, z) = (scalar(rng), scalar(rng), scalar(rng));
    let ko = (EdwardsPoint::mul_base(&x) + t() * y).compress().to_bytes();
    let c = (EdwardsPoint::mul_base(&z) + h() * Scalar::from(a))
        .compress()
        .to_bytes();
    Owned {
        x,
        y,
        z,
        a,
        leaf: Leaf::from_output(&ko, &c).unwrap(),
        ko,
    }
}

struct World {
    tree: CurveTree,
    leaves: Vec<Leaf>,
    chain_id: [u8; 32],
}

/// A tree of `n` random outputs with `ours` at `positions`.
fn world(rng: &mut ChaCha20Rng, n: usize, ours: &[&Owned], positions: &[usize]) -> World {
    let mut leaves: Vec<Leaf> = (0..n)
        .map(|_| Leaf::from_output(&point(rng), &point(rng)).unwrap())
        .collect();
    for (o, p) in ours.iter().zip(positions) {
        leaves[*p] = o.leaf;
    }
    let mut tree = CurveTree::new();
    tree.grow(&leaves);
    World {
        tree,
        leaves,
        chain_id: [42; 32],
    }
}

/// A transaction spending `spent` (at `positions`) to two new outputs, proven.
fn spend(
    rng: &mut ChaCha20Rng,
    w: &World,
    spent: &[&Owned],
    positions: &[usize],
    fee: u64,
) -> Transaction {
    let total: u64 = spent.iter().map(|o| o.a).sum::<u64>() - fee;
    let outs = [(total / 3, scalar(rng)), (total - total / 3, scalar(rng))];
    let mut outputs: Vec<(Output, OutputSecret)> = outs
        .iter()
        .map(|(a, m)| {
            let c = (EdwardsPoint::mul_base(m) + h() * Scalar::from(*a))
                .compress()
                .to_bytes();
            (
                Output {
                    onetime_address: point(rng),
                    amount_commitment: c,
                    amount_enc: [1; 8],
                    view_tag: [2; 3],
                    anchor_enc: [3; 16],
                },
                OutputSecret {
                    amount: *a,
                    mask: *m,
                },
            )
        })
        .collect();
    outputs.sort_by_key(|(o, _)| o.onetime_address);
    // inputs in key-image order
    let mut ins: Vec<(&Owned, usize, [u8; 32])> = spent
        .iter()
        .zip(positions)
        .map(|(o, p)| (*o, *p, key_image(&o.x, &o.ko)))
        .collect();
    ins.sort_by_key(|(_, _, ki)| *ki);
    let mut tx = Transaction {
        prefix: TxPrefix {
            version: VERSION,
            inputs: ins
                .iter()
                .map(|(_, _, ki)| Input { key_image: *ki })
                .collect(),
            outputs: outputs.iter().map(|(o, _)| o.clone()).collect(),
            ephemeral_pubkeys: vec![point(rng)],
            fee,
            encrypted_payment_id: [9; 8],
        },
        prunable: Prunable {
            reference_height: 100,
            proof_data: vec![],
        },
    };
    let spends: Vec<Spend> = ins
        .iter()
        .map(|(o, p, _)| Spend {
            x: o.x,
            y: o.y,
            mask: o.z,
            amount: o.a,
            leaf: o.leaf,
            path: w.tree.path(*p as u64, |i| w.leaves[i as usize]).unwrap(),
        })
        .collect();
    let secrets: Vec<OutputSecret> = outputs.into_iter().map(|(_, s)| s).collect();
    tx.prunable.proof_data =
        prove(rng, &w.chain_id, &tx, &spends, &secrets, w.tree.n_layers()).expect("proves");
    tx
}

fn check(w: &World, tx: &Transaction) -> Result<(), ProofError> {
    verify_tx(&w.chain_id, tx, w.tree.root().unwrap(), w.tree.n_layers())
}

#[test]
fn a_transaction_proven_against_the_tree_verifies() {
    let mut rng = ChaCha20Rng::seed_from_u64(1);
    let (a, b) = (owned(&mut rng, 1_000_000), owned(&mut rng, 2_500_000));
    let w = world(&mut rng, 120, &[&a, &b], &[7, 90]);
    let one = spend(&mut rng, &w, &[&a], &[7], 1_000);
    assert_eq!(check(&w, &one), Ok(()));
    let two = spend(&mut rng, &w, &[&a, &b], &[7, 90], 2_345);
    assert_eq!(check(&w, &two), Ok(()));
    // the size a wallet computes before proving is the size of the proof
    let layers = w.tree.n_layers();
    assert_eq!(one.prunable.proof_data.len(), proof_data_size(1, 2, layers));
    assert_eq!(two.prunable.proof_data.len(), proof_data_size(2, 2, layers));
    // both in one batch, as a block
    let mut batch = Batch::new();
    batch
        .add(&w.chain_id, &one, w.tree.root().unwrap(), w.tree.n_layers())
        .unwrap();
    batch
        .add(&w.chain_id, &two, w.tree.root().unwrap(), w.tree.n_layers())
        .unwrap();
    assert!(batch.finish());
}

#[test]
fn changing_anything_the_proofs_bind_is_refused() {
    let mut rng = ChaCha20Rng::seed_from_u64(2);
    let a = owned(&mut rng, 5_000_000);
    let w = world(&mut rng, 60, &[&a], &[33]);
    let tx = spend(&mut rng, &w, &[&a], &[33], 10_000);
    assert_eq!(check(&w, &tx), Ok(()));

    let mut bad = tx.clone();
    bad.prefix.fee += 1;
    assert_eq!(check(&w, &bad), Err(ProofError::Balance), "the fee");

    let mut bad = tx.clone();
    bad.prefix.encrypted_payment_id[0] ^= 1;
    assert_eq!(
        check(&w, &bad),
        Err(ProofError::Membership),
        "the payment ID (in the signed message)"
    );

    let mut bad = tx.clone();
    bad.prefix.ephemeral_pubkeys[0][0] ^= 1;
    assert_eq!(
        check(&w, &bad),
        Err(ProofError::Membership),
        "the ephemeral key"
    );

    let mut bad = tx.clone();
    bad.prunable.reference_height += 1;
    assert_eq!(
        check(&w, &bad),
        Err(ProofError::Membership),
        "the reference height"
    );

    let mut bad = tx.clone();
    bad.prefix.outputs[0].amount_enc[0] ^= 1;
    assert_eq!(
        check(&w, &bad),
        Err(ProofError::Membership),
        "an output's encrypted amount"
    );

    let mut bad = tx.clone();
    bad.prefix.inputs[0].key_image = point(&mut rng);
    assert_eq!(
        check(&w, &bad),
        Err(ProofError::Membership),
        "another key image"
    );

    let mut bad = tx.clone();
    bad.prunable.proof_data.push(0);
    assert!(
        matches!(check(&w, &bad), Err(ProofError::Malformed(_))),
        "a byte too many"
    );

    let mut bad = tx.clone();
    let last = bad.prunable.proof_data.len() - 1;
    bad.prunable.proof_data[last] ^= 1;
    assert!(check(&w, &bad).is_err(), "a flipped proof byte");

    // another chain
    let other = World {
        chain_id: [43; 32],
        ..w
    };
    assert_eq!(
        check(&other, &tx),
        Err(ProofError::Membership),
        "another chain"
    );
}

#[test]
fn a_proof_against_another_tree_is_refused() {
    let mut rng = ChaCha20Rng::seed_from_u64(3);
    let a = owned(&mut rng, 9_000);
    let w = world(&mut rng, 50, &[&a], &[3]);
    let tx = spend(&mut rng, &w, &[&a], &[3], 1);
    let mut bigger = w.tree.clone();
    bigger.grow(&[Leaf::from_output(&point(&mut rng), &point(&mut rng)).unwrap()]);
    assert!(verify_tx(&w.chain_id, &tx, bigger.root().unwrap(), bigger.n_layers()).is_err());
}

#[test]
fn one_bad_transaction_fails_the_whole_batch() {
    let mut rng = ChaCha20Rng::seed_from_u64(4);
    let (a, b) = (owned(&mut rng, 7_000), owned(&mut rng, 8_000));
    let w = world(&mut rng, 40, &[&a, &b], &[1, 2]);
    let good = spend(&mut rng, &w, &[&a], &[1], 5);
    let mut bad = spend(&mut rng, &w, &[&b], &[2], 5);
    let n = bad.prunable.proof_data.len();
    bad.prunable.proof_data[n - 40] ^= 0x10;
    let mut batch = Batch::new();
    batch
        .add(
            &w.chain_id,
            &good,
            w.tree.root().unwrap(),
            w.tree.n_layers(),
        )
        .unwrap();
    let _ = batch.add(&w.chain_id, &bad, w.tree.root().unwrap(), w.tree.n_layers());
    assert!(!batch.finish());
}
