//! The curve tree against MONERO'S OWN tree code (`tests/vectors/curve_tree_monero.json`, made by
//! `reference/tools/carrot_harness/tree_vectors.cpp` in WSL from Monero's production `get_tree_extension`, every block
//! audited by Monero's own from-scratch re-hash). Our tree must give the same root after every block, the same as a
//! from-scratch build, and the same root again after being trimmed back (a reorganisation).

use ciphersuite::group::ff::PrimeField;
use curve25519_dalek::edwards::EdwardsPoint;
use curve25519_dalek::scalar::Scalar;
use monero_primitives::keccak256;
use tenero_core::vectors::{hex, load};
use tenero_crypto::curve_tree::{n_layers, CurveTree, Leaf};

/// Output `i` of the vectors: key `o_i G`, commitment `c_i G`, `o_i = Keccak256(label || i LE) mod l`.
fn output(i: u64) -> Leaf {
    let point = |label: &str| {
        let mut data = label.as_bytes().to_vec();
        data.extend_from_slice(&i.to_le_bytes());
        EdwardsPoint::mul_base(&Scalar::from_bytes_mod_order(keccak256(&data)))
            .compress()
            .to_bytes()
    };
    Leaf::from_output(
        &point("tenero curve tree vector O"),
        &point("tenero curve tree vector C"),
    )
    .unwrap()
}

struct Block {
    added: u64,
    outputs: u64,
    layers: usize,
    root: [u8; 32],
}

fn blocks() -> Vec<Block> {
    load("curve_tree_monero").unwrap()["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| Block {
            added: b["added"].as_u64().unwrap(),
            outputs: b["outputs"].as_u64().unwrap(),
            layers: b["layers"].as_u64().unwrap() as usize,
            root: hex(b["root"].as_str().unwrap())
                .unwrap()
                .try_into()
                .unwrap(),
        })
        .collect()
}

#[test]
fn the_leaves_are_monero_s() {
    let file = load("curve_tree_monero").unwrap();
    for l in file["leaves"].as_array().unwrap() {
        let i = l["index"].as_u64().unwrap();
        let ours: Vec<String> = output(i)
            .scalars()
            .iter()
            .map(|s| s.to_repr().iter().map(|b| format!("{b:02x}")).collect())
            .collect();
        let want: Vec<&str> = l["scalars"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        assert_eq!(ours, want, "leaf {i}");
    }
}

#[test]
fn the_number_of_layers_is_monero_s() {
    for b in blocks() {
        assert_eq!(n_layers(b.outputs), b.layers, "{} outputs", b.outputs);
    }
    assert_eq!(n_layers(0), 0);
    assert_eq!(n_layers(467_856), 4);
    assert_eq!(n_layers(467_857), 5);
}

#[test]
fn growing_block_by_block_gives_monero_s_root_after_every_block() {
    let all: Vec<Leaf> = (0..blocks().last().unwrap().outputs).map(output).collect();
    let mut tree = CurveTree::new();
    assert!(tree.root().is_none());
    let mut n = 0usize;
    for b in blocks() {
        tree.grow(&all[n..n + b.added as usize]);
        n += b.added as usize;
        assert_eq!(tree.n_leaves(), b.outputs);
        assert_eq!(tree.n_layers(), b.layers, "{} outputs", b.outputs);
        assert_eq!(
            tree.root_bytes().unwrap(),
            b.root,
            "the root after {} outputs",
            b.outputs
        );
    }
}

#[test]
fn a_tree_built_from_scratch_is_the_same_tree() {
    let all: Vec<Leaf> = (0..blocks().last().unwrap().outputs).map(output).collect();
    let mut grown = CurveTree::new();
    let mut n = 0usize;
    for b in blocks() {
        grown.grow(&all[n..n + b.added as usize]);
        n += b.added as usize;
        if n < 3000 || b.outputs == blocks().last().unwrap().outputs {
            assert_eq!(grown, CurveTree::from_scratch(&all[..n]), "{n} outputs");
        }
    }
}

#[test]
fn trimming_back_gives_monero_s_earlier_roots() {
    let bs = blocks();
    let total = bs.last().unwrap().outputs;
    let all: Vec<Leaf> = (0..total).map(output).collect();
    let mut tree = CurveTree::from_scratch(&all);
    // undo the blocks one by one, newest first, as a reorganisation does
    for b in bs.iter().rev() {
        tree.trim(b.outputs, |i| all[i as usize]);
        assert_eq!(
            tree.root_bytes().unwrap(),
            b.root,
            "trimmed to {} outputs",
            b.outputs
        );
        assert_eq!(tree.n_layers(), b.layers);
    }
    tree.trim(0, |i| all[i as usize]);
    assert_eq!(tree, CurveTree::new());
}
