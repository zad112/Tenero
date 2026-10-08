//! The mempool and the node core. Blocks are really mined (SHA-256 test chain) and really accepted; every
//! rule is tested by breaking exactly it. A model-style test drives random operations, including
//! reorganisations, and checks after EVERY step that the pool is sound (everything in it is valid at the tip,
//! non-conflicting and unconfirmed) and complete (anything valid, offered and missing conflicts with something
//! in it).

use std::collections::HashSet;
use std::path::PathBuf;

use tenero_chain::{
    BlockError, ChainParams, ProofCheck, ProofsNotChecked, Sha256Pow, Submitted, TxContext,
    Validator,
};
use tenero_core::hash::sha256;
use tenero_core::u256::U256;
use tenero_core::v2::ids::PowKind;
use tenero_core::v3::ids::{self, tx_id};
use tenero_core::v3::rules::{self, ShapeError};
use tenero_core::v3::*;
use tenero_node::{AddOutcome, MempoolConfig, Node, NodeConfig, NodeError, PoolError, PoolLoad};
use tenero_store::Store;
use tenero_tree::hash_to_point;

const LABEL: &str = "tenero node test network";
const NOW: u64 = 4_000_000_000;
const T0: u64 = 1_700_000_000;

struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> TempDb {
        let p = std::env::temp_dir().join(format!(
            "tenero-node-test-{}-{name}.redb",
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
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn bytes<const N: usize>(&mut self) -> [u8; N] {
        let mut b = [0u8; N];
        for c in b.iter_mut() {
            *c = self.next() as u8;
        }
        b
    }
    fn point(&mut self) -> [u8; 32] {
        hash_to_point(self.bytes())
    }
}

fn params() -> ChainParams {
    ChainParams::version_3(LABEL, PowKind::Sha256, U256::pow2(254).unwrap())
}

/// Blocks before the first spend: the curve tree holds outputs from block 60 on (block 1's coinbase).
const SPENDABLE: usize = 64;
/// A "heavy" transaction's proof: it weighs about 15,300, so ten make a block over the 150,000 floor.
const BIG_PROOF: usize = 60_000;

/// Stands in for the FCMP++ check's binding to the tree: a transaction's proof is "valid" when it starts with the root of
/// the tree it is checked against. So a transaction made against one tree fails against another, as a real proof does.
struct TreeBound;

impl ProofCheck for TreeBound {
    fn check_tx(&self, ctx: &TxContext<'_>) -> Result<(), String> {
        if ctx.tx.prunable.proof_data.get(..32) == Some(&ctx.tree.root[..]) {
            Ok(())
        } else {
            Err("made against another tree".into())
        }
    }
    fn checks_proofs(&self) -> bool {
        true
    }
}

/// A store that follows one branch and builds blocks and transactions on it.
struct Net {
    _db: TempDb,
    store: Store,
    params: ChainParams,
    rng: Rng,
}

impl Net {
    fn new(name: &str, seed: u64) -> Net {
        let db = TempDb::new(name);
        let store = Store::open(&db.0, LABEL, PowKind::Sha256).unwrap();
        Net {
            _db: db,
            store,
            params: params(),
            rng: Rng(seed),
        }
    }

    fn validator(&self) -> Validator<'_> {
        Validator::new(&self.store, &self.params, &Sha256Pow, &ProofsNotChecked)
    }

    fn height(&self) -> u64 {
        self.store.tip().unwrap().0
    }

    /// A transaction spending the key image number `image`, its reference block this branch's tip, paying the minimum
    /// plus `fee_delta`, with `proof_len` bytes of (unchecked) proof. Different calls give different outputs, so the same
    /// `image` twice gives two conflicting transactions with different ids.
    fn tx(&mut self, image: u64, fee_delta: i64, proof_len: usize) -> Transaction {
        let next = self.validator().next_block().unwrap();
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
                inputs: vec![Input {
                    key_image: hash_to_point(sha256(&[b"key image", &image.to_le_bytes()])),
                }],
                outputs,
                ephemeral_pubkeys: vec![self.rng.point()],
                fee: 0,
                encrypted_payment_id: [0; 8],
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

    fn std_tx(&mut self, image: u64, fee_delta: i64) -> Transaction {
        self.tx(image, fee_delta, 200)
    }

    /// A transaction whose proof is "made" against this branch's tip tree, for [`TreeBound`].
    fn bound_tx(&mut self, image: u64) -> Transaction {
        let mut t = self.std_tx(image, 0);
        let root = self.store.tree_state(self.height()).unwrap().unwrap().root;
        t.prunable.proof_data[..32].copy_from_slice(&root);
        t
    }

    fn build(&mut self, txs: Vec<Transaction>) -> Block {
        let v = self.validator();
        let next = v.next_block().unwrap();
        let weight: u64 = txs.iter().map(|t| rules::tx_weight(t).unwrap()).sum();
        let fees_total: u64 = txs.iter().map(|t| t.prefix.fee).sum();
        let amount = v.coinbase_amount(&next, weight, fees_total).unwrap();
        let after_tip = if self.height() == 0 {
            T0
        } else {
            self.store.tip().unwrap().1.header.timestamp + 60
        };
        let timestamp = after_tip.max(u64::try_from(next.min_timestamp).unwrap_or(0));
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

    /// Builds a block and adds it to this branch.
    fn extend(&mut self, txs: Vec<Transaction>) -> Block {
        let b = self.build(txs);
        self.follow(&b);
        b
    }

    fn follow(&self, b: &Block) {
        self.validator()
            .accept_block(b, NOW)
            .unwrap_or_else(|e| panic!("the net could not follow a block: {e:?}"));
    }
}

/// The node under test: its store and rules live here so the `Node` can borrow them.
struct Rig {
    _db: TempDb,
    store: Store,
    params: ChainParams,
}

impl Rig {
    fn new(name: &str) -> Rig {
        let db = TempDb::new(name);
        let store = Store::open(&db.0, LABEL, PowKind::Sha256).unwrap();
        Rig {
            _db: db,
            store,
            params: params(),
        }
    }

    fn node(&self, max_bytes: u64) -> Node<'_> {
        self.node_with(max_bytes, &ProofsNotChecked)
    }

    fn node_with<'a>(&'a self, max_bytes: u64, proofs: &'a dyn ProofCheck) -> Node<'a> {
        Node::with_proof_check(
            &self.store,
            &self.params,
            &Sha256Pow,
            proofs,
            NodeConfig {
                pool: MempoolConfig { max_bytes },
                allow_unchecked_proofs_for_tests: true,
            },
        )
        .unwrap()
    }

    fn validator(&self) -> Validator<'_> {
        Validator::new(&self.store, &self.params, &Sha256Pow, &ProofsNotChecked)
    }
}

const BIG: u64 = 1 << 30;

/// `n` coinbase-only blocks on the net, fed to the node.
fn grow(net: &mut Net, node: &mut Node<'_>, n: usize) -> Vec<Block> {
    (0..n)
        .map(|_| {
            let b = net.extend(vec![]);
            assert!(matches!(
                node.submit_block(&b, NOW).unwrap(),
                Submitted::Extended(_)
            ));
            b
        })
        .collect()
}

fn id_of(t: &Transaction) -> [u8; 32] {
    tx_id(t).unwrap()
}

fn assert_sound(rig: &Rig, node: &Node<'_>) {
    let v = rig.validator();
    let mut images = HashSet::new();
    let mut bytes = 0;
    for id in node.pool().ids() {
        let t = node.pool().get(&id).unwrap();
        let info = v
            .check_pool_tx(t)
            .unwrap_or_else(|e| panic!("a pooled transaction is not valid at the tip: {e:?}"));
        bytes += info.size;
        for i in &t.prefix.inputs {
            assert!(
                images.insert(i.key_image),
                "two pooled transactions share a key image"
            );
        }
        assert_eq!(id_of(t), id);
    }
    assert_eq!(bytes, node.pool().total_bytes());
    // the key-image index has exactly one entry per pooled input, each pointing at its transaction
    assert_eq!(node.pool().tracked_key_images(), images.len());
    for id in node.pool().ids() {
        for i in &node.pool().get(&id).unwrap().prefix.inputs {
            assert_eq!(node.pool().spender_of(&i.key_image), Some(id));
        }
    }
}

// ------------------------------------------------------------------------------------------------

#[test]
fn a_valid_transaction_is_added_and_the_same_one_again_is_known() {
    let rig = Rig::new("basic");
    let mut net = Net::new("basic-net", 1);
    let mut node = rig.node(BIG);
    grow(&mut net, &mut node, SPENDABLE);
    let t = net.std_tx(1, 0);
    let id = id_of(&t);
    assert_eq!(
        node.submit_tx(t.clone()),
        Ok(AddOutcome::Added {
            id,
            evicted: vec![]
        })
    );
    assert!(node.pool().contains(&id));
    assert_eq!(node.submit_tx(t), Err(PoolError::AlreadyKnown));
    assert_sound(&rig, &node);
}

#[test]
fn a_transaction_breaking_a_rule_is_refused_with_that_rule() {
    let rig = Rig::new("rules");
    let mut net = Net::new("rules-net", 2);
    let mut node = rig.node(BIG);
    grow(&mut net, &mut node, SPENDABLE);
    // a fee one unit under the minimum
    let low = net.std_tx(1, -1);
    assert!(matches!(
        node.submit_tx(low),
        Err(PoolError::Invalid(BlockError::FeeTooLow { .. }))
    ));
    // outputs out of order
    let mut unordered = net.std_tx(2, 0);
    unordered.prefix.outputs.reverse();
    assert!(matches!(
        node.submit_tx(unordered),
        Err(PoolError::Invalid(BlockError::TxShape {
            error: ShapeError::OutputsNotAscending,
            ..
        }))
    ));
    // a reference block the chain does not have yet: not judged (a node behind the sender sees every new transaction
    // so), and not called invalid
    let mut ahead = net.std_tx(3, 0);
    ahead.prunable.reference_height += 1;
    let r = ahead.prunable.reference_height;
    assert_eq!(
        node.submit_tx(ahead),
        Err(PoolError::ReferenceAhead {
            reference_height: r
        })
    );
    // a key image the chain has already spent
    let spent = net.std_tx(4, 0);
    let b = net.extend(vec![spent]);
    node.submit_block(&b, NOW).unwrap();
    let again = net.std_tx(4, 0);
    assert!(matches!(
        node.submit_tx(again),
        Err(PoolError::Invalid(BlockError::KeyImageSpent { .. }))
    ));
    assert!(node.pool().is_empty());
}

#[test]
fn a_second_spend_of_the_same_key_image_conflicts() {
    let rig = Rig::new("conflict");
    let mut net = Net::new("conflict-net", 3);
    let mut node = rig.node(BIG);
    grow(&mut net, &mut node, SPENDABLE);
    let first = net.std_tx(7, 0);
    let rival = net.std_tx(7, 50);
    assert_ne!(id_of(&first), id_of(&rival));
    node.submit_tx(first.clone()).unwrap();
    // even paying more: there is no replace-by-fee
    assert_eq!(
        node.submit_tx(rival),
        Err(PoolError::Conflict {
            key_image: first.prefix.inputs[0].key_image,
            with: id_of(&first)
        })
    );
    assert_eq!(node.pool().len(), 1);
}

#[test]
fn a_block_removes_what_it_confirms_and_what_conflicts_with_it() {
    let rig = Rig::new("connect");
    let mut net = Net::new("connect-net", 4);
    let mut node = rig.node(BIG);
    grow(&mut net, &mut node, SPENDABLE);
    let confirmed = net.std_tx(1, 0);
    let loser = net.std_tx(2, 0);
    let winner_of_2 = net.std_tx(2, 0); // conflicts with `loser`, and goes into the block
    let kept = net.std_tx(3, 0);
    node.submit_tx(confirmed.clone()).unwrap();
    node.submit_tx(loser.clone()).unwrap();
    node.submit_tx(kept.clone()).unwrap();
    let b = net.extend(vec![confirmed.clone(), winner_of_2]);
    node.submit_block(&b, NOW).unwrap();
    assert!(
        !node.pool().contains(&id_of(&confirmed)),
        "confirmed, so gone"
    );
    assert!(
        !node.pool().contains(&id_of(&loser)),
        "its key image is spent now"
    );
    assert!(node.pool().contains(&id_of(&kept)));
    assert_eq!(node.pool().len(), 1);
    assert_sound(&rig, &node);
}

#[test]
fn when_full_the_lowest_fee_rate_is_pushed_out_only_for_a_better_one() {
    let rig = Rig::new("evict");
    let mut net = Net::new("evict-net", 5);
    let mut node = rig.node(BIG);
    grow(&mut net, &mut node, SPENDABLE);
    let size = net.std_tx(0, 0).to_bytes().unwrap().len() as u64;
    // room for two and a half
    let mut node = rig.node(size * 5 / 2);
    let a = net.std_tx(1, 0);
    let b = net.std_tx(2, 10);
    node.submit_tx(a.clone()).unwrap();
    node.submit_tx(b.clone()).unwrap();
    // a better payer pushes out the worst one (a)
    let c = net.std_tx(3, 20);
    assert_eq!(
        node.submit_tx(c.clone()),
        Ok(AddOutcome::Added {
            id: id_of(&c),
            evicted: vec![id_of(&a)]
        })
    );
    assert!(!node.pool().contains(&id_of(&a)));
    // the evicted transaction's key image is free again: a better-paying rival of `a` is accepted
    let a_rival = net.std_tx(1, 30);
    assert!(matches!(
        node.submit_tx(a_rival),
        Ok(AddOutcome::Added { .. })
    ));
    // one that pays less than everything is refused, and so is one that pays only the same as the worst
    let d = net.std_tx(4, 5);
    assert_eq!(node.submit_tx(d), Err(PoolError::PoolFull));
    // (the worst one now pays +20)
    let e = net.std_tx(5, 20);
    assert_eq!(node.submit_tx(e), Err(PoolError::PoolFull));
    assert!(node.pool().total_bytes() <= size * 5 / 2);
    assert_eq!(node.pool().len(), 2);
    assert_sound(&rig, &node);
}

/// A transaction heavier than a block may weigh is refused (`PoolError::TooLarge`). With the median never below 150,000, a
/// block may always weigh 300,000, and no transaction can be over 75,000 bytes: the check cannot be met today, and stays
/// as a guard should either limit change.
#[test]
fn no_transaction_can_be_heavier_than_the_smallest_block_limit() {
    assert!((MAX_TX_SIZE as u64) < rules::block_limit(rules::MIN_BLOCK_MEDIAN));
}

#[test]
fn a_block_template_never_holds_more_than_a_block_may_carry_whatever_it_is_asked_for() {
    // at the 150,000 floor a block may weigh 300,000; the pool holds about 380,000
    let rig = Rig::new("limit");
    let mut net = Net::new("limit-net", 8);
    let mut node = rig.node(BIG);
    grow(&mut net, &mut node, SPENDABLE);
    let limit = rules::block_limit(150_000);
    assert_eq!(limit, 300_000);
    for i in 0..25u64 {
        node.submit_tx(net.tx(i + 1, 5 + i as i64, BIG_PROOF))
            .unwrap();
    }
    let pooled: u64 = node
        .pool()
        .listing(usize::MAX)
        .iter()
        .map(|e| e.weight)
        .sum();
    assert!(pooled > limit, "the pool holds more than a block may");
    // asked for everything, it still gives no more than a valid block holds
    let weight =
        |txs: &[Transaction]| -> u64 { txs.iter().map(|t| rules::tx_weight(t).unwrap()).sum() };
    let got = node.block_template_txs(u64::MAX);
    assert!(
        !got.is_empty() && weight(&got) <= limit,
        "{} for a limit of {limit}",
        weight(&got)
    );
    // the same for the whole block the node builds for a miner (whose payout depends on the amount)
    let payout = |amount: u64| tenero_node::Payout {
        onetime_address: hash_to_point(sha256(&[b"payout", &amount.to_le_bytes()])),
        view_tag: [2; 3],
        ephemeral_pubkey: [3; 32],
        anchor_enc: [4; 16],
    };
    let template = node.block_template(NOW, u64::MAX, &payout).unwrap();
    assert!(
        !template.transactions.is_empty() && weight(&template.transactions) <= limit,
        "a template of {} for a limit of {limit}",
        weight(&template.transactions)
    );
    assert_eq!(
        template.coinbase.outputs[0].onetime_address,
        payout(template.coinbase.outputs[0].amount).onetime_address,
        "the payout was asked for with the amount it pays"
    );
    // and the block built from the first is accepted
    let b = net.extend(got);
    assert!(matches!(
        node.submit_block(&b, NOW).unwrap(),
        Submitted::Extended(_)
    ));
}

#[test]
fn a_block_template_takes_the_best_fee_rates_within_the_budget() {
    let rig = Rig::new("select");
    let mut net = Net::new("select-net", 7);
    let mut node = rig.node(BIG);
    grow(&mut net, &mut node, SPENDABLE);
    let size = rules::tx_weight(&net.std_tx(0, 0)).unwrap();
    let deltas = [5, 40, 20, 30];
    let txs: Vec<Transaction> = deltas
        .iter()
        .enumerate()
        .map(|(i, d)| net.std_tx(i as u64 + 1, *d))
        .collect();
    for t in &txs {
        node.submit_tx(t.clone()).unwrap();
    }
    let two = node.block_template_txs(size * 2);
    assert_eq!(two, vec![txs[1].clone(), txs[3].clone()], "40 then 30");
    assert!(node.block_template_txs(size - 1).is_empty());
    let all = node.block_template_txs(BIG);
    assert_eq!(
        all,
        vec![
            txs[1].clone(),
            txs[3].clone(),
            txs[2].clone(),
            txs[0].clone()
        ]
    );
    // and a block built from the template is valid
    let b = net.extend(all);
    assert!(matches!(
        node.submit_block(&b, NOW).unwrap(),
        Submitted::Extended(_)
    ));
    assert!(node.pool().is_empty());
}

#[test]
fn the_pool_listing_is_in_block_order_with_fee_size_and_arrival_time() {
    let rig = Rig::new("listing");
    let mut net = Net::new("listing-net", 7);
    let mut node = rig.node(BIG);
    grow(&mut net, &mut node, SPENDABLE);
    let deltas = [5, 40, 20];
    let txs: Vec<Transaction> = deltas
        .iter()
        .enumerate()
        .map(|(i, d)| net.std_tx(i as u64 + 1, *d))
        .collect();
    // the second arrives with no time, the others with one
    node.submit_tx_at(txs[0].clone(), T0 + 10).unwrap();
    node.submit_tx(txs[1].clone()).unwrap();
    node.submit_tx_at(txs[2].clone(), T0 + 30).unwrap();
    let list = node.pool().listing(100);
    let ids: Vec<[u8; 32]> = list.iter().map(|e| e.id).collect();
    assert_eq!(ids, vec![id_of(&txs[1]), id_of(&txs[2]), id_of(&txs[0])]);
    // the same order as a block template takes them
    let template: Vec<[u8; 32]> = node.block_template_txs(BIG).iter().map(id_of).collect();
    assert_eq!(ids, template);
    assert_eq!(
        list.iter().map(|e| e.received).collect::<Vec<_>>(),
        vec![0, T0 + 30, T0 + 10]
    );
    for (e, t) in list.iter().zip([&txs[1], &txs[2], &txs[0]]) {
        assert_eq!(e.fee, t.prefix.fee);
        assert_eq!(e.size, t.to_bytes().unwrap().len() as u64);
        assert_eq!(e.weight, rules::tx_weight(t).unwrap());
    }
    // a limit keeps the best
    assert_eq!(node.pool().listing(1)[0].id, id_of(&txs[1]));
    assert!(node.pool().listing(0).is_empty());
}

#[test]
fn a_node_refuses_to_run_without_real_proof_checking() {
    let rig = Rig::new("refuse");
    let r = Node::with_proof_check(
        &rig.store,
        &rig.params,
        &Sha256Pow,
        &ProofsNotChecked,
        NodeConfig::default(),
    );
    assert!(matches!(r, Err(NodeError::ProofsNotChecked)));
    // the default constructor uses the real check, so it is accepted
    assert!(Node::new(&rig.store, &rig.params, &Sha256Pow, NodeConfig::default()).is_ok());
}

// ---- reorganisations ---------------------------------------------------------------------------

/// Two nets sharing `common` blocks, and a node that has followed the shared part.
struct Fork {
    a: Net,
    b: Net,
}

fn fork(name: &str, common: usize, node: &mut Node<'_>) -> Fork {
    let mut a = Net::new(&format!("{name}-a"), 11);
    let shared = (0..common).map(|_| a.extend(vec![])).collect::<Vec<_>>();
    let b = Net::new(&format!("{name}-b"), 22);
    for blk in &shared {
        b.follow(blk);
        node.submit_block(blk, NOW).unwrap();
    }
    Fork { a, b }
}

#[test]
fn a_transaction_of_an_undone_block_goes_back_to_the_pool() {
    let rig = Rig::new("reorg1");
    let mut node = rig.node(BIG);
    let mut f = fork("reorg1", SPENDABLE, &mut node);
    let x = f.a.std_tx(1, 0);
    let a1 = f.a.extend(vec![x.clone()]);
    let a2 = f.a.extend(vec![]);
    node.submit_block(&a1, NOW).unwrap();
    node.submit_block(&a2, NOW).unwrap();
    assert!(node.pool().is_empty(), "x is confirmed on the chain");
    // the other branch, longer, does not have x
    let bs: Vec<Block> = (0..3).map(|_| f.b.extend(vec![])).collect();
    for blk in &bs[..2] {
        assert!(matches!(
            node.submit_block(blk, NOW).unwrap(),
            Submitted::SideChain { .. }
        ));
    }
    assert!(matches!(
        node.submit_block(&bs[2], NOW).unwrap(),
        Submitted::Reorganised { .. }
    ));
    assert!(node.pool().contains(&id_of(&x)), "x must be back");
    assert_sound(&rig, &node);
}

#[test]
fn a_transaction_confirmed_on_both_branches_is_not_put_back() {
    let rig = Rig::new("reorg2");
    let mut node = rig.node(BIG);
    let mut f = fork("reorg2", SPENDABLE, &mut node);
    let z = f.a.std_tx(1, 0);
    let a1 = f.a.extend(vec![z.clone()]);
    node.submit_block(&a1, NOW).unwrap();
    let b1 = f.b.extend(vec![z.clone()]);
    let b2 = f.b.extend(vec![]);
    node.submit_block(&b1, NOW).unwrap();
    node.submit_block(&b2, NOW).unwrap();
    assert_eq!(
        node.tip().unwrap().1,
        ids::block_id(&b2.header, PowKind::Sha256)
    );
    assert!(node.pool().is_empty());
}

#[test]
fn a_pooled_transaction_conflicting_with_the_new_branch_is_dropped() {
    let rig = Rig::new("reorg3");
    let mut node = rig.node(BIG);
    let mut f = fork("reorg3", SPENDABLE, &mut node);
    let a1 = f.a.extend(vec![]);
    node.submit_block(&a1, NOW).unwrap();
    let w = f.a.std_tx(5, 0);
    let w_rival = f.b.std_tx(5, 0);
    let bystander = f.a.std_tx(6, 0);
    node.submit_tx(w.clone()).unwrap();
    node.submit_tx(bystander.clone()).unwrap();
    let b1 = f.b.extend(vec![w_rival]);
    let b2 = f.b.extend(vec![]);
    node.submit_block(&b1, NOW).unwrap();
    node.submit_block(&b2, NOW).unwrap();
    assert!(
        !node.pool().contains(&id_of(&w)),
        "its key image is spent on the new chain"
    );
    assert!(node.pool().contains(&id_of(&bystander)));
    assert_sound(&rig, &node);
}

// ---- a model-style test ------------------------------------------------------------------------

#[test]
fn random_operations_keep_the_pool_sound_and_complete() {
    let rig = Rig::new("model");
    let mut node = rig.node(BIG);
    let mut f = fork("model", SPENDABLE, &mut node);
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);

    // the universe: transactions valid on every branch (they use only the shared outputs); some share a key
    // image, so some conflict
    let universe: Vec<Transaction> = (0..24u64)
        .map(|i| f.a.std_tx(i % 16, (i as i64 % 7) * 3))
        .collect();
    let mut offered: HashSet<[u8; 32]> = HashSet::new();

    // blocks built on each branch, and how many of each have been shown to the node
    let mut a_blocks: Vec<Block> = Vec::new();
    let mut b_blocks: Vec<Block> = Vec::new();
    let (mut a_sent, mut b_sent) = (0usize, 0usize);

    for step in 0..400 {
        match rng.below(6) {
            0 | 1 => {
                let t = universe[rng.below(universe.len() as u64) as usize].clone();
                offered.insert(id_of(&t));
                let _ = node.submit_tx(t);
            }
            2 | 3 => {
                // extend one branch with some of the universe that is valid on that branch
                let net = if rng.below(2) == 0 {
                    &mut f.a
                } else {
                    &mut f.b
                };
                let mut chosen: Vec<Transaction> = Vec::new();
                let mut used = HashSet::new();
                for _ in 0..rng.below(3) {
                    let t = &universe[rng.below(universe.len() as u64) as usize];
                    let img = t.prefix.inputs[0].key_image;
                    if !used.contains(&img) && net.validator().check_pool_tx(t).is_ok() {
                        used.insert(img);
                        chosen.push(t.clone());
                    }
                }
                let blk = net.extend(chosen);
                if std::ptr::eq(net, &f.a) {
                    a_blocks.push(blk);
                } else {
                    b_blocks.push(blk);
                }
            }
            _ => {
                // show the node the next block of one branch
                let (blocks, sent) = if rng.below(2) == 0 {
                    (&a_blocks, &mut a_sent)
                } else {
                    (&b_blocks, &mut b_sent)
                };
                if *sent < blocks.len() {
                    let r = node.submit_block(&blocks[*sent], NOW);
                    assert!(r.is_ok(), "step {step}: {r:?}");
                    *sent += 1;
                }
            }
        }

        // sound: everything in the pool is valid at the tip, non-conflicting, unconfirmed, and within size
        assert_sound(&rig, &node);
        // complete: anything offered that is valid at the tip and not in the pool conflicts with something in it
        let v = rig.validator();
        for t in &universe {
            let id = id_of(t);
            if offered.contains(&id) && !node.pool().contains(&id) && v.check_pool_tx(t).is_ok() {
                let img = &t.prefix.inputs[0].key_image;
                assert!(
                    node.pool().spender_of(img).is_some(),
                    "step {step}: a valid, offered transaction is missing and nothing conflicts with it"
                );
            }
        }
    }
    // the run must have exercised reorganisations for this to mean anything
    assert!(
        a_sent > 5 && b_sent > 5,
        "sent {a_sent} and {b_sent} blocks"
    );
}

