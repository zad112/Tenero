//! A node that is syncing does not judge new transactions, so it never blames an honest peer for one (reported against
//! v0.2.0-beta.4, 2026-10-08: a fresh node far behind the network fetched live transactions, found them "invalid" against
//! its old chain, and banned every healthy peer, seeds included, then said "in sync" with no peers). While a sync runs,
//! an announced transaction is not fetched and one already asked for is dropped unjudged; after it, they flow again.

use tenero_core::u256::U256;
use tenero_core::v3::{Input, Output, Prunable, Transaction, TxPrefix, VERSION};
use tenero_net::sim::{Sim, SimConfig, SimRig};
use tenero_net::{EngineConfig, Hello, Message, PROTOCOL_VERSION};

const T0: u64 = 1_700_000_000;
const SEC: u64 = 1000;

fn hello(rig: &SimRig, height: u64, work: U256) -> Message {
    Message::Hello(Hello {
        version: PROTOCOL_VERSION,
        chain_id: rig.store.chain_id(),
        tip_height: height,
        cumulative_work: work.to_be_bytes(),
        tip_id: [9; 32],
        pruned_below: 0,
        nonce: 0,
    })
}

fn config() -> EngineConfig {
    EngineConfig {
        ping_after_ms: 10_000_000,
        max_timeouts: 1000,
        ..EngineConfig::default()
    }
}

/// A transaction this node would call invalid (its points and proof are made up): what a node far behind its peers
/// makes of a real new one.
fn a_tx(n: u8) -> Transaction {
    let out = |k: u8| Output {
        onetime_address: [k; 32],
        amount_commitment: [k; 32],
        amount_enc: [k; 8],
        view_tag: [k; 3],
        anchor_enc: [k; 16],
    };
    Transaction {
        prefix: TxPrefix {
            version: VERSION,
            inputs: vec![Input { key_image: [n; 32] }],
            outputs: vec![out(1), out(2)],
            ephemeral_pubkeys: vec![[9; 32]],
            fee: 1,
            encrypted_payment_id: [0; 8],
        },
        prunable: Prunable {
            reference_height: 900,
            proof_data: vec![7; 20],
        },
    }
}

fn count(sim: &Sim<'_>, h: usize, kind: &str) -> usize {
    sim.hostiles[h]
        .inbox
        .iter()
        .filter(|m| m.kind() == kind)
        .count()
}

fn score(sim: &Sim<'_>, h: usize) -> Option<u32> {
    sim.engines[0].peer_score(sim.hostiles[h].peer_at_node()?)
}

#[test]
fn a_syncing_node_neither_fetches_nor_judges_new_transactions_and_blames_nobody() {
    let rigs = SimRig::rigs("sync-txs", 1);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), config());
    // a peer level with the node announces a transaction; the node asks for it
    let relay = sim.add_hostile(0, "relay");
    sim.hostile_send(relay, hello(&rigs[0], 0, U256::ZERO));
    let early = a_tx(1);
    let early_id = tenero_core::v3::ids::tx_id(&early).unwrap();
    sim.hostile_send(
        relay,
        Message::NewTx {
            ids: vec![early_id],
        },
    );
    sim.run_for(SEC);
    assert_eq!(count(&sim, relay, "get_txs"), 1);

    // a peer far ahead arrives: the node starts to sync
    let ahead = sim.add_hostile(0, "ahead");
    sim.hostile_send(ahead, hello(&rigs[0], 1500, U256::pow2(200).unwrap()));
    sim.run_for(SEC);
    assert!(sim.engines[0].is_syncing());

    // the answer to the request made before the sync arrives during it: dropped, not judged, nobody blamed
    sim.hostile_send(relay, Message::Txs { txs: vec![early] });
    sim.run_for(SEC);
    assert_eq!(score(&sim, relay), Some(0), "an answer the node asked for");
    // new announcements during the sync are not fetched, from either peer, however many
    for n in 2..12 {
        let id = tenero_core::v3::ids::tx_id(&a_tx(n)).unwrap();
        sim.hostile_send(relay, Message::NewTx { ids: vec![id] });
        sim.hostile_send(ahead, Message::NewTx { ids: vec![id] });
    }
    sim.run_for(SEC);
    assert!(sim.engines[0].is_syncing());
    assert_eq!(count(&sim, relay, "get_txs"), 1, "nothing more was asked");
    assert_eq!(count(&sim, ahead, "get_txs"), 0);
    assert_eq!((score(&sim, relay), score(&sim, ahead)), (Some(0), Some(0)));
    assert_eq!(sim.engines[0].stats.bans, 0);
}

#[test]
fn once_the_sync_is_over_transactions_are_fetched_again() {
    let rigs = SimRig::rigs("sync-txs-after", 1);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), config());
    let ahead = sim.add_hostile(0, "ahead");
    sim.hostile_send(ahead, hello(&rigs[0], 1500, U256::pow2(200).unwrap()));
    sim.run_for(SEC);
    assert!(sim.engines[0].is_syncing());
    // the peer never answers: the sync times out and ends
    sim.run_for(40 * SEC);
    assert!(!sim.engines[0].is_syncing());
    let relay = sim.add_hostile(0, "relay");
    sim.hostile_send(relay, hello(&rigs[0], 0, U256::ZERO));
    let id = tenero_core::v3::ids::tx_id(&a_tx(1)).unwrap();
    sim.hostile_send(relay, Message::NewTx { ids: vec![id] });
    sim.run_for(SEC);
    assert_eq!(count(&sim, relay, "get_txs"), 1);
}
