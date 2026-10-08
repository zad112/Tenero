//! Wallets pay each other through a node that verifies every proof for real (FCMP++ membership and spend authorisation,
//! Bulletproofs+, the balance). Carrot outputs and addresses throughout. The chain is the SHA-256 test chain (a CPU mines a
//! block in an instant), so no GPU and no 4 GiB dataset. **A test chain, not a real one.**
//!
//! A block reward can be spent 60 blocks after its block (when it enters the curve tree), so most tests first mine 64
//! blocks to Alice: the rewards of blocks 1 to 5 are spendable then (block 5's entered the tree with block 64).

use std::path::PathBuf;

use rand_core::OsRng;
use tenero_chain::{ChainParams, Sha256Pow, Submitted};
use tenero_core::u256::U256;
use tenero_core::v2::ids::PowKind;
use tenero_core::v3::ids::block_id;
use tenero_core::v3::{rules, Block, Wire};
use tenero_net::sim::{test_chain_params, LABEL};
use tenero_node::{Node, NodeConfig};
use tenero_store::Store;
use tenero_wallet::wallet::RECENT_BLOCKS;
use tenero_wallet::{
    coinbase_payout, max_inputs_for, transaction_size, Address, ChainView, FeeLevel, FileError,
    KdfParams, Kind, Network, Submitter, Wallet, WalletError, MAX_RECIPIENTS,
};

const T0: u64 = 1_700_000_000;
/// Blocks mined before a test spends: the rewards of blocks 1 to 5 are spendable then.
const READY: u64 = 64;

struct Rig {
    path: PathBuf,
    store: Store,
    params: ChainParams,
}

impl Rig {
    fn new(tag: &str) -> Rig {
        let path =
            std::env::temp_dir().join(format!("tenero-wallet-{}-{tag}.redb", std::process::id()));
        remove(&path);
        let store = Store::open(&path, LABEL, PowKind::Sha256).unwrap();
        Rig {
            path,
            store,
            params: test_chain_params(),
        }
    }

    /// A node with the REAL proof check.
    fn node(&self) -> Node<'_> {
        Node::new(&self.store, &self.params, &Sha256Pow, NodeConfig::default())
            .expect("a node with real proofs")
    }
}

fn remove(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let mut s = path.clone().into_os_string();
    s.push(".segments");
    let _ = std::fs::remove_dir_all(PathBuf::from(s));
}

impl Drop for Rig {
    fn drop(&mut self) {
        remove(&self.path);
    }
}

/// A mined block on the node's tip, with the pool's transactions, its reward paid to `to` (a Carrot coinbase output).
fn block_for(node: &Node<'_>, to: &Address, ts: u64) -> Block {
    let height = node.tip().unwrap().0 + 1;
    let mut b = node
        .block_template(ts, u64::MAX, &|amount| {
            coinbase_payout(&mut OsRng, to, height, amount).expect("a main address")
        })
        .expect("a template");
    let target = node.next_block().unwrap().target;
    for nonce in 0.. {
        b.header.nonce = nonce;
        if U256::from_be_bytes(&block_id(&b.header, PowKind::Sha256)) < target {
            break;
        }
    }
    b
}

/// Mines one block paying `to`; returns the reward.
fn mine(node: &mut Node<'_>, to: &Address, extra_seconds: u64) -> u64 {
    let height = node.tip().unwrap().0 + 1;
    let ts = T0 + 60 * height + extra_seconds;
    let block = block_for(node, to, ts);
    let amount = block.coinbase.outputs[0].amount;
    match node
        .submit_block(&block, ts + 10)
        .expect("the block is valid")
    {
        Submitted::Extended(_) => {}
        other => panic!("not extended: {other:?}"),
    }
    amount
}

fn mine_n(node: &mut Node<'_>, to: &Address, n: u64) -> u64 {
    (0..n).map(|_| mine(node, to, 0)).sum()
}

fn wallet(tag: u8) -> Wallet {
    Wallet::from_seed(&[tag; 32], Network::Test, 0)
}

