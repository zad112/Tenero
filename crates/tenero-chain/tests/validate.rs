//! The block validator. Blocks are really mined (on the SHA-256 test chain) and really accepted, and every
//! rule is tested by breaking exactly that rule in an otherwise valid block and checking for exactly that error.

use std::path::PathBuf;
use std::sync::Mutex;
use tenero_chain::{
    Accepted, BlockError, ChainParams, MatmulPow, Outcome, PowCheck, ProofCheck, ProofsNotChecked,
    Sha256Pow, TxContext, Validator,
};
use tenero_core::difficulty;
use tenero_core::fees;
use tenero_core::hash::sha256;
use tenero_core::matmulhash::{self, Params};
use tenero_core::u256::U256;
use tenero_core::v2::ids::PowKind;
use tenero_core::v3::rules::{self, ShapeError};
use tenero_core::v3::*;
use tenero_store::Store;
use tenero_tree::hash_to_point;

/// The proof length of a "big" transaction: it weighs about 15,300 (a quarter of its proof), so ten of them are over the
/// 150,000 median and under twice it.
const BIG_PROOF: usize = 60_000;

const LABEL: &str = "tenero chain test network";
/// The clock: far enough ahead of every test block that none is "not yet" unless a test says so.
const NOW: u64 = 4_000_000_000;
const T0: u64 = 1_700_000_000;

struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> TempDb {
        let p = std::env::temp_dir().join(format!(
            "tenero-chain-test-{}-{name}.redb",
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

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn bytes<const N: usize>(&mut self) -> [u8; N] {
        let mut b = [0u8; N];
        for c in b.iter_mut() {
            *c = self.next() as u8;
        }
        b
    }
    /// A valid point (prime order, canonical): what every one-time address, commitment and key image must be.
    fn point(&mut self) -> [u8; 32] {
        hash_to_point(self.bytes())
    }
}

fn test_params() -> ChainParams {
    // a target that about one hash in four meets, so blocks mine instantly
    ChainParams::version_3(LABEL, PowKind::Sha256, U256::pow2(254).unwrap())
}

/// A network under test: a store, the rules, and a way to build valid blocks on its tip.
struct Net {
    _db: TempDb,
    store: Store,
    params: ChainParams,
    pow: Sha256Pow,
    proofs: ProofsNotChecked,
    rng: Rng,
    next_key: u64,
}

impl Net {
    fn with_params(name: &str, params: ChainParams) -> Net {
        let db = TempDb::new(name);
        let store = Store::open(&db.0, LABEL, PowKind::Sha256).unwrap();
        Net {
            _db: db,
            store,
            params,
            pow: Sha256Pow,
            proofs: ProofsNotChecked,
            rng: Rng(0x2545_f491_4f6c_dd1d),
            next_key: 0,
        }
    }

    fn new(name: &str) -> Net {
        Net::with_params(name, test_params())
    }

    /// A network with `n` coinbase-only blocks already on it.
    fn prepared(name: &str, n: usize) -> Net {
        let mut net = Net::new(name);
        net.coinbase_blocks(n);
        net
    }

    fn validator(&self) -> Validator<'_> {
        Validator::new(&self.store, &self.params, &self.pow, &self.proofs)
    }

    fn height(&self) -> u64 {
        self.store.tip().unwrap().0
    }

    fn tip_timestamp(&self) -> u64 {
        self.store.tip().unwrap().1.header.timestamp
    }

    fn key_image(&mut self) -> [u8; 32] {
        self.next_key += 1;
        hash_to_point(sha256(&[b"key image", &self.next_key.to_le_bytes()]))
    }

    /// A transaction with `n_inputs` inputs and two outputs (all valid points, in order), its reference block the tip,
    /// a proof of `proof_len` bytes that only `ProofsNotChecked` accepts, paying exactly the minimum fee plus `fee_delta`.
    fn tx(&mut self, n_inputs: usize, proof_len: usize, fee_delta: i64) -> Transaction {
        let next = self.validator().next_block().unwrap();
        let mut keys: Vec<[u8; 32]> = (0..n_inputs).map(|_| self.key_image()).collect();
        keys.sort();
        let mut outputs: Vec<Output> = (0..2)
            .map(|_| Output {
                onetime_address: self.rng.point(),
                amount_commitment: self.rng.point(),
                amount_enc: self.rng.bytes(),
                view_tag: self.rng.bytes(),
                anchor_enc: self.rng.bytes(),
            })
            .collect();
        outputs.sort_by_key(|o| o.onetime_address);
        let mut t = Transaction {
            prefix: TxPrefix {
                version: VERSION,
                inputs: keys
                    .into_iter()
                    .map(|key_image| Input { key_image })
                    .collect(),
                outputs,
                ephemeral_pubkeys: vec![self.rng.point()],
                fee: 0,
                encrypted_payment_id: self.rng.bytes(),
            },
            prunable: Prunable {
                reference_height: self.height(),
                proof_data: vec![7; proof_len],
            },
        };
        let size = t.to_bytes().unwrap().len() as u64;
        let min = rules::min_fee(size, next.reward, next.median).unwrap();
        t.prefix.fee = u64::try_from(i64::try_from(min).unwrap() + fee_delta).unwrap();
        t
    }

    /// A spend with two inputs and a small proof (the tree must hold outputs: from block 61 on).
    fn spend(&mut self) -> Transaction {
        self.tx(2, 200, 0)
    }

    fn mine_to(&self, b: &mut Block, target: &U256) {
        for nonce in 0.. {
            b.header.nonce = nonce;
            if U256::from_be_bytes(&ids::block_id(&b.header, PowKind::Sha256)) < *target {
                return;
            }
        }
    }

    fn mine(&self, b: &mut Block) {
        let target = self.validator().next_block().unwrap().target;
        self.mine_to(b, &target);
    }

    /// Recomputes the Merkle root and mines: what a builder does after changing a block's contents.
    fn finish(&self, b: &mut Block) {
        b.header.tx_root = ids::block_tx_root(&b.coinbase, &b.transactions).unwrap();
        self.mine(b);
    }

    /// A valid block on the tip carrying `txs`, its coinbase paying exactly what the rules say, mined.
    fn block(&mut self, txs: Vec<Transaction>) -> Block {
        self.block_at(txs, None)
    }

    fn block_at(&mut self, txs: Vec<Transaction>, timestamp: Option<u64>) -> Block {
        let next = self.validator().next_block().unwrap();
        let weight: u64 = txs.iter().map(|t| rules::tx_weight(t).unwrap()).sum();
        let fees_total: u64 = txs.iter().map(|t| t.prefix.fee).sum();
        let amount = self
            .validator()
            .coinbase_amount(&next, weight, fees_total)
            .unwrap();
        let ts = timestamp.unwrap_or_else(|| {
            let after_tip = if self.height() == 0 {
                T0
            } else {
                self.tip_timestamp() + 60
            };
            after_tip.max(u64::try_from(next.min_timestamp).unwrap_or(0))
        });
        let coinbase = Coinbase {
            version: VERSION,
            height: next.height,
            outputs: vec![CoinbaseOutput {
                onetime_address: self.rng.point(),
                amount,
                view_tag: self.rng.bytes(),
                ephemeral_pubkey: self.rng.point(),
                anchor_enc: self.rng.bytes(),
            }],
            extra: vec![],
        };
        let mut b = Block {
            header: BlockHeader {
                version: VERSION,
                prev_id: next.prev_id,
                timestamp: ts,
                tx_root: [0; 32],
                nonce: 0,
                mix: [0; 64],
            },
            coinbase,
            transactions: txs,
        };
        self.finish(&mut b);
        b
    }

    fn accept(&mut self, b: &Block) -> tenero_chain::ValidatedBlock {
        match self.validator().accept_block(b, NOW) {
            Ok(Accepted::Added(v)) => v,
            other => panic!("the block was not accepted: {other:?}"),
        }
    }

    fn coinbase_blocks(&mut self, n: usize) {
        for _ in 0..n {
            let b = self.block(vec![]);
            self.accept(&b);
        }
    }

    /// The error `validate_block` gives for `b`, checking that the store was not touched.
    fn rejects(&self, b: &Block) -> BlockError {
        let before = (
            self.store.state_digest().unwrap(),
            self.store.tip().unwrap().0,
        );
        let err = self
            .validator()
            .accept_block(b, NOW)
            .expect_err("the block should have been refused");
        assert_eq!(
            (
                self.store.state_digest().unwrap(),
                self.store.tip().unwrap().0
            ),
            before,
            "a refused block changed the store"
        );
        err
    }
}

/// A height at which the curve tree holds outputs (block 1's coinbase enters it with block 60): the tests spend from here.
const FIRST_SPEND_HEIGHT: u64 = 76;

// ------------------------------------------------------------------ a valid chain

#[test]
fn a_chain_of_valid_blocks_is_mined_accepted_and_recorded() {
    let mut net = Net::prepared("valid", FIRST_SPEND_HEIGHT as usize - 1);
    assert_eq!(net.height(), 75);
    // from block 76 on there are transactions
    let mut sizes = vec![];
    for _ in 0..5 {
        let txs = vec![net.spend(), net.spend()];
        sizes.push(
            txs.iter()
                .map(|t| rules::tx_weight(t).unwrap())
                .sum::<u64>(),
        );
        let b = net.block(txs);
        let v = net.validator().validate_block(&b, NOW).unwrap();
        let Outcome::Valid(v) = v else {
            panic!("not valid")
        };
        assert!(
            !v.proofs_checked,
            "the proofs are not checked, and the result must say so"
        );
        assert_eq!(net.accept(&b).height, v.height);
    }
    assert_eq!(net.height(), 80);
    // what the store recorded is what the validator worked out: the target, the weight, the work
    let mut total = U256::ZERO;
    for h in 1..=80u64 {
        let idx = net.store.block_index(h).unwrap().unwrap();
        total = total
            .checked_add(&U256::work_of_target(&U256::from_be_bytes(&idx.target)).unwrap())
            .unwrap();
        assert_eq!(
            U256::from_be_bytes(&idx.cumulative_work),
            total,
            "cumulative work at {h}"
        );
        if h > 75 {
            assert_eq!(idx.body_weight, sizes[h as usize - 76]);
            assert_eq!(
                idx.long_term_weight,
                sizes[h as usize - 76],
                "under 1.4 times the floor"
            );
            assert_eq!(idx.tx_count, 2);
        } else {
            assert_eq!(idx.body_weight, 0);
        }
    }
    // the outputs the spends created are there, and the key images are spent
    let stored = net.store.get_block(80).unwrap().unwrap();
    for t in &stored.transactions {
        for i in &t.tx.prefix.inputs {
            assert_eq!(net.store.key_image_height(&i.key_image).unwrap(), Some(80));
        }
    }
}

#[test]
fn the_required_target_follows_the_timestamps_and_agrees_with_the_full_history_function() {
    let mut net = Net::new("difficulty");
    let p = net.params.difficulty;
    let mut timestamps: Vec<i64> = vec![];
    // blocks 10 seconds apart (six times too fast), then 200 seconds apart (over three times too slow)
    let gaps: Vec<u64> = (0..45).map(|_| 10).chain((0..45).map(|_| 200)).collect();
    let mut ts = T0;
    let mut targets_seen = vec![];
    for gap in gaps {
        ts += gap;
        let b = net.block_at(vec![], Some(ts));
        net.accept(&b);
        timestamps.push(ts as i64);
        // what the windowed, store-backed validator says the NEXT block needs, against the function that
        // reads the whole history (the one the vectors test)
        let want = difficulty::required_targets(&p, &timestamps).unwrap();
        let next = net.validator().next_block().unwrap();
        assert_eq!(
            next.target,
            *want.last().unwrap(),
            "after block {}",
            timestamps.len()
        );
        targets_seen.push(next.target);
    }
    // blocks 10 s apart make the target fall (harder), blocks 200 s apart make it rise
    assert!(
        targets_seen[40] < targets_seen[3],
        "fast blocks should make mining harder"
    );
    assert!(
        targets_seen[89] > targets_seen[44],
        "slow blocks should make mining easier"
    );
}

// ------------------------------------------------------------------ the header

#[test]
fn version_parent_and_time() {
    let mut net = Net::prepared("header", 20);
    let good = net.block(vec![]);
    assert!(matches!(
        net.validator().validate_block(&good, NOW),
        Ok(Outcome::Valid(_))
    ));

    let mut b = good.clone();
    b.header.version = 2;
    net.mine(&mut b);
    assert_eq!(net.rejects(&b), BlockError::BadVersion(2));

    let mut b = good.clone();
    b.header.prev_id = [9; 32];
    net.mine(&mut b);
    assert_eq!(net.rejects(&b), BlockError::BadParent);

    // a block's timestamp must be LATER than its parent's (M11.2; it replaced "not below the median of the last 11"). With blocks
    // 60 s apart from T0 the parent (block 20) is at T0 + 60 * 19, and the earliest the next may be is one second after it.
    let parent = net.tip_timestamp();
    assert_eq!(parent, T0 + 60 * 19);
    let earliest = net.validator().next_block().unwrap().min_timestamp;
    assert_eq!(earliest, i64::try_from(parent + 1).unwrap());
    // equal to the parent's: refused (the old rule allowed it), and one second earlier too
    for early in [parent, parent - 1] {
        let b = net.block_at(vec![], Some(early));
        assert_eq!(
            net.rejects(&b),
            BlockError::TimestampTooEarly {
                timestamp: early,
                earliest
            }
        );
    }
    // where the old median rule drew its line (block 15's time, T0 + 60 * 14) and anything up to the parent's own time: refused now
    for old_ok in [T0 + 60 * 14, T0 + 60 * 14 + 1, parent - 30] {
        let b = net.block_at(vec![], Some(old_ok));
        assert!(
            matches!(net.rejects(&b), BlockError::TimestampTooEarly { .. }),
            "{old_ok}"
        );
    }
    // exactly one second after the parent's: allowed
    let b = net.block_at(vec![], Some(earliest as u64));
    assert!(
        matches!(
            net.validator().validate_block(&b, NOW),
            Ok(Outcome::Valid(_))
        ),
        "one second after the parent is allowed"
    );
}

/// On a young chain the parent of block 1 is the genesis block, whose time is 0: block 1 must be later than that, so any real timestamp does,
/// and a block with timestamp 0 does not. After that the rule is the same as everywhere: one second after the parent's.
#[test]
fn on_a_young_chain_block_1_must_be_later_than_the_genesis_time_of_0() {
    let mut net = Net::new("young");
    assert_eq!(
        net.validator().next_block().unwrap().min_timestamp,
        1,
        "the genesis block's time is 0, so block 1 must carry at least 1"
    );
    let b = net.block_at(vec![], Some(0));
    assert_eq!(
        net.rejects(&b),
        BlockError::TimestampTooEarly {
            timestamp: 0,
            earliest: 1
        }
    );
    let b = net.block_at(vec![], Some(T0));
    net.accept(&b);
    assert_eq!(
        net.validator().next_block().unwrap().min_timestamp,
        i64::try_from(T0 + 1).unwrap()
    );
    let b = net.block_at(vec![], Some(T0 + 60));
    net.accept(&b);
    let next = net.validator().next_block().unwrap();
    assert_eq!(next.min_timestamp, i64::try_from(T0 + 61).unwrap());
    let b = net.block_at(vec![], Some(T0 + 30));
    assert_eq!(
        net.rejects(&b),
        BlockError::TimestampTooEarly {
            timestamp: T0 + 30,
            earliest: next.min_timestamp
        }
    );
    // the future limit is still held, and is a hold, not a refusal
    let far = net.block_at(vec![], Some(NOW + 100_000));
    assert!(matches!(
        net.validator().validate_block(&far, NOW),
        Ok(Outcome::NotYet)
    ));
}

/// The block-weight median is the upper median of the last ten block weights, never below the 150,000 floor: one heavy
/// block does not move it, five of the last ten do, and it falls back as they slide out of the window.
#[test]
fn the_median_follows_the_last_ten_block_weights() {
    let mut net = Net::prepared("median", FIRST_SPEND_HEIGHT as usize - 1);
    let big_block = |net: &mut Net| {
        let txs: Vec<Transaction> = (0..10).map(|_| net.tx(2, BIG_PROOF, 0)).collect();
        net.block(txs)
    };
    let median = |net: &Net| net.validator().next_block().unwrap().median;
    assert_eq!(median(&net), 150_000);
    let mut weight = 0;
    for i in 1..=4 {
        let b = big_block(&mut net);
        weight = net.accept(&b).meta.body_weight;
        assert!(weight > 150_000);
        assert_eq!(
            median(&net),
            150_000,
            "{i} heavy block(s) of the last ten: still the floor"
        );
    }
    // the fifth heavy block makes half of the window heavy: the upper median (index 5 of 10) is now their weight
    let b = big_block(&mut net);
    net.accept(&b);
    assert_eq!(median(&net), weight);
    // a block of exactly that weight now carries no penalty, so the coinbase pays reward + fees exactly
    let b = big_block(&mut net);
    let fees_total: u64 = b.transactions.iter().map(|t| t.prefix.fee).sum();
    assert_eq!(
        b.coinbase.outputs[0].amount,
        2_000_000_000 + fees_total,
        "no penalty at the median"
    );
    net.accept(&b);
    // ten light blocks later the heavy ones have left the window and the median is the floor again
    net.coinbase_blocks(10);
    assert_eq!(median(&net), 150_000);
}

/// The long-term median (15.4): a block's long-term weight is its weight, at most 1.4 times the long-term median, and the
/// median a block is judged by is at most ten times the long-term median. On a chain far shorter than the window, the
/// long-term median stays at the floor however heavy the blocks are.
#[test]
fn heavy_blocks_record_a_long_term_weight_of_at_most_one_point_four_times_the_floor() {
    let mut net = Net::prepared("long-term", FIRST_SPEND_HEIGHT as usize - 1);
    for _ in 0..3 {
        // fifteen heavy transactions: about 230,000, over 1.4 times the floor (and under twice it)
        let txs: Vec<Transaction> = (0..15).map(|_| net.tx(2, BIG_PROOF, 0)).collect();
        let b = net.block(txs);
        let v = net.accept(&b);
        assert!(v.meta.body_weight > 210_000, "{}", v.meta.body_weight);
        assert_eq!(v.meta.long_term_weight, 210_000);
        let idx = net.store.block_index(v.height).unwrap().unwrap();
        assert_eq!(idx.long_term_weight, v.meta.long_term_weight);
    }
    assert_eq!(
        net.validator().next_block().unwrap().long_term_median,
        150_000
    );
}

#[test]
fn a_block_too_far_ahead_is_not_yet_and_never_invalid() {
    let mut net = Net::prepared("future", 5);
    let b = net.block(vec![]);
    let ts = b.header.timestamp;
    // 121 seconds ahead of the clock: hold it; exactly 120 ahead: fine
    assert_eq!(
        net.validator().validate_block(&b, ts - 121).unwrap(),
        Outcome::NotYet
    );
    assert!(matches!(
        net.validator().validate_block(&b, ts - 120),
        Ok(Outcome::Valid(_))
    ));
    assert_eq!(
        net.validator().accept_block(&b, ts - 121).unwrap(),
        Accepted::NotYet
    );
    assert_eq!(
        net.height(),
        5,
        "a block that is not yet acceptable is not stored"
    );
    // once the clock has caught up the very same block is accepted: it was never invalid
    assert!(matches!(
        net.validator().accept_block(&b, ts).unwrap(),
        Accepted::Added(_)
    ));
    assert_eq!(net.height(), 6);
}

#[test]
fn with_the_adjustment_off_the_timestamp_rule_is_off_and_the_target_is_fixed() {
    let mut params = test_params();
    params.difficulty.window = 0;
    let mut net = Net::with_params("fixed", params);
    net.coinbase_blocks(3);
    let start = net.params.difficulty.start_target;
    assert_eq!(net.validator().next_block().unwrap().target, start);
    assert_eq!(net.validator().next_block().unwrap().min_timestamp, 0);
    // a timestamp far in the past would break the median rule if it were on
    let b = net.block_at(vec![], Some(5));
    net.accept(&b);
    assert_eq!(net.height(), 4);
}

#[test]
fn a_target_whose_work_cannot_be_represented_is_refused() {
    let mut params = test_params();
    params.difficulty.window = 0;
    params.difficulty.start_target = U256::ONE;
    let net = Net::with_params("target-one", params);
    let mut b = Block {
        header: BlockHeader {
            version: VERSION,
            prev_id: net.store.chain_id(),
            timestamp: T0,
            tx_root: [0; 32],
            nonce: 0,
            mix: [0; 64],
        },
        coinbase: Coinbase {
            version: VERSION,
            height: 1,
            outputs: vec![],
            extra: vec![],
        },
        transactions: vec![],
    };
    b.coinbase.outputs.push(CoinbaseOutput {
        onetime_address: hash_to_point([1; 32]),
        amount: 2_000_000_000,
        view_tag: [0; 3],
        ephemeral_pubkey: [2; 32],
        anchor_enc: [0; 16],
    });
    b.header.tx_root = ids::block_tx_root(&b.coinbase, &b.transactions).unwrap();
    assert_eq!(net.rejects(&b), BlockError::TargetUnrepresentable);
}

// ------------------------------------------------------------------ proof of work

#[test]
fn proof_of_work_the_cheap_check_then_the_full_check() {
    let mut net = Net::prepared("pow", 3);
    let good = net.block(vec![]);
    let target = net.validator().next_block().unwrap().target;

    // a nonce whose id is not below the target
    let mut b = good.clone();
    for nonce in 0.. {
        b.header.nonce = nonce;
        if U256::from_be_bytes(&ids::block_id(&b.header, PowKind::Sha256)) >= target {
            break;
        }
    }
    assert_eq!(net.rejects(&b), BlockError::PowTargetNotMet);

    // mined against an easier target than the chain requires: meets 2^255, not the required target
    let mut b = good.clone();
    let easy = U256::pow2(255).unwrap();
    for nonce in 0.. {
        b.header.nonce = nonce;
        let id = U256::from_be_bytes(&ids::block_id(&b.header, PowKind::Sha256));
        if id >= target && id < easy {
            break;
        }
    }
    assert_eq!(net.rejects(&b), BlockError::PowTargetNotMet);

    // the cheap check passes but the full check does not: on the SHA-256 chain the mix must be zero
    let mut b = good.clone();
    b.header.mix = [1; 64];
    net.mine(&mut b);
    assert!(U256::from_be_bytes(&ids::block_id(&b.header, PowKind::Sha256)) < target);
    assert!(matches!(net.rejects(&b), BlockError::PowInvalid(_)));
}

#[test]
fn a_forged_mix_that_meets_the_target_is_caught_by_the_full_matmulhash_check() {
    // tiny parameters, so the dataset is a few KiB and a "full check" costs microseconds
    let small = Params {
        m: 8,
        k: 64,
        nb: 64,
        num_blocks: 8,
    };
    let pow = MatmulPow::new(small, 3, 1).unwrap();
    let mut params = ChainParams::version_3(LABEL, PowKind::Matmul, U256::pow2(253).unwrap());
    params.pow_kind = PowKind::Matmul;
    let db = TempDb::new("matmul");
    let store = Store::open(&db.0, LABEL, PowKind::Matmul).unwrap();
    let proofs = ProofsNotChecked;
    let mut rng = Rng(0xdead_beef_cafe_f00d);
    let mut ts = T0;

    // mine and accept six blocks: three epochs' worth of two datasets (epochs of 3 blocks)
    for h in 1..=6u64 {
        let v = Validator::new(&store, &params, &pow, &proofs);
        let next = v.next_block().unwrap();
        let coinbase = Coinbase {
            version: VERSION,
            height: h,
            outputs: vec![CoinbaseOutput {
                onetime_address: rng.point(),
                amount: next.reward,
                view_tag: [0; 3],
                ephemeral_pubkey: rng.point(),
                anchor_enc: [0; 16],
            }],
            extra: vec![],
        };
        let mut b = Block {
            header: BlockHeader {
                version: VERSION,
                prev_id: next.prev_id,
                timestamp: ts,
                tx_root: ids::block_tx_root(&coinbase, &[]).unwrap(),
                nonce: 0,
                mix: [0; 64],
            },
            coinbase,
            transactions: vec![],
        };
        ts += 60;
        let data = pow.dataset_for(h).unwrap();
        let hh = ids::header_hash(&b.header);
        for nonce in 0.. {
            let a = matmulhash::compute_attempt(&data, &hh, nonce).unwrap();
            if U256::from_be_bytes(&a.digest) < next.target {
                b.header.nonce = nonce;
                b.header.mix = a.mix;
                break;
            }
        }
        assert!(
            matches!(v.accept_block(&b, NOW).unwrap(), Accepted::Added(_)),
            "block {h}"
        );
    }
    assert_eq!(store.tip().unwrap().0, 6);

    // now a FORGED block: a made-up mix, ground until sha256(seed || mix) is below the target. It meets the
    // cheap check (the id is below the target) but it is not what the proof of work produces.
    let v = Validator::new(&store, &params, &pow, &proofs);
    let next = v.next_block().unwrap();
    let coinbase = Coinbase {
        version: VERSION,
        height: 7,
        outputs: vec![CoinbaseOutput {
            onetime_address: hash_to_point([5; 32]),
            amount: next.reward,
            view_tag: [0; 3],
            ephemeral_pubkey: [6; 32],
            anchor_enc: [0; 16],
        }],
        extra: vec![],
    };
    let mut forged = Block {
        header: BlockHeader {
            version: VERSION,
            prev_id: next.prev_id,
            timestamp: ts,
            tx_root: ids::block_tx_root(&coinbase, &[]).unwrap(),
            nonce: 12345,
            mix: [0; 64],
        },
        coinbase,
        transactions: vec![],
    };
    for _ in 0..100_000 {
        forged.header.mix = rng.bytes();
        if pow.check_cheap(&forged.header, &next.target) {
            break;
        }
    }
    assert!(
        pow.check_cheap(&forged.header, &next.target),
        "the forgery meets the cheap check"
    );
    let before = store.state_digest().unwrap();
    assert!(matches!(
        v.accept_block(&forged, NOW),
        Err(BlockError::PowInvalid(_))
    ));
    assert_eq!(store.state_digest().unwrap(), before);
    // and the honest version of the same block goes in
    let data = pow.dataset_for(7).unwrap();
    let hh = ids::header_hash(&forged.header);
    let mut honest = forged.clone();
    for nonce in 0.. {
        let a = matmulhash::compute_attempt(&data, &hh, nonce).unwrap();
        if U256::from_be_bytes(&a.digest) < next.target {
            honest.header.nonce = nonce;
            honest.header.mix = a.mix;
            break;
        }
    }
    assert!(matches!(
        v.accept_block(&honest, NOW).unwrap(),
        Accepted::Added(_)
    ));
}

/// The gather fork (`CONSENSUS.md` 8.3): below the fork height a block needs the first design's mix, from it on the
/// gathered one, and a block mined with the other design is refused on each side, including right at the boundary.
#[test]
fn across_the_gather_fork_each_side_accepts_only_its_own_design() {
    let small = Params {
        m: 8,
        k: 64,
        nb: 64,
        num_blocks: 8,
    };
    const FORK: u64 = 4;
    let pow = MatmulPow::new(small, 3, 1).unwrap().gathered_from(FORK);
    assert_eq!(pow.gather_from(), FORK);
    let params = ChainParams::version_3(LABEL, PowKind::Matmul, U256::pow2(253).unwrap());
    let db = TempDb::new("gatherfork");
    let store = Store::open(&db.0, LABEL, PowKind::Matmul).unwrap();
    let proofs = ProofsNotChecked;
    let mut rng = Rng(0x0123_4567_89ab_cdef);
    let mut ts = T0;

    // a block for the next height, mined with the gathered design or the first one
    let mut make = |v: &Validator<'_>, gathered: bool, ts: u64| -> Block {
        let next = v.next_block().unwrap();
        let coinbase = Coinbase {
            version: VERSION,
            height: next.height,
            outputs: vec![CoinbaseOutput {
                onetime_address: rng.point(),
                amount: next.reward,
                view_tag: [0; 3],
                ephemeral_pubkey: rng.point(),
                anchor_enc: [0; 16],
            }],
            extra: vec![],
        };
        let mut b = Block {
            header: BlockHeader {
                version: VERSION,
                prev_id: next.prev_id,
                timestamp: ts,
                tx_root: ids::block_tx_root(&coinbase, &[]).unwrap(),
                nonce: 0,
                mix: [0; 64],
            },
            coinbase,
            transactions: vec![],
        };
        let data = pow.dataset_for(next.height).unwrap();
        let hh = ids::header_hash(&b.header);
        for nonce in 0.. {
            let a = if gathered {
                matmulhash::compute_gathered_attempt(&data, &hh, nonce).unwrap()
            } else {
                matmulhash::compute_attempt(&data, &hh, nonce).unwrap()
            };
            if U256::from_be_bytes(&a.digest) < next.target {
                b.header.nonce = nonce;
                b.header.mix = a.mix;
                break;
            }
        }
        b
    };

    for h in 1..=6u64 {
        let v = Validator::new(&store, &params, &pow, &proofs);
        let before = store.state_digest().unwrap();
        let wrong = make(&v, h < FORK, ts);
        assert!(
            matches!(v.accept_block(&wrong, NOW), Err(BlockError::PowInvalid(_))),
            "height {h}: the other design was accepted"
        );
        assert_eq!(store.state_digest().unwrap(), before);
        let right = make(&v, h >= FORK, ts);
        assert!(
            matches!(v.accept_block(&right, NOW).unwrap(), Accepted::Added(_)),
            "height {h}: its own design was refused"
        );
        ts += 60;
    }
    assert_eq!(store.tip().unwrap().0, 6);
}

