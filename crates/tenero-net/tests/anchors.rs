//! Anchor peers (M9, threat model C1): a node remembers a few of its long-standing outbound peers across a restart and dials
//! them first, before the address book or the seeds, so that a restart does not leave it alone with an attacker's peers.
//! These drive one engine directly (the real sockets are in `transport.rs`).

use tenero_chain::{ProofsNotChecked, Sha256Pow};
use tenero_core::u256::U256;
use tenero_net::addrbook::group_of;
use tenero_net::anchors;
use tenero_net::sim::{sim_addr, sim_addr_in, SimRig};
use tenero_net::{Action, Engine, EngineConfig, Event, Hello, Message, PeerId, PROTOCOL_VERSION};
use tenero_node::{Node, NodeConfig};

const T0: u64 = 1_700_000_000 * 1000;
const MIN: u64 = 60_000;

fn engine_on(rig: &SimRig, cfg: EngineConfig) -> Engine<'_> {
    let node = Node::with_proof_check(
        &rig.store,
        &rig.params,
        &Sha256Pow,
        &ProofsNotChecked,
        NodeConfig {
            allow_unchecked_proofs_for_tests: true,
            ..NodeConfig::default()
        },
    )
    .expect("a test node");
    Engine::new(node, cfg)
}

fn cfg() -> EngineConfig {
    EngineConfig {
        nonce: 7,
        ..EngineConfig::default()
    }
}

fn hello(rig: &SimRig, nonce: u64) -> Message {
    Message::Hello(Hello {
        version: PROTOCOL_VERSION,
        chain_id: rig.store.chain_id(),
        tip_height: 0,
        cumulative_work: U256::from_be_bytes(&[0; 32]).to_be_bytes(),
        tip_id: [9; 32],
        pruned_below: 0,
        nonce,
    })
}

fn connects(actions: &[Action]) -> Vec<String> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Connect { addr } => Some(addr.clone()),
            _ => None,
        })
        .collect()
}

/// A peer connected at `at` (its hello sent if `ready`).
fn connect(
    e: &mut Engine<'_>,
    rig: &SimRig,
    peer: PeerId,
    addr: &str,
    inbound: bool,
    ready: bool,
    at: u64,
) {
    e.handle(
        at,
        Event::PeerConnected {
            peer,
            addr: addr.to_string(),
            inbound,
        },
    );
    if ready {
        e.handle(
            at + 1,
            Event::Message {
                peer,
                msg: hello(rig, 100 + peer),
            },
        );
    }
}

// ---- the saved form --------------------------------------------------------------------------------------------------

#[test]
fn the_anchor_list_round_trips_and_every_damage_is_refused() {
    let list: Vec<String> = vec!["8.8.4.4:18331".into(), "[2001:db8::1]:18331".into()];
    let bytes = anchors::to_bytes(&list);
    assert_eq!(anchors::from_bytes(&bytes).unwrap(), list);
    assert_eq!(
        anchors::from_bytes(&anchors::to_bytes(&[])).unwrap(),
        Vec::<String>::new()
    );
    // a bit flipped anywhere, a cut anywhere, anything added
    for i in 0..bytes.len() {
        let mut d = bytes.clone();
        d[i] ^= 1;
        assert!(
            anchors::from_bytes(&d).is_err(),
            "a flip at byte {i} was accepted"
        );
    }
    for cut in 0..bytes.len() {
        assert!(anchors::from_bytes(&bytes[..cut]).is_err(), "cut at {cut}");
    }
    let mut padded = bytes.clone();
    padded.push(0);
    assert!(anchors::from_bytes(&padded).is_err());
    // damage that the checksum does not see, because the checksum is made to fit it: a byte too many, a count too high
    let resealed = |mut body: Vec<u8>| {
        let sum = tenero_core::hash::sha256(&[&body]);
        body.extend_from_slice(&sum[..4]);
        body
    };
    let body = bytes[..bytes.len() - 4].to_vec();
    let mut extra = body.clone();
    extra.push(0);
    assert_eq!(
        anchors::from_bytes(&resealed(extra)).unwrap_err(),
        "trailing bytes"
    );
    let mut count_high = body.clone();
    count_high[4] = 3; // says three entries, holds two
    assert!(anchors::from_bytes(&resealed(count_high)).is_err());
    let mut count_low = body;
    count_low[4] = 1; // says one entry, holds two
    assert!(anchors::from_bytes(&resealed(count_low)).is_err());
    // more than the most there may be
    let many: Vec<String> = (0..9).map(|i| format!("8.8.4.{}:18331", i + 1)).collect();
    assert!(anchors::from_bytes(&anchors::to_bytes(&many)).is_err());
    // an entry that is not an address and a port, or has port 0 (the checksum is right: the content is not)
    for bad in [
        "not an address",
        "8.8.4.4",
        "8.8.4.4:0",
        "8.8.4.4:99999",
        "",
    ] {
        assert!(
            anchors::from_bytes(&anchors::to_bytes(&[bad.to_string()])).is_err(),
            "{bad:?}"
        );
    }
}

