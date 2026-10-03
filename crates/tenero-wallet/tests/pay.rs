//! Two wallets pay each other through a node that verifies every proof for real (CLSAG, Bulletproofs+, balance).
//! The chain is the SHA-256 test chain (a CPU mines a block in an instant), so no GPU and no 4 GiB dataset.
//! **A test chain, not a real one.**

use std::path::PathBuf;

use rand_core::OsRng;
use tenero_chain::{ChainParams, Sha256Pow, Submitted};
use tenero_core::fees;
use tenero_core::v2::Wire;
use tenero_net::sim::{mine_test_block, test_chain_params, LABEL};
use tenero_node::{Node, NodeConfig};
use tenero_store::Store;
use tenero_wallet::{
    coinbase_payout, Address, ChainView, FileError, KdfParams, Submitter, Wallet, WalletError,
};

const T0: u64 = 1_700_000_000;

struct Rig {
    path: PathBuf,
    store: Store,
    params: ChainParams,
}

impl Rig {
    fn new(tag: &str, ring_size: usize, maturity: u64) -> Rig {
        let path =
            std::env::temp_dir().join(format!("tenero-wallet-{}-{tag}.redb", std::process::id()));
        remove(&path);
        let store = Store::open(&path, LABEL, tenero_core::v2::ids::PowKind::Sha256).unwrap();
        let mut params = test_chain_params();
        params.ring_size = ring_size;
        params.coinbase_maturity = maturity;
        params.spend_maturity = maturity;
        Rig {
            path,
            store,
            params,
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

/// Mines one block paying `to`; returns the coinbase amount.
fn mine(node: &mut Node<'_>, to: &Address, extra_seconds: u64) -> u64 {
    let height = node.tip().unwrap().0 + 1;
    let payout = coinbase_payout(&mut OsRng, to, height).unwrap();
    let ts = T0 + 60 * height + extra_seconds;
    let block = mine_test_block(node, ts, payout);
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
    Wallet::from_seed(&[tag; 32], 0)
}

#[test]
fn two_wallets_pay_each_other_through_a_node_with_real_proofs() {
    let rig = Rig::new("pay", 2, 2);
    let mut node = rig.node();
    let (mut alice, mut bob) = (wallet(1), wallet(2));
    let mut mined = mine_n(&mut node, &alice.address(), 5);
    alice.sync(&node).unwrap();
    let b = alice.balance(&node).unwrap();
    assert_eq!(b.total, mined);
    assert!(
        b.spendable > 0 && b.spendable < b.total,
        "the newest coinbase is not yet mature"
    );

    // Alice pays Bob
    let pay = 1_000_000_000;
    let built = alice
        .pay(&mut node, &mut OsRng, &bob.address(), pay)
        .unwrap();
    assert_eq!(built.amount, pay);
    assert!(node.pool().len() == 1);
    // the fee is the minimum plus a margin, not more than that
    let size = built.tx.to_bytes().unwrap().len() as u64;
    let next = node.next_block().unwrap();
    let min = fees::dynamic_min_fee(size, next.reward, next.median).unwrap();
    assert!(
        built.fee * 100 >= min * 120 && built.fee <= min * 3,
        "fee {} vs minimum {min}",
        built.fee
    );
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
    // nothing is created or lost: what the coinbases paid (the emission, and the fee a second time, because the
    // miner collects it) is Alice's, Bob's, or the fee the transaction left behind
    assert_eq!(a.total + pay + built.fee, mined);

    // Bob (whose coins are not mature yet) cannot pay until a block passes; then he can, and Alice receives
    assert!(matches!(
        bob.build_payment(&node, &mut OsRng, &alice.address(), 1),
        Err(WalletError::NotEnough { spendable: 0, .. })
    ));
    mined += mine(&mut node, &alice.address(), 0);
    bob.sync(&node).unwrap();
    let back = 400_000_000;
    let alice_before = {
        alice.sync(&node).unwrap();
        alice.balance(&node).unwrap().total
    };
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
fn the_chain_never_sees_a_spent_output_twice() {
    let rig = Rig::new("double", 2, 1);
    let mut node = rig.node();
    let mut alice = wallet(1);
    let bob = wallet(2);
    mine_n(&mut node, &alice.address(), 6);
    alice.sync(&node).unwrap();
    let first = alice
        .pay(&mut node, &mut OsRng, &bob.address(), 1_000)
        .unwrap();
    // spending the same coin again, by hand, is refused by the node's key-image rule (a copy of the transaction
    // with another id would have the same key image)
    let dup = first.tx.clone();
    assert!(node.submit_tx(dup).is_err(), "the same transaction twice");
    mine(&mut node, &alice.address(), 0);
    for ki in &first.spends {
        assert!(
            node.key_image_spent(ki).unwrap(),
            "the spent coin's key image is in the chain"
        );
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
    let rig = Rig::new("reserve", 2, 1);
    let mut node = rig.node();
    let mut alice = wallet(1);
    let bob = wallet(2);
    mine_n(&mut node, &alice.address(), 3);
    alice.sync(&node).unwrap();
    let spendable = alice.balance(&node).unwrap().spendable;
    // the first payment takes almost everything spendable
    let _ = alice
        .pay(
            &mut node,
            &mut OsRng,
            &bob.address(),
            spendable - 2_000_000_000,
        )
        .unwrap();
    // so a second one cannot, until the first is mined (or the reservation runs out)
    let again = alice.pay(
        &mut node,
        &mut OsRng,
        &bob.address(),
        spendable - 2_000_000_000,
    );
    assert!(
        matches!(again, Err(WalletError::NotEnough { .. })),
        "{again:?}"
    );
}

#[test]
fn rings_of_sixteen_with_real_signatures() {
    let rig = Rig::new("ring16", 16, 1);
    let mut node = rig.node();
    let (mut alice, mut bob) = (wallet(1), wallet(2));
    // fewer than sixteen outputs: cannot hide yet
    mine_n(&mut node, &alice.address(), 8);
    alice.sync(&node).unwrap();
    assert!(matches!(
        alice.build_payment(&node, &mut OsRng, &bob.address(), 1_000),
        Err(WalletError::NotEnoughDecoys)
    ));
    mine_n(&mut node, &alice.address(), 14);
    alice.sync(&node).unwrap();
    let built = alice
        .pay(&mut node, &mut OsRng, &bob.address(), 5_000_000)
        .unwrap();
    // the fee is at least 20% over the minimum the transaction's size asks for, and not wasteful
    let size = built.tx.to_bytes().unwrap().len() as u64;
    let next = node.next_block().unwrap();
    let min = fees::dynamic_min_fee(size, next.reward, next.median).unwrap();
    println!(
        "ring-16 transaction: {size} bytes, fee {} vs minimum {min}",
        built.fee
    );
    assert!(built.fee * 100 >= min * 120 && built.fee <= min * 3);
    for ring in &built.tx.prunable.rings {
        assert_eq!(ring.len(), 16);
        assert!(
            ring.windows(2).all(|w| w[0] < w[1]),
            "ascending and distinct"
        );
    }
    mine(&mut node, &alice.address(), 0);
    bob.sync(&node).unwrap();
    assert_eq!(bob.balance(&node).unwrap().total, 5_000_000);
}

#[test]
fn a_payment_needing_several_inputs_is_built_with_them_in_key_image_order() {
    let rig = Rig::new("multi", 2, 1);
    let mut node = rig.node();
    let (mut alice, bob) = (wallet(1), wallet(2));
    mine_n(&mut node, &alice.address(), 6);
    alice.sync(&node).unwrap();
    let one = alice.owned()[0].amount;
    // more than any one output but within what is mature
    let built = alice
        .pay(&mut node, &mut OsRng, &bob.address(), one * 3)
        .unwrap();
    assert!(
        built.tx.prefix.inputs.len() >= 3,
        "{} inputs",
        built.tx.prefix.inputs.len()
    );
    assert!(built
        .tx
        .prefix
        .inputs
        .windows(2)
        .all(|w| w[0].key_image < w[1].key_image));
    assert_eq!(built.tx.prefix.outputs.len(), 2);
}

#[test]
fn too_many_inputs_is_said_plainly() {
    let rig = Rig::new("many", 2, 1);
    let mut node = rig.node();
    let (mut alice, bob) = (wallet(1), wallet(2));
    mine_n(&mut node, &alice.address(), 40);
    alice.sync(&node).unwrap();
    let spendable = alice.balance(&node).unwrap().spendable;
    let r = alice.build_payment(&node, &mut OsRng, &bob.address(), spendable - 1_000_000_000);
    assert_eq!(r.unwrap_err(), WalletError::TooManyInputs);
}

#[test]
fn bad_requests_are_refused_before_anything_is_built() {
    let rig = Rig::new("bad", 2, 1);
    let mut node = rig.node();
    let (mut alice, bob) = (wallet(1), wallet(2));
    mine_n(&mut node, &alice.address(), 4);
    alice.sync(&node).unwrap();
    assert_eq!(
        alice
            .build_payment(&node, &mut OsRng, &bob.address(), 0)
            .unwrap_err(),
        WalletError::ZeroAmount
    );
    let spendable = alice.balance(&node).unwrap().spendable;
    assert!(matches!(
        alice
            .build_payment(&node, &mut OsRng, &bob.address(), spendable + 1)
            .unwrap_err(),
        WalletError::NotEnough { .. }
    ));
    let bad = Address {
        spend: [0; 32],
        view: bob.address().view,
    };
    assert_eq!(
        alice
            .build_payment(&node, &mut OsRng, &bad, 1_000)
            .unwrap_err(),
        WalletError::BadAddress
    );
    // paying all of it leaves no room for the fee
    assert!(matches!(
        alice
            .build_payment(&node, &mut OsRng, &bob.address(), spendable)
            .unwrap_err(),
        WalletError::NotEnough { .. }
    ));
    assert_eq!(node.pool().len(), 0, "nothing was sent");
}

#[test]
fn a_wallet_restored_from_its_seed_finds_the_same_coins() {
    let rig = Rig::new("restore", 2, 1);
    let mut node = rig.node();
    let (mut alice, mut bob) = (wallet(1), wallet(2));
    mine_n(&mut node, &alice.address(), 5);
    alice.sync(&node).unwrap();
    alice
        .pay(&mut node, &mut OsRng, &bob.address(), 3_000_000)
        .unwrap();
    mine_n(&mut node, &alice.address(), 2);
    alice.sync(&node).unwrap();
    bob.sync(&node).unwrap();
    let mut again = Wallet::from_seed(alice.seed(), 0);
    again.sync(&node).unwrap();
    let mut a: Vec<_> = alice.owned().to_vec();
    let mut b: Vec<_> = again.owned().to_vec();
    a.sort_by_key(|o| o.global_index);
    b.sort_by_key(|o| o.global_index);
    assert_eq!(a, b);
    assert_eq!(alice.balance(&node).unwrap(), again.balance(&node).unwrap());
    // a wallet that starts at the tip does not look back
    let mut late = Wallet::from_seed(alice.seed(), node.tip().unwrap().0);
    late.sync(&node).unwrap();
    assert!(late.owned().len() < a.len());
    // an empty wallet is not confused by the same chain
    let mut nobody = wallet(9);
    let rep = nobody.sync(&node).unwrap();
    assert!(rep.blocks_scanned > 0 && rep.outputs_found == 0);
    assert_eq!(nobody.balance(&node).unwrap().total, 0);
}

#[test]
fn syncing_twice_changes_nothing() {
    let rig = Rig::new("twice", 2, 1);
    let mut node = rig.node();
    let mut alice = wallet(1);
    mine_n(&mut node, &alice.address(), 4);
    let first = alice.sync(&node).unwrap();
    assert_eq!(first.blocks_scanned, 5, "the genesis block and four more");
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

/// The wallet follows a reorganisation: a payment that was in the abandoned branch disappears, and the coins
/// it spent are spendable again.
#[test]
fn a_reorganisation_takes_back_what_the_wallet_thought_it_had() {
    let rig1 = Rig::new("reorg1", 2, 1);
    let rig2 = Rig::new("reorg2", 2, 1);
    let mut n1 = rig1.node();
    let mut n2 = rig2.node();
    let (mut alice, mut bob) = (wallet(1), wallet(2));
    // a shared past
    for _ in 0..5 {
        let h = n1.tip().unwrap().0 + 1;
        let payout = coinbase_payout(&mut OsRng, &alice.address(), h).unwrap();
        let ts = T0 + 60 * h;
        let b = mine_test_block(&n1, ts, payout);
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
    // n1 takes branch 2
    for h in 6..=8 {
        let stored = n2
            .store()
            .get_block(h)
            .unwrap()
            .unwrap()
            .into_full()
            .unwrap();
        let r = n1.submit_block(&stored, T0 + 60 * h + 100).unwrap();
        // the branch is longer than ours (height 6) once its second block arrives
        if h == 7 {
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
    // the transaction went back to the pool of the node that lost it, and pay works again
    let again = alice.pay(&mut n1, &mut OsRng, &bob.address(), 1_000_000);
    assert!(
        again.is_ok() || matches!(again, Err(WalletError::Submit(_))),
        "{again:?}"
    );
}

#[test]
fn a_reorganisation_deeper_than_the_wallet_remembers_makes_it_rescan() {
    let rig1 = Rig::new("deep1", 2, 1);
    let mut n1 = rig1.node();
    let mut alice = wallet(1);
    // more blocks than the wallet remembers
    mine_n(
        &mut n1,
        &alice.address(),
        tenero_wallet::wallet::RECENT_BLOCKS as u64 + 5,
    );
    alice.sync(&n1).unwrap();
    assert!(!alice.owned().is_empty());
    // a different chain (only the genesis block in common): every block the wallet remembers is gone
    let rig2 = Rig::new("deep2", 2, 1);
    let mut n2 = rig2.node();
    let other = wallet(5);
    for k in 0..4 {
        mine(&mut n2, &other.address(), k * 3 + 1);
    }
    let r = alice.sync(&n2).unwrap();
    assert!(r.rescanned, "{r:?}");
    assert_eq!(
        r.blocks_rolled_back,
        tenero_wallet::wallet::RECENT_BLOCKS as u64
    );
    assert_eq!(
        alice.balance(&n2).unwrap().total,
        0,
        "nothing of the other chain is hers"
    );
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
    let rig = Rig::new("file", 2, 1);
    let mut node = rig.node();
    let mut alice = wallet(1);
    mine_n(&mut node, &alice.address(), 4);
    alice.sync(&node).unwrap();
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
    assert_eq!(
        Wallet::load(&path, b"wrong").err(),
        Some(FileError::WrongPassphraseOrCorrupt)
    );
    assert_eq!(
        Wallet::load(&path, b"").err(),
        Some(FileError::WrongPassphraseOrCorrupt)
    );
    // the file does not contain the seed or an address in the clear
    let bytes = std::fs::read(&path).unwrap();
    assert!(!bytes.windows(32).any(|w| w == alice.seed()));
    assert!(!bytes.windows(32).any(|w| w == alice.address().spend));
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
    // truncated and extended
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
    std::fs::write(&path, b"short").unwrap();
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
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[4..8].copy_from_slice(&8u32.to_le_bytes());
    bytes[8..12].copy_from_slice(&1_000_000u32.to_le_bytes()); // passes
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
    assert_ne!(first, second);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    assert!(
        !PathBuf::from(tmp).exists(),
        "no half-written file is left behind"
    );
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

#[test]
fn the_default_key_derivation_settings_are_not_the_test_ones() {
    const {
        assert!(KdfParams::DEFAULT.memory_kib >= 64 * 1024);
        assert!(KdfParams::DEFAULT.iterations >= 3);
        assert!(KdfParams::TEST_ONLY_WEAK.memory_kib < KdfParams::DEFAULT.memory_kib);
    }
    // a file saved with the default settings loads (slow on purpose: about a quarter of a second)
    let alice = wallet(1);
    let path = temp_file("e");
    alice
        .save(&path, b"pw", KdfParams::DEFAULT, &mut OsRng)
        .unwrap();
    assert!(Wallet::load(&path, b"pw").is_ok());
    std::fs::remove_file(&path).unwrap();
}

// keep the unused-import check honest: these traits are what a node implements for the wallet
#[allow(dead_code)]
fn _a_node_is_a_chain_and_a_submitter(n: &mut Node<'_>) {
    fn both<T: ChainView + Submitter>(_: &mut T) {}
    both(n);
}

// ------------------------------------------------------------------------------------------------
// holes the fault-injection sweep found in the first version of these tests
// ------------------------------------------------------------------------------------------------

/// A "node" that takes a transaction and loses it: the wallet believes it sent the payment, the chain never sees it.
struct Blackhole<'a, 'n>(&'a mut Node<'n>);

impl ChainView for Blackhole<'_, '_> {
    fn tip(&self) -> Result<(u64, [u8; 32]), String> {
        ChainView::tip(&*self.0)
    }
    fn block(&self, h: u64) -> Result<Option<tenero_wallet::ScanBlock>, String> {
        self.0.block(h)
    }
    fn output(&self, i: u64) -> Result<Option<tenero_store::StoredOutput>, String> {
        ChainView::output(&*self.0, i)
    }
    fn output_count(&self) -> Result<u64, String> {
        ChainView::output_count(&*self.0)
    }
    fn key_image_spent(&self, k: &[u8; 32]) -> Result<bool, String> {
        self.0.key_image_spent(k)
    }
    fn rules(&self) -> Result<tenero_wallet::Rules, String> {
        ChainView::rules(&*self.0)
    }
}

impl Submitter for Blackhole<'_, '_> {
    fn submit(&mut self, _tx: tenero_core::v2::Transaction) -> Result<(), String> {
        Ok(())
    }
}

#[test]
fn coins_promised_to_a_lost_payment_come_back_after_a_while() {
    let rig = Rig::new("expire", 2, 1);
    let mut node = rig.node();
    let (mut alice, bob, carol) = (wallet(1), wallet(2), wallet(3));
    // one coin of hers, among other people's (a ring needs something to hide among)
    mine(&mut node, &carol.address(), 0);
    mine(&mut node, &alice.address(), 0);
    mine(&mut node, &carol.address(), 0);
    alice.sync(&node).unwrap();
    // her only coin goes into a payment that is lost
    let amount = alice.owned()[0].amount - 50_000_000;
    alice
        .pay(
            &mut Blackhole(&mut node),
            &mut OsRng,
            &bob.address(),
            amount,
        )
        .unwrap();
    mine_n(&mut node, &carol.address(), 5);
    let b = alice.balance(&node).unwrap();
    assert_eq!((b.spendable, b.reserved), (0, b.total), "still promised");
    assert!(matches!(
        alice.build_payment(&node, &mut OsRng, &bob.address(), amount),
        Err(WalletError::NotEnough { spendable: 0, .. })
    ));
    // once the promise has run out, the coin can be used again
    mine_n(
        &mut node,
        &carol.address(),
        tenero_wallet::wallet::RESERVE_BLOCKS + 2,
    );
    alice.sync(&node).unwrap();
    // (building comes first: it must work out for itself that the promise has run out)
    assert!(alice
        .build_payment(&node, &mut OsRng, &bob.address(), amount)
        .is_ok());
    let b = alice.balance(&node).unwrap();
    assert_eq!((b.reserved, b.spendable), (0, b.total));
}

/// Three coins; the first is spent; the wallet must neither offer it again nor pick it for the next payment.
#[test]
fn a_spent_coin_is_never_picked_again_and_the_smallest_covering_coin_is_preferred() {
    let rig = Rig::new("select", 2, 1);
    let mut node = rig.node();
    let (mut alice, bob, carol) = (wallet(1), wallet(2), wallet(3));
    mine_n(&mut node, &alice.address(), 3);
    alice.sync(&node).unwrap();
    let coin = alice.owned()[0].amount;
    // pay nearly a whole coin: what comes back is a small change coin
    let first = alice
        .pay(&mut node, &mut OsRng, &bob.address(), coin - 50_000_000)
        .unwrap();
    mine(&mut node, &carol.address(), 0);
    alice.sync(&node).unwrap();
    let change = alice
        .owned()
        .iter()
        .filter(|o| !o.coinbase)
        .map(|o| (o.amount, o.key_image))
        .collect::<Vec<_>>();
    assert_eq!(change.len(), 1);
    // a small payment: the smallest coin that covers it is the change, not a whole coinbase
    let small = alice
        .build_payment(&node, &mut OsRng, &bob.address(), 1_000_000)
        .unwrap();
    assert_eq!(
        small.spends,
        vec![change[0].1],
        "the change coin, which is the smallest that covers it"
    );
    // a payment larger than the change but smaller than a coin: a whole coin, and not the one already spent
    let mid = alice
        .build_payment(&node, &mut OsRng, &bob.address(), coin / 2)
        .unwrap();
    assert_eq!(mid.spends.len(), 1);
    assert!(
        mid.spends.iter().all(|k| !first.spends.contains(k)),
        "the spent coin was picked again"
    );
    // more than any one coin: the largest are combined first, so two coins and not three
    let big = alice
        .build_payment(&node, &mut OsRng, &bob.address(), coin + coin / 2)
        .unwrap();
    assert_eq!(
        big.tx.prefix.inputs.len(),
        2,
        "two whole coins, not the change and two more"
    );
    assert!(big
        .spends
        .iter()
        .all(|k| !first.spends.contains(k) && *k != change[0].1));
}

/// A wallet's CHANGE output is spendable: it sits at the second place of the transaction as often as the first,
/// and its position in the chain must be right either way (a signature over the wrong ring member does not verify).
#[test]
fn change_is_spendable_wherever_it_sits_in_the_transaction() {
    use rand_chacha::ChaCha20Rng;
    use rand_core::SeedableRng;
    let rig = Rig::new("change", 2, 1);
    let mut node = rig.node();
    let carol = wallet(99);
    mine_n(&mut node, &carol.address(), 3);
    let bob = wallet(98);
    let mut positions = std::collections::BTreeSet::new();
    for k in 0..12u8 {
        let mut rng = ChaCha20Rng::seed_from_u64(1000 + u64::from(k));
        let mut alice = wallet(10 + k);
        mine(&mut node, &alice.address(), 0);
        mine(&mut node, &carol.address(), 0);
        alice.sync(&node).unwrap();
        // her only coin: pay, so that the change is the only thing left
        let a = alice.owned()[0].amount;
        alice
            .pay(&mut node, &mut rng, &bob.address(), a / 3)
            .unwrap();
        mine(&mut node, &carol.address(), 0);
        let height = node.tip().unwrap().0;
        let first = node
            .store()
            .block_index(height)
            .unwrap()
            .unwrap()
            .first_output_index;
        alice.sync(&node).unwrap();
        let change: Vec<_> = alice
            .owned()
            .iter()
            .filter(|o| !o.coinbase)
            .cloned()
            .collect();
        assert_eq!(change.len(), 1);
        // its place in the transaction (the coinbase has one output)
        let place = change[0].global_index - (first + 1);
        positions.insert(place);
        // spending it works
        let again = alice.pay(&mut node, &mut rng, &bob.address(), 1_000_000);
        assert!(again.is_ok(), "change at position {place}: {again:?}");
        mine(&mut node, &carol.address(), 0);
    }
    assert_eq!(
        positions.len(),
        2,
        "the change was seen in both places: {positions:?}"
    );
}

/// A node that answers a request for blocks with the wrong ones (a bug or a lie).
struct Shifted<'a, 'n>(&'a Node<'n>);

impl ChainView for Shifted<'_, '_> {
    fn tip(&self) -> Result<(u64, [u8; 32]), String> {
        ChainView::tip(self.0)
    }
    fn block(&self, h: u64) -> Result<Option<tenero_wallet::ScanBlock>, String> {
        self.0.block(h)
    }
    fn blocks(&self, from: u64, max: u64) -> Result<Vec<tenero_wallet::ScanBlock>, String> {
        self.0.blocks(from + 1, max)
    }
    fn output(&self, i: u64) -> Result<Option<tenero_store::StoredOutput>, String> {
        ChainView::output(self.0, i)
    }
    fn output_count(&self) -> Result<u64, String> {
        ChainView::output_count(self.0)
    }
    fn key_image_spent(&self, k: &[u8; 32]) -> Result<bool, String> {
        self.0.key_image_spent(k)
    }
    fn rules(&self) -> Result<tenero_wallet::Rules, String> {
        ChainView::rules(self.0)
    }
}

#[test]
fn a_node_that_sends_the_wrong_blocks_is_not_believed() {
    let rig = Rig::new("shifted", 2, 1);
    let mut node = rig.node();
    let mut alice = wallet(1);
    mine_n(&mut node, &alice.address(), 4);
    let r = alice.sync(&Shifted(&node));
    let e = r.unwrap_err();
    assert!(
        matches!(&e, WalletError::Chain(m) if m.contains("asked for block")),
        "{e:?}"
    );
    assert!(
        alice.owned().is_empty(),
        "nothing was taken from the wrong blocks"
    );
}

/// Counts the questions put to the chain, to show that giving up is cheap.
struct Counting<'a, 'b> {
    node: &'a Node<'b>,
    outputs_asked: std::cell::Cell<u64>,
}

impl ChainView for Counting<'_, '_> {
    fn tip(&self) -> Result<(u64, [u8; 32]), String> {
        ChainView::tip(self.node)
    }
    fn block(&self, h: u64) -> Result<Option<tenero_wallet::ScanBlock>, String> {
        ChainView::block(self.node, h)
    }
    fn blocks(&self, from: u64, max: u64) -> Result<Vec<tenero_wallet::ScanBlock>, String> {
        ChainView::blocks(self.node, from, max)
    }
    fn output(&self, i: u64) -> Result<Option<tenero_store::StoredOutput>, String> {
        self.outputs_asked.set(self.outputs_asked.get() + 1);
        ChainView::output(self.node, i)
    }
    fn output_count(&self) -> Result<u64, String> {
        ChainView::output_count(self.node)
    }
    fn key_image_spent(&self, k: &[u8; 32]) -> Result<bool, String> {
        ChainView::key_image_spent(self.node, k)
    }
    fn rules(&self) -> Result<tenero_wallet::Rules, String> {
        ChainView::rules(self.node)
    }
}

#[test]
fn a_chain_with_too_few_matured_outputs_is_refused_at_once_not_after_thousands_of_questions() {
    // 20 outputs on the chain (more than a ring of 16) but only about 10 of them matured: the owner's young chain. Before the
    // check, the wallet asked the node for outputs one by one thousands of times (and then walked the whole chain) before saying so.
    let rig = Rig::new("young", 16, 10);
    let mut node = rig.node();
    let (mut alice, bob) = (wallet(1), wallet(2));
    mine_n(&mut node, &alice.address(), 20);
    alice.sync(&node).unwrap();
    assert!(
        alice.balance(&node).unwrap().spendable > 0,
        "her own old coins are spendable"
    );
    let view = Counting {
        node: &node,
        outputs_asked: std::cell::Cell::new(0),
    };
    let r = alice.build_payment(&view, &mut OsRng, &bob.address(), 1_000);
    assert!(matches!(r, Err(WalletError::NotEnoughDecoys)), "{r:?}");
    assert!(
        view.outputs_asked.get() < 40,
        "{} questions to give up",
        view.outputs_asked.get()
    );
    // and once enough have matured it works, with the same wallet
    mine_n(&mut node, &alice.address(), 15);
    alice.sync(&node).unwrap();
    assert!(alice
        .build_payment(&node, &mut OsRng, &bob.address(), 1_000)
        .is_ok());
}

#[test]
fn a_spent_output_is_asked_about_once() {
    let rig = Rig::new("spentcache", 2, 1);
    let mut node = rig.node();
    let (mut alice, bob) = (wallet(1), wallet(2));
    mine_n(&mut node, &alice.address(), 6);
    alice.sync(&node).unwrap();
    alice
        .pay(&mut node, &mut OsRng, &bob.address(), 1_000)
        .unwrap();
    mine(&mut node, &alice.address(), 0);
    alice.sync(&node).unwrap();
    struct Spent<'a, 'b>(Counting<'a, 'b>, std::cell::Cell<u64>);
    impl ChainView for Spent<'_, '_> {
        fn tip(&self) -> Result<(u64, [u8; 32]), String> {
            self.0.tip()
        }
        fn block(&self, h: u64) -> Result<Option<tenero_wallet::ScanBlock>, String> {
            self.0.block(h)
        }
        fn blocks(&self, f: u64, m: u64) -> Result<Vec<tenero_wallet::ScanBlock>, String> {
            self.0.blocks(f, m)
        }
        fn output(&self, i: u64) -> Result<Option<tenero_store::StoredOutput>, String> {
            self.0.output(i)
        }
        fn output_count(&self) -> Result<u64, String> {
            self.0.output_count()
        }
        fn key_image_spent(&self, k: &[u8; 32]) -> Result<bool, String> {
            self.1.set(self.1.get() + 1);
            self.0.key_image_spent(k)
        }
        fn rules(&self) -> Result<tenero_wallet::Rules, String> {
            self.0.rules()
        }
    }
    let view = Spent(
        Counting {
            node: &node,
            outputs_asked: std::cell::Cell::new(0),
        },
        std::cell::Cell::new(0),
    );
    let first = alice.balance(&view).unwrap();
    let asked_first = view.1.get();
    view.1.set(0);
    let second = alice.balance(&view).unwrap();
    assert_eq!(first, second);
    // the second time the spent coins are not asked about again (and the unspent ones are, because they may be spent now)
    assert!(
        view.1.get() < asked_first,
        "{} then {}",
        asked_first,
        view.1.get()
    );
}
