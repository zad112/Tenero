//! Property tests on the version 2 decoders (M9 groundwork; proptest, a dev-dependency). Whatever bytes a stranger sends:
//! decoding must not panic, hang or allocate wildly; and the decoders are STRICT, so anything that does decode must
//! re-encode to exactly the bytes it came from (one object, one encoding). Inputs are random bytes and, more usefully,
//! valid objects from the golden vectors with a few random edits.

use proptest::prelude::*;
use tenero_core::v2::{
    Block, BlockHeader, Coinbase, CoinbaseOutput, Input, Output, Prunable, PrunedBlock,
    PrunedTransaction, Transaction, Wire,
};
use tenero_core::vectors::{hex, load};

/// Decodes `bytes` as `T`; if it decodes, it must encode back to the same bytes.
fn check<T: Wire>(bytes: &[u8], what: &str) -> Result<(), TestCaseError> {
    if let Ok(x) = T::from_bytes(bytes) {
        let back = x.to_bytes().map_err(|e| {
            TestCaseError::fail(format!("{what}: decoded but cannot be encoded: {e}"))
        })?;
        prop_assert_eq!(back.as_slice(), bytes, "{}: a second encoding", what);
    }
    Ok(())
}

fn check_all(bytes: &[u8]) -> Result<(), TestCaseError> {
    check::<Output>(bytes, "output")?;
    check::<Input>(bytes, "input")?;
    check::<Transaction>(bytes, "transaction")?;
    check::<PrunedTransaction>(bytes, "pruned transaction")?;
    check::<CoinbaseOutput>(bytes, "coinbase output")?;
    check::<Coinbase>(bytes, "coinbase")?;
    check::<BlockHeader>(bytes, "header")?;
    check::<Block>(bytes, "block")?;
    check::<PrunedBlock>(bytes, "pruned block")?;
    Ok(())
}

fn valid_objects() -> Vec<Vec<u8>> {
    let v = load("v2_serialization").unwrap();
    v["valid"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| hex(c["hex"].as_str().unwrap()).unwrap())
        .collect()
}

/// One random edit of a byte string.
#[derive(Clone, Debug)]
enum Edit {
    Flip(usize, u8),
    Set(usize, u8),
    Insert(usize, u8),
    Delete(usize),
    Truncate(usize),
    Extend(Vec<u8>),
    /// a count or length field made huge: four bytes at a position set to 0xFF or a large value
    Big(usize, u32),
}

fn edit() -> impl Strategy<Value = Edit> {
    prop_oneof![
        (any::<usize>(), 0u8..8).prop_map(|(p, b)| Edit::Flip(p, b)),
        (any::<usize>(), any::<u8>()).prop_map(|(p, v)| Edit::Set(p, v)),
        (any::<usize>(), any::<u8>()).prop_map(|(p, v)| Edit::Insert(p, v)),
        any::<usize>().prop_map(Edit::Delete),
        any::<usize>().prop_map(Edit::Truncate),
        proptest::collection::vec(any::<u8>(), 1..16).prop_map(Edit::Extend),
        (
            any::<usize>(),
            prop_oneof![Just(u32::MAX), Just(1 << 24), any::<u32>()]
        )
            .prop_map(|(p, v)| Edit::Big(p, v)),
    ]
}

fn apply(mut b: Vec<u8>, edits: &[Edit]) -> Vec<u8> {
    for e in edits {
        if b.is_empty() {
            break;
        }
        let n = b.len();
        match e {
            Edit::Flip(p, bit) => b[p % n] ^= 1 << bit,
            Edit::Set(p, v) => b[p % n] = *v,
            Edit::Insert(p, v) => b.insert(p % n, *v),
            Edit::Delete(p) => {
                b.remove(p % n);
            }
            Edit::Truncate(p) => b.truncate(p % n),
            Edit::Extend(x) => b.extend_from_slice(x),
            Edit::Big(p, v) => {
                let at = p % n;
                for (i, byte) in v.to_le_bytes().iter().enumerate() {
                    if at + i < b.len() {
                        b[at + i] = *byte;
                    }
                }
            }
        }
    }
    b
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    #[test]
    fn random_bytes_never_break_a_decoder(bytes in proptest::collection::vec(any::<u8>(), 0..3000)) {
        check_all(&bytes)?;
    }

    #[test]
    fn valid_objects_with_a_few_edits_never_break_a_decoder(
        which in any::<usize>(),
        edits in proptest::collection::vec(edit(), 1..5),
    ) {
        let objects = valid_objects();
        let bytes = apply(objects[which % objects.len()].clone(), &edits);
        check_all(&bytes)?;
    }

    #[test]
    fn the_prunable_part_with_any_input_count_never_breaks(
        which in any::<usize>(),
        edits in proptest::collection::vec(edit(), 0..4),
        n in 0usize..40,
    ) {
        let objects = valid_objects();
        let bytes = apply(objects[which % objects.len()].clone(), &edits);
        if let Ok(p) = Prunable::from_bytes(&bytes, n) {
            let mut w = tenero_core::v2::Writer::new();
            p.write(&mut w, n).map_err(|e| TestCaseError::fail(format!("{e}")))?;
            prop_assert_eq!(w.into_bytes(), bytes);
        }
    }
}

#[test]
fn the_valid_objects_are_found_and_there_are_enough_of_them() {
    assert!(valid_objects().len() >= 15);
}