// ---- which peers are chosen ------------------------------------------------------------------------------------------

#[test]
fn only_old_ready_outbound_peers_from_different_groups_are_chosen_oldest_first() {
    let rig = SimRig::new("anchors-choose", 0);
    let config = EngineConfig {
        anchor_count: 3,
        anchor_min_age_ms: 10 * MIN,
        ..cfg()
    };
    let mut e = engine_on(&rig, config);
    let (a, b, c) = (sim_addr(1), sim_addr(2), sim_addr(3));
    // (the engine's clock only moves forward, so peers are connected in the order of their times)
    connect(&mut e, &rig, 4, &sim_addr(4), true, true, T0 + 100); // the oldest of all, but INBOUND: chosen by whoever connected
    connect(&mut e, &rig, 5, &sim_addr(5), false, false, T0 + 200); // old, outbound, but never said hello
    connect(&mut e, &rig, 1, &a, false, true, T0 + 1000); // the oldest outbound that qualifies
                                                          // the same network group as `a`, and older than `b`
    let same_group = sim_addr_in(3, 2);
    assert_eq!(
        group_of(&same_group),
        group_of(&a),
        "the test needs one group"
    );
    connect(&mut e, &rig, 6, &same_group, false, true, T0 + 1500);
    connect(&mut e, &rig, 2, &b, false, true, T0 + 2000);
    connect(&mut e, &rig, 3, &c, false, true, T0 + 3000);
    // too young
    connect(&mut e, &rig, 7, &sim_addr(7), false, true, T0 + 11 * MIN);
    e.handle(T0 + 12 * MIN, Event::Tick);
    assert_eq!(
        e.current_anchors(),
        vec![a.clone(), b.clone(), c.clone()],
        "outbound, ready, old enough, one per group, oldest first"
    );
    // the number is capped
    let config = EngineConfig {
        anchor_count: 2,
        anchor_min_age_ms: 10 * MIN,
        ..cfg()
    };
    let rig2 = SimRig::new("anchors-choose", 1);
    let mut e2 = engine_on(&rig2, config);
    for (i, addr) in [&a, &b, &c].iter().enumerate() {
        connect(
            &mut e2,
            &rig2,
            i as u64 + 1,
            addr,
            false,
            true,
            T0 + 1000 * (i as u64 + 1),
        );
    }
    e2.handle(T0 + 12 * MIN, Event::Tick);
    assert_eq!(e2.current_anchors(), vec![a, b]);
    // and none are kept when it is turned off
    let rig3 = SimRig::new("anchors-choose", 2);
    let mut e3 = engine_on(
        &rig3,
        EngineConfig {
            anchor_count: 0,
            anchor_min_age_ms: 0,
            ..cfg()
        },
    );
    connect(&mut e3, &rig3, 1, &sim_addr(1), false, true, T0);
    e3.handle(T0 + 12 * MIN, Event::Tick);
    assert!(e3.current_anchors().is_empty());
}

#[test]
fn a_peer_that_is_too_young_is_not_an_anchor_until_it_is_old_enough() {
    let rig = SimRig::new("anchors-age", 0);
    let mut e = engine_on(
        &rig,
        EngineConfig {
            anchor_min_age_ms: 10 * MIN,
            ..cfg()
        },
    );
    connect(&mut e, &rig, 1, &sim_addr(1), false, true, T0);
    e.handle(T0 + 10 * MIN - 1, Event::Tick);
    assert!(e.current_anchors().is_empty(), "one millisecond short");
    e.handle(T0 + 10 * MIN + 2, Event::Tick);
    assert_eq!(e.current_anchors(), vec![sim_addr(1)]);
}

