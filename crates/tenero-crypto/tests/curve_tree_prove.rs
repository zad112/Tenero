//! End to end: real FCMP++ proofs made from OUR tree's paths verify against OUR tree's root, for trees of one to four
//! layers and for several inputs at once (in one leaf chunk and in different ones); and a proof against another tree's
//! root does not. Monero agreeing on the root (`curve_tree_monero.rs`) plus this means a wallet's proof made with our
//! tree verifies on a node that keeps our tree.

use std::sync::LazyLock;

use ciphersuite::group::ff::Field;
use ciphersuite::group::Group;
use ciphersuite::Ciphersuite;
use dalek_ff_group::{EdwardsPoint, Scalar};
use ec_divisors::ScalarDecomposition;
use helioselene::{Helios, Selene};
use monero_ed25519::CompressedPoint;
use monero_fcmp_plus_plus::fcmps::{
    BranchBlind, Branches, CBlind, Fcmp, IBlind, IBlindBlind, OBlind, OutputBlinds, TreeRoot,
};
use monero_fcmp_plus_plus::sal::{OpenedInputTuple, RerandomizedOutput, SpendAuthAndLinkability};
use monero_fcmp_plus_plus::{
    FcmpPlusPlus, FCMP_PARAMS, HELIOS_FCMP_GENERATORS, SELENE_FCMP_GENERATORS,
};
use monero_fcmp_plus_plus_generators::{FCMP_PLUS_PLUS_U, FCMP_PLUS_PLUS_V};
use rand_chacha::ChaCha20Rng;
use rand_core::{OsRng, SeedableRng};
use tenero_crypto::curve_tree::{CurveTree, Leaf};

static T: LazyLock<EdwardsPoint> =
    LazyLock::new(|| EdwardsPoint(CompressedPoint::T.decompress().unwrap().into()));

struct Owned {
    x: Scalar,
    y: Scalar,
    leaf: Leaf,
}

fn owned(rng: &mut ChaCha20Rng) -> Owned {
    let x = Scalar::random(&mut *rng);
    let y = Scalar::random(&mut *rng);
    let o = (EdwardsPoint::generator() * x + *T * y)
        .0
        .compress()
        .to_bytes();
    let c = EdwardsPoint::random(&mut *rng).0.compress().to_bytes();
    Owned {
        x,
        y,
        leaf: Leaf::from_output(&o, &c).unwrap(),
    }
}

fn filler(rng: &mut ChaCha20Rng) -> Leaf {
    let p = |rng: &mut ChaCha20Rng| EdwardsPoint::random(rng).0.compress().to_bytes();
    Leaf::from_output(&p(rng), &p(rng)).unwrap()
}

/// Proves that `spent` (at `positions` in `tree`) are in it, and verifies the proof against `root`.
fn prove_and_verify(
    tree: &CurveTree,
    leaves: &[Leaf],
    spent: &[&Owned],
    positions: &[u64],
    root: TreeRoot<Selene, Helios>,
) -> bool {
    let layers = tree.n_layers();
    let hash = [9u8; 32];
    let mut inputs = vec![];
    let mut rerandomized = vec![];
    let mut key_images = vec![];
    for o in spent {
        let r = RerandomizedOutput::new(&mut OsRng, o.leaf.output);
        let opening = OpenedInputTuple::open(&r, &o.x, &o.y).unwrap();
        let (ki, sal) = SpendAuthAndLinkability::prove(&mut OsRng, hash, &opening);
        inputs.push((r.input(), sal));
        key_images.push(ki);
        rerandomized.push(r);
    }
    let paths = positions
        .iter()
        .map(|p| tree.path(*p, |i| leaves[i as usize]).unwrap())
        .collect();
    let branches = Branches::new(paths).expect("the paths share the root's chunk");
    let output_blinds = rerandomized
        .iter()
        .map(|r| {
            OutputBlinds::new(
                OBlind::new(*T, ScalarDecomposition::new(r.o_blind()).unwrap()),
                IBlind::new(
                    EdwardsPoint((*FCMP_PLUS_PLUS_U).into()),
                    EdwardsPoint((*FCMP_PLUS_PLUS_V).into()),
                    ScalarDecomposition::new(r.i_blind()).unwrap(),
                ),
                IBlindBlind::new(*T, ScalarDecomposition::new(r.i_blind_blind()).unwrap()),
                CBlind::new(
                    EdwardsPoint::generator(),
                    ScalarDecomposition::new(r.c_blind()).unwrap(),
                ),
            )
        })
        .collect();
    let c1 = (0..branches.necessary_c1_blinds())
        .map(|_| {
            BranchBlind::new(
                SELENE_FCMP_GENERATORS.generators.h(),
                ScalarDecomposition::new(<Selene as Ciphersuite>::F::random(&mut OsRng)).unwrap(),
            )
        })
        .collect();
    let c2 = (0..branches.necessary_c2_blinds())
        .map(|_| {
            BranchBlind::new(
                HELIOS_FCMP_GENERATORS.generators.h(),
                ScalarDecomposition::new(<Helios as Ciphersuite>::F::random(&mut OsRng)).unwrap(),
            )
        })
        .collect();
    let blinded = branches.blind(output_blinds, c1, c2).unwrap();
    let fcmp = Fcmp::prove(&mut OsRng, &*FCMP_PARAMS, blinded).unwrap();
    let mut bytes = vec![];
    FcmpPlusPlus::new(inputs, fcmp).write(&mut bytes).unwrap();
    let pseudo: Vec<[u8; 32]> = rerandomized.iter().map(|r| r.input().C_tilde()).collect();
    let proof = FcmpPlusPlus::read(&pseudo, layers, &mut bytes.as_slice()).unwrap();
    let mut ed = multiexp::BatchVerifier::new(spent.len());
    let mut v1 = generalized_bulletproofs::Generators::batch_verifier();
    let mut v2 = generalized_bulletproofs::Generators::batch_verifier();
    if proof
        .verify(
            &mut OsRng, &mut ed, &mut v1, &mut v2, root, layers, hash, key_images,
        )
        .is_err()
    {
        return false;
    }
    ed.verify_vartime()
        && SELENE_FCMP_GENERATORS.generators.verify(v1)
        && HELIOS_FCMP_GENERATORS.generators.verify(v2)
}

