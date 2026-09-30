//! Fork choice and reorganisation. Competing branches are built on separate stores that follow one branch
//! each; a third node is fed their blocks and must end up in exactly the state of the store that followed
//! the winning branch (compared by state digest and tip).

use std::path::PathBuf;

use tenero_chain::{
    BlockError, Chain, ChainParams, ProofCheck, ProofsNotChecked, Sha256Pow, Submitted, TxContext,
    Validator,
};
use tenero_core::fees;
use tenero_core::hash::sha256;
use tenero_core::u256::U256;
use tenero_core::v2::ids::{self, PowKind};
use tenero_core::v2::*;
use tenero_store::Store;

const LABEL: &str = "tenero chain test network";
const NOW: u64 = 4_000_000_000;
const T0: u64 = 1_700_000_000;

struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> TempDb {
        let p = std::env::temp_dir().join(format!(
            "tenero-fork-test-{}-{name}.redb",
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
}

/// Small rings and short maturity, so a few blocks are enough to build a spend.
fn params() -> ChainParams {
    let mut p = ChainParams::version_2(LABEL, PowKind::Sha256, U256::pow2(254).unwrap());
    p.ring_size = 2;
    p.coinbase_maturity = 1;
    p.spend_maturity = 1;
    p
}

/// A transaction the real rules accept, but whose proof bytes start with this are refused by
/// [`RejectMarked`]: a block that is valid to every rule except the proof check.
const MARK: u8 = 0xEE;

struct RejectMarked;

impl ProofCheck for RejectMarked {
    fn check_tx(&self, ctx: &TxContext<'_>) -> Result<(), String> {
        if ctx.tx.prunable.proof_data.first() == Some(&MARK) {
            Err("marked".into())
        } else {
            Ok(())
        }
    }
    fn checks_proofs(&self) -> bool {
        true
    }
}

/// A store that follows ONE branch and builds blocks on it.
struct Net {
    _db: TempDb,
    store: Store,
    params: ChainParams,
    rng: Rng,
    spacing: u64,
    next_key: u64,
}

impl Net {
    fn new(name: &str, seed: u64, spacing: u64) -> Net {
        let db = TempDb::new(name);
        let store = Store::open(&db.0, LABEL, PowKind::Sha256).unwrap();
        Net {
            _db: db,
            store,
            params: params(),
            rng: Rng(seed),
            spacing,
            next_key: seed << 32,
        }
    }

    fn validator(&self) -> Validator<'_> {
        Validator::new(&self.store, &self.params, &Sha256Pow, &ProofsNotChecked)
    }

    fn height(&self) -> u64 {
        self.store.tip().unwrap().0
    }

    fn digest(&self) -> [u8; 32] {
        self.store.state_digest().unwrap()
    }

    fn tip_id(&self) -> [u8; 32] {
        self.store.tip().unwrap().1.block_id
    }

    fn work(&self) -> U256 {
        U256::from_be_bytes(&self.store.tip().unwrap().1.cumulative_work)
    }

    fn spend_tx(&mut self, mark: bool) -> Transaction {
        let next = self.validator().next_block().unwrap();
        self.next_key += 1;
        let mut t = Transaction {
            prefix: TxPrefix {
                version: VERSION,
                inputs: vec![Input {
                    key_image: sha256(&[b"key image", &self.next_key.to_le_bytes()]),
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
                rings: vec![vec![0, 1]],
                proof_data: vec![if mark { MARK } else { 7 }; 200],
            },
        };
        let size = t.to_bytes().unwrap().len() as u64;
        t.prefix.fee = fees::dynamic_min_fee(size, next.reward, next.median).unwrap();
        t
    }

    /// A mined block on this branch's tip, not yet accepted by anyone.
    fn build(&mut self, txs: Vec<Transaction>) -> Block {
        let v = self.validator();
        let next = v.next_block().unwrap();
        let body: u64 = txs.iter().map(|t| t.to_bytes().unwrap().len() as u64).sum();
        let fees_total: u64 = txs.iter().map(|t| t.prefix.fee).sum();
        let amount = v.coinbase_amount(&next, body, fees_total).unwrap();
        let after_tip = if self.height() == 0 {
            T0
        } else {
            self.store.tip().unwrap().1.header.timestamp + self.spacing
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
        mine(&mut b, &next.target);
        b
    }

    /// Builds a block and adds it to this branch.
    fn extend(&mut self, txs: Vec<Transaction>) -> Block {
        let b = self.build(txs);
        self.follow(&b);
        b
    }

    /// Adds a block built elsewhere (it must extend this branch's tip).
    fn follow(&self, b: &Block) {
        self.validator()
            .accept_block(b, NOW)
            .unwrap_or_else(|e| panic!("the net could not follow a block: {e:?}"));
    }

    fn extend_n(&mut self, n: usize) -> Vec<Block> {
        (0..n).map(|_| self.extend(vec![])).collect()
    }
}

fn mine(b: &mut Block, target: &U256) {
    for nonce in 0.. {
        b.header.nonce = nonce;
        if U256::from_be_bytes(&ids::block_id(&b.header, PowKind::Sha256)) < *target {
            return;
        }
    }
}

/// The node under test: its own store, fed by `Chain::submit_block`.
struct Node {
    _db: TempDb,
    store: Store,
    params: ChainParams,
}

impl Node {
    fn new(name: &str) -> Node {
        let db = TempDb::new(name);
        let store = Store::open(&db.0, LABEL, PowKind::Sha256).unwrap();
        Node {
            _db: db,
            store,
            params: params(),
        }
    }
    fn digest(&self) -> [u8; 32] {
        self.store.state_digest().unwrap()
    }
    fn tip(&self) -> (u64, [u8; 32]) {
        let (h, i) = self.store.tip().unwrap();
        (h, i.block_id)
    }
}

fn submit_all(chain: &mut Chain<'_>, blocks: &[Block]) -> Vec<Submitted> {
    blocks
        .iter()
        .map(|b| chain.submit_block(b, NOW).expect("submit"))
        .collect()
}

/// Two branches from a common prefix: `(main net, side net, common blocks, main blocks, side blocks)`.
fn two_branches(
    tag: &str,
    common: usize,
    main_len: usize,
    side_len: usize,
    side_spacing: u64,
) -> (Net, Net, Vec<Block>, Vec<Block>, Vec<Block>) {
    let mut a = Net::new(&format!("{tag}-a"), 11, 60);
    let shared = a.extend_n(common);
    let mut b = Net::new(&format!("{tag}-b"), 22, side_spacing);
    for blk in &shared {
        b.follow(blk);
    }
    let a_blocks = a.extend_n(main_len);
    let b_blocks = b.extend_n(side_len);
    (a, b, shared, a_blocks, b_blocks)
}

// ------------------------------------------------------------------------------------------------

#[test]
fn blocks_that_extend_the_tip_are_added_like_before() {
    let (a, _b, shared, main, _side) = two_branches("extend", 4, 6, 0, 60);
    let node = Node::new("extend-node");
    let mut chain = Chain::new(&node.store, &node.params, &Sha256Pow, &ProofsNotChecked);
    let all: Vec<Block> = shared.iter().chain(&main).cloned().collect();
    for r in submit_all(&mut chain, &all) {
        assert!(matches!(r, Submitted::Extended(_)), "{r:?}");
    }
    assert_eq!(node.digest(), a.digest());
    assert_eq!(node.tip(), (10, a.tip_id()));
}

#[test]
fn a_shorter_side_branch_is_kept_and_not_adopted() {
    let (a, _b, shared, main, side) = two_branches("shorter", 4, 6, 3, 60);
    let node = Node::new("shorter-node");
    let mut chain = Chain::new(&node.store, &node.params, &Sha256Pow, &ProofsNotChecked);
    submit_all(&mut chain, &shared);
    submit_all(&mut chain, &main);
    let before = (node.digest(), node.tip());
    for (i, r) in submit_all(&mut chain, &side).into_iter().enumerate() {
        assert!(
            matches!(r, Submitted::SideChain { height, .. } if height == 5 + i as u64),
            "{r:?}"
        );
    }
    assert_eq!((node.digest(), node.tip()), before);
    assert_eq!(before.0, a.digest());
    assert_eq!(chain.side_block_count(), 3);
}

#[test]
fn equal_work_keeps_the_chain_that_was_seen_first() {
    let (a, _b, shared, main, side) = two_branches("tie", 4, 6, 6, 60);
    let node = Node::new("tie-node");
    let mut chain = Chain::new(&node.store, &node.params, &Sha256Pow, &ProofsNotChecked);
    submit_all(&mut chain, &shared);
    submit_all(&mut chain, &main);
    for r in submit_all(&mut chain, &side) {
        assert!(matches!(r, Submitted::SideChain { .. }), "{r:?}");
    }
    assert_eq!(node.tip(), (10, a.tip_id()), "a tie must not switch");
    assert_eq!(node.digest(), a.digest());
}

#[test]
fn a_branch_with_more_work_replaces_the_chain_and_the_old_chain_can_win_back() {
    let (mut a, b, shared, main, side_first) = two_branches("longer", 4, 6, 7, 60);
    let node = Node::new("longer-node");
    let mut chain = Chain::new(&node.store, &node.params, &Sha256Pow, &ProofsNotChecked);
    submit_all(&mut chain, &shared);
    submit_all(&mut chain, &main);
    let results = submit_all(&mut chain, &side_first);
    for r in &results[..6] {
        assert!(matches!(r, Submitted::SideChain { .. }), "{r:?}");
    }
    assert_eq!(
        results[6],
        Submitted::Reorganised {
            fork_height: 4,
            disconnected: 6,
            connected: 7,
            tip_height: 11
        }
    );
    assert_eq!(node.tip(), (11, b.tip_id()));
    assert_eq!(
        node.digest(),
        b.digest(),
        "the state must equal the winning branch's"
    );
    // the old chain's blocks are kept, so it can win back
    assert_eq!(chain.side_block_count(), 6);

    // the old branch grows by two blocks: the first ties, the second wins
    let more = a.extend_n(2);
    assert!(matches!(
        chain.submit_block(&more[0], NOW).unwrap(),
        Submitted::SideChain { .. }
    ));
    assert_eq!(
        chain.submit_block(&more[1], NOW).unwrap(),
        Submitted::Reorganised {
            fork_height: 4,
            disconnected: 7,
            connected: 8,
            tip_height: 12
        }
    );
    assert_eq!(node.tip(), (12, a.tip_id()));
    assert_eq!(node.digest(), a.digest());
}

#[test]
fn a_shorter_branch_with_more_work_wins() {
    // blocks one second apart push the difficulty up, so each is worth more than a minute-spaced one
    let (a, b, shared, main, side) = two_branches("heavy", 4, 8, 6, 1);
    assert!(b.height() < a.height(), "the side branch must be shorter");
    assert!(
        b.work() > a.work(),
        "precondition: the short branch must have more work"
    );
    let node = Node::new("heavy-node");
    let mut chain = Chain::new(&node.store, &node.params, &Sha256Pow, &ProofsNotChecked);
    submit_all(&mut chain, &shared);
    submit_all(&mut chain, &main);
    let results = submit_all(&mut chain, &side);
    // it overtakes the longer chain as soon as its work is greater (at its 4th block), and the rest of
    // the branch then simply extends the new tip
    assert_eq!(
        results[3],
        Submitted::Reorganised {
            fork_height: 4,
            disconnected: 8,
            connected: 4,
            tip_height: 8
        }
    );
    assert!(results[..3]
        .iter()
        .all(|r| matches!(r, Submitted::SideChain { .. })));
    assert!(results[4..]
        .iter()
        .all(|r| matches!(r, Submitted::Extended(_))));
    assert_eq!(node.tip(), (b.height(), b.tip_id()));
    assert_eq!(node.digest(), b.digest());
}

#[test]
fn a_branch_invalid_only_at_reorganisation_leaves_the_chain_exactly_as_it_was() {
    // The main chain has 10 blocks; the side branch has 7 after a fork at 4, and its LAST block carries a
    // transaction the proof check refuses. Every other rule accepts it, so it is found out only when the
    // branch is about to win.
    let mut a = Net::new("badreorg-a", 11, 60);
    let shared = a.extend_n(4);
    let mut b = Net::new("badreorg-b", 22, 60);
    for blk in &shared {
        b.follow(blk);
    }
    let main = a.extend_n(6);
    let mut side = b.extend_n(6);
    let marked = b.spend_tx(true);
    let bad = b.extend(vec![marked]);
    let child = b.extend(vec![]);
    side.push(bad.clone());

    let node = Node::new("badreorg-node");
    let mut chain = Chain::new(&node.store, &node.params, &Sha256Pow, &RejectMarked);
    submit_all(&mut chain, &shared);
    submit_all(&mut chain, &main);
    let before = (node.digest(), node.tip());
    assert_eq!(before.0, a.digest());

    let results: Vec<_> = side[..6]
        .iter()
        .map(|b| chain.submit_block(b, NOW).unwrap())
        .collect();
    assert!(results
        .iter()
        .all(|r| matches!(r, Submitted::SideChain { .. })));
    let bad_id = ids::block_id(&bad.header, PowKind::Sha256);
    let err = chain.submit_block(&bad, NOW).unwrap_err();
    assert!(
        matches!(&err, BlockError::BranchInvalid { block_id, .. } if *block_id == bad_id),
        "{err:?}"
    );
    assert_eq!(
        (node.digest(), node.tip()),
        before,
        "a failed reorganisation must restore the chain exactly"
    );
    // the valid part of the branch stays available; the bad block is remembered
    assert_eq!(chain.side_block_count(), 6);
    assert!(chain.is_known_invalid(&bad_id));
    assert_eq!(chain.submit_block(&bad, NOW), Err(BlockError::KnownInvalid));
    // a block built on the invalid one is refused without being looked at again
    assert_eq!(
        chain.submit_block(&child, NOW),
        Err(BlockError::KnownInvalid)
    );
    // and the node carries on: the chain it kept still extends
    let next = a.extend(vec![]);
    assert!(matches!(
        chain.submit_block(&next, NOW).unwrap(),
        Submitted::Extended(_)
    ));
}

#[test]
fn a_reorganisation_below_the_pruned_part_is_refused() {
    let (a, _b, shared, main, side) = two_branches("pruned", 3, 7, 8, 60);
    let node = Node::new("pruned-node");
    let mut chain = Chain::new(&node.store, &node.params, &Sha256Pow, &ProofsNotChecked);
    submit_all(&mut chain, &shared);
    submit_all(&mut chain, &main);
    node.store.prune_below(8).unwrap();
    let before = (node.digest(), node.tip());
    for b in &side[..7] {
        assert!(matches!(
            chain.submit_block(b, NOW).unwrap(),
            Submitted::SideChain { .. }
        ));
    }
    let err = chain.submit_block(&side[7], NOW).unwrap_err();
    assert_eq!(
        err,
        BlockError::ReorgTooDeep {
            fork_height: 3,
            pruned_below: 8
        }
    );
    assert_eq!((node.digest(), node.tip()), before);
    assert_eq!(before.0, a.digest());
    // nothing was undone: the chain still has all its blocks
    assert_eq!(node.tip().0, 10);
}

#[test]
fn a_block_with_an_unknown_parent_is_an_orphan_and_is_not_kept() {
    let (_a, _b, shared, _main, side) = two_branches("orphan", 4, 0, 3, 60);
    let node = Node::new("orphan-node");
    let mut chain = Chain::new(&node.store, &node.params, &Sha256Pow, &ProofsNotChecked);
    submit_all(&mut chain, &shared);
    // the second side block, without its parent
    assert_eq!(
        chain.submit_block(&side[1], NOW).unwrap(),
        Submitted::Orphan
    );
    assert_eq!(chain.side_block_count(), 0);
    assert_eq!(node.tip().0, 4);
}

#[test]
fn a_block_seen_twice_is_already_known() {
    let (_a, _b, shared, main, side) = two_branches("dup", 4, 3, 2, 60);
    let node = Node::new("dup-node");
    let mut chain = Chain::new(&node.store, &node.params, &Sha256Pow, &ProofsNotChecked);
    submit_all(&mut chain, &shared);
    submit_all(&mut chain, &main);
    assert_eq!(
        chain.submit_block(&main[1], NOW).unwrap(),
        Submitted::AlreadyKnown
    );
    chain.submit_block(&side[0], NOW).unwrap();
    assert_eq!(
        chain.submit_block(&side[0], NOW).unwrap(),
        Submitted::AlreadyKnown
    );
}

#[test]
fn a_side_block_too_far_ahead_of_the_clock_is_not_yet_and_is_not_kept() {
    let (_a, _b, shared, main, side) = two_branches("future", 4, 3, 2, 60);
    let node = Node::new("future-node");
    let mut chain = Chain::new(&node.store, &node.params, &Sha256Pow, &ProofsNotChecked);
    submit_all(&mut chain, &shared);
    submit_all(&mut chain, &main);
    // a clock far in the past makes every block "ahead"
    assert_eq!(
        chain.submit_block(&side[0], 1_000).unwrap(),
        Submitted::NotYet
    );
    assert_eq!(chain.side_block_count(), 0);
    assert!(!chain.is_known_invalid(&ids::block_id(&side[0].header, PowKind::Sha256)));
    // and later it is fine
    assert!(matches!(
        chain.submit_block(&side[0], NOW).unwrap(),
        Submitted::SideChain { .. }
    ));
}

#[test]
fn a_side_block_that_breaks_a_rule_is_refused_and_remembered() {
    let (_a, mut b, shared, main, _side) = two_branches("badside", 4, 3, 0, 60);
    let node = Node::new("badside-node");
    let mut chain = Chain::new(&node.store, &node.params, &Sha256Pow, &ProofsNotChecked);
    submit_all(&mut chain, &shared);
    submit_all(&mut chain, &main);
    // a block on the side branch whose coinbase pays one unit too much
    let mut bad = b.build(vec![]);
    bad.coinbase.outputs[0].amount += 1;
    bad.header.tx_root = ids::block_tx_root(&bad.coinbase, &bad.transactions).unwrap();
    let target = b.validator().next_block().unwrap().target;
    mine(&mut bad, &target);
    let err = chain.submit_block(&bad, NOW).unwrap_err();
    assert!(matches!(err, BlockError::CoinbaseAmount { .. }), "{err:?}");
    assert_eq!(chain.submit_block(&bad, NOW), Err(BlockError::KnownInvalid));
    assert_eq!(chain.side_block_count(), 0);
}

#[test]
fn a_side_block_with_a_wrong_target_is_refused() {
    // a side block must meet the target the BRANCH requires, worked out from its own ancestors
    let (_a, mut b, shared, main, _side) = two_branches("target", 4, 3, 0, 60);
    let node = Node::new("target-node");
    let mut chain = Chain::new(&node.store, &node.params, &Sha256Pow, &ProofsNotChecked);
    submit_all(&mut chain, &shared);
    submit_all(&mut chain, &main);
    let mut blk = b.build(vec![]);
    // find a nonce that does NOT meet the required target
    let target = b.validator().next_block().unwrap().target;
    for nonce in 0.. {
        blk.header.nonce = nonce;
        if U256::from_be_bytes(&ids::block_id(&blk.header, PowKind::Sha256)) >= target {
            break;
        }
    }
    let err = chain.submit_block(&blk, NOW).unwrap_err();
    assert_eq!(err, BlockError::PowTargetNotMet);
}

#[test]
fn the_side_pool_is_bounded_and_a_branch_that_lost_its_start_is_an_orphan() {
    let (_a, _b, shared, main, side) = two_branches("bounded", 4, 6, 6, 60);
    let node = Node::new("bounded-node");
    let mut chain = Chain::new(&node.store, &node.params, &Sha256Pow, &ProofsNotChecked)
        .with_max_side_blocks(3);
    submit_all(&mut chain, &shared);
    submit_all(&mut chain, &main);
    for b in &side[..5] {
        chain.submit_block(b, NOW).unwrap();
    }
    assert_eq!(chain.side_block_count(), 3);
    // side[0..2] were dropped, so the next block's ancestry no longer leads back to the chain
    assert_eq!(
        chain.submit_block(&side[5], NOW).unwrap(),
        Submitted::Orphan
    );
}

#[test]
fn a_side_branch_may_spend_what_the_chain_also_spent() {
    // The same key image is spent once on each branch: each is valid on its own. A side block must not be
    // judged against the chain's state, or it would be refused as a double spend.
    let mut a = Net::new("both-a", 11, 60);
    let shared = a.extend_n(4);
    let mut b = Net::new("both-b", 22, 60);
    for blk in &shared {
        b.follow(blk);
    }
    b.next_key = a.next_key;
    let tx_a = a.spend_tx(false);
    let tx_b = b.spend_tx(false);
    assert_eq!(tx_a.prefix.inputs, tx_b.prefix.inputs);
    let mut main = vec![a.extend(vec![tx_a])];
    main.extend(a.extend_n(5));
    let mut side = vec![b.extend(vec![tx_b])];
    side.extend(b.extend_n(6));

    let node = Node::new("both-node");
    let mut chain = Chain::new(&node.store, &node.params, &Sha256Pow, &ProofsNotChecked);
    submit_all(&mut chain, &shared);
    submit_all(&mut chain, &main);
    let results = submit_all(&mut chain, &side);
    assert!(
        matches!(results[0], Submitted::SideChain { .. }),
        "{results:?}"
    );
    assert!(
        matches!(results[6], Submitted::Reorganised { .. }),
        "{results:?}"
    );
    assert_eq!(node.digest(), b.digest());
}

#[test]
fn a_reorganisation_exactly_at_the_pruning_boundary_is_allowed() {
    // blocks 4 and up keep their proofs, and a fork at 3 undoes only those
    let (_a, b, shared, main, side) = two_branches("boundary", 3, 7, 8, 60);
    let node = Node::new("boundary-node");
    let mut chain = Chain::new(&node.store, &node.params, &Sha256Pow, &ProofsNotChecked);
    submit_all(&mut chain, &shared);
    submit_all(&mut chain, &main);
    node.store.prune_below(4).unwrap();
    let results = submit_all(&mut chain, &side);
    assert!(
        matches!(results[7], Submitted::Reorganised { .. }),
        "{results:?}"
    );
    assert_eq!(node.digest(), b.digest());
}

#[test]
fn everything_built_on_a_block_found_invalid_is_dropped_and_remembered() {
    // the bad block is in the MIDDLE of the branch: the blocks after it were already waiting in the pool
    let mut a = Net::new("mid-a", 11, 60);
    let shared = a.extend_n(4);
    let mut b = Net::new("mid-b", 22, 60);
    for blk in &shared {
        b.follow(blk);
    }
    let main = a.extend_n(6);
    let mut side = b.extend_n(3); // heights 5..7
    let marked = b.spend_tx(true);
    let bad = b.extend(vec![marked]); // height 8
    side.push(bad.clone());
    side.extend(b.extend_n(3)); // heights 9..11: the last one has enough work to trigger the reorg

    let node = Node::new("mid-node");
    let mut chain = Chain::new(&node.store, &node.params, &Sha256Pow, &RejectMarked);
    submit_all(&mut chain, &shared);
    submit_all(&mut chain, &main);
    let before = (node.digest(), node.tip());
    for blk in &side[..6] {
        assert!(matches!(
            chain.submit_block(blk, NOW).unwrap(),
            Submitted::SideChain { .. }
        ));
    }
    assert_eq!(chain.side_block_count(), 6);
    let err = chain.submit_block(&side[6], NOW).unwrap_err();
    assert!(matches!(err, BlockError::BranchInvalid { .. }), "{err:?}");
    assert_eq!((node.digest(), node.tip()), before);
    // the three blocks before the bad one stay; it and the three after it are gone and remembered
    assert_eq!(chain.side_block_count(), 3);
    for blk in &side[3..] {
        assert!(chain.is_known_invalid(&ids::block_id(&blk.header, PowKind::Sha256)));
    }
    for blk in &side[..3] {
        assert!(chain.in_side_pool(&ids::block_id(&blk.header, PowKind::Sha256)));
    }
}
