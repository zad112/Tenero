//! The first-start bootstrap rules (M9, threat model C1; `docs/SEED_POLICY.md`): a fresh node dials only its seeds until every seed
//! group has answered (or a wait is over), and no one seed's descendants may fill more than their share of the first outbound slots.
//! These drive one engine directly with the time and the replies under the test's control. (`eclipse_sim.rs` measures what the
//! rules are worth against hostile seeds.)

use tenero_chain::{ProofsNotChecked, Sha256Pow};
use tenero_core::u256::U256;
use tenero_net::addrbook::{string_to_peer_addr, AddrBook, AddrBookConfig, MAX_REPORTERS};
use tenero_net::sim::{sim_addr, SimRig};
use tenero_net::{Action, Engine, EngineConfig, Event, Hello, Message, PeerId, PROTOCOL_VERSION};
use tenero_node::{Node, NodeConfig};

const T0: u64 = 1_700_000_000 * 1000;

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

fn cfg(seeds: &[String]) -> EngineConfig {
    EngineConfig {
        nonce: 7,
        seeds: seeds.to_vec(),
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

/// An outbound peer at `addr` connects and says hello (the engine then asks it for addresses).
fn connect(e: &mut Engine<'_>, rig: &SimRig, peer: PeerId, addr: &str, at: u64) {
    e.handle(
        at,
        Event::PeerConnected {
            peer,
            addr: addr.to_string(),
            inbound: false,
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

/// The peer answers the engine's address request with `list`.
fn answer(e: &mut Engine<'_>, peer: PeerId, list: &[String], at: u64) {
    let addrs = list
        .iter()
        .map(|a| string_to_peer_addr(a, at / 1000).unwrap())
        .collect();
    e.handle(
        at,
        Event::Message {
            peer,
            msg: Message::Addrs { addrs },
        },
    );
}

fn list(from: usize, n: usize) -> Vec<String> {
    (from..from + n).map(sim_addr).collect()
}

fn seeds3() -> Vec<String> {
    (1..=3).map(sim_addr).collect()
}

/// A fresh engine with `seeds3`, its first tick done and all three seeds connected.
fn three_seeds_connected<'a>(rig: &'a SimRig, c: EngineConfig) -> Engine<'a> {
    let mut e = engine_on(rig, c);
    let first = connects(&e.handle(T0, Event::Tick));
    assert_eq!(
        first.len(),
        3,
        "the first dials are the three seeds: {first:?}"
    );
    for (i, s) in seeds3().iter().enumerate() {
        connect(&mut e, rig, i as u64 + 1, s, T0 + 10 * (i as u64 + 1));
    }
    e
}

// ---- the wait ---------------------------------------------------------------------------------------------------------------

#[test]
fn a_fresh_node_dials_only_its_seeds_until_every_seed_group_has_answered() {
    let rig = SimRig::new("boot-wait", 0);
    let mut e = three_seeds_connected(&rig, cfg(&seeds3()));
    assert!(e.is_bootstrapping());
    // the first seed answers with twenty addresses: nothing is dialled yet
    answer(&mut e, 1, &list(100, 20), T0 + 1000);
    let acts = e.handle(T0 + 2000, Event::Tick);
    assert!(
        connects(&acts).is_empty(),
        "dialled before the others answered: {:?}",
        connects(&acts)
    );
    assert!(e.is_bootstrapping());
    // the second: still waiting for the third
    answer(&mut e, 2, &list(200, 20), T0 + 3000);
    let acts = e.handle(T0 + 4000, Event::Tick);
    assert!(connects(&acts).is_empty());
    assert!(e.is_bootstrapping());
    // the third: now the node dials from what all three told it
    answer(&mut e, 3, &list(300, 20), T0 + 5000);
    let acts = e.handle(T0 + 6000, Event::Tick);
    let dialled = connects(&acts);
    assert!(
        !dialled.is_empty(),
        "nothing was dialled after every seed answered"
    );
    assert!(!e.is_bootstrapping());
    assert_eq!(e.stats.bootstrap_started, 1);
    assert_eq!(e.stats.bootstrap_done, 1);
    assert_eq!(e.stats.bootstrap_timeouts, 0);
    // they were chosen from the three lists, not just the first
    let from = |lo: usize| dialled.iter().filter(|a| list(lo, 20).contains(a)).count();
    assert!(
        from(100) + from(200) + from(300) == dialled.len(),
        "{dialled:?}"
    );
}

#[test]
fn the_wait_ends_after_bootstrap_wait_ms_when_a_seed_never_answers() {
    let rig = SimRig::new("boot-timeout", 0);
    let c = EngineConfig {
        bootstrap_wait_ms: 20_000,
        ..cfg(&seeds3())
    };
    let mut e = three_seeds_connected(&rig, c);
    answer(&mut e, 1, &list(100, 20), T0 + 1000);
    // one nanosecond... one millisecond short of the wait: still only seeds
    let acts = e.handle(T0 + 19_999, Event::Tick);
    assert!(connects(&acts).is_empty());
    assert!(e.is_bootstrapping());
    assert_eq!(e.stats.bootstrap_timeouts, 0);
    // at the wait: it goes on with what it has
    let acts = e.handle(T0 + 20_000, Event::Tick);
    assert!(!connects(&acts).is_empty(), "the wait did not end");
    assert!(!e.is_bootstrapping());
    assert_eq!(e.stats.bootstrap_timeouts, 1);
    assert_eq!(e.stats.bootstrap_done, 0);
}

#[test]
fn a_wait_of_zero_turns_the_rule_off() {
    let rig = SimRig::new("boot-off", 0);
    let c = EngineConfig {
        bootstrap_wait_ms: 0,
        ..cfg(&seeds3())
    };
    let mut e = three_seeds_connected(&rig, c);
    assert!(!e.is_bootstrapping());
    answer(&mut e, 1, &list(100, 20), T0 + 1000);
    let acts = e.handle(T0 + 2000, Event::Tick);
    assert!(
        !connects(&acts).is_empty(),
        "with the rule off, the first answer is dialled at once"
    );
    assert_eq!(e.stats.bootstrap_started, 0);
}

#[test]
fn a_node_with_no_seeds_does_not_wait_for_anything() {
    let rig = SimRig::new("boot-noseeds", 0);
    let mut e = engine_on(&rig, cfg(&[]));
    e.handle(T0, Event::Tick);
    assert!(!e.is_bootstrapping());
    assert_eq!(e.stats.bootstrap_started, 0);
}

#[test]
fn only_seed_groups_count_so_seeds_in_one_group_are_one_answer() {
    // two seeds in the same /16 (the same network group) and one elsewhere: two answers (one per group) end the wait
    let rig = SimRig::new("boot-groups", 0);
    let seeds = vec![
        "20.5.1.1:8333".to_string(),
        "20.5.2.1:8333".to_string(),
        sim_addr(50),
    ];
    let mut e = engine_on(&rig, cfg(&seeds));
    assert_eq!(connects(&e.handle(T0, Event::Tick)).len(), 3);
    for (i, s) in seeds.iter().enumerate() {
        connect(&mut e, &rig, i as u64 + 1, s, T0 + 10 * (i as u64 + 1));
    }
    answer(&mut e, 1, &list(100, 20), T0 + 1000);
    let acts = e.handle(T0 + 2000, Event::Tick);
    assert!(connects(&acts).is_empty());
    // the second seed of the same group answers: still one group
    answer(&mut e, 2, &list(150, 20), T0 + 3000);
    e.handle(T0 + 4000, Event::Tick);
    assert!(
        e.is_bootstrapping(),
        "two seeds of one group counted as two"
    );
    answer(&mut e, 3, &list(300, 20), T0 + 5000);
    e.handle(T0 + 6000, Event::Tick);
    assert!(!e.is_bootstrapping());
}

#[test]
fn a_node_that_has_a_tried_address_from_an_earlier_run_does_not_wait() {
    let rig_a = SimRig::new("boot-restart-a", 0);
    let rig_b = SimRig::new("boot-restart-b", 0);
    let mut a = engine_on(&rig_a, cfg(&seeds3()));
    a.handle(T0, Event::Tick);
    connect(&mut a, &rig_a, 1, &sim_addr(1), T0 + 10);
    answer(&mut a, 1, &list(100, 20), T0 + 1000);
    let state = a.export_state();
    // the restarted node
    let mut b = engine_on(&rig_b, cfg(&seeds3()));
    b.import_state(&state).unwrap();
    let acts = b.handle(T0 + 5000, Event::Tick);
    assert!(!b.is_bootstrapping());
    assert_eq!(b.stats.bootstrap_started, 0);
    let dialled = connects(&acts);
    assert!(
        dialled.iter().any(|a| list(100, 20).contains(a)),
        "it did not use the addresses it remembered: {dialled:?}"
    );
}

// ---- the address book ---------------------------------------------------------------------------------------------------------

fn book(prefer: bool) -> AddrBook {
    AddrBook::new(AddrBookConfig {
        accept_private: false,
        prefer_corroborated: prefer,
        ..AddrBookConfig::default()
    })
}

#[test]
fn reporters_are_the_distinct_sources_up_to_the_most_and_seeds_do_not_count() {
    let mut b = book(false);
    b.add("20.1.1.1:8333", 0, "seed", 0);
    // a seed telling its own address again is not a second reporter
    b.add("20.1.1.1:8333", 0, "seed", 0);
    assert_eq!(b.get("20.1.1.1:8333").unwrap().reporters.len(), 1);
    for i in 0..10u8 {
        b.add("30.1.1.1:8333", 5, &format!("v4:9.{i}"), 5);
    }
    let r = &b.get("30.1.1.1:8333").unwrap().reporters;
    assert_eq!(r.len(), MAX_REPORTERS);
    // the same source twice counts once
    b.add("31.1.1.1:8333", 5, "v4:9.1", 5);
    b.add("31.1.1.1:8333", 5, "v4:9.1", 5);
    assert_eq!(b.get("31.1.1.1:8333").unwrap().reporters, vec!["v4:9.1"]);
}

fn firsts(prefer: bool) -> Vec<String> {
    let mut b = book(prefer);
    for (i, a) in [
        "20.1.1.1:8333",
        "21.1.1.1:8333",
        "22.1.1.1:8333",
        "23.1.1.1:8333",
    ]
    .iter()
    .enumerate()
    {
        b.add(a, 5, &format!("v4:9.{i}"), 5);
    }
    // the first of them is reported by a second source as well
    b.add("20.1.1.1:8333", 5, "v4:8.8", 5);
    (0..30)
        .map(|_| b.candidates(1_000_000, 1, &|_| false, &|_| false).remove(0))
        .collect()
}

#[test]
fn preferring_corroborated_addresses_puts_them_first_and_off_it_does_not() {
    assert!(firsts(true).iter().all(|a| a == "20.1.1.1:8333"));
    assert!(firsts(false).iter().any(|a| a != "20.1.1.1:8333"));
}

#[test]
fn candidates_can_be_limited_to_the_configured_seeds() {
    let mut b = book(false);
    b.add("20.1.1.1:8333", 0, "seed", 0);
    b.add("30.1.1.1:8333", 5, "v4:9.9", 5);
    b.add("31.1.1.1:8333", 5, "v4:9.9", 5);
    b.add("40.1.1.1:8333", 5, "v4:8.8", 5);
    let mut seeds = b.candidates_with(1_000_000, 10, &|_| false, &|_| false, true);
    seeds.sort();
    assert_eq!(seeds, vec!["20.1.1.1:8333"]);
    assert_eq!(
        b.candidates_with(1_000_000, 10, &|_| false, &|_| false, false)
            .len(),
        4
    );
}