/// A node with `READY` blocks paid to a fresh Alice, and Alice synced.
fn funded(rig: &Rig) -> (Node<'_>, Wallet, u64) {
    let mut node = rig.node();
    let mut alice = wallet(1);
    let mined = mine_n(&mut node, &alice.address(), READY);
    alice.sync(&node).unwrap();
    (node, alice, mined)
}

#[test]
fn two_wallets_pay_each_other_through_a_node_with_real_proofs() {
    let rig = Rig::new("pay");
    let (mut node, mut alice, mut mined) = funded(&rig);
    let mut bob = wallet(2);
    let b = alice.balance(&node).unwrap();
    assert_eq!(b.total, mined);
    assert_eq!(
        b.spendable,
        5 * 2_000_000_000,
        "the rewards of blocks 1 to 5"
    );

    // Alice pays Bob
    let pay = 1_000_000_000;
    let built = alice
        .pay(&mut node, &mut OsRng, &bob.address(), pay)
        .unwrap();
    assert_eq!(built.amount, pay);
    assert_eq!(node.pool().len(), 1);
    // the fee is the Low level's share of the minimum for the transaction's real size, and its size was known to the byte
    let size = built.tx.to_bytes().unwrap().len();
    let layers = node.rules().unwrap().tree_layers;
    assert_eq!(size, transaction_size(built.spends.len(), 2, layers));
    let next = node.next_block().unwrap();
    let min = rules::min_fee(size as u64, next.reward, next.median).unwrap();
    assert_eq!(built.fee, min * 125 / 100 + 1);
    // the coins it spends are promised, so they are not offered again
    let after_send = alice.balance(&node).unwrap();
    assert!(after_send.reserved > 0);
    assert_eq!(after_send.total, b.total, "nothing has left yet");

    // a block takes it in (Alice mines it, so she also gets the fee)
    mined += mine(&mut node, &alice.address(), 0);
    assert_eq!(node.pool().len(), 0);
    alice.sync(&node).unwrap();
    bob.sync(&node).unwrap();
    assert_eq!(bob.balance(&node).unwrap().total, pay);
    let a = alice.balance(&node).unwrap();
    // nothing is created or lost: what the rewards paid (with the fee a second time, because the miner collects it) is
    // Alice's, Bob's, or the fee the transaction left behind
    assert_eq!(a.total + pay + built.fee, mined);
    // Alice's change is hers, and she knows it for change (an internal self-send), not a payment received
    let change: Vec<_> = alice.owned().iter().filter(|o| o.internal).collect();
    assert_eq!(change.len(), 1);
    assert_eq!(change[0].onetime_address, built.change_onetime);
    assert!(!bob.owned()[0].internal);

    // Bob's coin enters the tree 10 blocks after its block: not spendable before
    assert!(matches!(
        bob.build_payment(&node, &mut OsRng, &alice.address(), 1),
        Err(WalletError::NotEnough { spendable: 0, .. })
    ));
    mined += mine_n(&mut node, &alice.address(), 9);
    bob.sync(&node).unwrap();
    let back = 400_000_000;
    alice.sync(&node).unwrap();
    let alice_before = alice.balance(&node).unwrap().total;
    let built2 = bob
        .pay(&mut node, &mut OsRng, &alice.address(), back)
        .unwrap();
    mined += mine(&mut node, &alice.address(), 0);
    alice.sync(&node).unwrap();
    bob.sync(&node).unwrap();
    let (a2, b2) = (alice.balance(&node).unwrap(), bob.balance(&node).unwrap());
    assert_eq!(b2.total, pay - back - built2.fee, "Bob keeps his change");
    assert_eq!(a2.total + b2.total + built.fee + built2.fee, mined);
    assert!(a2.total > alice_before);
}

