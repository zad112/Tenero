//! Compact blocks (`docs/WIRE_PROTOCOL.md` 3, plan F14): a new block travels as its header, coinbase and transaction ids;
//! a node fills it from its mempool and fetches only what it lacks, by index; and a block bigger than a reply is synced the
//! same way, its transactions in pieces. Real engines on the simulated network.

use tenero_core::hash::sha256;
use tenero_core::v3::ids::tx_id;
use tenero_core::v3::rules;
use tenero_core::v3::*;
use tenero_net::sim::{Sim, SimConfig, SimRig};
use tenero_net::{EngineConfig, Message};
use tenero_tree::hash_to_point;

const T0: u64 = 1_700_000_000;
const SEC: u64 = 1000;

/// A transaction valid at `node`'s tip (its proofs are junk: the simulator does not check proofs).
fn make_tx(sim: &Sim<'_>, node: usize, image: u64) -> Transaction {
    let next = sim.engines[node].node().next_block().unwrap();
    let pt = |tag: &[u8], n: u8| hash_to_point(sha256(&[tag, &image.to_le_bytes(), &[n]]));
    let mut outputs: Vec<Output> = (1..=2)
        .map(|n| Output {
            onetime_address: pt(b"addr", n),
            amount_commitment: pt(b"commit", n),
            amount_enc: [n; 8],
            view_tag: [n; 3],
            anchor_enc: [n; 16],
        })
        .collect();
    outputs.sort_by_key(|o| o.onetime_address);
    let mut t = Transaction {
        prefix: TxPrefix {
            version: VERSION,
            inputs: vec![Input {
                key_image: pt(b"key image", 0),
            }],
            outputs,
            ephemeral_pubkeys: vec![pt(b"eph", 0)],
            fee: 0,
            encrypted_payment_id: [0; 8],
        },
        prunable: Prunable {
            reference_height: next.height - 1,
            proof_data: vec![7; 2000],
        },
    };
    let size = t.to_bytes().unwrap().len() as u64;
    t.prefix.fee = rules::min_fee(size, next.reward, next.median).unwrap();
    t
}

/// Mines on node 0 until block 61, so that the curve tree holds outputs and transactions can be made.
fn ready(sim: &mut Sim<'_>) {
    while sim.tip(0).0 < 61 {
        sim.mine(0, None);
        assert!(sim.run_until(60 * SEC, |s| s.all_agree()));
        sim.run_for(61 * SEC);
    }
}

fn sent(sim: &Sim<'_>, kind: &str) -> u64 {
    sim.sent_by_kind.get(kind).copied().unwrap_or(0)
}

#[test]
fn a_new_block_whose_transactions_every_node_has_is_relayed_without_sending_one_of_them() {
    let rigs = SimRig::rigs("compact-hit", 4);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), EngineConfig::default());
    sim.connect_all();
    ready(&mut sim);
    let txs: Vec<Transaction> = (0..5).map(|i| make_tx(&sim, 0, 100 + i)).collect();
    let ids: Vec<[u8; 32]> = txs.iter().map(|t| tx_id(t).unwrap()).collect();
    for t in txs {
        sim.submit_tx(2, t);
    }
    assert!(
        sim.run_until(60 * SEC, |s| (0..4).all(|n| ids.iter().all(|id| s.engines
            [n]
            .node()
            .pool()
            .contains(id))))
    );
    let before = sent(&sim, "block_txs");
    let b = sim.mine(1, None);
    assert_eq!(b.transactions.len(), 5);
    assert!(sim.run_until(60 * SEC, |s| s.all_agree()));
    assert_eq!(
        sent(&sim, "block_txs"),
        before,
        "every transaction came from the mempools"
    );
    for n in [0, 2, 3] {
        assert_eq!(sim.engines[n].stats.compact_fetched_txs, 0, "node {n}");
        assert!(sim.engines[n].node().pool().is_empty());
    }
}

