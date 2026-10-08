//! "Slow is not hostile": a reply that arrives AFTER the request it answers has timed out is not punished. The first
//! day-long run (`docs/TESTNET.md`) split three nodes into three chains, with 24-hour bans, after a node stalled and its
//! (or its peers') late replies were scored as unsolicited. Scripted peers answer late; the engine must forgive each late
//! reply once, and only to the peer that was slow, and only for a while.

use tenero_core::u256::U256;
use tenero_core::v3::{Input, Output, Prunable, Transaction, TxPrefix, VERSION};
use tenero_net::sim::{Sim, SimConfig, SimRig};
use tenero_net::{AssumeValid, EngineConfig, Hello, Message, PROTOCOL_VERSION};

const T0: u64 = 1_700_000_000;
const SEC: u64 = 1000;
/// The engine's default request timeout, and how long a late reply stays forgivable (four times it).
const TIMEOUT: u64 = 30 * SEC;

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
        // a peer that is slow is dropped after this many unanswered requests in a row; not what these tests are about
        max_timeouts: 1000,
        ..EngineConfig::default()
    }
}

fn score(sim: &Sim<'_>, h: usize) -> Option<u32> {
    sim.engines[0].peer_score(sim.hostiles[h].peer_at_node()?)
}

fn count(sim: &Sim<'_>, h: usize, kind: &str) -> usize {
    sim.hostiles[h]
        .inbox
        .iter()
        .filter(|m| m.kind() == kind)
        .count()
}

fn forgiven(sim: &Sim<'_>) -> u64 {
    sim.engines[0].stats.late_replies_forgiven
}

fn a_tx() -> Transaction {
    let out = |n: u8| Output {
        onetime_address: [n; 32],
        amount_commitment: [n; 32],
        amount_enc: [n; 8],
        view_tag: [n; 3],
        anchor_enc: [n; 16],
    };
    Transaction {
        prefix: TxPrefix {
            version: VERSION,
            inputs: vec![Input { key_image: [5; 32] }],
            outputs: vec![out(1), out(2)],
            ephemeral_pubkeys: vec![[9; 32]],
            fee: 1,
            encrypted_payment_id: [0; 8],
        },
        prunable: Prunable {
            reference_height: 0,
            proof_data: vec![7; 20],
        },
    }
}

// ---- block ids ---------------------------------------------------------------------------------------------------

/// A scripted peer claims a lot of work, so the node asks it for block ids; the peer does not answer in time.
fn sync_that_times_out<'a>(rigs: &'a [SimRig], cfg: EngineConfig) -> (Sim<'a>, usize) {
    let mut sim = Sim::new(rigs, T0, SimConfig::default(), cfg);
    let h = sim.add_hostile(0, "slow");
    sim.hostile_send(h, hello(&rigs[0], 500, U256::pow2(200).unwrap()));
    sim.run_for(2 * SEC);
    assert_eq!(count(&sim, h, "get_block_ids"), 1, "the node asked for ids");
    sim.run_for(TIMEOUT + 5 * SEC);
    assert!(!sim.engines[0].is_syncing(), "the sync timed out");
    (sim, h)
}

fn late_block_ids() -> Message {
    Message::BlockIds {
        first_height: 1,
        ids: vec![[7; 32]],
    }
}

#[test]
fn a_late_answer_to_the_block_id_request_is_forgiven_once() {
    let rigs = SimRig::rigs("late-ids", 1);
    let (mut sim, h) = sync_that_times_out(&rigs, config());
    assert_eq!(score(&sim, h), Some(0), "being slow is not scored");
    sim.hostile_send(h, late_block_ids());
    sim.run_for(SEC);
    assert_eq!(score(&sim, h), Some(0), "the late answer is not punished");
    assert_eq!(forgiven(&sim), 1);
    // one forgiveness for one timeout: the same answer again is unsolicited
    sim.hostile_send(h, late_block_ids());
    sim.run_for(SEC);
    assert_eq!(
        score(&sim, h),
        Some(20),
        "a second answer to one request is punished"
    );
    assert_eq!(forgiven(&sim), 1);
}