// ------------------------------------------------------------------ the body

#[test]
fn the_merkle_root_must_match_the_block() {
    let mut net = Net::prepared("root", FIRST_SPEND_HEIGHT as usize - 1);
    let tx = net.spend();
    let good = net.block(vec![tx]);
    let mut b = good.clone();
    b.header.tx_root = [1; 32];
    net.mine(&mut b);
    assert_eq!(net.rejects(&b), BlockError::BadTxRoot);
    // changing anything in a transaction without fixing the root
    let mut b = good.clone();
    b.transactions[0].prunable.proof_data[0] ^= 1;
    net.mine(&mut b);
    assert_eq!(net.rejects(&b), BlockError::BadTxRoot);
    let mut b = good.clone();
    b.coinbase.extra = vec![1];
    net.mine(&mut b);
    assert_eq!(net.rejects(&b), BlockError::BadTxRoot);
}

#[test]
fn the_weight_penalty_and_the_hard_limit() {
    let mut net = Net::prepared("size", FIRST_SPEND_HEIGHT as usize - 1);
    // twelve transactions of about 15,300 weight: over the 150,000 median, under twice it
    let txs: Vec<Transaction> = (0..12).map(|_| net.tx(2, BIG_PROOF, 0)).collect();
    let body: u64 = txs.iter().map(|t| rules::tx_weight(t).unwrap()).sum();
    let fees_total: u64 = txs.iter().map(|t| t.prefix.fee).sum();
    assert!(body > 150_000 && body < 300_000, "{body}");
    let next = net.validator().next_block().unwrap();
    assert_eq!(next.median, 150_000);
    let penalty = fees::penalty(next.reward, body, next.median).unwrap();
    assert!(penalty > 0 && penalty < next.reward);

    let good = net.block(txs.clone());
    assert_eq!(
        good.coinbase.outputs[0].amount,
        next.reward - penalty + fees_total
    );
    // paying the whole reward (ignoring the penalty), and one unit more or less than exact, are refused
    for wrong in [
        next.reward + fees_total,
        good.coinbase.outputs[0].amount + 1,
        good.coinbase.outputs[0].amount - 1,
    ] {
        let mut b = good.clone();
        b.coinbase.outputs[0].amount = wrong;
        net.finish(&mut b);
        assert_eq!(
            net.rejects(&b),
            BlockError::CoinbaseAmount {
                expected: next.reward - penalty + fees_total,
                got: wrong
            }
        );
    }
    net.accept(&good);

    // twenty-five of them weigh over twice the median: invalid whatever the coinbase pays
    let txs: Vec<Transaction> = (0..25).map(|_| net.tx(2, BIG_PROOF, 0)).collect();
    let body: u64 = txs.iter().map(|t| rules::tx_weight(t).unwrap()).sum();
    let size: u64 = txs.iter().map(|t| t.to_bytes().unwrap().len() as u64).sum();
    assert!(body > 300_000);
    let next = net.validator().next_block().unwrap();
    assert_eq!(
        next.median, 150_000,
        "one heavy block among empty ones does not move the median"
    );
    let fees_total: u64 = txs.iter().map(|t| t.prefix.fee).sum();
    let mut b = net.block(vec![]);
    b.transactions = txs;
    b.coinbase.outputs[0].amount = next.reward + fees_total;
    net.finish(&mut b);
    assert_eq!(
        net.rejects(&b),
        BlockError::BlockTooLarge {
            weight: body,
            size,
            weight_limit: 300_000
        }
    );
}