/// The curve tree holds only outputs at least 10 blocks old (60 for a coinbase), so a reorganisation shallower than that
/// leaves every reference block's tree as it was: a pooled transaction stays valid, its proof included.
#[test]
fn a_pooled_transaction_survives_a_reorganisation_shallower_than_10_blocks() {
    let rig = Rig::new("shallow");
    let mut node = rig.node_with(BIG, &TreeBound);
    let mut f = fork("shallow", SPENDABLE, &mut node);
    let txs = vec![f.a.bound_tx(1), f.a.bound_tx(2)];
    let a1 = f.a.extend(txs);
    node.submit_block(&a1, NOW).unwrap();
    let pooled = f.a.bound_tx(9); // made against A's tip tree, which A's block's outputs are not in yet
    node.submit_tx(pooled.clone()).unwrap();
    let bs: Vec<Block> = (0..2).map(|_| f.b.extend(vec![])).collect();
    for b in &bs {
        node.submit_block(b, NOW).unwrap();
    }
    assert_eq!(
        node.tip().unwrap().1,
        ids::block_id(&bs[1].header, PowKind::Sha256)
    );
    assert!(node.pool().contains(&id_of(&pooled)));
}

/// A deeper reorganisation can change a reference block's tree (here the new branch lacks a block's outputs that had
/// entered it): a pooled transaction made against the old tree no longer verifies, and is dropped.
#[test]
fn a_pooled_transaction_made_against_a_tree_the_new_branch_does_not_have_is_dropped() {
    let rig = Rig::new("deep");
    let mut node = rig.node_with(BIG, &TreeBound);
    let mut f = fork("deep", SPENDABLE, &mut node);
    // A: a block with transactions, then 11 more (its outputs enter A's tree 10 blocks later)
    let txs = vec![f.a.bound_tx(1), f.a.bound_tx(2)];
    let mut a_blocks = vec![f.a.extend(txs)];
    a_blocks.extend((0..11).map(|_| f.a.extend(vec![])));
    for b in &a_blocks {
        node.submit_block(b, NOW).unwrap();
    }
    let pooled = f.a.bound_tx(9);
    node.submit_tx(pooled.clone()).unwrap();
    // B: 13 empty blocks, more work
    let bs: Vec<Block> = (0..13).map(|_| f.b.extend(vec![])).collect();
    for b in &bs {
        node.submit_block(b, NOW).unwrap();
    }
    assert_eq!(
        node.tip().unwrap().1,
        ids::block_id(&bs[12].header, PowKind::Sha256)
    );
    assert!(
        !node.pool().contains(&id_of(&pooled)),
        "its reference block's tree is different on the new chain"
    );
}