#[test]
fn a_subaddress_and_an_integrated_address_are_paid_and_found() {
    let rig = Rig::new("subaddress");
    let (mut node, mut alice, _) = funded(&rig);
    let mut bob = wallet(2);
    let sub = bob.subaddress(7).unwrap();
    assert_eq!(sub.kind, Kind::Subaddress);
    assert_ne!(sub, bob.address());
    let integrated = bob.integrated_address(*b"invoice1").unwrap();
    assert!(integrated.to_text().len() == 110 && integrated.to_text().starts_with("TENt"));
    alice.pay(&mut node, &mut OsRng, &sub, 300_000_000).unwrap();
    alice.sync(&node).unwrap();
    alice
        .pay(&mut node, &mut OsRng, &integrated, 500_000_000)
        .unwrap();
    mine(&mut node, &alice.address(), 0);
    bob.sync(&node).unwrap();
    let mut got: Vec<_> = bob
        .owned()
        .iter()
        .map(|o| (o.amount, o.address.minor, o.payment_id))
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![(300_000_000, 7, [0; 8]), (500_000_000, 0, *b"invoice1")]
    );
    // a coin received at a subaddress is spent like any other
    mine_n(&mut node, &alice.address(), 10);
    bob.sync(&node).unwrap();
    let built = bob
        .build_payment(&node, &mut OsRng, &alice.address(), 250_000_000)
        .unwrap();
    assert_eq!(built.spends.len(), 1);
    node.submit_tx(built.tx).unwrap();
    // a wallet restored from Bob's seed finds the subaddress payment too (it watches the first subaddresses)
    let mut again = Wallet::from_seed(bob.seed(), Network::Test, 0);
    again.sync(&node).unwrap();
    assert_eq!(again.owned().len(), 2);
}

#[test]
fn a_block_reward_cannot_go_to_a_subaddress_or_another_network() {
    let mut bob = wallet(2);
    let sub = bob.subaddress(1).unwrap();
    assert!(coinbase_payout(&mut OsRng, &sub, 5, 100).is_none());
    assert!(coinbase_payout(&mut OsRng, &bob.address(), 5, 100).is_some());
    // a payment to an address of another network is refused before anything is built
    let rig = Rig::new("network");
    let (node, mut alice, _) = funded(&rig);
    let gamma = Wallet::from_seed(&[2; 32], Network::Gamma, 0).address();
    assert_eq!(
        alice
            .build_payment(&node, &mut OsRng, &gamma, 1_000)
            .unwrap_err(),
        WalletError::BadAddress
    );
}

#[test]
fn the_chain_never_sees_a_spent_output_twice() {
    let rig = Rig::new("double");
    let (mut node, mut alice, _) = funded(&rig);
    let bob = wallet(2);
    let first = alice
        .pay(&mut node, &mut OsRng, &bob.address(), 1_000)
        .unwrap();
    assert!(
        node.submit_tx(first.tx.clone()).is_err(),
        "the same transaction twice"
    );
    mine(&mut node, &alice.address(), 0);
    for ki in &first.spends {
        assert!(node.key_image_spent(ki).unwrap());
    }
    // a second payment now uses other coins, and the wallet knows the first is spent
    alice.sync(&node).unwrap();
    let second = alice
        .pay(&mut node, &mut OsRng, &bob.address(), 2_000)
        .unwrap();
    assert!(second.spends.iter().all(|k| !first.spends.contains(k)));
}

#[test]
fn a_payment_that_is_still_waiting_blocks_its_coins_from_a_second_payment() {
    let rig = Rig::new("reserve");
    let (mut node, mut alice, _) = funded(&rig);
    let bob = wallet(2);
    let spendable = alice.balance(&node).unwrap().spendable;
    // the first payment takes all but one coin's worth
    alice
        .pay(
            &mut node,
            &mut OsRng,
            &bob.address(),
            spendable - 2_100_000_000,
        )
        .unwrap();
    let b = alice.balance(&node).unwrap();
    assert!(b.reserved >= spendable - 2_100_000_000);
    // what is left is not enough for the same again
    assert!(matches!(
        alice.build_payment(&node, &mut OsRng, &bob.address(), spendable - 2_100_000_000),
        Err(WalletError::NotEnough { .. })
    ));
}

