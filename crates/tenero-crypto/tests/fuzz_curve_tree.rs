//! Property test of the curve tree (`docs/FCMP_CARROT_PLAN.md` 4.3: a tree that differs between nodes is a chain split):
//! any sequence of blocks added and blocks undone, of any sizes, must leave exactly the tree a from-scratch build of
//! the surviving leaves gives. Sizes reach past the first two layer boundaries (38 and 684 outputs).

use std::sync::LazyLock;

use curve25519_dalek::edwards::EdwardsPoint;
use curve25519_dalek::scalar::Scalar;
use proptest::prelude::*;
use tenero_crypto::curve_tree::{CurveTree, Leaf};

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