#[test]
fn a_rising_minimum_fee_drops_what_no_longer_pays_it() {
    // Heavy blocks raise the median (so the minimum fee falls); letting it fall back raises the fee again: a
    // transaction that paid the lower minimum must then be dropped.
    let rig = Rig::new("minfee");
    let mut net = Net::new("minfee-net", 8);
    let mut node = rig.node(BIG);
    grow(&mut net, &mut node, SPENDABLE);
    let mut image = 100;
    let mut heavy = |net: &mut Net| -> Vec<Transaction> {
        (0..10)
            .map(|_| {
                image += 1;
                net.tx(image, 0, BIG_PROOF)
            })
            .collect()
    };
    let floor_fee = net.std_tx(1, 0).prefix.fee;
    for _ in 0..6 {
        let txs = heavy(&mut net);
        let b = net.extend(txs);
        node.submit_block(&b, NOW).unwrap();
    }
    assert!(net.validator().next_block().unwrap().median > 150_000);
    let cheap = net.std_tx(2, 0);
    assert!(
        cheap.prefix.fee < floor_fee,
        "precondition: a bigger median must lower the minimum fee ({} vs {floor_fee})",
        cheap.prefix.fee
    );
    node.submit_tx(cheap.clone()).unwrap();
    assert_sound(&rig, &node);
    // let the median fall back with empty blocks; at some point the minimum rises above what `cheap` pays
    let mut dropped = false;
    for _ in 0..12 {
        let b = net.extend(vec![]);
        node.submit_block(&b, NOW).unwrap();
        assert_sound(&rig, &node);
        if !node.pool().contains(&id_of(&cheap)) {
            dropped = true;
            break;
        }
    }
    assert!(
        dropped,
        "the transaction should have been dropped when the minimum fee rose"
    );
}

