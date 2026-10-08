//! Assume-valid sync: headers first, the path to a shipped checkpoint proved linked, and only then blocks applied
//! without their full proof of work and proofs. Real nodes mine and sync on the SHA-256 test chain; scripted peers
//! send the headers a hostile or mistaken peer might.

use tenero_core::u256::U256;
use tenero_core::v2::ids::PowKind;
use tenero_core::v3::ids::block_id;
use tenero_core::v3::{BlockHeader, VERSION};
use tenero_net::sim::{Sim, SimConfig, SimRig};
use tenero_net::{AssumeValid, EngineConfig, Hello, Limits, Message, PROTOCOL_VERSION};

const T0: u64 = 1_700_000_000;
const SEC: u64 = 1000;

fn id_at(sim: &Sim<'_>, node: usize, h: u64) -> [u8; 32] {
    sim.engines[node]
        .node()
        .store()
        .block_index(h)
        .unwrap()
        .unwrap()
        .block_id
}

fn config(assume: Option<AssumeValid>) -> EngineConfig {
    EngineConfig {
        assume_valid: assume,
        ..EngineConfig::default()
    }
}

fn two_nodes<'a>(rigs: &'a [SimRig], assume: Option<AssumeValid>) -> Sim<'a> {
    two_nodes_at(rigs, T0, assume)
}

fn two_nodes_at<'a>(rigs: &'a [SimRig], start: u64, assume: Option<AssumeValid>) -> Sim<'a> {
    Sim::with_configs(
        rigs,
        start,
        SimConfig::default(),
        vec![EngineConfig::default(), config(assume)],
    )
}

fn hello_for(rig: &SimRig, height: u64, work: U256) -> Message {
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

fn huge_work() -> U256 {
    U256::pow2(200).unwrap()
}

fn header(prev: [u8; 32], nonce: u64) -> BlockHeader {
    BlockHeader {
        version: VERSION,
        prev_id: prev,
        timestamp: 1,
        tx_root: [0; 32],
        nonce,
        mix: [0; 64],
    }
}

fn id_of(h: &BlockHeader) -> [u8; 32] {
    block_id(h, PowKind::Sha256)
}

/// `n` headers, each linking to the one before, the first to `parent`; and their ids.
fn chain_of(parent: [u8; 32], n: usize, salt: u64) -> (Vec<BlockHeader>, Vec<[u8; 32]>) {
    let mut headers = Vec::new();
    let mut ids = Vec::new();
    let mut prev = parent;
    for i in 0..n {
        let h = header(prev, salt + i as u64);
        prev = id_of(&h);
        ids.push(prev);
        headers.push(h);
    }
    (headers, ids)
}

fn score(sim: &Sim<'_>, h: usize) -> Option<u32> {
    let peer = sim.hostiles[h].peer_at_node()?;
    sim.engines[sim.hostiles[h].node].peer_score(peer)
}

fn count(sim: &Sim<'_>, h: usize, kind: &str) -> usize {
    sim.hostiles[h]
        .inbox
        .iter()
        .filter(|m| m.kind() == kind)
        .count()
}

/// One node that is to sync, and one scripted peer claiming a great deal of work, with a checkpoint.
fn with_scripted_peer<'a>(
    rigs: &'a [SimRig],
    assume: AssumeValid,
    limits: Limits,
) -> (Sim<'a>, usize) {
    let cfg = EngineConfig {
        assume_valid: Some(assume),
        limits,
        ping_after_ms: 10_000_000,
        max_timeouts: 2,
        ..EngineConfig::default()
    };
    let mut sim = Sim::new(rigs, T0, SimConfig::default(), cfg);
    let h = sim.add_hostile(0, "scripted");
    sim.hostile_send(h, hello_for(&rigs[0], 500, huge_work()));
    sim.run_for(2 * SEC);
    (sim, h)
}

fn small_limits() -> Limits {
    Limits {
        max_headers: 3,
        ..Limits::default()
    }
}

// ---- real nodes ---------------------------------------------------------------------------------------

/// Mines `n` blocks on node 0 with no checkpoint, and returns the simulated time after them. The chain stays in the
/// rig's store, so a second simulator over the same rigs starts from it.
fn mined(rigs: &[SimRig], n: usize) -> u64 {
    let mut sim = Sim::new(rigs, T0, SimConfig::default(), EngineConfig::default());
    sim.mine_chain(0, n);
    assert_eq!(sim.tip(0).0, n as u64);
    T0 + (n as u64 + 10) * 60
}

