//! A real version 3 spend through the real block validator: mined blocks, coinbase outputs to keys we hold, which enter
//! the curve tree 60 blocks later, and a transaction with a real FCMP++ proof against the stored tree, accepted under
//! `FcmpProofs` and refused when anything about it is wrong. The same broken block is ACCEPTED under
//! `ProofsNotChecked`, which is why that must never be used on a chain that matters.

use std::path::PathBuf;

use curve25519_dalek::edwards::EdwardsPoint;
use curve25519_dalek::scalar::Scalar;
use monero_ed25519::CompressedPoint;
use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;
use tenero_chain::{
    Accepted, BlockError, ChainParams, ProofCheck, ProofsNotChecked, Sha256Pow, Validator,
};
use tenero_core::u256::U256;
use tenero_core::v2::ids::PowKind;
use tenero_core::v3::rules;
use tenero_core::v3::*;
use tenero_crypto::fcmp::{key_image, prove, FcmpProofs, OutputSecret, Spend};
use tenero_store::Store;

const LABEL: &str = "tenero chain test network";
const NOW: u64 = 4_000_000_000;
const T0: u64 = 1_700_000_000;
/// Block 1's coinbase enters the tree when block 60 is applied; by 75 the tree holds the coinbases of blocks 1..=16.
const TIP: u64 = 75;

struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> TempDb {
        let p = std::env::temp_dir().join(format!(
            "tenero-crypto-v3-{}-{name}.redb",
            std::process::id()
        ));
        let db = TempDb(p);
        db.remove();
        db
    }
    fn remove(&self) {
        let _ = std::fs::remove_file(&self.0);
        let mut s = self.0.clone().into_os_string();
        s.push(".segments");
        let _ = std::fs::remove_dir_all(PathBuf::from(s));
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        self.remove();
    }
}

fn t() -> EdwardsPoint {
    CompressedPoint::T.decompress().unwrap().into()
}

fn h() -> EdwardsPoint {
    CompressedPoint::H.decompress().unwrap().into()
}

fn hash_scalar(tag: &[u8], n: u64) -> Scalar {
    let mut wide = [0u8; 64];
    let a = tenero_core::hash::sha256(&[tag, &n.to_le_bytes(), b"a"]);
    let b = tenero_core::hash::sha256(&[tag, &n.to_le_bytes(), b"b"]);
    wide[..32].copy_from_slice(&a);
    wide[32..].copy_from_slice(&b);
    Scalar::from_bytes_mod_order_wide(&wide)
}

/// The two secrets of the key of block `height`'s coinbase output: `O = x G + y T`.
fn coinbase_keys(height: u64) -> (Scalar, Scalar) {
    (hash_scalar(b"x", height), hash_scalar(b"y", height))
}

fn coinbase_key(height: u64) -> [u8; 32] {
    let (x, y) = coinbase_keys(height);
    (EdwardsPoint::mul_base(&x) + t() * y).compress().to_bytes()
}

struct Net {
    _db: TempDb,
    store: Store,
    params: ChainParams,
    pow: Sha256Pow,
}

impl Net {
    fn new(name: &str) -> Net {
        let db = TempDb::new(name);
        let store = Store::open(&db.0, LABEL, PowKind::Sha256).unwrap();
        Net {
            _db: db,
            store,
            params: ChainParams::version_3(LABEL, PowKind::Sha256, U256::pow2(254).unwrap()),
            pow: Sha256Pow,
        }
    }

    fn validator<'a>(&'a self, proofs: &'a dyn ProofCheck) -> Validator<'a> {
        Validator::new(&self.store, &self.params, &self.pow, proofs)
    }

    fn height(&self) -> u64 {
        self.store.tip().unwrap().0
    }

    /// A mined block on the tip with `txs`, its coinbase (to a key we hold) paying exactly the rules.
    fn block(&self, txs: Vec<Transaction>) -> Block {
        let v = self.validator(&ProofsNotChecked);
        let next = v.next_block().unwrap();
        let weight: u64 = txs.iter().map(|t| rules::tx_weight(t).unwrap()).sum();
        let fees_total: u64 = txs.iter().map(|t| t.prefix.fee).sum();
        let amount = v.coinbase_amount(&next, weight, fees_total).unwrap();
        let timestamp = if self.height() == 0 {
            T0
        } else {
            self.store.tip().unwrap().1.header.timestamp + 60
        }
        .max(u64::try_from(next.min_timestamp).unwrap_or(0));
        let coinbase = Coinbase {
            version: VERSION,
            height: next.height,
            outputs: vec![CoinbaseOutput {
                onetime_address: coinbase_key(next.height),
                amount,
                view_tag: [1; 3],
                ephemeral_pubkey: [2; 32],
                anchor_enc: [3; 16],
            }],
            extra: vec![],
        };
        let mut b = Block {
            header: BlockHeader {
                version: VERSION,
                prev_id: next.prev_id,
                timestamp,
                tx_root: ids::block_tx_root(&coinbase, &txs).unwrap(),
                nonce: 0,
                mix: [0; 64],
            },
            coinbase,
            transactions: txs,
        };
        for nonce in 0.. {
            b.header.nonce = nonce;
            if U256::from_be_bytes(&ids::block_id(&b.header, PowKind::Sha256)) < next.target {
                break;
            }
        }
        b
    }