// ---- saving and loading the block pools --------------------------------------------------------------

fn pool_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "tenero-node-pool-{}-{name}.bin",
        std::process::id()
    ))
}

fn remove_pool_files(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let _ = std::fs::remove_file(PathBuf::from(tmp));
}

/// A node on a four-block chain with two side blocks of a branch that has less work, and an orphan of that branch:
/// `(rig, the side and orphan blocks)`. The node is dropped; the store stays in the rig, as after a restart.
fn rig_with_pools(name: &str) -> (Rig, Vec<Block>) {
    let rig = Rig::new(name);
    let mut main = Net::new(&format!("{name}-main"), 5);
    let mut node = rig.node(BIG);
    let common = grow(&mut main, &mut node, 3);
    let mut side = Net::new(&format!("{name}-side"), 6);
    for b in &common {
        side.follow(b);
    }
    grow(&mut main, &mut node, 3);
    let side_blocks: Vec<Block> = (0..4).map(|_| side.extend(vec![])).collect();
    for b in [&side_blocks[0], &side_blocks[1]] {
        assert!(matches!(
            node.submit_block(b, NOW).unwrap(),
            Submitted::SideChain { .. }
        ));
    }
    assert!(matches!(
        node.submit_block(&side_blocks[3], NOW).unwrap(),
        Submitted::Orphan
    ));
    let path = pool_path(name);
    remove_pool_files(&path);
    node.save_pool(&path).unwrap();
    (rig, side_blocks)
}

