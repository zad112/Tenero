//! The mempool and the node core. Blocks are really mined (SHA-256 test chain) and really accepted; every
//! rule is tested by breaking exactly it. A model-style test drives random operations, including
//! reorganisations, and checks after EVERY step that the pool is sound (everything in it is valid at the tip,
//! non-conflicting and unconfirmed) and complete (anything valid, offered and missing conflicts with something
//! in it).

use std::collections::HashSet;
use std::path::PathBuf;

use tenero_chain::{BlockError, ChainParams, ProofsNotChecked, Sha256Pow, Submitted, Validator};
use tenero_core::fees;
use tenero_core::hash::sha256;
use tenero_core::u256::U256;
use tenero_core::v2::ids::{self, tx_id, PowKind};
use tenero_core::v2::*;
use tenero_node::{AddOutcome, MempoolConfig, Node, NodeConfig, NodeError, PoolError};
use tenero_store::Store;

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
}

fn params(min_median: Option<u64>) -> ChainParams {
    let mut p = ChainParams::version_2(LABEL, PowKind::Sha256, U256::pow2(254).unwrap());
    p.ring_size = 2;
    p.coinbase_maturity = 1;
    p.spend_maturity = 1;
    if let Some(m) = min_median {
        p.min_block_median = m;
    }
    p
}

/// A store that follows one branch and builds blocks and transactions on it.
struct Net {
    _db: TempDb,
    store: Store,
    params: ChainParams,
    rng: Rng,
}

impl Net {
    fn new(name: &str, seed: u64, min_median: Option<u64>) -> Net {
        let db = TempDb::new(name);
        let store = Store::open(&db.0, LABEL, PowKind::Sha256).unwrap();
        Net {
            _db: db,
            store,
            params: params(min_median),
            rng: Rng(seed),
        }
    }