    fn accept(&self, proofs: &dyn ProofCheck, b: &Block) -> tenero_chain::ValidatedBlock {
        match self.validator(proofs).accept_block(b, NOW) {
            Ok(Accepted::Added(v)) => v,
            other => panic!("the block was not accepted: {other:?}"),
        }
    }

    fn refuse(&self, proofs: &dyn ProofCheck, b: &Block) -> BlockError {
        let before = self.height();
        let e = self
            .validator(proofs)
            .accept_block(b, NOW)
            .expect_err("the block must be refused");
        assert_eq!(self.height(), before, "a refused block changed the chain");
        e
    }

    fn empty_blocks(&self, n: u64) {
        for _ in 0..n {
            self.accept(&ProofsNotChecked, &self.block(vec![]));
        }
    }

    /// A real spend of block `of`'s coinbase output, proven for `chain_id` against the tree at the tip (the reference
    /// block), paying the minimum fee plus `extra_fee`.
    fn spend(&self, chain_id: &[u8; 32], of: u64, extra_fee: u64, seed: u64) -> Transaction {
        let next = self.validator(&ProofsNotChecked).next_block().unwrap();
        let index = self
            .store
            .block_index(of)
            .unwrap()
            .unwrap()
            .first_output_index;
        let out = self.store.output(index).unwrap().unwrap();
        assert_eq!(out.onetime_address, coinbase_key(of));
        let position = self
            .store
            .leaf_of_output(index)
            .unwrap()
            .expect("in the tree");
        let reference = self.height();
        let layers = usize::from(self.store.tree_state(reference).unwrap().unwrap().n_layers);
        let (x, y) = coinbase_keys(of);
        let amount = out.public_amount;
        let build = |fee: u64| {
            let mut rng = ChaCha20Rng::seed_from_u64(seed);
            let a = (amount - fee) / 3;
            let mut outs: Vec<(Output, OutputSecret)> = [a, amount - fee - a]
                .iter()
                .enumerate()
                .map(|(n, &value)| {
                    let mask = hash_scalar(b"mask", seed * 16 + n as u64);
                    let key = EdwardsPoint::mul_base(&hash_scalar(b"out", seed * 16 + n as u64));
                    (
                        Output {
                            onetime_address: key.compress().to_bytes(),
                            amount_commitment: (EdwardsPoint::mul_base(&mask)
                                + h() * Scalar::from(value))
                            .compress()
                            .to_bytes(),
                            amount_enc: [4; 8],
                            view_tag: [5; 3],
                            anchor_enc: [7; 16],
                        },
                        OutputSecret {
                            amount: value,
                            mask,
                        },
                    )
                })
                .collect();
            outs.sort_by_key(|(o, _)| o.onetime_address);
            let mut tx = Transaction {
                prefix: TxPrefix {
                    version: VERSION,
                    inputs: vec![Input {
                        key_image: key_image(&x, &out.onetime_address),
                    }],
                    outputs: outs.iter().map(|(o, _)| o.clone()).collect(),
                    ephemeral_pubkeys: vec![EdwardsPoint::mul_base(&hash_scalar(b"eph", seed))
                        .compress()
                        .to_bytes()],
                    fee,
                    encrypted_payment_id: [8; 8],
                },
                prunable: Prunable {
                    reference_height: reference,
                    proof_data: vec![],
                },
            };
            let spend = Spend {
                x,
                y,
                mask: Scalar::ONE,
                amount,
                leaf: self.store.leaf(position).unwrap().unwrap(),
                path: self.store.tree_path(position).unwrap().unwrap(),
            };
            let secrets: Vec<OutputSecret> = outs.into_iter().map(|(_, s)| s).collect();
            tx.prunable.proof_data =
                prove(&mut rng, chain_id, &tx, &[spend], &secrets, layers).expect("proving");
            tx
        };
        // the proof's size does not depend on the fee and every field is fixed-width: one build gives the size
        let size = build(0).to_bytes().unwrap().len() as u64;
        let min = rules::min_fee(size, next.reward, next.median).unwrap();
        build(min + extra_fee)
    }
}

fn ready(name: &str) -> Net {
    let net = Net::new(name);
    net.empty_blocks(TIP);
    assert_eq!(net.store.tree_state(TIP).unwrap().unwrap().n_leaves, 16);
    net
}