#[test]
fn a_restarted_node_gets_its_side_blocks_and_orphans_back_and_they_still_work() {
    let (rig, side) = rig_with_pools("pool-restart");
    let path = pool_path("pool-restart");
    let mut node = rig.node(BIG);
    assert_eq!(
        node.chain().side_block_count(),
        0,
        "a restart empties the pools"
    );
    let report = node.load_pool(&path, NOW).unwrap();
    assert_eq!(
        report,
        PoolLoad {
            side: 2,
            orphans: 1,
            ..PoolLoad::default()
        }
    );
    assert!(node
        .chain()
        .in_side_pool(&ids::block_id(&side[0].header, PowKind::Sha256)));
    assert!(node
        .chain()
        .is_orphan(&ids::block_id(&side[3].header, PowKind::Sha256)));
    // the missing block arrives: the restored orphan can be connected to it
    assert!(matches!(
        node.submit_block(&side[2], NOW).unwrap(),
        Submitted::SideChain { .. } | Submitted::Reorganised { .. }
    ));
    let waiting = node.take_orphans_of(&ids::block_id(&side[2].header, PowKind::Sha256));
    assert_eq!(waiting, vec![side[3].clone()]);
    // loading the same file again finds it all known
    let mut again = rig.node(BIG);
    again.load_pool(&path, NOW).unwrap();
    let report = again.load_pool(&path, NOW).unwrap();
    assert_eq!(
        report,
        PoolLoad {
            known: 3,
            ..PoolLoad::default()
        }
    );
    remove_pool_files(&path);
}