/// A tree of `n` leaves with `ours` placed at `positions`, grown in uneven blocks as a node grows it.
fn tree_with(
    n: usize,
    ours: &[&Owned],
    positions: &[u64],
    rng: &mut ChaCha20Rng,
) -> (CurveTree, Vec<Leaf>) {
    let mut leaves: Vec<Leaf> = (0..n).map(|_| filler(rng)).collect();
    for (o, p) in ours.iter().zip(positions) {
        leaves[*p as usize] = o.leaf;
    }
    let mut tree = CurveTree::new();
    let mut at = 0;
    for size in [1, 7, 40, 300, 5000].iter().cycle() {
        if at == n {
            break;
        }
        let k = (*size).min(n - at);
        tree.grow(&leaves[at..at + k]);
        at += k;
    }
    (tree, leaves)
}

#[test]
fn proofs_from_our_paths_verify_against_our_root_at_every_depth() {
    let mut rng = ChaCha20Rng::seed_from_u64(11);
    let a = owned(&mut rng);
    // 1 layer, 2 layers, 3 layers; the spent output first, last, and inside a chunk
    for (n, pos) in [
        (1usize, 0u64),
        (30, 29),
        (39, 38),
        (700, 0),
        (700, 699),
        (1200, 777),
    ] {
        let (tree, leaves) = tree_with(n, &[&a], &[pos], &mut rng);
        let root = tree.root().unwrap();
        assert!(
            prove_and_verify(&tree, &leaves, &[&a], &[pos], root),
            "{n} outputs, at {pos}"
        );
    }
}

#[test]
fn several_inputs_in_one_chunk_and_in_different_chunks() {
    let mut rng = ChaCha20Rng::seed_from_u64(12);
    let (a, b, c) = (owned(&mut rng), owned(&mut rng), owned(&mut rng));
    let pos = [3u64, 20, 650];
    let (tree, leaves) = tree_with(800, &[&a, &b, &c], &pos, &mut rng);
    let root = tree.root().unwrap();
    assert!(prove_and_verify(&tree, &leaves, &[&a, &b, &c], &pos, root));
}

#[test]
#[ignore = "about 30 s: four layers need more than 25,992 outputs"]
fn a_four_layer_tree() {
    let mut rng = ChaCha20Rng::seed_from_u64(13);
    let a = owned(&mut rng);
    let (tree, leaves) = tree_with(26_100, &[&a], &[26_050], &mut rng);
    assert_eq!(tree.n_layers(), 4);
    assert!(prove_and_verify(
        &tree,
        &leaves,
        &[&a],
        &[26_050],
        tree.root().unwrap()
    ));
}

#[test]
fn a_proof_against_another_tree_s_root_is_refused() {
    let mut rng = ChaCha20Rng::seed_from_u64(14);
    let a = owned(&mut rng);
    let (tree, leaves) = tree_with(100, &[&a], &[42], &mut rng);
    let (other, _) = tree_with(100, &[&a], &[42], &mut rng); // same spent output, other neighbours
    assert_ne!(tree.root_bytes(), other.root_bytes());
    assert!(!prove_and_verify(
        &tree,
        &leaves,
        &[&a],
        &[42],
        other.root().unwrap()
    ));
}
