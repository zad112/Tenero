//! The fee a wallet shows before a payment is made ([`Wallet::quote_batch`]) is worked out without the proofs, which take
//! about a second a coin. It must be exactly what the payment, once made, is: the same coins, the same size, the same fee.
//! (Reported 2026-10-08: a payment of 1,000 coins from mining rewards took about 40 s to show its fees, because the
//! quote built the whole payment and threw it away, and as long again to review.) **A test chain, not a real one.**

use std::path::PathBuf;

use rand_core::OsRng;
use tenero_chain::Sha256Pow;
use tenero_core::v2::ids::PowKind;
use tenero_core::v3::Wire;
use tenero_net::sim::{test_chain_params, LABEL};
use tenero_node::{Node, NodeConfig};
use tenero_store::Store;
use tenero_wallet::testing::{test_block_to, READY};
use tenero_wallet::{Address, FeeLevel, Network, Wallet, WalletError};

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
            std::env::temp_dir().join(format!("tenero-quote-{}-{tag}.redb", std::process::id()));
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
fn the_quote_is_exactly_the_payment_it_quotes_without_making_its_proofs() {
    let rig = Rig::new("exact");
    let mut node = rig.node();
    let mut miner = Wallet::from_seed(&[1; 32], Network::Test, 0);
    // a miner's wallet: many block rewards of 20 coins
    for _ in 0..READY + 12 {
        mine(&mut node, &miner.address());
    }
    miner.sync(&node).unwrap();
    let to = |n: u8| Wallet::from_seed(&[n; 32], Network::Test, 0).address();
    // one recipient paid from many coins, and twenty recipients (more than one transaction holds)
    let many: Vec<(Address, u64)> = (0..20).map(|i| (to(10 + i), 3 * COIN)).collect();
    for (dests, level) in [
        (vec![(to(2), 210 * COIN)], FeeLevel::Low),
        (vec![(to(3), 5 * COIN)], FeeLevel::High),
        (many, FeeLevel::Normal),
    ] {
        let quotes = miner.quote_batch(&node, &dests, level).unwrap();
        let plan = miner.build_batch(&node, &mut OsRng, &dests, level).unwrap();
        assert_eq!(quotes.len(), plan.txs.len());
        for (q, b) in quotes.iter().zip(&plan.txs) {
            assert_eq!(q.inputs, b.spends.len());
            assert_eq!(q.inputs, b.tx.prefix.inputs.len());
            assert_eq!(q.outputs, b.tx.prefix.outputs.len());
            assert_eq!(
                q.size,
                b.tx.to_bytes().unwrap().len() as u64,
                "the exact size"
            );
            assert_eq!(q.fee, b.fee);
        }
    }
    let one = miner
        .quote_batch(&node, &[(to(2), 210 * COIN)], FeeLevel::Low)
        .unwrap();
    assert!(
        one[0].inputs > 10,
        "a payment of many rewards: {}",
        one[0].inputs
    );
    // what cannot be paid is refused as the build refuses it
    assert!(matches!(
        miner.quote_batch(&node, &[(to(2), 1_000_000 * COIN)], FeeLevel::Low),
        Err(WalletError::NotEnough { .. })
    ));
    // and a view-only wallet gets no quote
    let mut view = Wallet::from_view_key(
        &miner.view_key(tenero_wallet::ViewTier::ViewAll).unwrap(),
        Network::Test,
    )
    .unwrap();
    view.sync(&node).unwrap();
    assert_eq!(
        view.quote_batch(&node, &[(to(2), COIN)], FeeLevel::Low),
        Err(WalletError::ViewOnly)
    );
}

/// On a chain so young that no output has entered the curve tree yet (a coinbase output enters 60 blocks after its block),
/// the tree has no layers, and a wallet holds nothing it can spend. Asking what a payment would cost, building one,
/// combining coins or sweeping them must say so. Combining and sweeping worked out a transaction's size before looking for
/// coins, which reached the FCMP++ proof-size arithmetic for zero layers: it underflows (a panic in a debug build, a
/// meaningless size in a release build).
#[test]
fn a_chain_with_an_empty_tree_quotes_and_pays_nothing_instead_of_failing() {
    let rig = Rig::new("emptytree");
    let mut node = rig.node();
    let mut miner = Wallet::from_seed(&[1; 32], Network::Test, 0);
    for _ in 0..10 {
        mine(&mut node, &miner.address());
    }
    miner.sync(&node).unwrap();
    assert_eq!(
        tenero_wallet::ChainView::rules(&node).unwrap().tree_layers,
        0
    );
    assert_eq!(
        tenero_wallet::wallet::transaction_size(1, 2, 0),
        tenero_wallet::wallet::transaction_size(1, 2, 1)
    );
    let to = Wallet::from_seed(&[2; 32], Network::Test, 0).address();
    assert!(matches!(
        miner.quote_batch(&node, &[(to, COIN)], FeeLevel::Low),
        Err(WalletError::NotEnough { spendable: 0, .. })
    ));
    assert!(matches!(
        miner.build_batch(&node, &mut OsRng, &[(to, COIN)], FeeLevel::Low),
        Err(WalletError::NotEnough { spendable: 0, .. })
    ));
    // combining and sweeping have nothing to combine
    assert!(miner
        .build_combine(&node, &mut OsRng, 2, FeeLevel::Low)
        .is_err());
    assert!(miner
        .build_sweep(&node, &mut OsRng, None, FeeLevel::Low)
        .is_err());
}
