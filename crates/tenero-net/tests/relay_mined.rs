//! A peer a little behind relays transactions this node has already seen confirmed, or one that lost to another spend
//! of the same key image. From its chain they may be valid: they are neither fetched again nor held against it. Before
//! this, each cost the peer 20 points (a ban at 100), so a busy network banned honest peers for being a block late.

use tenero_core::hash::sha256;
use tenero_core::u256::U256;
use tenero_core::v3::ids::tx_id;
use tenero_core::v3::rules;
use tenero_core::v3::*;
use tenero_net::sim::{Sim, SimConfig, SimRig};
use tenero_net::{EngineConfig, Hello, Message, PROTOCOL_VERSION};
use tenero_tree::hash_to_point;

const T0: u64 = 1_700_000_000;
const SEC: u64 = 1000;

/// A transaction valid at the node's tip spending key image `image` (its proofs are junk: the simulator does not check
/// proofs); `salt` changes its outputs, so two with one image are two spends of it.
fn make_tx(sim: &Sim<'_>, image: u64, salt: u8) -> Transaction {
    let next = sim.engines[0].node().next_block().unwrap();
    let pt = |tag: &[u8], n: u8| hash_to_point(sha256(&[tag, &image.to_le_bytes(), &[n, salt]]));
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
    let key_image = hash_to_point(sha256(&[b"key image", &image.to_le_bytes()]));
    let mut t = Transaction {
        prefix: TxPrefix {
            version: VERSION,
            inputs: vec![Input { key_image }],
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

fn ready(sim: &mut Sim<'_>) {
    while sim.tip(0).0 < 61 {
        sim.mine(0, None);
        sim.run_for(61 * SEC);
    }
}

/// A peer level with nobody's work: the node never syncs from it, and takes its announcements.
fn peer(sim: &mut Sim<'_>, rig: &SimRig, name: &str) -> usize {
    let h = sim.add_hostile(0, name);
    sim.hostile_send(
        h,
        Message::Hello(Hello {
            version: PROTOCOL_VERSION,
            chain_id: rig.store.chain_id(),
            tip_height: 0,
            cumulative_work: U256::ZERO.to_be_bytes(),
            tip_id: [9; 32],
            pruned_below: 0,
            nonce: 0,
        }),
    );
    sim.run_for(SEC);
    h
}

fn get_txs(sim: &Sim<'_>, h: usize) -> usize {
    sim.hostiles[h]
        .inbox
        .iter()
        .filter(|m| m.kind() == "get_txs")
        .count()
}

fn score(sim: &Sim<'_>, h: usize) -> Option<u32> {
    sim.engines[0].peer_score(sim.hostiles[h].peer_at_node()?)
}

fn config() -> EngineConfig {
    EngineConfig {
        ping_after_ms: 10_000_000,
        max_timeouts: 1000,
        ..EngineConfig::default()
    }
}

#[test]
fn a_transaction_already_confirmed_here_is_not_fetched_again() {
    let rigs = SimRig::rigs("relay-mined", 1);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), config());
    ready(&mut sim);
    let t = make_tx(&sim, 1, 0);
    let id = tx_id(&t).unwrap();
    sim.submit_tx(0, t);
    let b = sim.mine(0, None);
    assert_eq!(b.transactions.len(), 1, "the block confirmed it");
    // a peer that has not seen the block yet relays it
    let late = peer(&mut sim, &rigs[0], "late");
    sim.hostile_send(late, Message::NewTx { ids: vec![id] });
    sim.run_for(SEC);
    assert_eq!(get_txs(&sim, late), 0, "known, so not asked for");
    assert_eq!(score(&sim, late), Some(0));
}

#[test]
fn a_spend_of_a_key_image_already_spent_here_is_refused_without_blaming_the_sender() {
    let rigs = SimRig::rigs("relay-spent", 1);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), config());
    ready(&mut sim);
    let winner = make_tx(&sim, 2, 0);
    let loser = make_tx(&sim, 2, 1);
    let loser_id = tx_id(&loser).unwrap();
    assert_ne!(tx_id(&winner).unwrap(), loser_id);
    sim.submit_tx(0, winner);
    assert_eq!(sim.mine(0, None).transactions.len(), 1);
    // a peer on whose chain the other spend is the one waiting relays it, five times as many as a ban would need
    let other = peer(&mut sim, &rigs[0], "other");
    sim.hostile_send(
        other,
        Message::NewTx {
            ids: vec![loser_id],
        },
    );
    sim.run_for(SEC);
    assert_eq!(get_txs(&sim, other), 1, "a new id: asked for");
    sim.hostile_send(other, Message::Txs { txs: vec![loser] });
    sim.run_for(SEC);
    assert_eq!(score(&sim, other), Some(0), "valid on its chain, perhaps");
    assert!(!sim.engines[0].node().pool().contains(&loser_id));
    // refused once: not asked for again
    for _ in 0..5 {
        sim.hostile_send(
            other,
            Message::NewTx {
                ids: vec![loser_id],
            },
        );
    }
    sim.run_for(SEC);
    assert_eq!(get_txs(&sim, other), 1);
    assert_eq!(sim.engines[0].stats.bans, 0);
}