#[test]
fn a_node_behind_the_checkpoint_syncs_it_assumed_and_the_rest_in_full() {
    let rigs = SimRig::rigs("av-basic", 2);
    let start = mined(&rigs, 120);
    let checkpoint = {
        let probe = two_nodes_at(&rigs, start, None);
        AssumeValid {
            height: 100,
            id: id_at(&probe, 0, 100),
        }
    };
    let mut sim = two_nodes_at(&rigs, start, Some(checkpoint));
    assert!(sim.connect(1, 0));
    assert!(sim.run_until(600 * SEC, |s| s.tip(1).0 == 120));
    assert_eq!(sim.tip(1).1, sim.tip(0).1);
    let s = &sim.engines[1].stats;
    assert_eq!(s.blocks_applied, 120);
    assert_eq!(s.assumed_blocks, 100, "blocks 1 to 100, and not one more");
    assert!(s.sent["get_headers"] >= 1);
    assert_eq!(
        sim.engines[1].node().chain().assumed_count(),
        0,
        "nothing stays assumed once the tip is past the checkpoint"
    );
}

#[test]
fn headers_come_in_batches_of_five_hundred() {
    let rigs = SimRig::rigs("av-batches", 2);
    let start = mined(&rigs, 1100);
    let checkpoint = {
        let probe = two_nodes_at(&rigs, start, None);
        AssumeValid {
            height: 1000,
            id: id_at(&probe, 0, 1000),
        }
    };
    let mut sim = two_nodes_at(&rigs, start, Some(checkpoint));
    assert!(sim.connect(1, 0));
    assert!(sim.run_until(1200 * SEC, |s| s.tip(1).0 == 1100));
    assert_eq!(sim.engines[1].stats.assumed_blocks, 1000);
    assert!(
        sim.engines[1].stats.sent["get_headers"] >= 2,
        "a thousand headers do not fit in one reply"
    );
    assert_eq!(sim.tip(1).1, sim.tip(0).1);
}

#[test]
fn a_peer_whose_chain_has_another_block_at_the_checkpoint_height_gets_nothing_assumed_and_is_banned(
) {
    let rigs = SimRig::rigs("av-wrong", 2);
    let wrong = AssumeValid {
        height: 100,
        id: [7; 32],
    };
    let mut sim = two_nodes(&rigs, Some(wrong));
    sim.mine_chain(0, 120);
    assert!(sim.connect(1, 0));
    sim.run_for(300 * SEC);
    assert_eq!(sim.tip(1).0, 0, "no block of that chain was taken on trust");
    assert_eq!(sim.engines[1].stats.assumed_blocks, 0);
    assert_eq!(sim.engines[1].node().chain().assumed_count(), 0);
    assert_eq!(sim.engines[1].peer_count(), 0, "50 points twice: banned");
}

#[test]
fn a_peer_with_a_chain_shorter_than_the_checkpoint_is_synced_from_in_full_and_not_blamed() {
    let rigs = SimRig::rigs("av-short", 2);
    let far = AssumeValid {
        height: 100,
        id: [7; 32],
    };
    let mut sim = two_nodes(&rigs, Some(far));
    sim.mine_chain(0, 50);
    assert!(sim.connect(1, 0));
    assert!(sim.run_until(600 * SEC, |s| s.tip(1).0 == 50));
    assert_eq!(sim.engines[1].stats.assumed_blocks, 0);
    assert_eq!(
        sim.engines[1].peer_count(),
        1,
        "not punished: it had nothing wrong"
    );
}

#[test]
fn without_a_checkpoint_no_headers_are_asked_for() {
    let rigs = SimRig::rigs("av-off", 2);
    let mut sim = two_nodes(&rigs, None);
    sim.mine_chain(0, 30);
    assert!(sim.connect(1, 0));
    assert!(sim.run_until(300 * SEC, |s| s.tip(1).0 == 30));
    assert!(!sim.engines[1].stats.sent.contains_key("get_headers"));
    assert_eq!(sim.engines[1].stats.assumed_blocks, 0);
}

// ---- scripted peers -----------------------------------------------------------------------------------