#[test]
fn saving_replaces_the_file_whole_and_leaves_nothing_else_behind() {
    let (rig, _side) = rig_with_pools("pool-atomic");
    let path = pool_path("pool-atomic");
    let first = std::fs::read(&path).unwrap();
    // a second save of an empty pool replaces it
    let node = rig.node(BIG);
    node.save_pool(&path).unwrap();
    let second = std::fs::read(&path).unwrap();
    assert_ne!(first, second);
    assert_eq!(second.len(), 12, "an empty pool: magic, count, checksum");
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    assert!(
        !PathBuf::from(tmp).exists(),
        "the temporary file was renamed away"
    );
    remove_pool_files(&path);
}

#[test]
fn a_missing_pool_file_is_an_empty_pool_and_a_damaged_one_changes_nothing() {
    let rig = Rig::new("pool-missing");
    let mut node = rig.node(BIG);
    let path = pool_path("pool-missing");
    remove_pool_files(&path);
    assert_eq!(node.load_pool(&path, NOW).unwrap(), PoolLoad::default());
    std::fs::write(&path, b"this is not a pool file").unwrap();
    assert!(node.load_pool(&path, NOW).is_err());
    assert_eq!(node.chain().side_block_count(), 0);
    assert_eq!(node.chain().orphan_count(), 0);
    remove_pool_files(&path);
}