/// The ceilings (12 MiB of weight, 48 MiB of real bytes) need a median the long-term median takes months to reach, so the
/// validator's use of them is checked here through the rule it calls, and the rule itself by the vectors (`v3_weight.json`).
#[test]
fn the_limit_is_twice_the_median_up_to_the_ceilings() {
    assert_eq!(rules::block_limit(150_000), 300_000);
    assert_eq!(rules::block_limit(7 * 1024 * 1024), rules::MAX_BLOCK_WEIGHT);
    assert!(!rules::block_too_large(
        rules::MAX_BLOCK_WEIGHT,
        rules::MAX_BLOCK_BYTES,
        u64::MAX / 4
    ));
    assert!(rules::block_too_large(
        rules::MAX_BLOCK_WEIGHT + 1,
        0,
        u64::MAX / 4
    ));
    assert!(rules::block_too_large(
        0,
        rules::MAX_BLOCK_BYTES + 1,
        u64::MAX / 4
    ));
}

// ------------------------------------------------------------------ the coinbase

#[test]
fn the_coinbase_version_height_and_amount() {
    let mut net = Net::prepared("coinbase", 10);
    let good = net.block(vec![]);
    let reward = net.validator().next_block().unwrap().reward;
    assert_eq!(reward, 2_000_000_000);
    assert_eq!(good.coinbase.outputs[0].amount, reward);

    let mut b = good.clone();
    b.coinbase.version = 2;
    net.finish(&mut b);
    assert_eq!(net.rejects(&b), BlockError::CoinbaseVersion(2));

    let mut b = good.clone();
    b.coinbase.height = 12;
    net.finish(&mut b);
    assert_eq!(
        net.rejects(&b),
        BlockError::CoinbaseHeight {
            expected: 11,
            got: 12
        }
    );

    for wrong in [reward + 1, reward - 1, 0, u64::MAX] {
        let mut b = good.clone();
        b.coinbase.outputs[0].amount = wrong;
        net.finish(&mut b);
        assert_eq!(
            net.rejects(&b),
            BlockError::CoinbaseAmount {
                expected: reward,
                got: wrong
            }
        );
    }
    // the rule is on the SUM: two outputs adding up to exactly the reward are fine (in order, with their own keys)
    let mut b = good.clone();
    let second = CoinbaseOutput {
        amount: 1,
        onetime_address: net.rng.point(),
        ephemeral_pubkey: net.rng.point(),
        ..b.coinbase.outputs[0].clone()
    };
    b.coinbase.outputs[0].amount = reward - 1;
    b.coinbase.outputs.push(second);
    b.coinbase.outputs.sort_by_key(|o| o.onetime_address);
    net.finish(&mut b);
    assert!(matches!(
        net.validator().validate_block(&b, NOW),
        Ok(Outcome::Valid(_))
    ));
    // amounts that overflow a u64 are refused, not wrapped into a valid sum
    let mut b = good.clone();
    b.coinbase.outputs[0].amount = u64::MAX;
    b.coinbase.outputs.push(CoinbaseOutput {
        amount: reward + 1,
        onetime_address: net.rng.point(),
        ephemeral_pubkey: net.rng.point(),
        ..b.coinbase.outputs[0].clone()
    });
    b.coinbase.outputs.sort_by_key(|o| o.onetime_address);
    net.finish(&mut b);
    assert!(matches!(net.rejects(&b), BlockError::Malformed(_)));
}

