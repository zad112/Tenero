//! Property test of the curve tree (`docs/FCMP_CARROT_PLAN.md` 4.3: a tree that differs between nodes is a chain split):
//! any sequence of blocks added and blocks undone, of any sizes, must leave exactly the tree a from-scratch build of
//! the surviving leaves gives. Sizes reach past the first two layer boundaries (38 and 684 outputs).

use std::sync::LazyLock;

use curve25519_dalek::edwards::EdwardsPoint;
use curve25519_dalek::scalar::Scalar;
use proptest::prelude::*;
use tenero_tree::{CurveTree, Leaf};

const MAX: usize = 1_500;

static LEAVES: LazyLock<Vec<Leaf>> = LazyLock::new(|| {
    (0..MAX as u64)
        .map(|i| {
            let p = |k: u64| {
                (EdwardsPoint::mul_base(&Scalar::from(k)) * Scalar::from(0x5eed_u64))
                    .compress()
                    .to_bytes()
            };
            Leaf::from_output(&p(2 * i + 1), &p(2 * i + 2)).unwrap()
        })
        .collect()
});

#[derive(Clone, Debug)]
enum Step {
    /// a block adding this many leaves
    Grow(usize),
    /// undo this many of the most recent blocks
    Undo(usize),
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        4 => prop_oneof![1usize..5, 30usize..45, 1usize..300].prop_map(Step::Grow),
        1 => (1usize..4).prop_map(Step::Undo),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    #[test]
    fn any_history_of_blocks_and_reorganisations_gives_the_from_scratch_tree(steps in prop::collection::vec(step(), 1..12)) {
        let mut tree = CurveTree::new();
        let mut sizes: Vec<usize> = vec![]; // leaves after each applied block
        for s in steps {
            let n = sizes.last().copied().unwrap_or(0);
            match s {
                Step::Grow(k) => {
                    let k = k.min(MAX - n);
                    if k == 0 { continue; }
                    tree.grow(&LEAVES[n..n + k]);
                    sizes.push(n + k);
                }
                Step::Undo(k) => {
                    for _ in 0..k.min(sizes.len()) { sizes.pop(); }
                    tree.trim(sizes.last().copied().unwrap_or(0) as u64, |i| LEAVES[i as usize]);
                }
            }
            let n = sizes.last().copied().unwrap_or(0);
            prop_assert_eq!(&tree, &CurveTree::from_scratch(&LEAVES[..n]));
        }
    }
}

#[test]
fn a_tree_saved_as_bytes_and_read_back_is_the_same_tree() {
    for n in [0usize, 1, 38, 39, 700, 1_500] {
        let tree = CurveTree::from_scratch(&LEAVES[..n]);
        let layers: Vec<Vec<[u8; 32]>> = tree
            .layer_lens()
            .iter()
            .enumerate()
            .map(|(l, len)| {
                (0..*len)
                    .map(|i| tree.element_bytes(l, i).unwrap())
                    .collect()
            })
            .collect();
        assert_eq!(
            CurveTree::from_layers(n as u64, layers.clone()),
            Some(tree),
            "{n} leaves"
        );
        if !layers.is_empty() {
            assert_eq!(
                CurveTree::from_layers(n as u64 + 100, layers),
                None,
                "a shape that does not fit"
            );
        }
    }
}

/// The leaf pool's bytes: the one-time address and commitment of leaf `i`.
fn leaf_bytes(i: u64) -> ([u8; 32], [u8; 32]) {
    let p = |k: u64| {
        (EdwardsPoint::mul_base(&Scalar::from(k)) * Scalar::from(0x5eed_u64))
            .compress()
            .to_bytes()
    };
    (p(2 * i + 1), p(2 * i + 2))
}

#[test]
fn a_path_sent_as_bytes_is_the_tree_s_own_path() {
    for n in [1usize, 38, 39, 700, 1_500] {
        let tree = CurveTree::from_scratch(&LEAVES[..n]);
        for i in [0, n / 3, n / 2, n - 1] {
            let direct = tree.path(i as u64, |j| LEAVES[j as usize]).unwrap();
            let bytes = tree.path_bytes(i as u64, leaf_bytes).unwrap();
            let rebuilt = tenero_tree::path_from_bytes(&bytes).unwrap();
            assert_eq!(rebuilt.output, direct.output, "{n} leaves, leaf {i}");
            assert_eq!(rebuilt.leaves, direct.leaves);
            assert_eq!(rebuilt.curve_1_layers, direct.curve_1_layers);
            assert_eq!(rebuilt.curve_2_layers, direct.curve_2_layers);
        }
        assert!(tree.path_bytes(n as u64, leaf_bytes).is_none());
    }
    // damaged bytes are refused, not trusted
    let tree = CurveTree::from_scratch(&LEAVES[..700]);
    let good = tree.path_bytes(5, leaf_bytes).unwrap();
    let mut bad = good.clone();
    bad.leaves[0].0 = [0; 32];
    assert!(
        tenero_tree::path_from_bytes(&bad).is_none(),
        "a leaf that is not a point"
    );
    let mut bad = good.clone();
    bad.layers[0][0] = [0xff; 32];
    assert!(
        tenero_tree::path_from_bytes(&bad).is_none(),
        "a layer element that is not a point"
    );
    let mut bad = good.clone();
    bad.layers[0].clear();
    assert!(
        tenero_tree::path_from_bytes(&bad).is_none(),
        "an empty chunk"
    );
    let mut bad = good;
    bad.position = 37;
    bad.leaves.truncate(10);
    assert!(
        tenero_tree::path_from_bytes(&bad).is_none(),
        "a position outside the chunk"
    );
}