    fn validator(&self) -> Validator<'_> {
        Validator::new(&self.store, &self.params, &Sha256Pow, &ProofsNotChecked)
    }

    fn height(&self) -> u64 {
        self.store.tip().unwrap().0
    }

    /// A transaction spending ring `ring` with the key image number `image`, paying the dynamic minimum plus
    /// `fee_delta`, with `proof_len` bytes of (unchecked) proof. Different calls give different outputs, so the
    /// same `image` twice gives two conflicting transactions with different ids.
    fn tx(&mut self, image: u64, fee_delta: i64, ring: &[u64], proof_len: usize) -> Transaction {
        let next = self.validator().next_block().unwrap();
        let mut t = Transaction {
            prefix: TxPrefix {
                version: VERSION,
                inputs: vec![Input {
                    key_image: sha256(&[b"key image", &image.to_le_bytes()]),
                }],
                outputs: (0..2)
                    .map(|_| Output {
                        onetime_address: self.rng.bytes(),
                        amount_commitment: self.rng.bytes(),
                        amount_enc: self.rng.bytes(),
                        view_tag: self.rng.bytes(),
                        ephemeral_pubkey: self.rng.bytes(),
                        anchor_enc: self.rng.bytes(),
                    })
                    .collect(),
                fee: 0,
                extra: vec![],
            },
            prunable: Prunable {
                rings: vec![ring.to_vec()],
                proof_data: vec![7; proof_len],
            },
        };
        let size = t.to_bytes().unwrap().len() as u64;
        let min = fees::dynamic_min_fee(size, next.reward, next.median).unwrap();
        t.prefix.fee = u64::try_from(i64::try_from(min).unwrap() + fee_delta).unwrap();
        t
    }

    fn std_tx(&mut self, image: u64, fee_delta: i64) -> Transaction {
        self.tx(image, fee_delta, &[0, 1], 200)
    }

    fn build(&mut self, txs: Vec<Transaction>) -> Block {
        let v = self.validator();
        let next = v.next_block().unwrap();
        let body: u64 = txs.iter().map(|t| t.to_bytes().unwrap().len() as u64).sum();
        let fees_total: u64 = txs.iter().map(|t| t.prefix.fee).sum();
        let amount = v.coinbase_amount(&next, body, fees_total).unwrap();
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
                onetime_address: self.rng.bytes(),
                amount,
                view_tag: self.rng.bytes(),
                ephemeral_pubkey: self.rng.bytes(),
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
    fn new(name: &str, min_median: Option<u64>) -> Rig {
        let db = TempDb::new(name);
        let store = Store::open(&db.0, LABEL, PowKind::Sha256).unwrap();
        Rig {
            _db: db,
            store,
            params: params(min_median),
        }
    }

    fn node(&self, max_bytes: u64) -> Node<'_> {
        Node::with_proof_check(
            &self.store,
            &self.params,
            &Sha256Pow,
            &ProofsNotChecked,
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
    let rig = Rig::new("basic", None);
    let mut net = Net::new("basic-net", 1, None);
    let mut node = rig.node(BIG);
    grow(&mut net, &mut node, 4);
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
    let rig = Rig::new("rules", None);
    let mut net = Net::new("rules-net", 2, None);
    let mut node = rig.node(BIG);
    grow(&mut net, &mut node, 4);
    // a fee one unit under the minimum
    let low = net.std_tx(1, -1);
    assert!(matches!(
        node.submit_tx(low),
        Err(PoolError::Invalid(BlockError::FeeTooLow { .. }))
    ));
    // a ring member that does not exist
    let missing = net.tx(2, 0, &[0, 999], 200);
    assert!(matches!(
        node.submit_tx(missing),
        Err(PoolError::Invalid(BlockError::RingMemberMissing { .. }))
    ));
    // a ring that is not in ascending order
    let unordered = net.tx(3, 0, &[1, 0], 200);
    assert!(matches!(
        node.submit_tx(unordered),
        Err(PoolError::Invalid(BlockError::RingNotAscending { .. }))
    ));
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
    let rig = Rig::new("conflict", None);
    let mut net = Net::new("conflict-net", 3, None);
    let mut node = rig.node(BIG);
    grow(&mut net, &mut node, 4);
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
    let rig = Rig::new("connect", None);
    let mut net = Net::new("connect-net", 4, None);
    let mut node = rig.node(BIG);
    grow(&mut net, &mut node, 4);
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
    let rig = Rig::new("evict", None);
    let mut net = Net::new("evict-net", 5, None);
    let mut node = rig.node(BIG);
    grow(&mut net, &mut node, 4);
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

#[test]
fn a_transaction_no_block_could_hold_is_refused() {
    // with a tiny block-size median, a transaction bigger than twice it can never be mined
    let rig = Rig::new("large", Some(600));
    let mut net = Net::new("large-net", 6, Some(600));
    let mut node = rig.node(BIG);
    grow(&mut net, &mut node, 4);
    let big = net.tx(1, 1_000_000, &[0, 1], 3000);
    assert!(matches!(
        node.submit_tx(big),
        Err(PoolError::TooLarge { .. })
    ));
}

#[test]
fn a_block_template_takes_the_best_fee_rates_within_the_budget() {
    let rig = Rig::new("select", None);
    let mut net = Net::new("select-net", 7, None);
    let mut node = rig.node(BIG);
    grow(&mut net, &mut node, 4);
    let size = net.std_tx(0, 0).to_bytes().unwrap().len() as u64;
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
fn a_node_refuses_to_run_without_real_proof_checking() {
    let rig = Rig::new("refuse", None);
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
    let mut a = Net::new(&format!("{name}-a"), 11, None);
    let shared = (0..common).map(|_| a.extend(vec![])).collect::<Vec<_>>();
    let b = Net::new(&format!("{name}-b"), 22, None);
    for blk in &shared {
        b.follow(blk);
        node.submit_block(blk, NOW).unwrap();
    }
    Fork { a, b }
}

#[test]
fn a_transaction_of_an_undone_block_goes_back_to_the_pool() {
    let rig = Rig::new("reorg1", None);
    let mut node = rig.node(BIG);
    let mut f = fork("reorg1", 4, &mut node);
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
    let rig = Rig::new("reorg2", None);
    let mut node = rig.node(BIG);
    let mut f = fork("reorg2", 4, &mut node);
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
    let rig = Rig::new("reorg3", None);
    let mut node = rig.node(BIG);
    let mut f = fork("reorg3", 4, &mut node);
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
    let rig = Rig::new("model", None);
    let mut node = rig.node(BIG);
    let mut f = fork("model", 4, &mut node);
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

#[test]
fn a_pooled_transaction_whose_ring_member_vanishes_in_a_reorganisation_is_dropped() {
    // On branch A, blocks 5 and 6 carry transactions, so A has more outputs; a pooled transaction uses one
    // of the outputs only A has. Branch B has more work but fewer outputs, so after the switch that ring
    // member does not exist and the transaction must go.
    let rig = Rig::new("vanish", None);
    let mut node = rig.node(BIG);
    let mut f = fork("vanish", 4, &mut node);
    let txs5 = vec![f.a.std_tx(1, 0), f.a.std_tx(2, 0)];
    let a5 = f.a.extend(txs5);
    let txs6 = vec![f.a.std_tx(3, 0), f.a.std_tx(4, 0)];
    let a6 = f.a.extend(txs6);
    node.submit_block(&a5, NOW).unwrap();
    node.submit_block(&a6, NOW).unwrap();
    let count = rig.store.output_count().unwrap();
    assert_eq!(count, 14);
    let v = f.a.tx(9, 0, &[0, 10], 200);
    node.submit_tx(v.clone()).unwrap();
    let bs: Vec<Block> = (0..3).map(|_| f.b.extend(vec![])).collect();
    for b in &bs {
        node.submit_block(b, NOW).unwrap();
    }
    assert_eq!(rig.store.output_count().unwrap(), 7);
    assert!(
        !node.pool().contains(&id_of(&v)),
        "ring member 10 no longer exists"
    );
    assert_sound(&rig, &node);
}

#[test]
fn a_rising_minimum_fee_drops_what_no_longer_pays_it() {
    // With a tiny block-size floor, filling blocks raises the median (so the minimum fee falls) and letting
    // it fall back raises the fee again: a transaction that paid the lower minimum must then be dropped.
    let rig = Rig::new("minfee", Some(600));
    let mut net = Net::new("minfee-net", 8, Some(600));
    let mut node = rig.node(BIG);
    grow(&mut net, &mut node, 4);
    let size = net.tx(0, 0, &[0, 1], 200).to_bytes().unwrap().len() as u64;
    let per_block = if size * 2 <= 1200 { 2 } else { 1 };
    let mut image = 100;
    let mut filler = |net: &mut Net| {
        let txs: Vec<Transaction> = (0..per_block)
            .map(|_| {
                image += 1;
                net.tx(image, 0, &[0, 1], 200)
            })
            .collect();
        txs
    };
    let floor_fee = net.tx(1, 0, &[0, 1], 200).prefix.fee;
    // raise the median with full blocks
    for _ in 0..8 {
        let txs = filler(&mut net);
        let b = net.extend(txs);
        node.submit_block(&b, NOW).unwrap();
    }
    let raised_fee = net.tx(2, 0, &[0, 1], 200).prefix.fee;
    assert!(
        raised_fee < floor_fee,
        "precondition: a bigger median must lower the minimum fee ({raised_fee} vs {floor_fee})"
    );
    let cheap = net.tx(2, 0, &[0, 1], 200);
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