#[test]
fn headers_that_do_not_link_to_our_chain_are_refused() {
    let rigs = SimRig::rigs("av-nolink", 1);
    let (mut sim, h) = with_scripted_peer(
        &rigs,
        AssumeValid {
            height: 5,
            id: [1; 32],
        },
        Limits::default(),
    );
    assert_eq!(count(&sim, h, "get_headers"), 1);
    let (headers, _) = chain_of([9; 32], 2, 10); // a parent we do not have
    sim.hostile_send(
        h,
        Message::Headers {
            first_height: 1,
            headers,
        },
    );
    sim.run_for(SEC);
    assert_eq!(score(&sim, h), Some(50));
    assert!(!sim.engines[0].is_syncing());
    assert_eq!(sim.engines[0].node().chain().assumed_count(), 0);
}

#[test]
fn headers_that_start_where_we_have_no_block_are_refused() {
    for first_height in [0u64, 7] {
        let rigs = SimRig::rigs(&format!("av-start{first_height}"), 1);
        let (mut sim, h) = with_scripted_peer(
            &rigs,
            AssumeValid {
                height: 50,
                id: [1; 32],
            },
            Limits::default(),
        );
        let genesis = id_at(&sim, 0, 0);
        let (headers, _) = chain_of(genesis, 2, 10);
        sim.hostile_send(
            h,
            Message::Headers {
                first_height,
                headers,
            },
        );
        sim.run_for(SEC);
        assert_eq!(score(&sim, h), Some(50), "first height {first_height}");
    }
}

#[test]
fn a_peer_that_runs_out_before_the_checkpoint_or_sends_none_is_not_blamed_and_nothing_is_assumed() {
    for n in [0usize, 2] {
        let rigs = SimRig::rigs(&format!("av-runout{n}"), 1);
        let (mut sim, h) = with_scripted_peer(
            &rigs,
            AssumeValid {
                height: 10,
                id: [1; 32],
            },
            small_limits(),
        );
        let genesis = id_at(&sim, 0, 0);
        let (headers, _) = chain_of(genesis, n, 10);
        sim.hostile_send(
            h,
            Message::Headers {
                first_height: 1,
                headers,
            },
        );
        sim.run_for(SEC);
        assert_eq!(score(&sim, h), Some(0), "{n} headers");
        assert_eq!(
            count(&sim, h, "get_block_ids"),
            1,
            "the ordinary sync follows"
        );
        assert_eq!(sim.engines[0].node().chain().assumed_count(), 0);
    }
}

