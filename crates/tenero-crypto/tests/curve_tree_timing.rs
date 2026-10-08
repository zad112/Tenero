//! What the curve tree costs a node (`docs/FCMP_CARROT_PLAN.md` 4.4, G3). Build with `--release`:
//! `cargo test --release -p tenero-crypto --test curve_tree_timing -- --ignored --nocapture`. Rule 5: numbers from the
//! machine that ran it, one thread.

use std::time::Instant;

use curve25519_dalek::edwards::EdwardsPoint;
use curve25519_dalek::scalar::Scalar;
use tenero_crypto::curve_tree::{CurveTree, Leaf};

#[test]
#[ignore]
fn time_the_tree() {
    const N: usize = 100_000;
    let p = |k: u64| {
        (EdwardsPoint::mul_base(&Scalar::from(k + 1)) * Scalar::from(77u64))
            .compress()
            .to_bytes()
    };
    let points: Vec<([u8; 32], [u8; 32])> =
        (0..N as u64).map(|i| (p(2 * i), p(2 * i + 1))).collect();

    let start = Instant::now();
    let leaves: Vec<Leaf> = points
        .iter()
        .map(|(o, c)| Leaf::from_output(o, c).unwrap())
        .collect();
    let per_leaf = start.elapsed() / N as u32;
    println!("an output to its leaf (decode O and C, I = Hp2(O)): {per_leaf:.2?} each");

    let start = Instant::now();
    for l in &leaves {
        std::hint::black_box(l.scalars());
    }
    println!(
        "a leaf to its six scalars (Weierstrass coordinates): {:.2?} each",
        start.elapsed() / N as u32
    );

    for block in [10usize, 100, 1000] {
        let mut tree = CurveTree::new();
        let start = Instant::now();
        for chunk in leaves.chunks(block) {
            tree.grow(chunk);
        }
        let all = start.elapsed();
        println!(
            "growing to {N} outputs in blocks of {block:>4}: {:.2?} in all, {:.2?} a block, {:.2?} an output; {} layers",
            all,
            all / (N / block) as u32,
            all / N as u32,
            tree.n_layers()
        );
    }

    let mut tree = CurveTree::from_scratch(&leaves);
    let start = Instant::now();
    for k in 1..=10 {
        tree.trim((N - 100 * k) as u64, |i| leaves[i as usize]);
    }
    println!(
        "undoing a block of 100 outputs (trim): {:.2?}",
        start.elapsed() / 10
    );

    let start = Instant::now();
    for i in 0..100u64 {
        let _ = tree
            .path(i * 997 % tree.n_leaves(), |j| leaves[j as usize])
            .unwrap();
    }
    println!("a path for a proof: {:.2?}", start.elapsed() / 100);
    println!(
        "(one thread; the tree above the leaves holds about {} points)",
        N / 38 + N / 684 + N / 25_992 + 1
    );
}