#[test]
fn the_coinbase_shape_and_points() {
    let mut net = Net::prepared("coinbase-shape", 10);
    let good = net.block(vec![]);
    let two = |net: &mut Net, edit: &dyn Fn(&mut Vec<CoinbaseOutput>)| {
        let mut b = good.clone();
        let mut second = b.coinbase.outputs[0].clone();
        second.onetime_address = net.rng.point();
        second.ephemeral_pubkey = net.rng.point();
        second.amount = 0;
        b.coinbase.outputs.push(second);
        b.coinbase.outputs.sort_by_key(|o| o.onetime_address);
        edit(&mut b.coinbase.outputs);
        net.finish(&mut b);
        b
    };
    let b = two(&mut net, &|o| o.reverse());
    assert_eq!(
        net.rejects(&b),
        BlockError::CoinbaseShape(ShapeError::OutputsNotAscending)
    );
    let b = two(&mut net, &|o| o[1].ephemeral_pubkey = o[0].ephemeral_pubkey);
    assert_eq!(
        net.rejects(&b),
        BlockError::CoinbaseShape(ShapeError::EphemeralKeysRepeat)
    );
    let mut b = good.clone();
    b.coinbase.outputs[0].ephemeral_pubkey = [0; 32];
    net.finish(&mut b);
    assert_eq!(
        net.rejects(&b),
        BlockError::CoinbaseShape(ShapeError::ZeroEphemeralKey)
    );
    // a one-time address that is not a valid point: the identity, a point of small order, bytes that decode to no point
    let mut identity = [0u8; 32];
    identity[0] = 1;
    let mut not_a_point = [0xff; 32];
    not_a_point[31] = 0x7f;
    for bad in [identity, [0; 32], not_a_point] {
        let mut b = good.clone();
        b.coinbase.outputs[0].onetime_address = bad;
        net.finish(&mut b);
        assert_eq!(net.rejects(&b), BlockError::CoinbaseBadPoint { output: 0 });
    }
}

// ------------------------------------------------------------------ the transactions

fn one_tx_block(net: &mut Net, edit: impl FnOnce(&mut Transaction)) -> Block {
    let mut t = net.spend();
    edit(&mut t);
    // the coinbase is computed from the edited fees, so only the rule under test is broken
    let mut b = net.block(vec![t]);
    net.finish(&mut b);
    b
}

