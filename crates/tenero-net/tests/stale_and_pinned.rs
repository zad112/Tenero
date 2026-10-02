//! A stale tip makes the node look for more peers, and pinned peers are always dialled (M9, threat model C1). One engine is
//! driven directly, with a clock the test moves.

use tenero_chain::{ProofsNotChecked, Sha256Pow};
use tenero_core::u256::U256;
use tenero_net::addrbook::group_of;
use tenero_net::sim::{mine_test_block, sim_addr, SimRig};
use tenero_net::{Action, Engine, EngineConfig, Event, Hello, Message, PeerId, PROTOCOL_VERSION};
use tenero_node::{Node, NodeConfig, Payout};

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

/// Settings in which ordinary dialling wants nothing (three outbound peers is the target), so any dial is the feature's.
fn quiet_cfg(seeds: Vec<String>) -> EngineConfig {
    EngineConfig {
        nonce: 7,
        peer_target: 3,
        outbound_target: 3,
        max_outbound_per_group: 2,
        seeds,
        // (feelers are another feature's dials: these tests count the dials of this one)
        feeler_interval_ms: 0,
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

fn connect(e: &mut Engine<'_>, rig: &SimRig, peer: PeerId, addr: &str, inbound: bool, at: u64) {
    e.handle(
        at,
        Event::PeerConnected {
            peer,
            addr: addr.to_string(),
            inbound,
        },
    );
    e.handle(
        at + 1,
        Event::Message {
            peer,
            msg: hello(rig, 100 + peer),
        },
    );
}

/// Three outbound peers, which is the target of `quiet_cfg`.
fn connect_three(e: &mut Engine<'_>, rig: &SimRig, at: u64) {
    for i in 1..=3u64 {
        connect(e, rig, i, &sim_addr(i as usize), false, at + i);
    }
}

/// A block on the engine's tip (mined on the test chain), so that the tip moves.
fn new_block(e: &mut Engine<'_>, at: u64) {
    let payout = Payout {
        onetime_address: [at as u8; 32],
        view_tag: [1; 3],
        ephemeral_pubkey: [2; 32],
        anchor_enc: [3; 16],
    };
    let b = mine_test_block(e.node(), at / 1000, payout);
    e.handle(at, Event::LocalBlock(b));
}

// ---- a stale tip ---------------------------------------------------------------------------------------------------------

#[test]
fn a_tip_that_stops_moving_makes_the_node_dial_extra_peers_from_new_groups_and_again_later() {
    let rig = SimRig::new("stale-dial", 0);
    // plenty of known addresses, each in a network group of its own
    let book: Vec<String> = (10..40).map(sim_addr).collect();
    let mut e = engine_on(&rig, quiet_cfg(book.clone()));
    let mine: Vec<String> = (1..=3).map(sim_addr).collect();
    for (i, a) in mine.iter().enumerate() {
        connect(&mut e, &rig, i as u64 + 1, a, false, T0 + 100 * i as u64);
    }
    // the first look starts the clock
    assert!(
        connects(&e.handle(T0 + 1000, Event::Tick)).is_empty(),
        "ordinary dialling wants nothing"
    );
    assert!(!e.is_tip_stale());
    assert!(
        connects(&e.handle(T0 + 9 * MIN, Event::Tick)).is_empty(),
        "not stale yet"
    );
    assert_eq!(e.stats.stale_tip_events, 0);
    // ten minutes without a block
    let first = connects(&e.handle(T0 + 11 * MIN, Event::Tick));
    assert!(e.is_tip_stale());
    assert_eq!(first.len(), 2, "two extra peers: {first:?}");
    assert_eq!(
        (e.stats.stale_tip_events, e.stats.stale_extra_dials),
        (1, 2)
    );
    let have: Vec<String> = mine.iter().map(|a| group_of(a)).collect();
    let groups: Vec<String> = first.iter().map(|a| group_of(a)).collect();
    assert!(
        groups.iter().all(|g| !have.contains(g)),
        "groups already in use were chosen: {groups:?}"
    );
    assert_ne!(groups[0], groups[1], "two from one group");
    assert!(first.iter().all(|a| book.contains(a)));
    // not again at once, and not before the retry time
    assert!(connects(&e.handle(T0 + 12 * MIN, Event::Tick)).is_empty());
    assert!(connects(&e.handle(T0 + 15 * MIN, Event::Tick)).is_empty());
    // after it, two more, and still new
    let second = connects(&e.handle(T0 + 17 * MIN, Event::Tick));
    assert_eq!(second.len(), 2);
    assert!(
        second.iter().all(|a| !first.contains(a)),
        "the same peers again"
    );
    assert_eq!(
        e.stats.stale_tip_events, 1,
        "one episode, however many attempts"
    );
    assert_eq!(e.stats.stale_extra_dials, 4);
}

#[test]
fn a_new_block_ends_the_stale_state() {
    let rig = SimRig::new("stale-ends", 0);
    let mut e = engine_on(&rig, quiet_cfg((10..30).map(sim_addr).collect()));
    connect(&mut e, &rig, 1, &sim_addr(1), false, T0);
    e.handle(T0 + 1000, Event::Tick);
    e.handle(T0 + 11 * MIN, Event::Tick);
    assert!(e.is_tip_stale());
    new_block(&mut e, T0 + 12 * MIN);
    e.handle(T0 + 12 * MIN + 500, Event::Tick);
    assert!(!e.is_tip_stale(), "a new tip");
    assert!(e.tip_age_ms() < MIN);
    // and a second stale period is a second episode
    e.handle(T0 + 25 * MIN, Event::Tick);
    assert!(e.is_tip_stale());
    assert_eq!(e.stats.stale_tip_events, 2);
}

#[test]
fn a_tip_that_keeps_moving_never_dials_extra_peers() {
    let rig = SimRig::new("stale-moving", 0);
    let mut e = engine_on(&rig, quiet_cfg((10..30).map(sim_addr).collect()));
    connect_three(&mut e, &rig, T0);
    let mut t = T0 + 1000;
    for _ in 0..30 {
        // a block every five minutes, a look every minute
        for _ in 0..5 {
            t += MIN;
            // (the silent test peers are dropped for not answering pings, so ordinary dialling goes on: only the stale
            // feature's own counters are looked at)
            e.handle(t, Event::Tick);
            assert!(!e.is_tip_stale());
        }
        new_block(&mut e, t);
    }
    assert_eq!(
        (e.stats.stale_tip_events, e.stats.stale_extra_dials),
        (0, 0)
    );
}

#[test]
fn the_feature_can_be_turned_off_and_never_goes_past_the_peer_limit() {
    // off
    let rig = SimRig::new("stale-off", 0);
    let cfg = EngineConfig {
        stale_tip_ms: 0,
        ..quiet_cfg((10..30).map(sim_addr).collect())
    };
    let mut e = engine_on(&rig, cfg);
    connect_three(&mut e, &rig, T0);
    e.handle(T0 + 1000, Event::Tick);
    assert!(connects(&e.handle(T0 + 60 * MIN, Event::Tick)).is_empty());
    assert!(!e.is_tip_stale());
    // zero extra peers: stale is noticed but nothing is dialled
    let rig2 = SimRig::new("stale-off", 1);
    let cfg = EngineConfig {
        stale_extra_outbound: 0,
        ..quiet_cfg((10..30).map(sim_addr).collect())
    };
    let mut e = engine_on(&rig2, cfg);
    connect_three(&mut e, &rig2, T0);
    e.handle(T0 + 1000, Event::Tick);
    assert!(connects(&e.handle(T0 + 60 * MIN, Event::Tick)).is_empty());
    // no room: the limit is held
    let rig3 = SimRig::new("stale-off", 2);
    let cfg = EngineConfig {
        max_peers: 3,
        ..quiet_cfg((10..30).map(sim_addr).collect())
    };
    let mut e = engine_on(&rig3, cfg);
    for i in 1..=3u64 {
        connect(&mut e, &rig3, i, &sim_addr(i as usize), false, T0 + i);
    }
    e.handle(T0 + 1000, Event::Tick);
    assert!(
        connects(&e.handle(T0 + 60 * MIN, Event::Tick)).is_empty(),
        "past max_peers"
    );
    // some room: only as many as fit
    let rig4 = SimRig::new("stale-off", 3);
    let cfg = EngineConfig {
        max_peers: 4,
        stale_extra_outbound: 3,
        ..quiet_cfg((10..30).map(sim_addr).collect())
    };
    let mut e = engine_on(&rig4, cfg);
    for i in 1..=3u64 {
        connect(&mut e, &rig4, i, &sim_addr(i as usize), false, T0 + i);
    }
    e.handle(T0 + 1000, Event::Tick);
    assert_eq!(connects(&e.handle(T0 + 60 * MIN, Event::Tick)).len(), 1);
}

#[test]
fn many_known_addresses_in_a_group_already_in_use_do_not_crowd_out_the_new_groups() {
    // forty known addresses share the group of one of our outbound peers; two others are in groups of their own (the candidates
    // are drawn at random, so without the filter two good ones would almost always be lost among the forty)
    let rig = SimRig::new("stale-crowd", 0);
    let same_group: Vec<String> = (0..40)
        .map(|i| tenero_net::sim::sim_addr_in(i, 100))
        .collect();
    let others: Vec<String> = (100..102).map(sim_addr).collect();
    let seeds: Vec<String> = same_group.iter().chain(others.iter()).cloned().collect();
    let mut e = engine_on(&rig, quiet_cfg(seeds));
    let mine = [
        tenero_net::sim::sim_addr_in(50, 100),
        sim_addr(1),
        sim_addr(2),
    ];
    assert_eq!(
        group_of(&mine[0]),
        group_of(&same_group[0]),
        "the test needs one group"
    );
    for (i, a) in mine.iter().enumerate() {
        connect(&mut e, &rig, i as u64 + 1, a, false, T0 + i as u64);
    }
    e.handle(T0 + 1000, Event::Tick);
    let dials = connects(&e.handle(T0 + 11 * MIN, Event::Tick));
    assert_eq!(dials.len(), 2, "{dials:?}");
    assert!(
        dials.iter().all(|a| others.contains(a)),
        "a crowded group was chosen: {dials:?}"
    );
}

#[test]
fn only_groups_the_node_has_no_outbound_peer_in_are_tried() {
    // every known address is in a group already in use: nothing to add
    let rig = SimRig::new("stale-groups", 0);
    let mine: Vec<String> = (1..=3).map(sim_addr).collect();
    let mut e = engine_on(&rig, quiet_cfg(mine.clone()));
    for (i, a) in mine.iter().enumerate() {
        connect(&mut e, &rig, i as u64 + 1, a, false, T0 + i as u64);
    }
    e.handle(T0 + 1000, Event::Tick);
    assert!(connects(&e.handle(T0 + 11 * MIN, Event::Tick)).is_empty());
    assert!(e.is_tip_stale(), "stale, with nowhere new to look");
}

// ---- pinned peers -------------------------------------------------------------------------------------------------------

fn pinned() -> Vec<String> {
    vec![sim_addr(90), sim_addr(91)]
}

#[test]
fn pinned_peers_are_dialled_first_and_again_when_they_drop_but_not_too_often() {
    let rig = SimRig::new("pinned-first", 0);
    let seeds: Vec<String> = (10..40).map(sim_addr).collect();
    let cfg = EngineConfig {
        nonce: 7,
        seeds: seeds.clone(),
        trusted: pinned(),
        ..EngineConfig::default()
    };
    let mut e = engine_on(&rig, cfg);
    let first = connects(&e.handle(T0, Event::Tick));
    assert_eq!(&first[..2], pinned().as_slice(), "the pinned peers lead");
    assert!(first.len() > 2 && first[2..].iter().all(|a| seeds.contains(a)));
    assert_eq!(e.stats.trusted_dialled, 2);
    // being dialled, they are not dialled again
    assert!(!connects(&e.handle(T0 + 1000, Event::Tick))
        .iter()
        .any(|a| pinned().contains(a)));
    // one connects and then drops; the other never answers
    connect(&mut e, &rig, 1, &pinned()[0], false, T0 + 2000);
    e.handle(T0 + 5000, Event::PeerDisconnected { peer: 1 });
    e.handle(
        T0 + 5000,
        Event::ConnectFailed {
            addr: pinned()[1].clone(),
        },
    );
    // before the retry time: nothing; after it: both again
    assert!(!connects(&e.handle(T0 + 10_000, Event::Tick))
        .iter()
        .any(|a| pinned().contains(a)));
    let again = connects(&e.handle(T0 + 31_000, Event::Tick));
    assert!(
        again.contains(&pinned()[0]) && again.contains(&pinned()[1]),
        "{again:?}"
    );
    assert_eq!(e.stats.trusted_dialled, 4);
}

#[test]
fn a_pinned_peer_is_dialled_even_when_the_node_wants_no_more_and_beyond_the_group_limit() {
    let rig = SimRig::new("pinned-exempt", 0);
    // the target is met (one outbound peer), and two outbound peers are already in the pinned peer's group
    let pin = tenero_net::sim::sim_addr_in(7, 3); // a group shared with 6 and 8
    let cfg = EngineConfig {
        nonce: 7,
        peer_target: 1,
        outbound_target: 1,
        max_outbound_per_group: 1,
        trusted: vec![pin.clone()],
        ..EngineConfig::default()
    };
    let mut e = engine_on(&rig, cfg);
    let neighbour = tenero_net::sim::sim_addr_in(6, 3);
    assert_eq!(
        group_of(&pin),
        group_of(&neighbour),
        "the test needs one group"
    );
    connect(&mut e, &rig, 1, &neighbour, false, T0);
    let dials = connects(&e.handle(T0 + 1000, Event::Tick));
    assert_eq!(
        dials,
        vec![pin],
        "dialled although the target is met and the group is full"
    );
}

#[test]
fn a_pinned_peer_is_not_dialled_when_banned_connected_or_the_node_is_full_and_is_never_passed_on() {
    let rig = SimRig::new("pinned-skip", 0);
    let p = pinned();
    let cfg = EngineConfig {
        nonce: 7,
        peer_target: 1,
        trusted: p.clone(),
        ..EngineConfig::default()
    };
    let mut e = engine_on(&rig, cfg);
    // the first is already connected (inbound, another port of the same host)
    let host0 = p[0].rsplit_once(':').unwrap().0.to_string();
    connect(&mut e, &rig, 1, &format!("{host0}:40000"), true, T0);
    // the second misbehaves, is banned, and is therefore not dialled
    let host1 = p[1].rsplit_once(':').unwrap().0.to_string();
    connect(&mut e, &rig, 2, &format!("{host1}:40001"), true, T0 + 10);
    e.handle(
        T0 + 20,
        Event::BadBytes {
            peer: 2,
            why: "test".into(),
        },
    );
    assert!(e.is_banned(&p[1], T0 + 1000));
    let dials = connects(&e.handle(T0 + 60_000, Event::Tick));
    assert!(!dials.iter().any(|a| p.contains(a)), "{dials:?}");
    assert_eq!(e.stats.trusted_dialled, 0);
    // never in the address book, and so never in what is saved or told to others
    assert!(!e.addr_book().addrs().iter().any(|a| p.contains(a)));
    let state = e.export_state();
    for a in &p {
        assert!(
            !state.windows(a.len()).any(|w| w == a.as_bytes()),
            "{a} is in the saved state"
        );
    }
}

#[test]
fn the_peer_limit_holds_for_pinned_peers_too() {
    let rig = SimRig::new("pinned-limit", 0);
    let cfg = EngineConfig {
        nonce: 7,
        max_peers: 2,
        trusted: pinned(),
        ..EngineConfig::default()
    };
    let mut e = engine_on(&rig, cfg);
    connect(&mut e, &rig, 1, &sim_addr(1), false, T0);
    connect(&mut e, &rig, 2, &sim_addr(2), false, T0 + 1);
    assert!(connects(&e.handle(T0 + 2000, Event::Tick)).is_empty());
}