#[test]
fn a_payment_needing_several_inputs_spends_them_in_key_image_order_and_is_its_computed_size() {
    let rig = Rig::new("inputs");
    let (mut node, mut alice, _) = funded(&rig);
    let bob = wallet(2);
    // more than four rewards: all five coins
    let built = alice
        .pay(&mut node, &mut OsRng, &bob.address(), 9_000_000_000)
        .unwrap();
    assert_eq!(built.spends.len(), 5);
    assert!(built.spends.windows(2).all(|w| w[0] < w[1]));
    let layers = node.rules().unwrap().tree_layers;
    assert_eq!(
        built.tx.to_bytes().unwrap().len(),
        transaction_size(5, 2, layers)
    );
    mine(&mut node, &alice.address(), 0);
    assert!(built
        .spends
        .iter()
        .all(|k| node.key_image_spent(k).unwrap()));
}

#[test]
fn one_transaction_pays_several_recipients_and_sixteen_do_not_fit() {
    let rig = Rig::new("several");
    let (mut node, mut alice, _) = funded(&rig);
    let mut bobs: Vec<Wallet> = (10..13).map(wallet).collect();
    let dests: Vec<(Address, u64)> = bobs
        .iter()
        .enumerate()
        .map(|(i, w)| (w.address(), 100_000_000 * (i as u64 + 1)))
        .collect();
    let built = alice
        .build_to_at(&node, &mut OsRng, &dests, FeeLevel::Normal)
        .unwrap();
    // four outputs: each with its own ephemeral key
    assert_eq!(built.tx.prefix.outputs.len(), 4);
    assert_eq!(built.tx.prefix.ephemeral_pubkeys.len(), 4);
    assert_eq!(built.parts.len(), 3);
    let layers = node.rules().unwrap().tree_layers;
    assert_eq!(
        built.tx.to_bytes().unwrap().len(),
        transaction_size(built.spends.len(), 4, layers)
    );
    alice.send_built(&mut node, &built).unwrap();
    mine(&mut node, &alice.address(), 0);
    for (i, b) in bobs.iter_mut().enumerate() {
        b.sync(&node).unwrap();
        assert_eq!(
            b.balance(&node).unwrap().total,
            100_000_000 * (i as u64 + 1)
        );
    }
    let many: Vec<(Address, u64)> = (0..MAX_RECIPIENTS + 1)
        .map(|_| (bobs[0].address(), 1_000))
        .collect();
    assert_eq!(
        alice
            .build_to_at(&node, &mut OsRng, &many, FeeLevel::Low)
            .unwrap_err(),
        WalletError::TooManyRecipients {
            max: MAX_RECIPIENTS
        }
    );
}

#[test]
fn a_batch_pays_more_recipients_than_one_transaction_holds_with_different_coins() {
    let rig = Rig::new("batch");
    let (mut node, mut alice, _) = funded(&rig);
    let bob = wallet(2);
    let dests: Vec<(Address, u64)> = (0..20).map(|_| (bob.address(), 10_000_000)).collect();
    let plan = alice
        .build_batch(&node, &mut OsRng, &dests, FeeLevel::Low)
        .unwrap();
    assert_eq!(plan.txs.len(), 2, "15 recipients, then 5");
    assert!(plan.unsent.is_empty());
    let first: std::collections::HashSet<_> = plan.txs[0].spends.iter().collect();
    assert!(plan.txs[1].spends.iter().all(|k| !first.contains(k)));
    let sent = alice.send_batch(&mut node, &plan.txs);
    assert_eq!((sent.sent, sent.failed), (2, None));
    mine(&mut node, &alice.address(), 0);
    let mut bob = bob;
    bob.sync(&node).unwrap();
    assert_eq!(bob.balance(&node).unwrap().total, 200_000_000);
    assert_eq!(bob.owned().len(), 20);
}

