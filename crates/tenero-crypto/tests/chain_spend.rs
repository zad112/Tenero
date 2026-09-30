//! A real spend through the real block validator: mined blocks, coinbase outputs to keys we hold, a
//! transaction with real CLSAG and Bulletproofs+ proofs, accepted under `RingCtProofs`, and refused when
//! anything about its proofs is wrong. The same broken block is ACCEPTED under `ProofsNotChecked`, which is
//! why that default must never be used on a chain that matters.

use std::path::PathBuf;

use monero_ed25519::Scalar;
use rand_chacha::{rand_core::SeedableRng, ChaCha20Rng};
use tenero_chain::{
    Accepted, BlockError, ChainParams, ProofCheck, ProofsNotChecked, Sha256Pow, Validator,
};
use tenero_core::fees;
use tenero_core::u256::U256;
use tenero_core::v2::ids::{self, PowKind};
use tenero_core::v2::*;
use tenero_crypto::ringct::{
    commit, key_image, prove, public_amount_commitment, public_key, OutputSecret, RingCtProofs,
    SpendInput,
};
use tenero_store::Store;

const LABEL: &str = "tenero chain test network";
const NOW: u64 = 4_000_000_000;
const T0: u64 = 1_700_000_000;
/// Block 16's coinbase matures 60 blocks later; the first 16 coinbase outputs are the first ring.
const SPEND_HEIGHT: u64 = 76;

struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> TempDb {
        let p = std::env::temp_dir().join(format!(
            "tenero-crypto-test-{}-{name}.redb",
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

/// The secret key of the coinbase output of block `height`: known to us, so we can spend it.
fn coinbase_secret(height: u64) -> [u8; 32] {
    <[u8; 32]>::from(Scalar::hash(
        [b"coinbase secret".as_slice(), &height.to_le_bytes()].concat(),
    ))
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
            params: ChainParams::version_2(LABEL, PowKind::Sha256, U256::pow2(254).unwrap()),
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
        let body: u64 = txs.iter().map(|t| t.to_bytes().unwrap().len() as u64).sum();
        let fees_total: u64 = txs.iter().map(|t| t.prefix.fee).sum();
        let amount = v.coinbase_amount(&next, body, fees_total).unwrap();
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
                onetime_address: public_key(&coinbase_secret(next.height)).unwrap(),
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

    /// Mines `n` blocks of coinbase only.
    fn coinbase_blocks(&self, n: u64) {
        for _ in 0..n {
            self.accept(&ProofsNotChecked, &self.block(vec![]));
        }
    }

    /// A real spend of the coinbase output with global index `signer` (inside the first ring of 16),
    /// with real proofs made for `chain_id`, paying `extra_fee` above the dynamic minimum.
    fn spend(&self, chain_id: &[u8; 32], signer: u64, extra_fee: u64, seed: u64) -> Transaction {
        let next = self.validator(&ProofsNotChecked).next_block().unwrap();
        let ring_indexes: Vec<u64> = (0..16).collect();
        let outputs: Vec<_> = ring_indexes
            .iter()
            .map(|&i| self.store.output(i).unwrap().unwrap())
            .collect();
        // the coinbase output of block h is global index h-1 here (one output per block)
        let secret = coinbase_secret(signer + 1);
        let amount = outputs[signer as usize].public_amount;
        assert_eq!(
            outputs[signer as usize].onetime_address,
            public_key(&secret).unwrap()
        );
        let ring: Vec<[[u8; 32]; 2]> = outputs
            .iter()
            .map(|o| [o.onetime_address, public_amount_commitment(o.public_amount)])
            .collect();
        let input = SpendInput {
            secret_key: secret,
            mask: <[u8; 32]>::from(Scalar::ONE),
            amount,
            ring_indexes,
            ring,
            signer: signer as usize,
        };
        let build = |fee: u64| {
            let mut rng = ChaCha20Rng::seed_from_u64(seed);
            let a = (amount - fee) / 3;
            let amounts = [a, amount - fee - a];
            let mut prefix_outputs = Vec::new();
            let mut secrets = Vec::new();
            for (n, &value) in amounts.iter().enumerate() {
                let mask = <[u8; 32]>::from(Scalar::hash(
                    [b"mask".as_slice(), &seed.to_le_bytes(), &[n as u8]].concat(),
                ));
                prefix_outputs.push(Output {
                    onetime_address: public_key(&<[u8; 32]>::from(Scalar::hash(
                        [b"out".as_slice(), &seed.to_le_bytes(), &[n as u8]].concat(),
                    )))
                    .unwrap(),
                    amount_commitment: commit(&mask, value).unwrap(),
                    amount_enc: [4; 8],
                    view_tag: [5; 3],
                    ephemeral_pubkey: [6; 32],
                    anchor_enc: [7; 16],
                });
                secrets.push(OutputSecret {
                    amount: value,
                    mask,
                });
            }
            let prefix = TxPrefix {
                version: VERSION,
                inputs: vec![Input {
                    key_image: key_image(&input.secret_key).unwrap(),
                }],
                outputs: prefix_outputs,
                fee,
                extra: vec![],
            };
            let prunable = prove(
                &mut rng,
                chain_id,
                &prefix,
                std::slice::from_ref(&input),
                &secrets,
            )
            .expect("proving");
            Transaction { prefix, prunable }
        };
        // proofs have the same size whatever the fee, and every field is fixed-width: so one build gives
        // the size, and the second carries the real fee
        let size = build(0).to_bytes().unwrap().len() as u64;
        let min = fees::dynamic_min_fee(size, next.reward, next.median).unwrap();
        build(min + extra_fee)
    }
}

fn ready(name: &str) -> Net {
    let net = Net::new(name);
    net.coinbase_blocks(SPEND_HEIGHT - 1);
    assert_eq!(net.height(), SPEND_HEIGHT - 1);
    net
}

#[test]
fn a_real_spend_is_accepted_with_proofs_checked_and_recorded() {
    let net = ready("accept");
    let chain_id = net.store.chain_id();
    let tx = net.spend(&chain_id, 3, 0, 1);
    let image = tx.prefix.inputs[0].key_image;
    let b = net.block(vec![tx]);
    let v = net.accept(&RingCtProofs, &b);
    assert!(v.proofs_checked, "RingCtProofs really checks, and says so");
    assert_eq!(v.height, SPEND_HEIGHT);
    assert_eq!(
        net.store.key_image_height(&image).unwrap(),
        Some(SPEND_HEIGHT)
    );
}

#[test]
fn a_broken_proof_is_refused_under_ringct_and_accepted_when_proofs_are_not_checked() {
    let net = ready("broken");
    let chain_id = net.store.chain_id();
    // one byte of a signature flipped: everything else about the block is valid
    let mut tx = net.spend(&chain_id, 3, 0, 1);
    let last = tx.prunable.proof_data.len() - 1;
    tx.prunable.proof_data[last] ^= 1;
    let b = net.block(vec![tx]);

    let err = net
        .validator(&RingCtProofs)
        .accept_block(&b, NOW)
        .expect_err("a block with a broken signature must be refused");
    assert!(
        matches!(err, BlockError::ProofRejected { tx: 0, .. }),
        "{err:?}"
    );
    assert_eq!(
        net.height(),
        SPEND_HEIGHT - 1,
        "a refused block changed the chain"
    );

    // the hole the default leaves open
    let v = net.accept(&ProofsNotChecked, &b);
    assert!(!v.proofs_checked);
}

#[test]
fn a_spend_made_for_another_chain_is_refused() {
    let net = ready("otherchain");
    let tx = net.spend(&[0xab; 32], 3, 0, 1);
    let b = net.block(vec![tx]);
    let err = net
        .validator(&RingCtProofs)
        .accept_block(&b, NOW)
        .unwrap_err();
    match err {
        BlockError::ProofRejected { reason, .. } => {
            assert!(reason.contains("ring signature"), "{reason}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn paying_more_fee_than_the_proofs_account_for_is_refused() {
    // The fee is a public field: raising it after signing leaves the balance unsatisfied.
    let net = ready("fee");
    let chain_id = net.store.chain_id();
    let mut tx = net.spend(&chain_id, 3, 0, 1);
    tx.prefix.fee += 1;
    let b = net.block(vec![tx]);
    let err = net
        .validator(&RingCtProofs)
        .accept_block(&b, NOW)
        .unwrap_err();
    match err {
        BlockError::ProofRejected { reason, .. } => assert!(reason.contains("balance"), "{reason}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn spending_the_same_output_again_is_refused_even_with_fresh_valid_proofs() {
    let net = ready("double");
    let chain_id = net.store.chain_id();
    let first = net.block(vec![net.spend(&chain_id, 3, 0, 1)]);
    net.accept(&RingCtProofs, &first);
    // a new signature (different randomness and outputs) for the same output: same key image
    let again = net.spend(&chain_id, 3, 0, 2);
    let b = net.block(vec![again]);
    let err = net
        .validator(&RingCtProofs)
        .accept_block(&b, NOW)
        .unwrap_err();
    assert!(matches!(err, BlockError::KeyImageSpent { .. }), "{err:?}");
}

#[test]
fn two_different_outputs_can_be_spent_in_one_block() {
    let net = ready("two");
    let chain_id = net.store.chain_id();
    let txs = vec![net.spend(&chain_id, 3, 0, 1), net.spend(&chain_id, 9, 0, 2)];
    let b = net.block(txs);
    assert!(net.accept(&RingCtProofs, &b).proofs_checked);
}