#[test]
fn a_block_in_a_pool_file_that_is_no_longer_acceptable_is_dropped_not_trusted() {
    let rig = Rig::new("pool-forged");
    let mut main = Net::new("pool-forged-main", 5);
    let mut node = rig.node(BIG);
    let common = grow(&mut main, &mut node, 3);
    let mut side = Net::new("pool-forged-side", 6);
    for b in &common {
        side.follow(b);
    }
    // the chain is one block ahead, so the honest side block below ties it and is kept aside
    grow(&mut main, &mut node, 1);
    let good = side.extend(vec![]);
    // a forged side block: the right parent, but a proof of work that does not meet the target
    let target = node.next_block().unwrap().target;
    let mut forged = good.clone();
    forged.header.timestamp += 1;
    while U256::from_be_bytes(&ids::block_id(&forged.header, PowKind::Sha256)) < target {
        forged.header.nonce += 1;
    }
    // written by hand, with a correct checksum: the checksum proves nothing about the blocks
    let mut body = b"TPL1".to_vec();
    body.extend_from_slice(&2u32.to_le_bytes());
    for b in [&forged, &good] {
        let bytes = b.to_bytes().unwrap();
        body.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        body.extend_from_slice(&bytes);
    }
    let sum = sha256(&[&body]);
    body.extend_from_slice(&sum[..4]);
    let path = pool_path("pool-forged");
    std::fs::write(&path, body).unwrap();
    let mut fresh = rig.node(BIG);
    let report = fresh.load_pool(&path, NOW).unwrap();
    assert_eq!(
        report,
        PoolLoad {
            side: 1,
            dropped: 1,
            ..PoolLoad::default()
        }
    );
    assert_eq!(fresh.chain().side_block_count(), 1);
    remove_pool_files(&path);
}