#[test]
fn a_real_spend_is_accepted_with_proofs_checked_and_recorded() {
    let net = ready("accept");
    let tx = net.spend(&net.store.chain_id(), 3, 0, 1);
    let image = tx.prefix.inputs[0].key_image;
    let v = net.accept(&FcmpProofs::new(), &net.block(vec![tx]));
    assert!(v.proofs_checked, "FcmpProofs really checks, and says so");
    assert_eq!(net.store.key_image_height(&image).unwrap(), Some(TIP + 1));
}

#[test]
fn a_broken_proof_is_refused_and_accepted_only_when_proofs_are_not_checked() {
    let net = ready("broken");
    let mut tx = net.spend(&net.store.chain_id(), 3, 0, 1);
    let last = tx.prunable.proof_data.len() - 1;
    tx.prunable.proof_data[last] ^= 1;
    let b = net.block(vec![tx]);
    let err = net.refuse(&FcmpProofs::new(), &b);
    assert!(
        matches!(err, BlockError::ProofRejected { tx: 0, .. }),
        "{err:?}"
    );
    // the hole the unchecked mode leaves open
    assert!(!net.accept(&ProofsNotChecked, &b).proofs_checked);
}

#[test]
fn a_spend_for_another_chain_or_with_a_raised_fee_is_refused() {
    let net = ready("other");
    let b = net.block(vec![net.spend(&[0xab; 32], 3, 0, 1)]);
    assert!(matches!(
        net.refuse(&FcmpProofs::new(), &b),
        BlockError::ProofRejected { .. }
    ));
    let mut tx = net.spend(&net.store.chain_id(), 3, 0, 1);
    tx.prefix.fee += 1;
    match net.refuse(&FcmpProofs::new(), &net.block(vec![tx])) {
        BlockError::ProofRejected { reason, .. } => assert!(reason.contains("balance"), "{reason}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_older_reference_block_still_works_and_a_spent_output_cannot_be_spent_again() {
    let net = ready("older");
    let chain_id = net.store.chain_id();
    let tx = net.spend(&chain_id, 3, 0, 1);
    // three blocks later the tree has grown, but the proof names its reference block, whose tree is kept
    net.empty_blocks(3);
    net.accept(&FcmpProofs::new(), &net.block(vec![tx]));
    // fresh proofs (other randomness, other outputs) for the same output: the same key image
    let again = net.spend(&chain_id, 3, 0, 2);
    assert!(matches!(
        net.refuse(&FcmpProofs::new(), &net.block(vec![again])),
        BlockError::KeyImageSpent { .. }
    ));
}

#[test]
fn a_reference_block_must_be_below_the_block_and_have_outputs_in_its_tree() {
    let net = ready("reference");
    let chain_id = net.store.chain_id();
    for (r, why) in [(TIP + 1, "the block itself"), (40, "an empty tree")] {
        let mut tx = net.spend(&chain_id, 3, 0, 1);
        tx.prunable.reference_height = r;
        let err = net.refuse(&ProofsNotChecked, &net.block(vec![tx]));
        assert_eq!(
            err,
            BlockError::BadReference {
                tx: 0,
                reference_height: r
            },
            "{why}"
        );
    }
}

#[test]
fn one_bad_transaction_among_several_is_named_whichever_thread_checks_it() {
    let net = ready("batch");
    let chain_id = net.store.chain_id();
    let mut txs: Vec<Transaction> = (1..=4).map(|of| net.spend(&chain_id, of, 0, of)).collect();
    txs.sort_by_key(|t| ids::tx_id(t).unwrap());
    let good = net.block(txs.clone());
    let n = txs[2].prunable.proof_data.len();
    txs[2].prunable.proof_data[n - 40] ^= 0x10;
    let bad = net.block(txs);
    for threads in [1, 2, 4] {
        let err = net.refuse(&FcmpProofs::with_threads(threads), &bad);
        assert!(
            matches!(err, BlockError::ProofRejected { tx: 2, .. }),
            "{threads} threads: {err:?}"
        );
    }
    net.accept(&FcmpProofs::with_threads(3), &good);
}

#[test]
fn points_are_checked_even_in_a_block_assumed_valid() {
    let net = ready("assumed");
    let mut tx = net.spend(&net.store.chain_id(), 3, 0, 1);
    // a commitment of small order: no proof would ever be checked for it, but it would enter the tree
    tx.prefix.outputs[0].amount_commitment = [0; 32];
    tx.prefix.outputs[0].amount_commitment[0] = 1; // the identity
    let b = net.block(vec![tx]);
    let id = ids::block_id(&b.header, PowKind::Sha256);
    let assumed = std::sync::Arc::new([id].into_iter().collect());
    let err = net
        .validator(&ProofsNotChecked)
        .with_assumed(Some(assumed))
        .accept_block(&b, NOW)
        .unwrap_err();
    assert_eq!(err, BlockError::BadOutputPoint { tx: 0, output: 0 });
}