#[test]
fn the_version_the_fee_and_the_shape_of_a_transaction() {
    let mut net = Net::prepared("tx-shape", FIRST_SPEND_HEIGHT as usize - 1);
    let valid = one_tx_block(&mut net, |_| {});
    assert!(matches!(
        net.validator().validate_block(&valid, NOW),
        Ok(Outcome::Valid(_))
    ));

    let b = one_tx_block(&mut net, |t| t.prefix.version = 2);
    assert_eq!(net.rejects(&b), BlockError::TxVersion { tx: 0, version: 2 });

    // the fee: exactly the minimum of its real size passes (above), one unit less does not
    let low = net.tx(2, 200, -1);
    let min = low.prefix.fee + 1;
    let b = net.block(vec![low.clone()]);
    assert_eq!(
        net.rejects(&b),
        BlockError::FeeTooLow {
            tx: 0,
            fee: min - 1,
            min
        }
    );
    let at_min = net.tx(2, 200, 0);
    assert_eq!(at_min.prefix.fee, min);
    let at_min_block = net.block(vec![at_min]);
    assert!(matches!(
        net.validator().validate_block(&at_min_block, NOW),
        Ok(Outcome::Valid(_))
    ));

    // key images, and outputs, in strictly ascending order: swapped, or the same twice
    let shape = |error| BlockError::TxShape { tx: 0, error };
    let b = one_tx_block(&mut net, |t| t.prefix.inputs.reverse());
    assert_eq!(net.rejects(&b), shape(ShapeError::KeyImagesNotAscending));
    let b = one_tx_block(&mut net, |t| {
        let k = t.prefix.inputs[0].key_image;
        t.prefix.inputs[1].key_image = k;
    });
    assert_eq!(net.rejects(&b), shape(ShapeError::KeyImagesNotAscending));
    let b = one_tx_block(&mut net, |t| t.prefix.outputs.reverse());
    assert_eq!(net.rejects(&b), shape(ShapeError::OutputsNotAscending));
    let b = one_tx_block(&mut net, |t| {
        t.prefix.outputs[1].onetime_address = t.prefix.outputs[0].onetime_address
    });
    assert_eq!(net.rejects(&b), shape(ShapeError::OutputsNotAscending));
    let b = one_tx_block(&mut net, |t| t.prefix.ephemeral_pubkeys[0] = [0; 32]);
    assert_eq!(net.rejects(&b), shape(ShapeError::ZeroEphemeralKey));

    // every one-time address, commitment and key image a valid point
    let mut identity = [0u8; 32];
    identity[0] = 1;
    let b = one_tx_block(&mut net, |t| {
        t.prefix.outputs[1].amount_commitment = identity
    });
    assert_eq!(
        net.rejects(&b),
        BlockError::BadOutputPoint { tx: 0, output: 1 }
    );
    let b = one_tx_block(&mut net, |t| {
        // the largest one-time address, so the outputs stay in order
        t.prefix.outputs[1].onetime_address = [0xff; 32]
    });
    assert_eq!(
        net.rejects(&b),
        BlockError::BadOutputPoint { tx: 0, output: 1 }
    );
    let b = one_tx_block(&mut net, |t| t.prefix.inputs[0].key_image = [0; 32]);
    assert_eq!(net.rejects(&b), BlockError::BadKeyImage { tx: 0, input: 0 });

    // a transaction outside the limits of the wire format cannot even be encoded
    let b = one_tx_block(&mut net, |_| {});
    let mut bad = b.clone();
    bad.transactions[0].prefix.outputs.truncate(1);
    assert!(matches!(net.rejects(&bad), BlockError::Malformed(_)));
}

#[test]
fn the_reference_block_must_be_below_the_block_and_its_tree_hold_outputs() {
    let mut net = Net::prepared("reference", FIRST_SPEND_HEIGHT as usize - 1);
    let bad = |r: u64| BlockError::BadReference {
        tx: 0,
        reference_height: r,
    };
    // the block itself, and later: refused
    for r in [76, 77, u64::MAX] {
        let b = one_tx_block(&mut net, |t| t.prunable.reference_height = r);
        assert_eq!(net.rejects(&b), bad(r), "reference {r}");
    }
    // a block whose tree is empty (block 1's coinbase enters with block 60)
    for r in [0, 1, 59] {
        let b = one_tx_block(&mut net, |t| t.prunable.reference_height = r);
        assert_eq!(net.rejects(&b), bad(r), "reference {r}");
    }
    // the first block whose tree has an output, and the parent: fine
    for r in [60, 75] {
        let b = one_tx_block(&mut net, |t| t.prunable.reference_height = r);
        assert!(
            matches!(
                net.validator().validate_block(&b, NOW),
                Ok(Outcome::Valid(_))
            ),
            "reference {r}"
        );
    }
}

/// A reference block at most 1,440 blocks below: older is refused. (A long chain: about 1,500 blocks.)
#[test]
fn a_reference_block_more_than_1440_blocks_back_is_refused() {
    let mut net = Net::prepared("reference-age", 1500);
    let next = net.height() + 1;
    let oldest = next - rules::MAX_REFERENCE_AGE;
    let b = one_tx_block(&mut net, |t| t.prunable.reference_height = oldest);
    assert!(matches!(
        net.validator().validate_block(&b, NOW),
        Ok(Outcome::Valid(_))
    ));
    let b = one_tx_block(&mut net, |t| t.prunable.reference_height = oldest - 1);
    assert_eq!(
        net.rejects(&b),
        BlockError::BadReference {
            tx: 0,
            reference_height: oldest - 1
        }
    );
}

#[test]
fn key_images_are_spent_once_in_the_chain_and_once_in_a_block() {
    let mut net = Net::prepared("images", FIRST_SPEND_HEIGHT as usize - 1);
    let first = net.spend();
    let spent = first.prefix.inputs[0].key_image;
    let b = net.block(vec![first]);
    net.accept(&b);

    // spending it again in a later block
    let mut again = net.spend();
    again.prefix.inputs[0].key_image = spent;
    again.prefix.inputs.sort_by_key(|a| a.key_image);
    let b = net.block(vec![again]);
    assert_eq!(
        net.rejects(&b),
        BlockError::KeyImageSpent {
            tx: 0,
            key_image: spent
        }
    );

    // twice inside one block, in two transactions
    let (a, mut c) = (net.spend(), net.spend());
    let shared = a.prefix.inputs[1].key_image;
    c.prefix.inputs[0].key_image = shared;
    c.prefix.inputs.sort_by_key(|x| x.key_image);
    let b = net.block(vec![a.clone(), c]);
    assert_eq!(
        net.rejects(&b),
        BlockError::KeyImageRepeated {
            tx: 1,
            key_image: shared
        }
    );
    // whereas two transactions with different key images are fine
    let a = net.spend();
    let c = net.spend();
    let two = net.block(vec![a, c]);
    assert!(matches!(
        net.validator().validate_block(&two, NOW),
        Ok(Outcome::Valid(_))
    ));
}

/// An output can be spent once it is in the curve tree: a coinbase output enters 60 blocks after its block, any other 10
/// (the store's schedule, `v3_tree_schedule.json`). Here: the tree of each block holds exactly what the schedule says.
#[test]
fn outputs_enter_the_tree_60_blocks_after_a_coinbase_and_10_after_a_transaction() {
    let mut net = Net::prepared("maturity", 75);
    let leaves = |net: &Net, h: u64| net.store.tree_state(h).unwrap().unwrap().n_leaves;
    assert_eq!(leaves(&net, 59), 0);
    assert_eq!(leaves(&net, 60), 1, "block 1's coinbase");
    assert_eq!(leaves(&net, 75), 16);
    // a block with a transaction: its two outputs enter with block 76 + 9 = 85
    let t = net.spend();
    let b = net.block(vec![t]);
    net.accept(&b); // 76
    net.coinbase_blocks(8); // 77..=84
    assert_eq!(leaves(&net, 84), 16 + 9, "coinbases of 17..=25");
    net.coinbase_blocks(1); // 85
    assert_eq!(
        leaves(&net, 85),
        16 + 10 + 2,
        "and the transaction's two outputs"
    );
}

