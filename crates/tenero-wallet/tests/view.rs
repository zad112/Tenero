//! View-only wallets (Carrot 5.4): a view-all wallet sees what its full wallet sees, the balance included, and a
//! view-received wallet sees what comes in; neither can spend or sign. On the SHA-256 test chain with a node that checks
//! every proof. **A test chain, not a real one.**

use std::path::PathBuf;

use rand_core::OsRng;
use tenero_chain::Sha256Pow;
use tenero_core::v2::ids::PowKind;
use tenero_net::sim::{test_chain_params, LABEL};
use tenero_node::{Node, NodeConfig};
use tenero_store::Store;
use tenero_wallet::testing::{test_block_to, READY};
use tenero_wallet::{Address, KdfParams, Network, Purse, ViewTier, Wallet, WalletError};

const T0: u64 = 1_700_000_000;

struct Rig {
    path: PathBuf,
    store: Store,
    params: tenero_chain::ChainParams,
}

impl Rig {
    fn new(tag: &str) -> Rig {
        let path =
            std::env::temp_dir().join(format!("tenero-view-{}-{tag}.redb", std::process::id()));
        remove(&path);
        Rig {
            store: Store::open(&path, LABEL, PowKind::Sha256).unwrap(),
            path,
            params: test_chain_params(),
        }
    }
    fn node(&self) -> Node<'_> {
        Node::new(&self.store, &self.params, &Sha256Pow, NodeConfig::default()).unwrap()
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

fn mine(node: &mut Node<'_>, to: &Address) {
    let height = node.tip().unwrap().0 + 1;
    let ts = T0 + 60 * height;
    let block = test_block_to(node, to, ts);
    node.submit_block(&block, ts + 10).unwrap();
}

#[test]
fn a_view_all_wallet_sees_the_balance_and_a_view_received_one_what_came_in_and_neither_spends() {
    let rig = Rig::new("tiers");
    let mut node = rig.node();
    let mut alice = Wallet::from_seed(&[1; 32], Network::Test, 0);
    let bob = Wallet::from_seed(&[2; 32], Network::Test, 0);
    for _ in 0..READY {
        mine(&mut node, &alice.address());
    }
    alice.sync(&node).unwrap();
    // Alice pays Bob; a block takes it in, with its change back to her
    let built = alice
        .pay(&mut node, &mut OsRng, &bob.address(), 1_500_000_000)
        .unwrap();
    mine(&mut node, &alice.address());
    alice.sync(&node).unwrap();
    let full = alice.balance(&node).unwrap();

    // the two view keys of her wallet, made into wallets that scan the same chain
    let all_key = alice.view_key(ViewTier::ViewAll).unwrap();
    let received_key = alice.view_key(ViewTier::ViewReceived).unwrap();
    assert!(all_key.starts_with("TENview1") && received_key.starts_with("TENview1"));
    assert_eq!(
        alice.view_key(ViewTier::Full),
        None,
        "the seed is not a view key"
    );
    let mut all = Wallet::from_view_key(&all_key, Network::Test).unwrap();
    let mut received = Wallet::from_view_key(&received_key, Network::Test).unwrap();
    assert_eq!(
        (all.tier(), received.tier()),
        (ViewTier::ViewAll, ViewTier::ViewReceived)
    );
    assert_eq!(all.address(), alice.address());
    assert_eq!(received.address(), alice.address());
    assert_eq!(all.subaddress(4), alice.subaddress(4));
    assert!(all.seed().is_none() && received.seed().is_none());
    all.sync(&node).unwrap();
    received.sync(&node).unwrap();

    // view-all: the same outputs (the change included), the same key images, the same balance
    assert_eq!(all.owned(), alice.owned());
    assert_eq!(all.balance(&node).unwrap(), full);
    assert!(all.owned().iter().any(|o| o.internal), "it sees the change");
    // view-received: what came in (every reward, not the change), and no claim about what is spent
    let b = received.balance(&node).unwrap();
    let came_in: u64 = alice
        .owned()
        .iter()
        .filter(|o| !o.internal)
        .map(|o| o.amount)
        .sum();
    assert_eq!(b.total, came_in, "every reward, the last with the fee");
    assert!(came_in > 65 * 2_000_000_000 && built.fee > 0);
    assert_eq!(b.spendable, 0);
    assert!(received.owned().iter().all(|o| !o.internal));
    assert!(
        full.total < b.total,
        "the balance is less than what came in: some was spent"
    );

    // neither can spend or sign
    for w in [&mut all, &mut received] {
        let e = w
            .build_payment(&node, &mut OsRng, &bob.address(), 1_000)
            .unwrap_err();
        assert_eq!(e, WalletError::ViewOnly);
        assert!(e.to_string().contains("view-only"));
        assert!(w
            .sign_message(tenero_carrot::account::AddressIndex::MAIN, b"x", &mut OsRng)
            .is_none());
    }
    // but either can prove what it received (a payment proof: no signature) and the proof checks
    let reward = all.owned()[0].global_index;
    let p = received
        .prove_received(&node, reward, b"", &mut OsRng)
        .unwrap();
    assert!(p.signature.is_none());
    assert!(tenero_wallet::proofs::check_payment(&node, &p, b"").is_ok());

    // a view-all wallet gives a view-received key (the same as the full wallet's), a view-received one only its own
    assert_eq!(
        all.view_key(ViewTier::ViewReceived),
        Some(received_key.clone())
    );
    assert_eq!(all.view_key(ViewTier::ViewAll), Some(all_key.clone()));
    assert_eq!(received.view_key(ViewTier::ViewAll), None);
    assert_eq!(
        received.view_key(ViewTier::ViewReceived),
        Some(received_key)
    );
}

#[test]
fn a_view_key_is_refused_on_another_network_or_with_a_character_changed() {
    let w = Wallet::from_seed(&[3; 32], Network::Gamma, 77);
    let key = w.view_key(ViewTier::ViewAll).unwrap();
    let v = Wallet::from_view_key(&key, Network::Gamma).unwrap();
    assert_eq!(
        v.birth_height(),
        77,
        "the key carries where to start scanning"
    );
    let e = Wallet::from_view_key(&key, Network::Test).err().unwrap();
    assert!(e.contains("test"), "{e}");
    for i in [10, key.len() / 2, key.len() - 1] {
        let mut bad: Vec<char> = key.chars().collect();
        bad[i] = if bad[i] == 'x' { 'y' } else { 'x' };
        let bad: String = bad.into_iter().collect();
        assert!(Wallet::from_view_key(&bad, Network::Gamma).is_err(), "{i}");
    }
    assert!(Wallet::from_view_key("TENview1", Network::Gamma).is_err());
    assert!(Wallet::from_view_key(w.address().to_text().as_str(), Network::Gamma).is_err());
}

#[test]
fn a_view_only_wallet_survives_its_file_and_is_not_taken_for_a_purse() {
    let dir = std::env::temp_dir().join(format!("tenero-view-file-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let alice = Wallet::from_seed(&[4; 32], Network::Test, 5);
    for tier in [ViewTier::ViewAll, ViewTier::ViewReceived] {
        let v = Wallet::from_view_key(&alice.view_key(tier).unwrap(), Network::Test).unwrap();
        let path = dir.join(format!("{tier:?}.wallet"));
        v.save(&path, b"passphrase", KdfParams::TEST_ONLY_WEAK, &mut OsRng)
            .unwrap();
        let back = Wallet::load(&path, b"passphrase").unwrap();
        assert_eq!((back.tier(), back.address()), (tier, alice.address()));
        assert!(back.seed().is_none());
        assert_eq!(back.view_key(tier), v.view_key(tier));
        // the wallet app opens purses: a view-only file is refused with the reason, never taken for one
        let e = Purse::load(&path, b"passphrase").err().unwrap();
        assert!(e.to_string().contains("view-only"), "{e}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