#[test]
fn a_sweep_makes_many_coins_one() {
    let rig = Rig::new("sweep");
    let (mut node, mut alice, _) = funded(&rig);
    let before = alice.balance(&node).unwrap().spendable;
    let txs = alice
        .build_sweep(&node, &mut OsRng, None, FeeLevel::Low)
        .unwrap();
    assert_eq!(txs.len(), 1);
    assert_eq!(txs[0].spends.len(), 5);
    assert_eq!(txs[0].amount + txs[0].fee, before);
    alice.send_batch(&mut node, &txs);
    mine(&mut node, &alice.address(), 0);
    alice.sync(&node).unwrap();
    let swept: Vec<_> = alice
        .owned()
        .iter()
        .filter(|o| o.onetime_address == txs[0].parts[0].onetime)
        .collect();
    assert_eq!(swept.len(), 1);
    assert_eq!(swept[0].amount, before - txs[0].fee);
    // a combine of fewer coins than one transaction can spend, and of more
    let max = max_inputs_for(2, node.rules().unwrap().tree_layers);
    assert!(max >= 5, "{max}");
    assert_eq!(
        alice
            .build_combine(&node, &mut OsRng, max + 1, FeeLevel::Low)
            .unwrap_err(),
        WalletError::TooManyInputs { max }
    );
}

#[test]
fn bad_requests_are_refused_before_anything_is_built() {
    let rig = Rig::new("bad");
    let (node, mut alice, _) = funded(&rig);
    let bob = wallet(2);
    assert_eq!(
        alice
            .build_payment(&node, &mut OsRng, &bob.address(), 0)
            .unwrap_err(),
        WalletError::ZeroAmount
    );
    assert!(matches!(
        alice.build_payment(&node, &mut OsRng, &bob.address(), u64::MAX),
        Err(WalletError::NotEnough { .. })
    ));
    // two integrated addresses in one transaction (Carrot carries one payment ID)
    let one = bob.integrated_address([1; 8]).unwrap();
    let two = bob.integrated_address([2; 8]).unwrap();
    assert_eq!(
        alice
            .build_to_at(&node, &mut OsRng, &[(one, 5), (two, 5)], FeeLevel::Low)
            .unwrap_err(),
        WalletError::BadAddress
    );
}

#[test]
fn a_wallet_restored_from_its_seed_finds_the_same_coins() {
    let rig = Rig::new("restore");
    let (mut node, mut alice, _) = funded(&rig);
    let mut bob = wallet(2);
    alice
        .pay(&mut node, &mut OsRng, &bob.address(), 3_000_000)
        .unwrap();
    mine_n(&mut node, &alice.address(), 2);
    alice.sync(&node).unwrap();
    bob.sync(&node).unwrap();
    let mut again = Wallet::from_seed(alice.seed(), Network::Test, 0);
    again.sync(&node).unwrap();
    let mut a: Vec<_> = alice.owned().to_vec();
    let mut b: Vec<_> = again.owned().to_vec();
    a.sort_by_key(|o| o.global_index);
    b.sort_by_key(|o| o.global_index);
    assert_eq!(
        a, b,
        "the change included: a restored wallet finds its own change"
    );
    // a wallet that starts at the tip does not look back
    let mut late = Wallet::from_seed(alice.seed(), Network::Test, node.tip().unwrap().0);
    late.sync(&node).unwrap();
    assert!(late.owned().len() < a.len());
    // an empty wallet is not confused by the same chain
    let mut nobody = wallet(9);
    let rep = nobody.sync(&node).unwrap();
    assert!(rep.blocks_scanned > 0 && rep.outputs_found == 0);
}

#[test]
fn syncing_twice_changes_nothing() {
    let rig = Rig::new("twice");
    let mut node = rig.node();
    let mut alice = wallet(1);
    mine_n(&mut node, &alice.address(), 4);
    let first = alice.sync(&node).unwrap();
    assert_eq!(first.blocks_scanned, 5, "the genesis block and four more");
    assert_eq!(first.outputs_found, 4);
    let owned = alice.owned().to_vec();
    let second = alice.sync(&node).unwrap();
    assert_eq!(
        (
            second.blocks_scanned,
            second.outputs_found,
            second.blocks_rolled_back
        ),
        (0, 0, 0)
    );
    assert_eq!(alice.owned(), &owned[..]);
}