#[test]
fn a_saved_branch_that_now_has_more_work_is_adopted_on_load_and_the_mempool_follows() {
    // the pool file holds a side branch; by the time it is loaded the node has not moved, but the branch is longer
    // than the chain (it was saved mid-way through being received): loading it reorganises
    let rig = Rig::new("pool-adopt");
    let mut main = Net::new("pool-adopt-main", 5);
    let mut node = rig.node(BIG);
    let common = grow(&mut main, &mut node, SPENDABLE);
    let mut side = Net::new("pool-adopt-side", 6);
    for b in &common {
        side.follow(b);
    }
    grow(&mut main, &mut node, 1);
    // a transaction that the old chain's block 4 does not contain, pooled
    let t = main.std_tx(1, 0);
    assert!(node.submit_tx(t.clone()).is_ok());
    assert_eq!(node.pool().len(), 1);
    // the branch confirms a transaction that spends the same key image as the pooled one, so once the chain moves
    // to the branch the pooled transaction can no longer be valid
    let conflicting = side.std_tx(1, 0);
    let branch: Vec<Block> = vec![
        side.extend(vec![conflicting]),
        side.extend(vec![]),
        side.extend(vec![]),
    ];
    // a file written by hand from the branch
    let mut body = b"TPL1".to_vec();
    body.extend_from_slice(&3u32.to_le_bytes());
    for b in &branch {
        let bytes = b.to_bytes().unwrap();
        body.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        body.extend_from_slice(&bytes);
    }
    let sum = sha256(&[&body]);
    body.extend_from_slice(&sum[..4]);
    let path = pool_path("pool-adopt");
    std::fs::write(&path, body).unwrap();
    let report = node.load_pool(&path, NOW).unwrap();
    // the first block ties the chain (kept aside); the second makes the branch heavier, so the chain moves to it;
    // the third then simply extends it
    assert_eq!(
        report,
        PoolLoad {
            side: 1,
            adopted: 2,
            ..PoolLoad::default()
        }
    );
    assert_eq!(node.tip().unwrap().0, SPENDABLE as u64 + 3);
    assert_eq!(
        node.pool().len(),
        0,
        "the pooled transaction conflicts with one the new chain confirms: loading must update the mempool too"
    );
    remove_pool_files(&path);
}