#[test]
fn a_peer_that_has_not_said_hello_is_not_an_anchor_even_with_no_minimum_age() {
    // (with the default age the engine would have dropped it at the handshake timeout long before; with none, only this rule
    // keeps it out)
    let rig = SimRig::new("anchors-hello", 0);
    let mut e = engine_on(
        &rig,
        EngineConfig {
            anchor_min_age_ms: 0,
            ..cfg()
        },
    );
    connect(&mut e, &rig, 1, &sim_addr(1), false, false, T0);
    connect(&mut e, &rig, 2, &sim_addr(2), false, true, T0 + 1);
    assert_eq!(e.current_anchors(), vec![sim_addr(2)]);
}

// ---- remembered across a restart, and dialled first -----------------------------------------------------------------------

/// An engine that has been connected to `honest` for a long time, and its saved state.
fn state_with(rig: &SimRig, honest: &[String]) -> Vec<u8> {
    let mut e = engine_on(
        rig,
        EngineConfig {
            anchor_min_age_ms: 10 * MIN,
            ..cfg()
        },
    );
    for (i, a) in honest.iter().enumerate() {
        connect(
            &mut e,
            rig,
            i as u64 + 1,
            a,
            false,
            true,
            T0 + 1000 * (i as u64 + 1),
        );
    }
    e.handle(T0 + 30 * MIN, Event::Tick);
    e.export_state()
}

#[test]
fn after_a_restart_the_anchors_are_dialled_before_anything_else_and_only_once() {
    let rigs = SimRig::rigs("anchors-restart", 2);
    let honest = vec![sim_addr(1), sim_addr(2)];
    let state = state_with(&rigs[0], &honest);
    // the restarted node: an address book and seeds that offer thirty addresses, all of them an attacker's
    let attackers: Vec<String> = (50..80).map(sim_addr).collect();
    let mut e = engine_on(
        &rigs[1],
        EngineConfig {
            seeds: attackers.clone(),
            ..cfg()
        },
    );
    e.import_state(&state).unwrap();
    assert_eq!(e.pending_anchors(), honest.as_slice());
    let first = connects(&e.handle(T0 + 40 * MIN, Event::Tick));
    assert!(first.len() > 2, "the node also dials others");
    assert_eq!(
        &first[..2],
        honest.as_slice(),
        "the anchors come first, in order"
    );
    assert!(first[2..].iter().all(|a| attackers.contains(a)));
    assert_eq!(e.stats.anchors_dialled, 2);
    assert!(e.pending_anchors().is_empty(), "each is tried once");
    // and not again, whatever happens to the connection
    e.handle(
        T0 + 40 * MIN + 1,
        Event::ConnectFailed {
            addr: honest[0].clone(),
        },
    );
    let later: Vec<String> = connects(&e.handle(T0 + 50 * MIN, Event::Tick));
    assert_eq!(e.stats.anchors_dialled, 2);
    let _ = later;
}

#[test]
fn without_anchors_the_same_restart_dials_only_the_attacker() {
    // the contrast that is the point: with the address book and seeds all an attacker's, and no anchors saved, nothing
    // honest is dialled
    let rigs = SimRig::rigs("anchors-contrast", 2);
    let honest = [sim_addr(1), sim_addr(2)];
    let mut old = engine_on(
        &rigs[0],
        EngineConfig {
            anchor_count: 0,
            anchor_min_age_ms: 10 * MIN,
            ..cfg()
        },
    );
    for (i, a) in honest.iter().enumerate() {
        connect(
            &mut old,
            &rigs[0],
            i as u64 + 1,
            a,
            false,
            true,
            T0 + 1000 * (i as u64 + 1),
        );
    }
    old.handle(T0 + 30 * MIN, Event::Tick);
    let state = old.export_state();
    let attackers: Vec<String> = (50..80).map(sim_addr).collect();
    let mut e = engine_on(
        &rigs[1],
        EngineConfig {
            seeds: attackers.clone(),
            ..cfg()
        },
    );
    e.import_state(&state).unwrap();
    assert!(e.pending_anchors().is_empty());
    let dials = connects(&e.handle(T0 + 40 * MIN, Event::Tick));
    assert!(!dials.is_empty());
    assert!(dials.iter().all(|a| attackers.contains(a)), "{dials:?}");
}