// ------------------------------------------------------------------ the proof hook

/// What the recorder saw of one transaction: the height, the chain id and the leaves of the reference block's tree.
type Seen = (u64, [u8; 32], u64);

struct Recorder {
    real: bool,
    fail_on_call: Option<usize>,
    seen: Mutex<Vec<Seen>>,
}

impl ProofCheck for Recorder {
    fn check_tx(&self, ctx: &TxContext<'_>) -> Result<(), String> {
        let mut seen = self.seen.lock().unwrap();
        let call = seen.len();
        seen.push((ctx.height, ctx.chain_id, ctx.tree.n_leaves));
        if self.fail_on_call == Some(call) {
            return Err("the proof does not verify".into());
        }
        Ok(())
    }

    fn checks_proofs(&self) -> bool {
        self.real
    }
}

#[test]
fn the_proof_hook_sees_every_transaction_with_its_reference_tree_and_can_reject() {
    let mut net = Net::prepared("hook", FIRST_SPEND_HEIGHT as usize - 1);
    let txs = vec![net.spend(), net.spend()];
    let b = net.block(txs.clone());

    let recorder = Recorder {
        real: true,
        fail_on_call: None,
        seen: Mutex::new(vec![]),
    };
    let v = Validator::new(&net.store, &net.params, &net.pow, &recorder);
    let Outcome::Valid(valid) = v.validate_block(&b, NOW).unwrap() else {
        panic!("not valid")
    };
    assert!(
        valid.proofs_checked,
        "a checker that verifies proofs is reported as having done so"
    );
    let seen = recorder.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    for (height, chain_id, leaves) in seen.iter() {
        assert_eq!(*height, 76);
        assert_eq!(
            *chain_id,
            net.store.chain_id(),
            "the proofs are bound to the chain id"
        );
        // the reference block is 75, whose tree holds the coinbases of blocks 1..=16
        assert_eq!(*leaves, 16);
    }
    drop(seen);

    // a rejection names the transaction and stops the block
    let failing = Recorder {
        real: true,
        fail_on_call: Some(1),
        seen: Mutex::new(vec![]),
    };
    let v = Validator::new(&net.store, &net.params, &net.pow, &failing);
    assert_eq!(
        v.accept_block(&b, NOW).unwrap_err(),
        BlockError::ProofRejected {
            tx: 1,
            reason: "the proof does not verify".into()
        }
    );
    assert_eq!(
        net.height(),
        75,
        "a block with a rejected proof is not stored"
    );
}

#[test]
fn a_rejected_block_can_be_followed_by_the_valid_one() {
    let mut net = Net::prepared("recover", 10);
    let good = net.block(vec![]);
    let mut bad = good.clone();
    bad.coinbase.height = 99;
    net.finish(&mut bad);
    let _ = net.rejects(&bad);
    assert!(matches!(
        net.validator().accept_block(&good, NOW).unwrap(),
        Accepted::Added(_)
    ));
    assert_eq!(net.height(), 11);
}

// ------------------------------------------------------------------ assume-valid

/// The SHA-256 rules, but the full proof of work always says "wrong": what a block with a forged mix meets.
struct FullCheckAlwaysFails;

impl PowCheck for FullCheckAlwaysFails {
    fn kind(&self) -> PowKind {
        PowKind::Sha256
    }

    fn check_full(&self, _header: &BlockHeader, _height: u64) -> Result<bool, String> {
        Ok(false)
    }
}

fn assumed(ids: &[[u8; 32]]) -> Option<std::sync::Arc<std::collections::HashSet<[u8; 32]>>> {
    Some(std::sync::Arc::new(ids.iter().copied().collect()))
}

fn id_of(b: &Block) -> [u8; 32] {
    ids::block_id(&b.header, PowKind::Sha256)
}

fn failing_proofs() -> Recorder {
    Recorder {
        real: true,
        fail_on_call: Some(0),
        seen: Mutex::new(vec![]),
    }
}

#[test]
fn an_assumed_block_skips_the_transaction_proofs_and_says_so_and_another_block_does_not() {
    let mut net = Net::prepared("assume-proofs", FIRST_SPEND_HEIGHT as usize - 1);
    let t = net.spend();
    let b = net.block(vec![t]);
    let failing = failing_proofs();
    // the control: the same block, not assumed, is refused for its proof
    let v = Validator::new(&net.store, &net.params, &net.pow, &failing);
    assert!(matches!(
        v.validate_block(&b, NOW),
        Err(BlockError::ProofRejected { tx: 0, .. })
    ));
    // assumed: accepted, the checker was never asked, and the result records that no proof was checked
    let failing = failing_proofs();
    let v = Validator::new(&net.store, &net.params, &net.pow, &failing)
        .with_assumed(assumed(&[id_of(&b)]));
    let Outcome::Valid(valid) = v.validate_block(&b, NOW).unwrap() else {
        panic!("not valid")
    };
    assert!(!valid.proofs_checked);
    assert!(failing.seen.lock().unwrap().is_empty());
    // a set that holds some OTHER id changes nothing for this block
    let failing = failing_proofs();
    let v = Validator::new(&net.store, &net.params, &net.pow, &failing)
        .with_assumed(assumed(&[[9; 32]]));
    assert!(matches!(
        v.validate_block(&b, NOW),
        Err(BlockError::ProofRejected { .. })
    ));
}

#[test]
fn an_assumed_block_skips_the_full_proof_of_work_but_not_the_cheap_one() {
    let mut net = Net::prepared("assume-pow", 5);
    let b = net.block(vec![]);
    let bad_full = FullCheckAlwaysFails;
    let v = Validator::new(&net.store, &net.params, &bad_full, &net.proofs);
    assert!(matches!(
        v.validate_block(&b, NOW),
        Err(BlockError::PowInvalid(_))
    ));
    let v = Validator::new(&net.store, &net.params, &bad_full, &net.proofs)
        .with_assumed(assumed(&[id_of(&b)]));
    assert!(matches!(v.validate_block(&b, NOW), Ok(Outcome::Valid(_))));

    // a block whose id misses the target is refused even when assumed: the cheap check stays
    let target = net.validator().next_block().unwrap().target;
    let mut worse = b.clone();
    while U256::from_be_bytes(&id_of(&worse)) < target {
        worse.header.nonce += 1;
    }
    let v = Validator::new(&net.store, &net.params, &bad_full, &net.proofs)
        .with_assumed(assumed(&[id_of(&worse)]));
    assert_eq!(
        v.validate_block(&worse, NOW).unwrap_err(),
        BlockError::PowTargetNotMet
    );
}