#[test]
fn a_node_that_lacks_some_transactions_fetches_only_those_by_index() {
    let rigs = SimRig::rigs("compact-miss", 2);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), EngineConfig::default());
    sim.connect(0, 1);
    ready(&mut sim);
    // two transactions both nodes have, then a split, and three only node 0 has
    let shared: Vec<Transaction> = (0..2).map(|i| make_tx(&sim, 0, 200 + i)).collect();
    for t in shared {
        sim.submit_tx(0, t);
    }
    sim.run_for(30 * SEC);
    assert_eq!(sim.engines[1].node().pool().len(), 2);
    sim.partition(&[vec![0], vec![1]]);
    sim.run_for(2 * SEC);
    let private: Vec<Transaction> = (0..3).map(|i| make_tx(&sim, 0, 300 + i)).collect();
    for t in private {
        sim.submit_tx(0, t);
    }
    sim.run_for(5 * SEC);
    assert_eq!(sim.engines[1].node().pool().len(), 2);
    sim.heal();
    sim.run_for(5 * SEC);
    let b = sim.mine(0, None);
    assert_eq!(b.transactions.len(), 5);
    assert!(sim.run_until(60 * SEC, |s| s.all_agree()));
    assert_eq!(
        sim.engines[1].stats.compact_fetched_txs, 3,
        "the three it lacked, and only those"
    );
    assert!(sim.engines[1].stats.compact_complete >= 1);
    // the request named their indexes
    assert!(sim.engines[0].stats.received.get("get_block_txs").copied() >= Some(1));
    assert_eq!(sim.engines[1].stats.bans, 0);
}

#[test]
fn blocks_bigger_than_a_reply_are_synced_compact_with_their_transactions_in_pieces() {
    let rigs = SimRig::rigs("compact-sync", 2);
    // node 0 answers with replies of at most 3,000 bytes: every block with a transaction is bigger than that
    let small = EngineConfig {
        blocks_reply_bytes: 3_000,
        ..EngineConfig::default()
    };
    let mut sim = Sim::with_configs(
        &rigs,
        T0,
        SimConfig::default(),
        vec![small, EngineConfig::default()],
    );
    sim.connect(0, 1);
    ready(&mut sim);
    // node 1 leaves; node 0 goes on alone with blocks of three transactions each
    sim.partition(&[vec![0], vec![1]]);
    sim.run_for(2 * SEC);
    for k in 0..5u64 {
        for i in 0..3 {
            let t = make_tx(&sim, 0, 1000 + k * 10 + i);
            sim.submit_tx(0, t);
        }
        let b = sim.mine(0, None);
        assert_eq!(b.transactions.len(), 3);
        sim.run_for(61 * SEC);
    }
    let target = sim.tip(0);
    sim.heal();
    assert!(
        sim.run_until(300 * SEC, |s| s.tip(1) == target),
        "node 1 reached {:?}, not {:?}",
        sim.tip(1),
        target
    );
    // each of those blocks came compact, and its transactions in pieces (one reply each: 3,000 bytes holds one)
    assert!(sim.engines[0].stats.sent.get("compact").copied() >= Some(5));
    assert!(sim.engines[0].stats.sent.get("block_txs").copied() >= Some(15));
    assert_eq!(sim.engines[1].stats.compact_fetched_txs, 15);
    assert_eq!(sim.engines[1].stats.bans, 0);
}

#[test]
fn a_compact_block_whose_ids_are_not_its_header_s_bans_the_peer() {
    let rigs = SimRig::rigs("compact-lie", 2);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), EngineConfig::default());
    sim.mine_chain(1, 2);
    let one = sim.engines[1]
        .node()
        .store()
        .get_block(1)
        .unwrap()
        .unwrap()
        .into_full()
        .unwrap();
    let id = tenero_core::v3::ids::block_id(&one.header, tenero_core::v2::ids::PowKind::Sha256);
    let h = sim.add_hostile(0, "liar");
    let hello = tenero_net::Hello {
        version: tenero_net::PROTOCOL_VERSION,
        chain_id: rigs[0].store.chain_id(),
        tip_height: 2,
        cumulative_work: tenero_core::u256::U256::pow2(200).unwrap().to_be_bytes(),
        tip_id: id,
        pruned_below: 0,
        nonce: 0,
    };
    sim.hostile_send(h, Message::Hello(hello));
    sim.run_for(SEC);
    sim.hostile_send(
        h,
        Message::NewBlock {
            id,
            height: 1,
            cumulative_work: tenero_core::u256::U256::pow2(200).unwrap().to_be_bytes(),
        },
    );
    sim.run_for(SEC);
    assert!(sim.hostiles[h]
        .inbox
        .iter()
        .any(|m| matches!(m, Message::GetCompact { id: x } if *x == id)));
    // the header's, with an id that is not in its root
    sim.hostile_send(
        h,
        Message::Compact(Box::new(tenero_net::CompactBlock {
            header: one.header.clone(),
            coinbase: one.coinbase.clone(),
            tx_ids: vec![[7; 32]],
        })),
    );
    sim.run_for(SEC);
    assert!(sim.hostiles[h].disconnected, "100 points: banned");
    assert_eq!(sim.tip(0).0, 0);
}