#[test]
fn an_anchor_on_a_host_already_connected_or_banned_is_not_dialled() {
    let rigs = SimRig::rigs("anchors-skip", 3);
    let honest = vec![sim_addr(1), sim_addr(2)];
    let state = state_with(&rigs[0], &honest);
    // 1. a peer from the first anchor's host is already connected (inbound, on another port)
    let mut e = engine_on(&rigs[1], cfg());
    e.import_state(&state).unwrap();
    let host = honest[0].rsplit_once(':').unwrap().0.to_string();
    connect(
        &mut e,
        &rigs[1],
        90,
        &format!("{host}:40000"),
        true,
        true,
        T0 + 35 * MIN,
    );
    let dials = connects(&e.handle(T0 + 40 * MIN, Event::Tick));
    assert!(
        !dials.contains(&honest[0]),
        "a host we are connected to is not dialled again"
    );
    assert!(dials.contains(&honest[1]));
    // 2. the second anchor's host is banned (it sent bytes that are not a message)
    let mut e = engine_on(&rigs[2], cfg());
    e.import_state(&state).unwrap();
    let host2 = honest[1].rsplit_once(':').unwrap().0.to_string();
    connect(
        &mut e,
        &rigs[2],
        91,
        &format!("{host2}:40001"),
        true,
        true,
        T0 + 35 * MIN,
    );
    e.handle(
        T0 + 35 * MIN + 5,
        Event::BadBytes {
            peer: 91,
            why: "test".into(),
        },
    );
    assert!(e.is_banned(&honest[1], T0 + 36 * MIN));
    let dials = connects(&e.handle(T0 + 40 * MIN, Event::Tick));
    assert!(!dials.contains(&honest[1]), "a banned host is not dialled");
    assert!(dials.contains(&honest[0]));
}

// ---- the saved file ------------------------------------------------------------------------------------------------------

#[test]
fn a_state_file_from_before_anchors_still_loads_and_has_none() {
    let rigs = SimRig::rigs("anchors-old", 2);
    let state = state_with(&rigs[0], &[sim_addr(1)]);
    // rebuild it as the old format: the same first two parts under the old magic
    assert_eq!(&state[..4], b"TNS2");
    let (mut pos, mut parts) = (4, vec![]);
    while pos < state.len() {
        let n = u32::from_le_bytes(state[pos..pos + 4].try_into().unwrap()) as usize;
        parts.push(state[pos..pos + 4 + n].to_vec());
        pos += 4 + n;
    }
    assert_eq!(parts.len(), 3);
    let mut old = b"TNS1".to_vec();
    old.extend_from_slice(&parts[0]);
    old.extend_from_slice(&parts[1]);
    let mut e = engine_on(&rigs[1], cfg());
    e.import_state(&old).unwrap();
    assert!(e.pending_anchors().is_empty());
}

#[test]
fn damage_anywhere_in_a_saved_state_is_refused_and_changes_nothing() {
    let rigs = SimRig::rigs("anchors-damage", 2);
    let state = state_with(&rigs[0], &[sim_addr(1), sim_addr(2)]);
    let mut e = engine_on(&rigs[1], cfg());
    for i in 0..state.len() {
        let mut d = state.clone();
        d[i] ^= 1;
        assert!(
            e.import_state(&d).is_err(),
            "a flip at byte {i} was accepted"
        );
    }
    for cut in 0..state.len() {
        assert!(e.import_state(&state[..cut]).is_err(), "cut at {cut}");
    }
    let mut t = state.clone();
    t.push(0);
    assert!(e.import_state(&t).is_err());
    assert!(
        e.pending_anchors().is_empty(),
        "nothing was changed by the refused ones"
    );
    // and the whole thing loads
    e.import_state(&state).unwrap();
    assert_eq!(e.pending_anchors().len(), 2);
}

#[test]
fn more_anchors_than_the_setting_allows_are_cut_and_none_are_kept_when_it_is_off() {
    let rigs = SimRig::rigs("anchors-cap", 3);
    let state = state_with(&rigs[0], &[sim_addr(1), sim_addr(2)]);
    let mut e = engine_on(
        &rigs[1],
        EngineConfig {
            anchor_count: 1,
            ..cfg()
        },
    );
    e.import_state(&state).unwrap();
    assert_eq!(e.pending_anchors(), [sim_addr(1)].as_slice());
    let mut e = engine_on(
        &rigs[2],
        EngineConfig {
            anchor_count: 0,
            ..cfg()
        },
    );
    e.import_state(&state).unwrap();
    assert!(e.pending_anchors().is_empty());
}