#[test]
fn an_assumed_block_is_held_to_every_other_rule() {
    let mut net = Net::prepared("assume-rules", FIRST_SPEND_HEIGHT as usize - 1);
    let t = net.spend();
    let good = net.block(vec![t]);

    // a wrong coinbase amount, correctly mined
    let mut b = good.clone();
    b.coinbase.outputs[0].amount += 1;
    net.finish(&mut b);
    let v = net.validator().with_assumed(assumed(&[id_of(&b)]));
    assert!(matches!(
        v.validate_block(&b, NOW),
        Err(BlockError::CoinbaseAmount { .. })
    ));

    // outputs out of order, and a reference block in the future (rules that are not the proof)
    let mut b = good.clone();
    b.transactions[0].prefix.outputs.reverse();
    net.finish(&mut b);
    let v = net.validator().with_assumed(assumed(&[id_of(&b)]));
    assert!(matches!(
        v.validate_block(&b, NOW),
        Err(BlockError::TxShape { .. })
    ));
    let mut b = good.clone();
    b.transactions[0].prunable.reference_height = 1_000;
    net.finish(&mut b);
    let v = net.validator().with_assumed(assumed(&[id_of(&b)]));
    assert!(matches!(
        v.validate_block(&b, NOW),
        Err(BlockError::BadReference { .. })
    ));

    // a key image that is already spent
    let t = net.spend();
    let first = net.block(vec![t]);
    net.accept(&first);
    let again = net.block(vec![first.transactions[0].clone()]);
    let v = net.validator().with_assumed(assumed(&[id_of(&again)]));
    assert!(matches!(
        v.validate_block(&again, NOW),
        Err(BlockError::KeyImageSpent { .. })
    ));

    // a wrong Merkle root (on a fresh block: the tip has moved on since `good` was built)
    let mut b = net.block(vec![]);
    b.header.tx_root = [1; 32];
    net.mine(&mut b);
    let v = net.validator().with_assumed(assumed(&[id_of(&b)]));
    assert_eq!(
        v.validate_block(&b, NOW).unwrap_err(),
        BlockError::BadTxRoot
    );
}

#[test]
fn a_pool_transaction_is_always_proof_checked_whatever_is_assumed() {
    let mut net = Net::prepared("assume-pool", FIRST_SPEND_HEIGHT as usize - 1);
    let t = net.spend();
    let failing = failing_proofs();
    let v = Validator::new(&net.store, &net.params, &net.pow, &failing)
        .with_assumed(assumed(&[[1; 32]]));
    assert!(matches!(
        v.check_pool_tx(&t),
        Err(BlockError::ProofRejected { .. })
    ));
}

/// B5 of the threat model: a stranger cannot make a node build a dataset for free. The cheap check (the id is below the target) comes before
/// anything that needs a dataset, and the epoch whose dataset the full check uses comes from the block's PARENT, not from anything the block
/// claims about itself.
#[test]
fn a_block_that_fails_the_cheap_check_costs_no_dataset_and_a_lie_about_the_height_chooses_no_epoch()
{
    let small = Params {
        m: 8,
        k: 64,
        nb: 64,
        num_blocks: 8,
    };
    let pow = MatmulPow::new(small, 3, 1).unwrap();
    let params = ChainParams::version_3(LABEL, PowKind::Matmul, U256::pow2(253).unwrap());
    let db = TempDb::new("matmul-b5");
    let store = Store::open(&db.0, LABEL, PowKind::Matmul).unwrap();
    let proofs = ProofsNotChecked;
    let v = Validator::new(&store, &params, &pow, &proofs);
    let next = v.next_block().unwrap();
    let mut rng = Rng(0xb5b5_b5b5_b5b5_b5b5);

    let block = |height: u64, mix: [u8; 64], nonce: u64| {
        let coinbase = Coinbase {
            version: VERSION,
            height,
            outputs: vec![CoinbaseOutput {
                onetime_address: hash_to_point([5; 32]),
                amount: next.reward,
                view_tag: [0; 3],
                ephemeral_pubkey: [6; 32],
                anchor_enc: [0; 16],
            }],
            extra: vec![],
        };
        Block {
            header: BlockHeader {
                version: VERSION,
                prev_id: next.prev_id,
                timestamp: T0,
                tx_root: ids::block_tx_root(&coinbase, &[]).unwrap(),
                nonce,
                mix,
            },
            coinbase,
            transactions: vec![],
        }
    };

    // 1. a block that misses the target is refused and builds nothing: not for its own epoch, not for any other
    let mut refused = 0;
    for nonce in 0..300u64 {
        let b = block(1, rng.bytes(), nonce);
        if pow.check_cheap(&b.header, &next.target) {
            continue;
        }
        assert_eq!(
            v.accept_block(&b, NOW).unwrap_err(),
            BlockError::PowTargetNotMet
        );
        refused += 1;
    }
    assert!(
        refused > 200,
        "most blocks miss a target that one in eight meets"
    );
    assert_eq!(
        pow.builds(),
        0,
        "{refused} blocks that miss the target built a dataset"
    );

    // 2. a block that meets the cheap check with a made-up mix gets the full check, once, and it is the dataset of the epoch of its PARENT
    // that is built, whatever height the block claims for itself (height 99 is epoch 33)
    let mut forged = block(99, [0; 64], 7);
    for _ in 0..100_000 {
        forged.header.mix = rng.bytes();
        if pow.check_cheap(&forged.header, &next.target) {
            break;
        }
    }
    assert!(pow.check_cheap(&forged.header, &next.target));
    assert!(matches!(
        v.accept_block(&forged, NOW),
        Err(BlockError::PowInvalid(_))
    ));
    assert_eq!(pow.builds(), 1, "the full check builds one dataset");
    assert!(pow.has_dataset(1), "the dataset of the block's real height");
    assert!(
        !pow.has_dataset(99),
        "the dataset of the height the block CLAIMED was built"
    );
    // and the same forgery again costs no second build
    let _ = v.accept_block(&forged, NOW);
    assert_eq!(pow.builds(), 1);
}

/// B8 of the threat model: the proof check is CPU a stranger can ask for, so every cheap, structural rule comes first. A transaction that
/// breaks any of them is refused without the proof hook being called once; a structurally sound one reaches it exactly once.
#[test]
fn a_transaction_that_breaks_a_structural_rule_is_refused_before_any_proof_is_checked() {
    let mut net = Net::prepared("b8", FIRST_SPEND_HEIGHT as usize - 1);
    let good = net.spend();
    let rec = Recorder {
        real: true,
        fail_on_call: None,
        seen: Mutex::new(vec![]),
    };
    let v = Validator::new(&net.store, &net.params, &net.pow, &rec);
    let calls = || rec.seen.lock().unwrap().len();

    let mut cases: Vec<(&str, Transaction)> = vec![];
    let mut t = good.clone();
    t.prefix.version += 1;
    cases.push(("a wrong version", t));
    let mut t = good.clone();
    t.prefix.fee = 0;
    cases.push(("a fee under the minimum", t));
    let mut t = good.clone();
    t.prefix.inputs.reverse();
    cases.push(("key images not in ascending order", t));
    let mut t = good.clone();
    t.prefix.outputs.reverse();
    cases.push(("outputs not in ascending order", t));
    let mut t = good.clone();
    t.prefix.ephemeral_pubkeys[0] = [0; 32];
    cases.push(("a zero ephemeral key", t));
    let mut t = good.clone();
    t.prefix.outputs[0].amount_commitment = [0; 32];
    cases.push(("a commitment that is not a valid point", t));
    let mut t = good.clone();
    t.prefix.inputs[1].key_image = [0xff; 32];
    cases.push(("a key image that is not a valid point", t));
    let mut t = good.clone();
    t.prunable.reference_height = 10_000;
    cases.push(("a reference block in the future", t));
    let mut t = good.clone();
    t.prunable.reference_height = 3;
    cases.push(("a reference block whose tree is empty", t));
    for (what, t) in &cases {
        assert!(v.check_pool_tx(t).is_err(), "{what} was accepted");
        assert_eq!(
            calls(),
            0,
            "{what}: the proof hook was called for a transaction that breaks a structural rule"
        );
    }
    // and the sound one reaches the hook, once
    v.check_pool_tx(&good).expect("a good spend is taken");
    assert_eq!(calls(), 1);
    // the proof rejects it: that is the only way to get a second call
    let failing = Recorder {
        real: true,
        fail_on_call: Some(0),
        seen: Mutex::new(vec![]),
    };
    let v = Validator::new(&net.store, &net.params, &net.pow, &failing);
    assert!(matches!(
        v.check_pool_tx(&good),
        Err(BlockError::ProofRejected { .. })
    ));
    assert_eq!(failing.seen.lock().unwrap().len(), 1);
}