#[test]
fn only_the_peer_that_was_slow_is_forgiven() {
    let rigs = SimRig::rigs("late-other", 1);
    let (mut sim, slow) = sync_that_times_out(&rigs, config());
    let other = sim.add_hostile(0, "other");
    sim.hostile_send(other, hello(&rigs[0], 0, U256::ZERO));
    sim.run_for(SEC);
    sim.hostile_send(other, late_block_ids());
    sim.run_for(SEC);
    assert_eq!(
        score(&sim, other),
        Some(20),
        "another peer's block ids nobody asked for"
    );
    assert_eq!(forgiven(&sim), 0);
    // the slow one is still forgiven
    sim.hostile_send(slow, late_block_ids());
    sim.run_for(SEC);
    assert_eq!(score(&sim, slow), Some(0));
    assert_eq!(forgiven(&sim), 1);
}

#[test]
fn a_reply_that_is_late_by_more_than_one_timeout_is_still_forgiven() {
    let rigs = SimRig::rigs("late-two", 1);
    let cfg = EngineConfig {
        sync_cooldown_ms: 10_000_000,
        ..config()
    };
    let (mut sim, h) = sync_that_times_out(&rigs, cfg);
    // the request timed out 5 s ago; this answer comes two whole timeouts after that: slow, still not hostile
    sim.run_for(2 * TIMEOUT);
    sim.hostile_send(h, late_block_ids());
    sim.run_for(SEC);
    assert_eq!(score(&sim, h), Some(0));
    assert_eq!(forgiven(&sim), 1);
}

#[test]
fn a_reply_that_comes_far_too_late_is_punished_after_all() {
    let rigs = SimRig::rigs("late-ttl", 1);
    // (no second sync with the same peer: that one would time out too, and its late reply WOULD be forgiven)
    let cfg = EngineConfig {
        sync_cooldown_ms: 10_000_000,
        ..config()
    };
    let (mut sim, h) = sync_that_times_out(&rigs, cfg);
    // the grace is four request timeouts; wait out more than that
    sim.run_for(4 * TIMEOUT + 10 * SEC);
    sim.hostile_send(h, late_block_ids());
    sim.run_for(SEC);
    assert_eq!(score(&sim, h), Some(20));
    assert_eq!(forgiven(&sim), 0);
}

#[test]
fn block_ids_nobody_asked_for_are_still_punished() {
    let rigs = SimRig::rigs("unsolicited-ids", 1);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), config());
    let h = sim.add_hostile(0, "chatty");
    sim.hostile_send(h, hello(&rigs[0], 0, U256::ZERO));
    sim.run_for(SEC);
    sim.hostile_send(h, late_block_ids());
    sim.run_for(SEC);
    assert_eq!(score(&sim, h), Some(20));
    assert_eq!(forgiven(&sim), 0);
}

// ---- headers (assume-valid) ------------------------------------------------------------------------------------

#[test]
fn a_late_answer_to_the_headers_request_is_forgiven_once() {
    let rigs = SimRig::rigs("late-headers", 1);
    let cfg = EngineConfig {
        assume_valid: Some(AssumeValid {
            height: 100,
            id: [1; 32],
        }),
        ..config()
    };
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), cfg);
    let h = sim.add_hostile(0, "slow");
    sim.hostile_send(h, hello(&rigs[0], 500, U256::pow2(200).unwrap()));
    sim.run_for(2 * SEC);
    assert_eq!(count(&sim, h, "get_headers"), 1);
    sim.run_for(TIMEOUT + 5 * SEC);
    assert!(!sim.engines[0].is_syncing());
    let late = || Message::Headers {
        first_height: 1,
        headers: vec![],
    };
    sim.hostile_send(h, late());
    sim.run_for(SEC);
    assert_eq!(score(&sim, h), Some(0));
    assert_eq!(forgiven(&sim), 1);
    sim.hostile_send(h, late());
    sim.run_for(SEC);
    assert_eq!(score(&sim, h), Some(20), "the second one was not asked for");
}

