//! Payments out that the wallet did not record itself: a view-all wallet, or a wallet restored from its words, finds the
//! transactions that spend its coins while it scans, and lists them as sent (what left it and the fee; not whom it paid,
//! which only the sender's own record knows). A wallet that sent and recorded a payment lists it once, with the recipient.
//! A view-received wallet sees no spending at all. **A test chain, not a real one.**

use std::path::PathBuf;

use rand_core::OsRng;
use tenero_chain::Sha256Pow;
use tenero_core::v2::ids::PowKind;
use tenero_net::sim::{test_chain_params, LABEL};
use tenero_node::{Node, NodeConfig};
use tenero_store::Store;
use tenero_wallet::testing::{test_block_to, READY};
use tenero_wallet::{Address, EntryKind, FeeLevel, KdfParams, Network, Purse, ViewTier, Wallet};

const T0: u64 = 1_700_000_000;
const COIN: u64 = 100_000_000;

struct Rig {
    path: PathBuf,
    store: Store,
    params: tenero_chain::ChainParams,
}

impl Rig {
    fn new(tag: &str) -> Rig {
        let path =
            std::env::temp_dir().join(format!("tenero-outgoing-{}-{tag}.redb", std::process::id()));
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

/// (kind, amount) of every entry that is not a block reward or a coin received, oldest first.
fn outs(p: &Purse, node: &Node<'_>) -> Vec<(EntryKind, u64)> {
    let mut v: Vec<(u64, EntryKind, u64)> = p
        .history(node)
        .unwrap()
        .into_iter()
        .filter(|e| !matches!(e.kind, EntryKind::Mined | EntryKind::Received))
        .map(|e| (e.height, e.kind, e.amount))
        .collect();
    v.sort_by_key(|(h, _, _)| *h);
    v.into_iter().map(|(_, k, a)| (k, a)).collect()
}

#[test]
fn payments_out_are_found_on_the_chain_by_a_wallet_that_did_not_record_them() {
    let rig = Rig::new("found");
    let mut node = rig.node();
    let mut alice = Purse::from_seed(&[1; 32], Network::Test, 0);
    let bob = Wallet::from_seed(&[2; 32], Network::Test, 0).address();
    let alice_addr = alice.accounts()[0].address();
    for _ in 0..READY + 4 {
        mine(&mut node, &alice_addr);
    }
    alice.sync(&node).unwrap();
    // a payment to bob (recorded by alice's wallet), then her own coins combined (recorded too, as a payment to herself)
    let paid = alice
        .pay(
            0,
            &mut node,
            &mut OsRng,
            &bob,
            15 * COIN,
            FeeLevel::Normal,
            T0,
        )
        .unwrap();
    mine(&mut node, &alice_addr);
    alice.sync(&node).unwrap();
    for _ in 0..10 {
        mine(&mut node, &alice_addr);
    }
    alice.sync(&node).unwrap();
    let combine = alice
        .build_combine(0, &node, &mut OsRng, 3, FeeLevel::Low)
        .unwrap();
    alice
        .send_own(
            0,
            &mut node,
            std::slice::from_ref(&combine),
            FeeLevel::Low,
            "",
            T0,
        )
        .unwrap();
    mine(&mut node, &alice_addr);
    alice.sync(&node).unwrap();

    // the wallet that sent them lists each once, as recorded (with the recipient), never also as found
    let own = outs(&alice, &node);
    assert!(
        own.iter().all(|(k, _)| matches!(k, EntryKind::Sent { .. })),
        "{own:?}"
    );
    assert!(own
        .iter()
        .any(|(k, a)| matches!(k, EntryKind::Sent { to, .. } if *to == bob) && *a == 15 * COIN));

    // its words on a new computer, and its view-all key: both find the two transactions, without whom they paid
    let mut restored = Purse::from_seed(alice.master_seed().unwrap(), Network::Test, 0);
    restored.sync(&node).unwrap();
    let key = alice.accounts()[0]
        .wallet()
        .view_key(ViewTier::ViewAll)
        .unwrap();
    let mut view_all = Purse::from_view_key(&key, Network::Test).unwrap();
    view_all.sync(&node).unwrap();
    for p in [&restored, &view_all] {
        let found = outs(p, &node);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(
            found[0],
            (
                EntryKind::SentUnrecorded {
                    fee: paid.fee,
                    to_self: false
                },
                15 * COIN
            ),
            "exactly what left it, the fee apart"
        );
        assert!(matches!(
            found[1],
            (EntryKind::SentUnrecorded { fee, to_self: true }, moved)
                if fee == combine.fee && moved > 0
        ));
    }
    // the wallet's own record of the transactions agrees: what it spent, less what came back, less the fee
    let w = view_all.accounts()[0].wallet();
    assert_eq!(w.outgoing().len(), 2);
    let o = &w.outgoing()[0];
    let sorted = |v: &[[u8; 32]]| {
        let mut v = v.to_vec();
        v.sort();
        v
    };
    assert_eq!(sorted(&o.spends), sorted(&paid.spends));
    assert_eq!(o.spent - o.returned - o.fee, 15 * COIN);

    // a view-received wallet sees none of it: it watches what comes in only
    let key = alice.accounts()[0]
        .wallet()
        .view_key(ViewTier::ViewReceived)
        .unwrap();
    let mut received = Purse::from_view_key(&key, Network::Test).unwrap();
    received.sync(&node).unwrap();
    assert!(outs(&received, &node).is_empty());
    assert!(received.accounts()[0].wallet().outgoing().is_empty());

    // and what was found survives the wallet file
    let dir = std::env::temp_dir().join(format!("tenero-outgoing-file-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("view.twl");
    view_all
        .save(&path, b"pw", KdfParams::TEST_ONLY_WEAK, &mut OsRng)
        .unwrap();
    let back = Purse::load(&path, b"pw").unwrap();
    assert_eq!(
        back.accounts()[0].wallet().outgoing(),
        view_all.accounts()[0].wallet().outgoing()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