/// The wallet follows a reorganisation: a payment that was in the abandoned branch disappears, and the coins it spent are
/// spendable again.
#[test]
fn a_reorganisation_takes_back_what_the_wallet_thought_it_had() {
    let rig1 = Rig::new("reorg1");
    let rig2 = Rig::new("reorg2");
    let mut n1 = rig1.node();
    let mut n2 = rig2.node();
    let (mut alice, mut bob) = (wallet(1), wallet(2));
    // a shared past
    for _ in 0..READY {
        let h = n1.tip().unwrap().0 + 1;
        let ts = T0 + 60 * h;
        let b = block_for(&n1, &alice.address(), ts);
        n1.submit_block(&b, ts + 10).unwrap();
        n2.submit_block(&b, ts + 10).unwrap();
    }
    alice.sync(&n1).unwrap();
    let before = alice.balance(&n1).unwrap();
    // branch 1: Alice pays Bob and it is mined; Bob sees it
    alice
        .pay(&mut n1, &mut OsRng, &bob.address(), 2_000_000)
        .unwrap();
    mine(&mut n1, &alice.address(), 0);
    bob.sync(&n1).unwrap();
    alice.sync(&n1).unwrap();
    assert_eq!(bob.balance(&n1).unwrap().total, 2_000_000);
    // branch 2: longer, without the payment (mined to somebody else)
    let carol = wallet(3);
    for k in 0..3 {
        mine(&mut n2, &carol.address(), 7 + k);
    }
    for h in READY + 1..=READY + 3 {
        let stored = n2
            .store()
            .get_block(h)
            .unwrap()
            .unwrap()
            .into_full()
            .unwrap();
        let r = n1.submit_block(&stored, T0 + 60 * h + 100).unwrap();
        if h == READY + 2 {
            assert!(matches!(r, Submitted::Reorganised { .. }), "{r:?}");
        }
    }
    let rb = bob.sync(&n1).unwrap();
    assert!(rb.blocks_rolled_back >= 1);
    assert_eq!(
        bob.balance(&n1).unwrap().total,
        0,
        "the payment is gone with its block"
    );
    let ra = alice.sync(&n1).unwrap();
    assert!(ra.blocks_rolled_back >= 1 && !ra.rescanned);
    let after = alice.balance(&n1).unwrap();
    assert_eq!(
        after.total, before.total,
        "Alice has her coins back, and not the block she mined on the lost branch"
    );
    assert!(after.spendable > 0);
}

#[test]
fn a_reorganisation_deeper_than_the_wallet_remembers_makes_it_rescan() {
    let rig1 = Rig::new("deep1");
    let mut n1 = rig1.node();
    let mut alice = wallet(1);
    mine_n(&mut n1, &alice.address(), RECENT_BLOCKS as u64 + 5);
    alice.sync(&n1).unwrap();
    assert!(!alice.owned().is_empty());
    // a different chain (only the genesis block in common): every block the wallet remembers is gone
    let rig2 = Rig::new("deep2");
    let mut n2 = rig2.node();
    let other = wallet(5);
    for k in 0..4 {
        mine(&mut n2, &other.address(), k * 3 + 1);
    }
    let r = alice.sync(&n2).unwrap();
    assert!(r.rescanned, "{r:?}");
    assert_eq!(r.blocks_rolled_back, RECENT_BLOCKS as u64);
    assert_eq!(alice.balance(&n2).unwrap().total, 0);
    assert!(alice.owned().is_empty());
}

// ------------------------------------------------------------------------------------------------
// the wallet file
// ------------------------------------------------------------------------------------------------

fn temp_file(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("tenero-wallet-file-{}-{tag}", std::process::id()))
}