#[test]
fn a_chain_of_headers_that_reaches_the_checkpoint_makes_those_blocks_assumed_and_they_are_requested(
) {
    let rigs = SimRig::rigs("av-reach", 1);
    let probe = SimRig::rigs("av-reach-probe", 1);
    let genesis = probe[0].store.tip().unwrap().1.block_id;
    let (headers, ids) = chain_of(genesis, 10, 10);
    let checkpoint = AssumeValid {
        height: 10,
        id: ids[9],
    };
    let (mut sim, h) = with_scripted_peer(&rigs, checkpoint, small_limits());
    assert_eq!(id_at(&sim, 0, 0), genesis);
    // three at a time: the node keeps asking, from the last id it has
    for (i, batch) in headers.chunks(3).enumerate() {
        sim.hostile_send(
            h,
            Message::Headers {
                first_height: 1 + 3 * i as u64,
                headers: batch.to_vec(),
            },
        );
        sim.run_for(SEC);
        if i < 3 {
            let asks: Vec<&Message> = sim.hostiles[h]
                .inbox
                .iter()
                .filter(|m| m.kind() == "get_headers")
                .collect();
            assert_eq!(asks.len(), 2 + i);
            assert_eq!(
                asks.last().unwrap(),
                &&Message::GetHeaders {
                    locator: vec![ids[3 * i + 2]]
                }
            );
        }
    }
    assert_eq!(score(&sim, h), Some(0));
    assert_eq!(sim.engines[0].node().chain().assumed_count(), 10);
    for id in &ids {
        assert!(sim.engines[0].node().chain().is_assumed(id));
    }
    // and the blocks on that path are asked for
    let asked: Vec<[u8; 32]> = sim.hostiles[h]
        .inbox
        .iter()
        .filter_map(|m| match m {
            Message::GetBlocks { ids } => Some(ids.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(asked, ids);
}

#[test]
fn headers_past_the_checkpoint_in_the_same_reply_are_ignored() {
    let rigs = SimRig::rigs("av-past", 1);
    let probe = SimRig::rigs("av-past-probe", 1);
    let genesis = probe[0].store.tip().unwrap().1.block_id;
    let (headers, ids) = chain_of(genesis, 6, 10);
    let checkpoint = AssumeValid {
        height: 4,
        id: ids[3],
    };
    let (mut sim, h) = with_scripted_peer(&rigs, checkpoint, Limits::default());
    sim.hostile_send(
        h,
        Message::Headers {
            first_height: 1,
            headers,
        },
    );
    sim.run_for(SEC);
    assert_eq!(score(&sim, h), Some(0));
    assert_eq!(
        sim.engines[0].node().chain().assumed_count(),
        4,
        "blocks 1 to 4 only"
    );
    assert!(!sim.engines[0].node().chain().is_assumed(&ids[4]));
}

#[test]
fn a_wrong_block_at_the_checkpoint_height_is_refused_even_when_everything_links() {
    let rigs = SimRig::rigs("av-wrongcp", 1);
    let probe = SimRig::rigs("av-wrongcp-probe", 1);
    let genesis = probe[0].store.tip().unwrap().1.block_id;
    let (headers, ids) = chain_of(genesis, 4, 10);
    let checkpoint = AssumeValid {
        height: 4,
        id: [3; 32], // not what the fourth header hashes to
    };
    assert_ne!(ids[3], checkpoint.id);
    let (mut sim, h) = with_scripted_peer(&rigs, checkpoint, Limits::default());
    sim.hostile_send(
        h,
        Message::Headers {
            first_height: 1,
            headers,
        },
    );
    sim.run_for(SEC);
    assert_eq!(score(&sim, h), Some(50));
    assert_eq!(sim.engines[0].node().chain().assumed_count(), 0);
    assert!(!sim.engines[0].is_syncing());
}

#[test]
fn a_second_batch_must_continue_the_first() {
    let rigs = SimRig::rigs("av-continue", 1);
    let probe = SimRig::rigs("av-continue-probe", 1);
    let genesis = probe[0].store.tip().unwrap().1.block_id;
    let (headers, ids) = chain_of(genesis, 10, 10);
    let checkpoint = AssumeValid {
        height: 10,
        id: ids[9],
    };
    // (a) the next batch starts at the wrong height
    let (mut sim, h) = with_scripted_peer(&rigs, checkpoint, small_limits());
    sim.hostile_send(
        h,
        Message::Headers {
            first_height: 1,
            headers: headers[0..3].to_vec(),
        },
    );
    sim.run_for(SEC);
    assert_eq!(score(&sim, h), Some(0));
    sim.hostile_send(
        h,
        Message::Headers {
            first_height: 5,
            headers: headers[3..6].to_vec(),
        },
    );
    sim.run_for(SEC);
    assert_eq!(score(&sim, h), Some(50), "it does not continue");
    drop(sim);

    // (b) the next batch starts at the right height but does not link to the last header
    let rigs = SimRig::rigs("av-continue2", 1);
    let (mut sim, h) = with_scripted_peer(&rigs, checkpoint, small_limits());
    sim.hostile_send(
        h,
        Message::Headers {
            first_height: 1,
            headers: headers[0..3].to_vec(),
        },
    );
    sim.run_for(SEC);
    sim.hostile_send(
        h,
        Message::Headers {
            first_height: 4,
            headers: headers[4..7].to_vec(),
        },
    );
    sim.run_for(SEC);
    assert_eq!(score(&sim, h), Some(50), "it does not link");
    assert!(!sim.engines[0].is_syncing());
}

#[test]
fn headers_from_a_peer_we_are_not_syncing_from_are_punished() {
    let rigs = SimRig::rigs("av-other", 1);
    let (mut sim, first) = with_scripted_peer(
        &rigs,
        AssumeValid {
            height: 5,
            id: [1; 32],
        },
        Limits::default(),
    );
    let other = sim.add_hostile(0, "bystander");
    sim.hostile_send(other, hello_for(&rigs[0], 0, U256::from_be_bytes(&[0; 32])));
    sim.run_for(SEC);
    let genesis = id_at(&sim, 0, 0);
    let (headers, _) = chain_of(genesis, 1, 10);
    sim.hostile_send(
        other,
        Message::Headers {
            first_height: 1,
            headers,
        },
    );
    sim.run_for(SEC);
    assert_eq!(score(&sim, other), Some(20));
    assert_eq!(score(&sim, first), Some(0));
    assert!(
        sim.engines[0].is_syncing(),
        "the real sync is not disturbed"
    );
}

#[test]
fn a_checkpoint_at_height_zero_changes_nothing() {
    let rigs = SimRig::rigs("av-zero", 1);
    let probe = SimRig::rigs("av-zero-probe", 1);
    let genesis = probe[0].store.tip().unwrap().1.block_id;
    let (sim, h) = with_scripted_peer(
        &rigs,
        AssumeValid {
            height: 0,
            id: genesis,
        },
        Limits::default(),
    );
    assert_eq!(count(&sim, h, "get_headers"), 0);
    assert_eq!(count(&sim, h, "get_block_ids"), 1);
}

#[test]
fn a_header_reply_counts_as_an_answer_so_a_peer_that_keeps_answering_is_not_dropped_as_silent() {
    let rigs = SimRig::rigs("av-answers", 1);
    let (mut sim, h) = with_scripted_peer(
        &rigs,
        AssumeValid {
            height: 5,
            id: [1; 32],
        },
        Limits::default(),
    );
    // the first request goes unanswered: one timeout (the limit is 2 in a row)
    assert!(sim.run_until(120 * SEC, |s| s.hostiles[h]
        .inbox
        .iter()
        .filter(|m| m.kind() == "get_headers")
        .count()
        >= 2));
    // the second is answered, with nothing: that is an answer, and clears the count
    sim.hostile_send(
        h,
        Message::Headers {
            first_height: 1,
            headers: vec![],
        },
    );
    sim.run_for(SEC);
    assert_eq!(count(&sim, h, "get_block_ids"), 1);
    // the ordinary sync's request goes unanswered: a second timeout, but not two in a row
    sim.run_for(60 * SEC);
    assert!(
        !sim.hostiles[h].disconnected,
        "one silence after an answer is not two in a row"
    );
}

#[test]
fn a_silent_header_peer_is_abandoned() {
    let rigs = SimRig::rigs("av-silent", 1);
    let (mut sim, _h) = with_scripted_peer(
        &rigs,
        AssumeValid {
            height: 5,
            id: [1; 32],
        },
        Limits::default(),
    );
    assert!(sim.engines[0].is_syncing());
    sim.run_for(40 * SEC);
    assert!(
        !sim.engines[0].is_syncing(),
        "no answer to get_headers within the timeout"
    );
}

#[test]
fn blocks_we_already_have_are_not_requested_again_and_a_slow_download_is_not_timed_out() {
    let rigs = SimRig::rigs("av-known", 1);
    let start = mined(&rigs, 3);
    // the node already has blocks 1 to 3; the scripted peer offers those headers and three more to the checkpoint
    let real: Vec<BlockHeader> = (1..=3)
        .map(|h| rigs[0].store.block_index(h).unwrap().unwrap().header)
        .collect();
    let (forged, forged_ids) = chain_of(id_of(&real[2]), 3, 500);
    let checkpoint = AssumeValid {
        height: 6,
        id: forged_ids[2],
    };
    let cfg = EngineConfig {
        assume_valid: Some(checkpoint),
        limits: small_limits(),
        ping_after_ms: 10_000_000,
        ..EngineConfig::default()
    };
    let mut sim = Sim::new(&rigs, start, SimConfig::default(), cfg);
    assert_eq!(sim.tip(0).0, 3);
    let h = sim.add_hostile(0, "scripted");
    sim.hostile_send(h, hello_for(&rigs[0], 500, huge_work()));
    // the first batch comes late, the second later still: 40 seconds in all, over the 30-second timeout of one request
    sim.run_for(20 * SEC);
    sim.hostile_send(
        h,
        Message::Headers {
            first_height: 1,
            headers: real.clone(),
        },
    );
    sim.run_for(20 * SEC);
    assert!(
        sim.engines[0].is_syncing(),
        "each answer restarts the clock of the next request"
    );
    sim.hostile_send(
        h,
        Message::Headers {
            first_height: 4,
            headers: forged,
        },
    );
    sim.run_for(SEC);
    assert_eq!(score(&sim, h), Some(0));
    let asked: Vec<[u8; 32]> = sim.hostiles[h]
        .inbox
        .iter()
        .filter_map(|m| match m {
            Message::GetBlocks { ids } => Some(ids.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(asked, forged_ids, "only the three blocks we do not have");
    assert_eq!(sim.engines[0].node().chain().assumed_count(), 6);
}