// ---- transactions ------------------------------------------------------------------------------------------------

#[test]
fn a_late_transaction_we_asked_for_is_forgiven_once() {
    let rigs = SimRig::rigs("late-txs", 1);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), config());
    let tx = a_tx();
    let id = tenero_core::v3::ids::tx_id(&tx).unwrap();
    let h = sim.add_hostile(0, "slow");
    sim.hostile_send(h, hello(&rigs[0], 0, U256::ZERO));
    sim.hostile_send(h, Message::NewTx { ids: vec![id] });
    sim.run_for(SEC);
    assert_eq!(
        count(&sim, h, "get_txs"),
        1,
        "the node asked for the transaction"
    );
    sim.run_for(TIMEOUT + 5 * SEC);
    sim.hostile_send(
        h,
        Message::Txs {
            txs: vec![tx.clone()],
        },
    );
    sim.run_for(SEC);
    assert_eq!(score(&sim, h), Some(0), "late, not unsolicited");
    assert_eq!(forgiven(&sim), 1);
    sim.hostile_send(h, Message::Txs { txs: vec![tx] });
    sim.run_for(SEC);
    assert_eq!(
        score(&sim, h),
        Some(20),
        "the same transaction again was not asked for"
    );
    // a transaction it never announced is unsolicited, late or not
    let mut other = a_tx();
    other.prefix.fee = 99;
    sim.hostile_send(h, Message::Txs { txs: vec![other] });
    sim.run_for(SEC);
    assert_eq!(score(&sim, h), Some(40));
}

// ---- pongs -------------------------------------------------------------------------------------------------------

#[test]
fn a_pong_that_answers_an_old_ping_is_forgiven_once() {
    let rigs = SimRig::rigs("late-pong", 1);
    let cfg = EngineConfig {
        ping_after_ms: SEC,
        pong_timeout_ms: SEC,
        ..config()
    };
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), cfg);
    let h = sim.add_hostile(0, "slow");
    sim.hostile_send(h, hello(&rigs[0], 0, U256::ZERO));
    // it never answers: the node pings, waits, and pings again with a new nonce
    sim.run_for(6 * SEC);
    let pings: Vec<u64> = sim.hostiles[h]
        .inbox
        .iter()
        .filter_map(|m| match m {
            Message::Ping(n) => Some(*n),
            _ => None,
        })
        .collect();
    assert!(pings.len() >= 2, "{pings:?}");
    let old = pings[0];
    assert_eq!(score(&sim, h), Some(0));
    // the answer to the first ping finally comes
    sim.hostile_send(h, Message::Pong(old));
    sim.run_for(SEC);
    assert_eq!(score(&sim, h), Some(0), "late, not unsolicited");
    assert_eq!(forgiven(&sim), 1);
    sim.hostile_send(h, Message::Pong(old));
    sim.run_for(SEC);
    assert_eq!(score(&sim, h), Some(10), "a second pong for one ping");
    // and a nonce that was never sent is still unsolicited
    sim.hostile_send(h, Message::Pong(0xdead_beef));
    sim.run_for(SEC);
    assert_eq!(score(&sim, h), Some(20));
}

// ---- what is not forgiven ------------------------------------------------------------------------------------------

#[test]
fn forgiving_late_replies_does_not_let_a_peer_ignore_the_rules() {
    // a peer that is slow AND misbehaves (a locator that is not valid) is still punished as before
    let rigs = SimRig::rigs("late-rules", 1);
    let (mut sim, h) = sync_that_times_out(&rigs, config());
    sim.hostile_send(h, Message::GetBlockIds { locator: vec![] });
    sim.run_for(SEC);
    assert_eq!(
        score(&sim, h),
        Some(50),
        "a bad locator is 50 points, forgiveness or not"
    );
}