#[test]
fn the_wallet_file_round_trips_with_the_right_passphrase_only() {
    let rig = Rig::new("file");
    let mut node = rig.node();
    let mut alice = wallet(1);
    mine_n(&mut node, &alice.address(), 4);
    alice.sync(&node).unwrap();
    alice.subaddress(80).unwrap();
    let path = temp_file("a");
    alice
        .save(
            &path,
            b"correct horse",
            KdfParams::TEST_ONLY_WEAK,
            &mut OsRng,
        )
        .unwrap();
    let back = Wallet::load(&path, b"correct horse").unwrap();
    assert_eq!(back.address(), alice.address());
    assert_eq!(back.seed(), alice.seed());
    assert_eq!(back.owned(), alice.owned());
    assert_eq!(back.scanned_height(), alice.scanned_height());
    assert_eq!(back.birth_height(), alice.birth_height());
    assert_eq!(back.network(), Network::Test);
    assert_eq!(back.watched_subaddresses(), alice.watched_subaddresses());
    assert_eq!(
        Wallet::load(&path, b"wrong").err(),
        Some(FileError::WrongPassphraseOrCorrupt)
    );
    assert_eq!(
        Wallet::load(&path, b"").err(),
        Some(FileError::WrongPassphraseOrCorrupt)
    );
    // the file does not contain the seed or an address key in the clear
    let bytes = std::fs::read(&path).unwrap();
    assert!(!bytes.windows(32).any(|w| w == alice.seed()));
    assert!(!bytes.windows(32).any(|w| w == alice.address().spend_pubkey));
    // a restored wallet carries on (it does not rescan what it has)
    let mut back = back;
    let r = back.sync(&node).unwrap();
    assert_eq!(r.blocks_scanned, 0);
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn every_byte_of_the_wallet_file_is_protected() {
    let alice = wallet(1);
    let path = temp_file("b");
    alice
        .save(&path, b"pw", KdfParams::TEST_ONLY_WEAK, &mut OsRng)
        .unwrap();
    let good = std::fs::read(&path).unwrap();
    for i in 0..good.len() {
        let mut bad = good.clone();
        bad[i] ^= 0x01;
        std::fs::write(&path, &bad).unwrap();
        assert!(
            Wallet::load(&path, b"pw").is_err(),
            "a change at byte {i} was not noticed"
        );
    }
    std::fs::write(&path, &good[..good.len() - 1]).unwrap();
    assert!(Wallet::load(&path, b"pw").is_err());
    let mut longer = good.clone();
    longer.push(0);
    std::fs::write(&path, &longer).unwrap();
    assert!(Wallet::load(&path, b"pw").is_err());
    std::fs::write(
        &path,
        b"not a wallet at all, just some text that is long enough to pass the length check....",
    )
    .unwrap();
    assert_eq!(
        Wallet::load(&path, b"pw").err(),
        Some(FileError::NotAWalletFile)
    );
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn a_hostile_file_cannot_make_the_wallet_allocate_gigabytes() {
    let alice = wallet(1);
    let path = temp_file("c");
    alice
        .save(&path, b"pw", KdfParams::TEST_ONLY_WEAK, &mut OsRng)
        .unwrap();
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[4..8].copy_from_slice(&u32::MAX.to_le_bytes()); // memory
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(Wallet::load(&path, b"pw").err(), Some(FileError::BadParams));
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn saving_replaces_the_file_whole_and_uses_fresh_randomness() {
    let alice = wallet(1);
    let path = temp_file("d");
    alice
        .save(&path, b"pw", KdfParams::TEST_ONLY_WEAK, &mut OsRng)
        .unwrap();
    let first = std::fs::read(&path).unwrap();
    alice
        .save(&path, b"pw", KdfParams::TEST_ONLY_WEAK, &mut OsRng)
        .unwrap();
    let second = std::fs::read(&path).unwrap();
    assert_ne!(first[16..32], second[16..32], "a new salt each time");
    assert_ne!(first[32..44], second[32..44], "a new nonce each time");
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    assert!(!PathBuf::from(tmp).exists());
    assert!(Wallet::load(&path, b"pw").is_ok());
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn saving_to_a_place_that_does_not_exist_is_an_error_not_a_panic() {
    let alice = wallet(1);
    let path = std::env::temp_dir()
        .join("tenero-no-such-dir-xyz")
        .join("w");
    assert!(matches!(
        alice.save(&path, b"pw", KdfParams::TEST_ONLY_WEAK, &mut OsRng),
        Err(FileError::Io(_))
    ));
    assert!(matches!(Wallet::load(&path, b"pw"), Err(FileError::Io(_))));
}

// keep the unused-import check honest: these traits are what a node implements for the wallet
#[allow(dead_code)]
fn _a_node_is_a_chain_and_a_submitter(n: &mut Node<'_>) {
    fn both<T: ChainView + Submitter>(_: &mut T) {}
    both(n);
}
