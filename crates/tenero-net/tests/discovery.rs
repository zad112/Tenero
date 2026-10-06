//! Peer discovery and connection management (M8.3a): finding peers from seeds, keeping 50 or more connections,
//! replacing lost ones, and not being steered by an attacker's addresses. Simulated networks of up to 120 nodes,
//! plus tests that drive one engine directly.

use std::collections::{HashMap, HashSet};

use tenero_chain::{ProofsNotChecked, Sha256Pow};
use tenero_core::u256::U256;
use tenero_net::addrbook::{group_of, string_to_peer_addr, v4};
use tenero_net::message::PeerAddr;
use tenero_net::sim::{sim_addr, Sim, SimConfig, SimRig};
use tenero_net::{Action, Engine, EngineConfig, Event, Hello, Message, PROTOCOL_VERSION};
use tenero_node::{Node, NodeConfig};

const T0: u64 = 1_700_000_000;
const SEC: u64 = 1000;

fn cfg(target: usize, seeds: &[usize]) -> EngineConfig {
    EngineConfig {
        peer_target: target,
        outbound_target: target.min(8),
        seeds: seeds.iter().map(|&i| sim_addr(i)).collect(),
        ..EngineConfig::default()
    }
}

fn hello_from(rig: &SimRig) -> Message {
    hello_with_nonce(rig, 0)
}

fn hello_with_nonce(rig: &SimRig, nonce: u64) -> Message {
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

fn connects(actions: &[Action]) -> Vec<String> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Connect { addr } => Some(addr.clone()),
            _ => None,
        })
        .collect()
}

/// Answers, for scripted listeners, whichever node dials them: a handshake, then `payload` as the answer to
/// `GetAddrs`.
struct Script {
    hello: HashSet<usize>,
    answered: HashSet<usize>,
}

impl Script {
    fn new() -> Script {
        Script {
            hello: HashSet::new(),
            answered: HashSet::new(),
        }
    }

    fn tick(&mut self, sim: &mut Sim<'_>, rig: &SimRig, listeners: &[(usize, Vec<PeerAddr>)]) {
        for (h, payload) in listeners {
            if sim.hostiles[*h].disconnected {
                self.hello.remove(h);
                self.answered.remove(h);
                continue;
            }
            if self.hello.insert(*h) {
                sim.hostile_send(*h, hello_from(rig));
            }
            if !self.answered.contains(h)
                && sim.hostiles[*h]
                    .inbox
                    .iter()
                    .any(|m| matches!(m, Message::GetAddrs))
            {
                sim.hostile_send(
                    *h,
                    Message::Addrs {
                        addrs: payload.clone(),
                    },
                );
                self.answered.insert(*h);
            }
        }
    }
}

fn score(sim: &Sim<'_>, h: usize) -> Option<u32> {
    let peer = sim.hostiles[h].peer_at_node()?;
    sim.engines[sim.hostiles[h].node].peer_score(peer)
}

// ---- finding peers from seeds ------------------------------------------------------------------------

#[test]
fn a_node_that_knows_only_seeds_finds_the_network() {
    let n = 30;
    let rigs = SimRig::rigs("boot", n);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), cfg(8, &[0, 1]));
    sim.run_for(300 * SEC);
    for i in 0..n {
        assert!(
            sim.engines[i].peer_count() >= 8,
            "node {i} has only {} peers",
            sim.engines[i].peer_count()
        );
        // a node that is not a seed chose peers of its own
        if i >= 2 {
            assert!(
                sim.engines[i].outbound_count() >= 4,
                "node {i} has only {} outbound peers",
                sim.engines[i].outbound_count()
            );
        }
    }
    // the network works: a block from anywhere reaches everyone
    sim.mine(17, None);
    assert!(sim.run_until(120 * SEC, |s| s.all_agree()));
}

#[test]
fn every_node_of_a_120_node_network_reaches_50_peers() {
    let n = 120;
    let seeds = 5;
    let rigs = SimRig::rigs("fifty", n);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), cfg(50, &[0, 1, 2, 3, 4]));
    let reached = sim.run_until(1500 * SEC, |s| {
        (0..n).all(|i| s.engines[i].peer_count() >= 50)
    });
    let total: Vec<usize> = (0..n).map(|i| sim.engines[i].peer_count()).collect();
    let outbound: Vec<usize> = (0..n).map(|i| sim.engines[i].outbound_count()).collect();
    eprintln!(
        "peers: min {} max {}; outbound: min {} (non-seeds) max {}; simulated time {} s",
        total.iter().min().unwrap(),
        total.iter().max().unwrap(),
        outbound[seeds..].iter().min().unwrap(),
        outbound.iter().max().unwrap(),
        (sim.now_ms() / 1000) - T0
    );
    assert!(reached, "some node stayed below 50 peers: {total:?}");
    for (i, out) in outbound.iter().enumerate() {
        assert_eq!(
            sim.engines[i].stats.bans, 0,
            "node {i} banned an honest peer"
        );
        // every ordinary node also chose at least the minimum number of peers itself
        if i >= seeds {
            assert!(*out >= 8, "node {i}: {out}");
        }
    }
    // and 50 or more connections still make one working network
    sim.mine(77, None);
    assert!(sim.run_until(120 * SEC, |s| s.all_agree()));
    // a block body is still sent once per node, however many peers each has
    assert_eq!(sim.sent_by_kind["blocks"], (n - 1) as u64);
}

#[test]
fn lost_peers_are_replaced_and_returning_nodes_find_peers_again() {
    let n = 40;
    let rigs = SimRig::rigs("churn", n);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), cfg(10, &[0, 1]));
    sim.run_for(300 * SEC);
    assert!((0..n).all(|i| sim.engines[i].peer_count() >= 10));
    // 12 nodes (30%) vanish
    let gone: Vec<usize> = (20..32).collect();
    for &g in &gone {
        sim.set_online(g, false);
    }
    assert!(
        sim.run_until(600 * SEC, |s| (0..n)
            .filter(|i| s.is_online(*i))
            .all(|i| s.engines[i].peer_count() >= 10)),
        "the survivors did not refill their peer slots"
    );
    // they come back: their own address books find peers again
    for &g in &gone {
        sim.set_online(g, true);
    }
    assert!(
        sim.run_until(600 * SEC, |s| gone
            .iter()
            .all(|&g| s.engines[g].peer_count() >= 10)),
        "the returning nodes did not reconnect"
    );
}

#[test]
fn a_partition_heals_without_anyone_being_told_to_reconnect() {
    let n = 30;
    let rigs = SimRig::rigs("healquiet", n);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), cfg(8, &[0, 15]));
    sim.run_for(300 * SEC);
    sim.partition(&[(0..15).collect(), (15..30).collect()]);
    sim.run_for(300 * SEC);
    sim.mine(3, None);
    sim.mine(20, None);
    sim.run_for(120 * SEC);
    assert_eq!(
        sim.distinct_tips(),
        2,
        "the two halves each have their own block"
    );
    sim.heal_quiet();
    // the tips differ, and connections across the old split come only from the nodes' own dialling
    assert!(
        sim.run_until(1200 * SEC, |s| {
            let across = (0..15).any(|i| {
                s.engines[i]
                    .outbound_addrs()
                    .iter()
                    .any(|a| (15..30).any(|j| a == s.addr_of(j)))
            });
            across
        }),
        "nobody dialled across the healed partition"
    );
}

// ---- attackers -------------------------------------------------------------------------------------

#[test]
fn fake_addresses_from_malicious_peers_cannot_crowd_out_real_ones() {
    let n = 31;
    let rigs = SimRig::rigs("eclipse", n);
    let hostile_addrs = ["70.1.0.1:8333", "70.2.0.1:8333", "70.3.0.1:8333"];
    let mut configs = vec![cfg(8, &[1, 2]); n];
    // the victim's seeds: two honest nodes and three attackers
    configs[0].seeds = vec![sim_addr(1), sim_addr(2)]
        .into_iter()
        .chain(hostile_addrs.iter().map(|s| s.to_string()))
        .collect();
    let mut sim = Sim::with_configs(&rigs, T0, SimConfig::default(), configs);
    let mut listeners = Vec::new();
    for (k, a) in hostile_addrs.iter().enumerate() {
        let h = sim.add_hostile_listener(a);
        // 100 plausible addresses that nobody listens on, all in different network groups
        let fake: Vec<PeerAddr> = (0..100)
            .map(|i| string_to_peer_addr(&v4(80 + k as u8, 1 + i as u8, 1, 1, 8333), T0).unwrap())
            .collect();
        listeners.push((h, fake));
    }
    let mut script = Script::new();
    for _ in 0..600 {
        sim.run_for(SEC);
        script.tick(&mut sim, &rigs[0], &listeners);
    }
    let book = sim.engines[0].addr_book();
    let fake_total: usize = hostile_addrs
        .iter()
        .map(|a| book.from_source(&group_of(a)))
        .sum();
    for a in &hostile_addrs {
        assert!(
            book.from_source(&group_of(a)) <= 64,
            "one source filled {} entries",
            book.from_source(&group_of(a))
        );
    }
    assert!(
        fake_total >= 100,
        "the attack should really have reached the book ({fake_total} fake entries)"
    );
    // the victim is still connected to the real network
    let honest_out = sim.engines[0]
        .outbound_addrs()
        .iter()
        .filter(|a| (1..n).any(|j| **a == sim_addr(j)))
        .count();
    assert!(
        honest_out >= 5,
        "only {honest_out} honest outbound peers of the 8 wanted"
    );
    assert!(book.len() > fake_total, "and it learned real addresses too");
}

#[test]
fn a_crowded_network_group_gets_only_its_share_of_outbound_slots() {
    let n = 50;
    let rigs = SimRig::rigs("crowd", n);
    let simcfg = SimConfig {
        nodes_per_group: 10,
        ..SimConfig::default()
    };
    let seeds: Vec<String> = (0..n)
        .map(|i| tenero_net::sim::sim_addr_in(i, 10))
        .collect();
    let mut c = cfg(50, &[]);
    c.outbound_target = 20;
    c.seeds = seeds;
    let mut sim = Sim::new(&rigs, T0, simcfg, c);
    sim.run_for(600 * SEC);
    let mut per_group: HashMap<String, usize> = HashMap::new();
    for a in sim.engines[0].outbound_addrs() {
        *per_group.entry(group_of(&a)).or_default() += 1;
    }
    assert!(
        per_group.values().all(|&c| c <= 2),
        "more than 2 outbound peers in one group: {per_group:?}"
    );
    assert_eq!(per_group.len(), 5, "every group of 10 nodes is reached");
    assert_eq!(sim.engines[0].outbound_count(), 10, "2 in each of 5 groups");
}

#[test]
fn junk_addresses_in_an_answer_are_filtered_and_dates_are_clamped() {
    let rigs = SimRig::rigs("junk", 1);
    let h_addr = "71.1.0.1:8333";
    let mut c = cfg(1, &[]);
    c.seeds = vec![h_addr.to_string()];
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), c);
    let h = sim.add_hostile_listener(h_addr);
    let month = 30 * 24 * 3600;
    let entries: Vec<(&str, u64)> = vec![
        ("81.1.1.1:8333", T0 - 10),          // fine
        ("10.0.0.5:8333", T0),               // private
        ("127.0.0.1:8333", T0),              // loopback
        ("81.2.2.2:0", T0),                  // port 0
        ("0.0.0.0:8333", T0),                // unspecified
        ("81.3.3.3:8333", T0 + 999_999),     // claims the future
        ("81.4.4.4:8333", T0 - month - 500), // stale
        ("[2a00:1450::1]:8333", T0 - 100),   // a public IPv6 address
        ("[fe80::1]:8333", T0),              // link-local IPv6
    ];
    let payload: Vec<PeerAddr> = entries
        .iter()
        .map(|(a, t)| string_to_peer_addr(a, *t).unwrap())
        .collect();
    let mut script = Script::new();
    for _ in 0..30 {
        sim.run_for(SEC);
        script.tick(&mut sim, &rigs[0], &[(h, payload.clone())]);
    }
    let book = sim.engines[0].addr_book();
    for good in ["81.1.1.1:8333", "81.3.3.3:8333", "[2a00:1450::1]:8333"] {
        assert!(book.get(good).is_some(), "{good} should be in the book");
    }
    for bad in [
        "10.0.0.5:8333",
        "127.0.0.1:8333",
        "81.2.2.2:0",
        "0.0.0.0:8333",
        "81.4.4.4:8333",
        "[fe80::1]:8333",
    ] {
        assert!(book.get(bad).is_none(), "{bad} must not be in the book");
    }
    let future = book.get("81.3.3.3:8333").unwrap();
    assert!(
        future.last_seen <= sim.now_ms() / 1000,
        "a claim about the future is clamped to now"
    );
}

#[test]
fn addresses_are_answered_once_and_unsolicited_ones_punished() {
    let rigs = SimRig::rigs("once", 1);
    let l_addr = "72.1.0.1:8333";
    let mut c = cfg(1, &[]);
    c.seeds = vec![l_addr.to_string()];
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), c);
    // a scripted listener teaches the node ten fresh addresses
    let l = sim.add_hostile_listener(l_addr);
    let fresh: Vec<PeerAddr> = (1..=10u8)
        .map(|i| string_to_peer_addr(&v4(60 + i, 1, 1, 1, 8333), T0).unwrap())
        .collect();
    let mut script = Script::new();
    for _ in 0..20 {
        sim.run_for(SEC);
        script.tick(&mut sim, &rigs[0], &[(l, fresh.clone())]);
    }
    assert_eq!(sim.engines[0].addr_book().len(), 11);
    let h = sim.add_hostile(0, "82.1.1.1:41000");
    sim.hostile_send(h, hello_from(&rigs[0]));
    sim.run_for(SEC);
    // the first GetAddrs is answered with what the node knows (the ten, and the listener that proved itself)
    sim.hostile_send(h, Message::GetAddrs);
    sim.run_for(SEC);
    let answers: Vec<usize> = sim.hostiles[h]
        .inbox
        .iter()
        .filter_map(|m| match m {
            Message::Addrs { addrs } => Some(addrs.len()),
            _ => None,
        })
        .collect();
    // (the first message is the node announcing its own address)
    assert_eq!(answers, vec![1, 11]);
    // the second is a nuisance
    sim.hostile_send(h, Message::GetAddrs);
    sim.run_for(SEC);
    assert_eq!(score(&sim, h), Some(10));
    // addresses nobody asked for
    let two = vec![
        string_to_peer_addr("83.1.1.1:8333", T0).unwrap(),
        string_to_peer_addr("83.1.1.2:8333", T0).unwrap(),
    ];
    sim.hostile_send(h, Message::Addrs { addrs: two });
    sim.run_for(SEC);
    assert_eq!(score(&sim, h), Some(30));
    assert!(sim.engines[0].addr_book().get("83.1.1.1:8333").is_none());
    // its own address is welcome, once, if it is the host it connected from
    let own = string_to_peer_addr("82.1.1.1:8333", T0).unwrap();
    sim.hostile_send(h, Message::Addrs { addrs: vec![own] });
    sim.run_for(SEC);
    assert_eq!(score(&sim, h), Some(30), "no penalty for announcing itself");
    assert!(sim.engines[0].addr_book().get("82.1.1.1:8333").is_some());
    sim.hostile_send(h, Message::Addrs { addrs: vec![own] });
    sim.run_for(SEC);
    assert_eq!(score(&sim, h), Some(50), "but only once");
    // another host's address is not its own to announce
    let other = string_to_peer_addr("84.9.9.9:8333", T0).unwrap();
    sim.hostile_send(h, Message::Addrs { addrs: vec![other] });
    sim.run_for(SEC);
    assert_eq!(score(&sim, h), Some(70));
    assert!(sim.engines[0].addr_book().get("84.9.9.9:8333").is_none());
}

#[test]
fn too_many_addresses_in_one_message_is_a_violation() {
    let rigs = SimRig::rigs("toomany", 1);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), cfg(0, &[]));
    let h = sim.add_hostile(0, "85.1.1.1:41000");
    sim.hostile_send(h, hello_from(&rigs[0]));
    let addrs: Vec<PeerAddr> = (0..101)
        .map(|i| string_to_peer_addr(&v4(86, 1, 1, i as u8 + 1, 8333), T0).unwrap())
        .collect();
    sim.hostile_send(
        h,
        Message::Addrs {
            addrs: addrs.clone(),
        },
    );
    sim.hostile_send(h, Message::Addrs { addrs });
    sim.run_for(3 * SEC);
    assert!(
        sim.hostiles[h].disconnected,
        "two oversized answers: 50 + 50"
    );
}

#[test]
fn a_ban_applies_to_the_host_whatever_port_it_connects_from() {
    let rigs = SimRig::rigs("banhost", 1);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), cfg(0, &[]));
    let a = sim.add_hostile(0, "99.1.1.1:5000");
    let Message::Hello(mut bad) = hello_from(&rigs[0]) else {
        unreachable!()
    };
    bad.chain_id = [7; 32];
    sim.hostile_send(a, Message::Hello(bad));
    sim.run_for(2 * SEC);
    assert!(sim.hostiles[a].disconnected);
    // the same host, a different port
    let b = sim.add_hostile(0, "99.1.1.1:6000");
    sim.run_for(SEC);
    assert!(
        sim.hostiles[b].disconnected,
        "the ban is on the host, not the port"
    );
    // another host is fine
    let c = sim.add_hostile(0, "99.1.1.2:5000");
    sim.run_for(SEC);
    assert!(!sim.hostiles[c].disconnected);
}

// ---- state that survives a restart -----------------------------------------------------------------

#[test]
fn the_address_book_and_the_bans_survive_a_restart() {
    let n = 20;
    let rigs = SimRig::rigs("restart", n);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), cfg(6, &[0]));
    sim.run_for(300 * SEC);
    let bad = sim.add_hostile(5, "99.9.9.9:1");
    let Message::Hello(mut wrong) = hello_from(&rigs[5]) else {
        unreachable!()
    };
    wrong.chain_id = [8; 32];
    sim.hostile_send(bad, Message::Hello(wrong));
    sim.run_for(3 * SEC);
    let known = sim.engines[5].addr_book().addrs();
    assert!(
        known.len() > 6,
        "the node learned {} addresses",
        known.len()
    );
    let state = sim.engines[5].export_state();

    // a new engine with NO seeds, loaded from what was saved
    let mut c = cfg(6, &[]);
    c.seeds.clear();
    let mut fresh = engine_on(&rigs[5], c.clone());
    assert!(fresh.addr_book().is_empty());
    fresh.import_state(&state).expect("a good state loads");
    let mut restored = fresh.addr_book().addrs();
    let mut want = known.clone();
    restored.sort();
    want.sort();
    assert_eq!(restored, want);
    assert!(
        fresh.is_banned("99.9.9.9:12345", T0 * 1000 + 10_000),
        "the ban survived too"
    );
    // and it dials from the loaded book, with no seed
    let actions = fresh.handle(T0 * 1000 + 20_000, Event::Tick);
    let dials = connects(&actions);
    assert!(!dials.is_empty() && dials.iter().all(|d| known.contains(d)));

    // damage anywhere is refused and changes nothing
    let mut untouched = engine_on(&rigs[5], c);
    for i in (0..state.len()).step_by(3) {
        let mut damaged = state.clone();
        damaged[i] ^= 1;
        assert!(
            untouched.import_state(&damaged).is_err(),
            "a flip at byte {i} was accepted"
        );
    }
    for cut in [0, 3, 11, state.len() / 2, state.len() - 1] {
        assert!(untouched.import_state(&state[..cut]).is_err());
    }
    let mut trailing = state.clone();
    trailing.push(0);
    assert!(untouched.import_state(&trailing).is_err());
    assert!(untouched.addr_book().is_empty());
    assert!(!untouched.is_banned("99.9.9.9:1", T0 * 1000));
}

// ---- one engine, driven directly ----------------------------------------------------------------------

fn seeds_in_groups(n: u8) -> Vec<String> {
    (0..n).map(|i| v4(50 + i, 1, 1, 1, 8333)).collect()
}

#[test]
fn dialling_is_bounded_times_out_and_backs_off() {
    let rigs = SimRig::rigs("dial", 1);
    let mut c = cfg(3, &[]);
    c.seeds = seeds_in_groups(6);
    let mut e = engine_on(&rigs[0], c);
    let t = T0 * 1000;
    // the first tick starts exactly the target number of dials
    let first: HashSet<String> = connects(&e.handle(t + 1000, Event::Tick))
        .into_iter()
        .collect();
    assert_eq!(first.len(), 3);
    assert_eq!(e.connecting_count(), 3);
    // nothing more while three are outstanding
    assert!(connects(&e.handle(t + 2000, Event::Tick)).is_empty());
    // one fails at once: its slot is refilled from the addresses not yet tried
    let failed = first.iter().next().unwrap().clone();
    e.handle(
        t + 2500,
        Event::ConnectFailed {
            addr: failed.clone(),
        },
    );
    assert_eq!(e.connecting_count(), 2);
    let refill = connects(&e.handle(t + 3000, Event::Tick));
    assert_eq!(refill.len(), 1);
    assert!(!first.contains(&refill[0]), "not one already being dialled");
    // the others are never answered: after the timeout they count as failed and the rest are tried
    let second: HashSet<String> = connects(&e.handle(t + 13_000, Event::Tick))
        .into_iter()
        .collect();
    let all: HashSet<String> = seeds_in_groups(6).into_iter().collect();
    let expected: HashSet<String> = all
        .iter()
        .filter(|a| !first.contains(*a) && **a != refill[0])
        .cloned()
        .collect();
    assert_eq!(
        second, expected,
        "only addresses never tried, none backing off"
    );
    assert_eq!(
        e.connecting_count(),
        3,
        "the refill dial (not yet timed out) and the two new ones"
    );
    // an address that failed is not tried again before its backoff has passed (30 s after the failure)
    let mut e2 = engine_on(&rigs[0], {
        let mut c = cfg(1, &[]);
        c.seeds = seeds_in_groups(1);
        c
    });
    let a = connects(&e2.handle(t + 1000, Event::Tick));
    assert_eq!(a.len(), 1);
    e2.handle(t + 2000, Event::ConnectFailed { addr: a[0].clone() });
    assert!(
        connects(&e2.handle(t + 31_999, Event::Tick)).is_empty(),
        "still backing off"
    );
    assert_eq!(connects(&e2.handle(t + 32_000, Event::Tick)), a);
    e2.handle(t + 33_000, Event::ConnectFailed { addr: a[0].clone() });
    assert!(
        connects(&e2.handle(t + 92_999, Event::Tick)).is_empty(),
        "the second wait is 60 s"
    );
    assert_eq!(connects(&e2.handle(t + 93_000, Event::Tick)), a);
}

#[test]
fn a_dial_that_connects_is_no_longer_outstanding_and_inbound_peers_do_not_count() {
    let rigs = SimRig::rigs("dial2", 1);
    let mut c = cfg(2, &[]);
    c.seeds = seeds_in_groups(4);
    c.max_inbound = 2;
    c.max_addr_only = 0;
    let mut e = engine_on(&rigs[0], c);
    let t = T0 * 1000;
    // five inbound connections: two fit, three are refused as "full"
    let mut refused = 0;
    for i in 0..5u64 {
        let actions = e.handle(
            t,
            Event::PeerConnected {
                peer: 100 + i,
                addr: format!("9{i}.1.1.1:41000"),
                inbound: true,
            },
        );
        if actions
            .iter()
            .any(|a| matches!(a, Action::Disconnect { reason, .. } if reason == "full"))
        {
            refused += 1;
        }
    }
    assert_eq!(refused, 3);
    assert_eq!(e.inbound_count(), 2);
    // inbound peers do not reduce the minimum number of outbound peers: two dials are still wanted
    let dials = connects(&e.handle(t + 1000, Event::Tick));
    assert_eq!(dials.len(), 2);
    // when a dial connects it stops being outstanding
    e.handle(
        t + 1500,
        Event::PeerConnected {
            peer: 1,
            addr: dials[0].clone(),
            inbound: false,
        },
    );
    assert_eq!(e.connecting_count(), 1);
    assert_eq!(e.outbound_count(), 1);
    // and a peer lost before its handshake counts against its address
    e.handle(t + 1600, Event::PeerDisconnected { peer: 1 });
    assert_eq!(e.addr_book().get(&dials[0]).unwrap().failures, 1);
}

#[test]
fn dialling_leaves_out_ourselves_connected_hosts_banned_hosts_and_full_groups() {
    let rigs = SimRig::rigs("exclude", 1);
    let own = v4(51, 1, 1, 1, 8333);
    let mut c = cfg(10, &[]);
    c.advertise = Some(own.clone());
    c.seeds = vec![
        own.clone(),           // ourselves
        v4(52, 1, 1, 1, 8333), // a host we are connected to (as an inbound peer)
        v4(53, 1, 1, 1, 8333), // a host we have banned
        v4(54, 1, 1, 1, 8333), // four in one network group: only two may be dialled
        v4(54, 1, 1, 2, 8333),
        v4(54, 1, 1, 3, 8333),
        v4(54, 1, 1, 4, 8333),
    ];
    let mut e = engine_on(&rigs[0], c);
    let t = T0 * 1000;
    e.handle(
        t,
        Event::PeerConnected {
            peer: 1,
            addr: "52.1.1.1:41000".into(),
            inbound: true,
        },
    );
    e.handle(
        t,
        Event::PeerConnected {
            peer: 2,
            addr: "53.1.1.1:41000".into(),
            inbound: true,
        },
    );
    let Message::Hello(mut wrong) = hello_from(&rigs[0]) else {
        unreachable!()
    };
    wrong.chain_id = [3; 32];
    e.handle(
        t,
        Event::Message {
            peer: 2,
            msg: Message::Hello(wrong),
        },
    );
    assert!(e.is_banned("53.1.1.1:5", t + 1));
    let dials = connects(&e.handle(t + 1000, Event::Tick));
    assert_eq!(dials.len(), 2, "{dials:?}");
    assert!(dials.iter().all(|d| d.starts_with("54.1.1.")));
    assert_eq!(
        dials
            .iter()
            .map(|d| group_of(d))
            .collect::<HashSet<_>>()
            .len(),
        1
    );
}

#[test]
fn a_node_with_nothing_to_dial_does_nothing() {
    let rigs = SimRig::rigs("empty", 1);
    let mut e = engine_on(&rigs[0], cfg(50, &[]));
    for k in 1..20 {
        assert!(connects(&e.handle(T0 * 1000 + k * 1000, Event::Tick)).is_empty());
    }
    assert_eq!(e.connecting_count(), 0);
}

#[test]
fn seeds_that_fail_are_kept_and_tried_again() {
    let rigs = SimRig::rigs("seedkeep", 1);
    let mut c = cfg(1, &[]);
    c.seeds = seeds_in_groups(1);
    let mut e = engine_on(&rigs[0], c);
    let t = T0 * 1000;
    let a = connects(&e.handle(t + 1000, Event::Tick));
    // failed a dozen times: an ordinary address would be forgotten, a seed is not
    let mut now = t + 1000;
    for _ in 0..12 {
        e.handle(now, Event::ConnectFailed { addr: a[0].clone() });
        now += 7 * 3600 * 1000; // past any backoff
        let again = connects(&e.handle(now, Event::Tick));
        assert_eq!(again, a);
    }
    assert!(e.addr_book().get(&a[0]).is_some());
}

#[test]
fn a_peer_that_connects_and_shakes_hands_makes_its_address_tried() {
    let rigs = SimRig::rigs("tried", 1);
    let mut c = cfg(1, &[]);
    c.seeds = seeds_in_groups(1);
    let mut e = engine_on(&rigs[0], c);
    let t = T0 * 1000;
    let a = connects(&e.handle(t + 1000, Event::Tick));
    e.handle(
        t + 1200,
        Event::PeerConnected {
            peer: 1,
            addr: a[0].clone(),
            inbound: false,
        },
    );
    assert!(
        !e.addr_book().get(&a[0]).unwrap().tried,
        "not before the handshake"
    );
    let actions = e.handle(
        t + 1400,
        Event::Message {
            peer: 1,
            msg: hello_from(&rigs[0]),
        },
    );
    assert!(e.addr_book().get(&a[0]).unwrap().tried);
    // and it is asked what other addresses it knows
    assert!(actions.iter().any(|x| matches!(
        x,
        Action::Send {
            msg: Message::GetAddrs,
            ..
        }
    )));
}

// ---- a full node still helps newcomers ----------------------------------------------------------------

fn full_node_with_addresses<'a>(rigs: &'a [SimRig], max_addr_only: usize) -> (Sim<'a>, usize) {
    let l_addr = "72.1.0.1:8333";
    let mut c = cfg(1, &[]);
    c.seeds = vec![l_addr.to_string()];
    c.max_inbound = 1;
    c.max_addr_only = max_addr_only;
    let mut sim = Sim::new(rigs, T0, SimConfig::default(), c);
    let l = sim.add_hostile_listener(l_addr);
    let fresh: Vec<PeerAddr> = (1..=10u8)
        .map(|i| string_to_peer_addr(&v4(60 + i, 1, 1, 1, 8333), T0).unwrap())
        .collect();
    let mut script = Script::new();
    for _ in 0..20 {
        sim.run_for(SEC);
        script.tick(&mut sim, &rigs[0], &[(l, fresh.clone())]);
    }
    // the only inbound slot is taken
    let taker = sim.add_hostile(0, "90.1.1.1:41000");
    sim.hostile_send(taker, hello_from(&rigs[0]));
    sim.run_for(SEC);
    assert_eq!(sim.engines[0].inbound_count(), 1);
    (sim, taker)
}

#[test]
fn a_full_node_gives_a_visitor_addresses_and_sends_it_away() {
    let rigs = SimRig::rigs("visitor", 1);
    let (mut sim, taker) = full_node_with_addresses(&rigs, 2);
    let v = sim.add_hostile(0, "91.1.1.1:41000");
    sim.run_for(SEC);
    assert!(
        !sim.hostiles[v].disconnected,
        "accepted, though the node is full"
    );
    assert_eq!(sim.engines[0].addr_only_count(), 1);
    assert_eq!(
        sim.engines[0].peer_count(),
        2,
        "the visitor is not a peer (the listener and the taker are)"
    );
    sim.hostile_send(v, hello_from(&rigs[0]));
    sim.hostile_send(v, Message::GetAddrs);
    sim.run_for(2 * SEC);
    let answer = sim.hostiles[v]
        .inbox
        .iter()
        .find_map(|m| match m {
            Message::Addrs { addrs } if addrs.len() > 1 => Some(addrs.len()),
            _ => None,
        })
        .expect("the visitor should have been given addresses");
    assert_eq!(answer, 11);
    assert!(sim.hostiles[v].disconnected, "and sent away");
    assert_eq!(sim.engines[0].addr_only_count(), 0);
    let now = sim.now_ms();
    assert!(
        !sim.engines[0].is_banned("91.1.1.1:5", now),
        "a visitor is not punished"
    );
    // the peer already there is untouched
    assert!(!sim.hostiles[taker].disconnected);
}

#[test]
fn visitors_are_limited_and_get_no_other_service() {
    let rigs = SimRig::rigs("visitors", 1);
    let (mut sim, _taker) = full_node_with_addresses(&rigs, 2);
    let a = sim.add_hostile(0, "91.1.1.1:41000");
    let b = sim.add_hostile(0, "91.1.1.2:41000");
    let c = sim.add_hostile(0, "91.1.1.3:41000");
    sim.run_for(SEC);
    assert!(!sim.hostiles[a].disconnected && !sim.hostiles[b].disconnected);
    assert!(sim.hostiles[c].disconnected, "only two visitors at a time");
    // a visitor that asks for anything but addresses is sent away at once
    sim.hostile_send(a, hello_from(&rigs[0]));
    sim.hostile_send(
        a,
        Message::GetBlockIds {
            locator: vec![[1; 32]],
        },
    );
    // one that asks for addresses before saying hello likewise
    sim.hostile_send(b, Message::GetAddrs);
    sim.run_for(2 * SEC);
    assert!(sim.hostiles[a].disconnected && sim.hostiles[b].disconnected);
    assert!(
        !sim.hostiles[b]
            .inbox
            .iter()
            .any(|m| matches!(m, Message::Addrs { addrs } if addrs.len() > 1)),
        "a visitor that has not said hello is not served"
    );
    let now = sim.now_ms();
    assert!(
        !sim.engines[0].is_banned("91.1.1.1:5", now)
            && !sim.engines[0].is_banned("91.1.1.2:5", now)
    );
    // a visitor that never speaks is dropped at the handshake timeout
    let d = sim.add_hostile(0, "91.1.1.4:41000");
    sim.run_for(9 * SEC);
    assert!(!sim.hostiles[d].disconnected);
    sim.run_for(3 * SEC);
    assert!(sim.hostiles[d].disconnected);
    // one on another chain is banned like anyone else
    let e = sim.add_hostile(0, "91.1.1.5:41000");
    let Message::Hello(mut wrong) = hello_from(&rigs[0]) else {
        unreachable!()
    };
    wrong.chain_id = [5; 32];
    sim.hostile_send(e, Message::Hello(wrong));
    sim.run_for(2 * SEC);
    let now = sim.now_ms();
    assert!(sim.hostiles[e].disconnected && sim.engines[0].is_banned("91.1.1.5:5", now));
}

#[test]
fn visitors_hear_nothing_of_blocks_or_transactions() {
    let rigs = SimRig::rigs("visitor-quiet", 1);
    let (mut sim, _taker) = full_node_with_addresses(&rigs, 2);
    let v = sim.add_hostile(0, "91.1.1.1:41000");
    sim.hostile_send(v, hello_from(&rigs[0]));
    sim.run_for(SEC);
    sim.mine(0, None);
    sim.run_for(9 * SEC);
    assert!(
        !sim.hostiles[v]
            .inbox
            .iter()
            .any(|m| matches!(m, Message::NewBlock { .. } | Message::NewTx { .. })),
        "a visitor is told no news"
    );
}

// ---- the same node twice --------------------------------------------------------------------------

fn engine_with_nonce(rig: &SimRig, nonce: u64) -> Engine<'_> {
    let mut c = cfg(50, &[]);
    c.nonce = nonce;
    engine_on(rig, c)
}

fn disconnected(actions: &[Action]) -> Vec<(u64, String)> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Disconnect { peer, reason } => Some((*peer, reason.clone())),
            _ => None,
        })
        .collect()
}

fn say_hello(e: &mut Engine<'_>, rig: &SimRig, peer: u64, nonce: u64, t: u64) -> Vec<Action> {
    e.handle(
        t,
        Event::Message {
            peer,
            msg: hello_with_nonce(rig, nonce),
        },
    )
}

fn open(e: &mut Engine<'_>, peer: u64, addr: &str, inbound: bool, t: u64) {
    e.handle(
        t,
        Event::PeerConnected {
            peer,
            addr: addr.into(),
            inbound,
        },
    );
}

#[test]
fn a_connection_to_ourselves_is_dropped_without_a_ban() {
    let rigs = SimRig::rigs("self", 1);
    let mut e = engine_with_nonce(&rigs[0], 100);
    let t = T0 * 1000;
    open(&mut e, 1, "60.1.1.1:8333", false, t);
    let actions = say_hello(&mut e, &rigs[0], 1, 100, t + 10);
    assert_eq!(
        disconnected(&actions),
        vec![(1, "connected to ourselves".to_string())]
    );
    assert_eq!(e.peer_count(), 0);
    assert!(!e.is_banned("60.1.1.1:8333", t + 20));
}

#[test]
fn of_two_links_to_one_node_the_one_dialled_by_the_smaller_nonce_survives_whatever_the_order() {
    let t = T0 * 1000;
    // our nonce 100 < theirs 200: the link WE dialled (peer 1) is kept
    for order in [[1u64, 2], [2, 1]] {
        let rigs = SimRig::rigs("dup-a", 1);
        let mut e = engine_with_nonce(&rigs[0], 100);
        open(&mut e, 1, "60.1.1.1:8333", false, t);
        open(&mut e, 2, "60.1.1.1:41000", true, t);
        let mut dropped = Vec::new();
        for (k, p) in order.iter().enumerate() {
            dropped.extend(disconnected(&say_hello(
                &mut e,
                &rigs[0],
                *p,
                200,
                t + 10 + k as u64,
            )));
        }
        assert_eq!(
            dropped,
            vec![(2, "duplicate connection".to_string())],
            "order {order:?}"
        );
        assert_eq!(e.peer_count(), 1);
        assert_eq!(e.outbound_count(), 1);
    }
    // our nonce 300 > theirs 200: the link THEY dialled (peer 2) is kept
    for order in [[1u64, 2], [2, 1]] {
        let rigs = SimRig::rigs("dup-b", 1);
        let mut e = engine_with_nonce(&rigs[0], 300);
        open(&mut e, 1, "60.1.1.1:8333", false, t);
        open(&mut e, 2, "60.1.1.1:41000", true, t);
        let mut dropped = Vec::new();
        for (k, p) in order.iter().enumerate() {
            dropped.extend(disconnected(&say_hello(
                &mut e,
                &rigs[0],
                *p,
                200,
                t + 10 + k as u64,
            )));
        }
        assert_eq!(
            dropped,
            vec![(1, "duplicate connection".to_string())],
            "order {order:?}"
        );
        assert_eq!(e.inbound_count(), 1);
    }
    // both dialled by the same side: the first to shake hands is kept
    let rigs = SimRig::rigs("dup-c", 1);
    let mut e = engine_with_nonce(&rigs[0], 100);
    open(&mut e, 1, "60.1.1.1:8333", false, t);
    open(&mut e, 2, "60.1.1.1:8333", false, t);
    let first = say_hello(&mut e, &rigs[0], 2, 200, t + 10);
    assert!(disconnected(&first).is_empty());
    let second = say_hello(&mut e, &rigs[0], 1, 200, t + 20);
    assert_eq!(
        disconnected(&second),
        vec![(1, "duplicate connection".to_string())]
    );
    assert_eq!(e.peer_count(), 1);
}

#[test]
fn without_a_nonce_on_either_side_nothing_is_taken_for_a_duplicate() {
    let t = T0 * 1000;
    let rigs = SimRig::rigs("dup-d", 1);
    // their nonce 0
    let mut e = engine_with_nonce(&rigs[0], 100);
    open(&mut e, 1, "60.1.1.1:8333", false, t);
    open(&mut e, 2, "60.1.1.1:41000", true, t);
    say_hello(&mut e, &rigs[0], 1, 0, t + 10);
    say_hello(&mut e, &rigs[0], 2, 0, t + 20);
    assert_eq!(e.peer_count(), 2);
    // our nonce 0
    let rigs = SimRig::rigs("dup-e", 1);
    let mut e = engine_with_nonce(&rigs[0], 0);
    open(&mut e, 1, "60.1.1.1:8333", false, t);
    open(&mut e, 2, "60.1.1.1:41000", true, t);
    say_hello(&mut e, &rigs[0], 1, 200, t + 10);
    say_hello(&mut e, &rigs[0], 2, 200, t + 20);
    assert_eq!(e.peer_count(), 2);
}

#[test]
fn two_links_between_the_same_pair_settle_on_one_that_both_ends_keep() {
    let rigs = SimRig::rigs("dup-sim", 2);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), cfg(1, &[]));
    // each dials the other at the same moment
    sim.connect(0, 1);
    sim.connect(1, 0);
    sim.run_for(5 * SEC);
    assert_eq!(sim.engines[0].peer_count(), 1);
    assert_eq!(sim.engines[1].peer_count(), 1);
    // the surviving link works: a block crosses it
    sim.mine(0, None);
    assert!(sim.run_until(30 * SEC, |s| s.all_agree()));
}

// ---- more rules, each with its own test ---------------------------------------------------------------

#[test]
fn at_most_a_bounded_number_of_dials_start_in_one_tick() {
    let rigs = SimRig::rigs("pertick", 1);
    let mut c = cfg(50, &[]);
    c.seeds = (0..30u8).map(|i| v4(50 + i, 1, 1, 1, 8333)).collect();
    c.max_connect_per_tick = 8;
    let mut e = engine_on(&rigs[0], c);
    let t = T0 * 1000;
    assert_eq!(connects(&e.handle(t + 1000, Event::Tick)).len(), 8);
    assert_eq!(connects(&e.handle(t + 2000, Event::Tick)).len(), 8);
    assert_eq!(e.connecting_count(), 16);
}

#[test]
fn two_addresses_of_one_host_are_not_dialled_together() {
    let rigs = SimRig::rigs("onehost", 1);
    let mut c = cfg(10, &[]);
    c.seeds = vec![
        v4(55, 1, 1, 1, 8333),
        v4(55, 1, 1, 1, 8334),
        v4(56, 1, 1, 1, 8333),
    ];
    let mut e = engine_on(&rigs[0], c);
    let dials = connects(&e.handle(T0 * 1000 + 1000, Event::Tick));
    assert_eq!(dials.len(), 2, "{dials:?}");
    let hosts: HashSet<&str> = dials
        .iter()
        .map(|d| d.rsplit_once(':').unwrap().0)
        .collect();
    assert_eq!(hosts.len(), 2, "one address per host");
}

#[test]
fn an_address_that_connects_and_is_dropped_at_once_is_not_redialled_in_a_loop() {
    let rigs = SimRig::rigs("churn1", 1);
    let mut c = cfg(1, &[]);
    c.seeds = seeds_in_groups(1);
    let mut e = engine_on(&rigs[0], c);
    let t = T0 * 1000;
    let a = connects(&e.handle(t + 1000, Event::Tick));
    e.handle(
        t + 1100,
        Event::PeerConnected {
            peer: 1,
            addr: a[0].clone(),
            inbound: false,
        },
    );
    e.handle(
        t + 1200,
        Event::Message {
            peer: 1,
            msg: hello_from(&rigs[0]),
        },
    );
    // the peer hangs up straight after the handshake
    e.handle(t + 1300, Event::PeerDisconnected { peer: 1 });
    assert!(
        connects(&e.handle(t + 5000, Event::Tick)).is_empty(),
        "not five seconds later"
    );
    assert!(connects(&e.handle(t + 30_999, Event::Tick)).is_empty());
    assert_eq!(
        connects(&e.handle(t + 31_000, Event::Tick)),
        a,
        "after the minimum gap"
    );
}

#[test]
fn an_outbound_peer_that_never_shakes_hands_counts_as_a_failure_and_a_banned_one_leaves_the_book() {
    let rigs = SimRig::rigs("outfail", 1);
    let mut c = cfg(2, &[]);
    c.seeds = seeds_in_groups(2);
    let mut e = engine_on(&rigs[0], c);
    let t = T0 * 1000;
    let dials = connects(&e.handle(t + 1000, Event::Tick));
    assert_eq!(dials.len(), 2);
    // the first connects and stays silent
    e.handle(
        t + 1100,
        Event::PeerConnected {
            peer: 1,
            addr: dials[0].clone(),
            inbound: false,
        },
    );
    let actions = e.handle(t + 12_000, Event::Tick);
    assert!(disconnected(&actions)
        .iter()
        .any(|(p, why)| *p == 1 && why == "handshake timeout"));
    assert_eq!(e.addr_book().get(&dials[0]).unwrap().failures, 1);
    // the second connects and proves to be on another chain: banned, and gone from the book
    e.handle(
        t + 1200,
        Event::PeerConnected {
            peer: 2,
            addr: dials[1].clone(),
            inbound: false,
        },
    );
    let Message::Hello(mut wrong) = hello_from(&rigs[0]) else {
        unreachable!()
    };
    wrong.chain_id = [6; 32];
    e.handle(
        t + 1300,
        Event::Message {
            peer: 2,
            msg: Message::Hello(wrong),
        },
    );
    assert!(e.is_banned(&dials[1], t + 1400));
    assert!(
        e.addr_book().get(&dials[1]).is_none(),
        "a peer that earned a ban is forgotten, seed or not"
    );
    // an ordinary address is removed when it turns out to be a hostile peer
    let rigs2 = SimRig::rigs("outfail2", 1);
    let mut c = cfg(3, &[]); // wants more peers than the one it already has
    c.seeds = vec![];
    let mut e2 = engine_on(&rigs2[0], c);
    let ordinary = v4(45, 1, 1, 1, 8333);
    // learn it the way a node does: from a peer's answer
    e2.handle(
        t,
        Event::PeerConnected {
            peer: 9,
            addr: v4(46, 1, 1, 1, 8333),
            inbound: false,
        },
    );
    e2.handle(
        t + 10,
        Event::Message {
            peer: 9,
            msg: hello_from(&rigs2[0]),
        },
    );
    e2.handle(
        t + 20,
        Event::Message {
            peer: 9,
            msg: Message::Addrs {
                addrs: vec![string_to_peer_addr(&ordinary, T0).unwrap()],
            },
        },
    );
    assert!(e2.addr_book().get(&ordinary).is_some());
    let dial = connects(&e2.handle(t + 1000, Event::Tick));
    assert_eq!(dial, vec![ordinary.clone()]);
    e2.handle(
        t + 1100,
        Event::PeerConnected {
            peer: 10,
            addr: ordinary.clone(),
            inbound: false,
        },
    );
    let Message::Hello(mut wrong) = hello_from(&rigs2[0]) else {
        unreachable!()
    };
    wrong.chain_id = [6; 32];
    e2.handle(
        t + 1200,
        Event::Message {
            peer: 10,
            msg: Message::Hello(wrong),
        },
    );
    assert!(
        e2.addr_book().get(&ordinary).is_none(),
        "a peer that earned a ban is forgotten"
    );
}

#[test]
fn a_visitor_speaking_another_protocol_version_is_dropped_but_not_banned() {
    let rigs = SimRig::rigs("visitor-version", 1);
    let (mut sim, _taker) = full_node_with_addresses(&rigs, 2);
    let v = sim.add_hostile(0, "91.1.1.9:41000");
    let Message::Hello(mut old) = hello_from(&rigs[0]) else {
        unreachable!()
    };
    old.version = PROTOCOL_VERSION + 1;
    sim.hostile_send(v, Message::Hello(old));
    sim.run_for(2 * SEC);
    assert!(sim.hostiles[v].disconnected);
    let now = sim.now_ms();
    assert!(!sim.engines[0].is_banned("91.1.1.9:5", now));
}

#[test]
fn a_failed_dial_nobody_asked_for_changes_nothing() {
    let rigs = SimRig::rigs("strayfail", 1);
    let mut c = cfg(1, &[]);
    c.seeds = seeds_in_groups(1);
    let mut e = engine_on(&rigs[0], c);
    let seed = seeds_in_groups(1)[0].clone();
    e.handle(T0 * 1000, Event::ConnectFailed { addr: seed.clone() });
    assert_eq!(e.addr_book().get(&seed).unwrap().failures, 0);
}

#[test]
fn a_restored_state_keeps_the_configured_seeds() {
    let rigs = SimRig::rigs("seedsafter", 1);
    let mut first = cfg(1, &[]);
    first.seeds = vec![v4(45, 1, 1, 1, 8333)];
    let e1 = engine_on(&rigs[0], first);
    let state = e1.export_state();
    let mut second = cfg(1, &[]);
    second.seeds = vec![v4(46, 1, 1, 1, 8333)];
    let mut e2 = engine_on(&rigs[0], second);
    e2.import_state(&state).unwrap();
    assert!(
        e2.addr_book().get(&v4(45, 1, 1, 1, 8333)).is_some(),
        "what was saved"
    );
    assert!(
        e2.addr_book().get(&v4(46, 1, 1, 1, 8333)).is_some(),
        "and the seed in this configuration"
    );
}

#[test]
fn a_node_announces_its_own_address_to_each_peer_once() {
    let rigs = SimRig::rigs("advertise", 1);
    let own = v4(51, 1, 1, 1, 8333);
    let mut c = cfg(1, &[]);
    c.advertise = Some(own.clone());
    let mut e = engine_on(&rigs[0], c);
    let t = T0 * 1000;
    e.handle(
        t,
        Event::PeerConnected {
            peer: 1,
            addr: "60.1.1.1:41000".into(),
            inbound: true,
        },
    );
    let actions = e.handle(
        t + 10,
        Event::Message {
            peer: 1,
            msg: hello_from(&rigs[0]),
        },
    );
    let announced: Vec<&Vec<PeerAddr>> = actions
        .iter()
        .filter_map(|a| match a {
            Action::Send {
                msg: Message::Addrs { addrs },
                ..
            } => Some(addrs),
            _ => None,
        })
        .collect();
    assert_eq!(announced.len(), 1);
    assert_eq!(announced[0].len(), 1);
    assert_eq!(announced[0][0].port, 8333);
    // a node with no public address announces nothing
    let mut quiet = engine_on(&rigs[0], cfg(1, &[]));
    quiet.handle(
        t,
        Event::PeerConnected {
            peer: 1,
            addr: "60.1.1.1:41000".into(),
            inbound: true,
        },
    );
    let actions = quiet.handle(
        t + 10,
        Event::Message {
            peer: 1,
            msg: hello_from(&rigs[0]),
        },
    );
    assert!(!actions.iter().any(|a| matches!(
        a,
        Action::Send {
            msg: Message::Addrs { .. },
            ..
        }
    )));
}

#[test]
fn a_foreign_address_is_not_a_self_announcement_and_two_addresses_are_not_one() {
    let rigs = SimRig::rigs("selfannounce", 1);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), cfg(0, &[]));
    let foreign = sim.add_hostile(0, "82.1.1.1:41000");
    sim.hostile_send(foreign, hello_from(&rigs[0]));
    sim.run_for(SEC);
    // the first announcement is another host's address: refused, and punished
    let other = string_to_peer_addr("84.9.9.9:8333", T0).unwrap();
    sim.hostile_send(foreign, Message::Addrs { addrs: vec![other] });
    sim.run_for(SEC);
    assert_eq!(score(&sim, foreign), Some(20));
    assert!(sim.engines[0].addr_book().get("84.9.9.9:8333").is_none());
    // and its own address is still welcome afterwards, because the allowance was not used up
    let own = string_to_peer_addr("82.1.1.1:8333", T0).unwrap();
    sim.hostile_send(foreign, Message::Addrs { addrs: vec![own] });
    sim.run_for(SEC);
    assert_eq!(score(&sim, foreign), Some(20));
    assert!(sim.engines[0].addr_book().get("82.1.1.1:8333").is_some());

    // two addresses, the first of them its own host: not a self-announcement
    let two = sim.add_hostile(0, "82.2.2.2:41000");
    sim.hostile_send(two, hello_from(&rigs[0]));
    sim.run_for(SEC);
    let pair = vec![
        string_to_peer_addr("82.2.2.2:8333", T0).unwrap(),
        string_to_peer_addr("84.8.8.8:8333", T0).unwrap(),
    ];
    sim.hostile_send(two, Message::Addrs { addrs: pair });
    sim.run_for(SEC);
    assert_eq!(score(&sim, two), Some(20));
    assert!(sim.engines[0].addr_book().get("82.2.2.2:8333").is_none());
    assert!(sim.engines[0].addr_book().get("84.8.8.8:8333").is_none());
}

#[test]
fn an_announcement_of_the_unspecified_address_means_the_address_the_peer_connected_from() {
    // a node at home does not know its public IP and it changes: it announces `0.0.0.0:PORT` and the receiver fills in the IP it sees
    let rigs = SimRig::rigs("autoannounce", 1);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), cfg(0, &[]));
    let peer = sim.add_hostile(0, "82.1.1.1:41000");
    sim.hostile_send(peer, hello_from(&rigs[0]));
    sim.run_for(SEC);
    let auto = string_to_peer_addr("0.0.0.0:38333", T0).unwrap();
    sim.hostile_send(peer, Message::Addrs { addrs: vec![auto] });
    sim.run_for(SEC);
    assert_eq!(score(&sim, peer), Some(0));
    assert!(sim.engines[0].addr_book().get("82.1.1.1:38333").is_some());
    assert!(
        sim.engines[0].addr_book().get("0.0.0.0:38333").is_none(),
        "the placeholder itself is never stored"
    );
    // still once per connection
    let again = string_to_peer_addr("0.0.0.0:9999", T0).unwrap();
    sim.hostile_send(peer, Message::Addrs { addrs: vec![again] });
    sim.run_for(SEC);
    assert_eq!(score(&sim, peer), Some(20));
    assert!(sim.engines[0].addr_book().get("82.1.1.1:9999").is_none());

    // port 0 is no address at all: refused and punished, nothing stored
    let zero = sim.add_hostile(0, "82.2.2.2:41000");
    sim.hostile_send(zero, hello_from(&rigs[0]));
    sim.run_for(SEC);
    let nothing = string_to_peer_addr("0.0.0.0:0", T0).unwrap();
    sim.hostile_send(
        zero,
        Message::Addrs {
            addrs: vec![nothing],
        },
    );
    sim.run_for(SEC);
    assert_eq!(score(&sim, zero), Some(20));
    assert!(sim.engines[0].addr_book().get("82.2.2.2:0").is_none());

    // the same for an IPv6 peer
    let six = sim.add_hostile(0, "[2606:4700::1]:41000");
    sim.hostile_send(six, hello_from(&rigs[0]));
    sim.run_for(SEC);
    let auto6 = string_to_peer_addr("[::]:38333", T0).unwrap();
    sim.hostile_send(six, Message::Addrs { addrs: vec![auto6] });
    sim.run_for(SEC);
    assert_eq!(score(&sim, six), Some(0));
    assert!(sim.engines[0]
        .addr_book()
        .get("[2606:4700::1]:38333")
        .is_some());
}

#[test]
fn an_address_that_turns_out_to_be_ourselves_is_not_dialled_again() {
    // a node with a changing address hears its own address from its peers: dialling it reaches ourselves (the same nonce), which must cost one
    // connection once, not one every few minutes
    let rigs = SimRig::rigs("selfdial", 1);
    let mut c = cfg(1, &[]);
    c.nonce = 77;
    c.seeds = seeds_in_groups(1);
    let mut e = engine_on(&rigs[0], c);
    let t = T0 * 1000;
    let dials = connects(&e.handle(t + 1000, Event::Tick));
    assert_eq!(dials.len(), 1);
    e.handle(
        t + 1500,
        Event::PeerConnected {
            peer: 1,
            addr: dials[0].clone(),
            inbound: false,
        },
    );
    e.handle(
        t + 1600,
        Event::Message {
            peer: 1,
            msg: hello_with_nonce(&rigs[0], 77),
        },
    );
    e.handle(t + 1700, Event::PeerDisconnected { peer: 1 });
    assert_eq!(e.outbound_count(), 0);
    // an hour of ticks, well past any back-off: it is never dialled again
    for k in 1..=60u64 {
        let again = connects(&e.handle(t + 1000 + k * 60_000, Event::Tick));
        assert!(
            !again.contains(&dials[0]),
            "dialled ourselves again at minute {k}"
        );
    }
}

#[test]
fn on_a_private_network_a_peer_may_announce_an_address_other_than_the_one_it_dialled_from() {
    // one machine dials out from 127.0.0.1 whatever address it listens on: a local test network needs this to learn where its peers are,
    // and only a private network (`accept_private`) gets it; the default (public) behaviour is pinned by the test above
    let rigs = SimRig::rigs("privateannounce", 1);
    let mut c = cfg(0, &[]);
    c.addrbook.accept_private = true;
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), c);
    let peer = sim.add_hostile(0, "127.0.0.1:41000");
    sim.hostile_send(peer, hello_from(&rigs[0]));
    sim.run_for(SEC);
    let own = string_to_peer_addr("127.2.0.1:18331", T0).unwrap();
    sim.hostile_send(peer, Message::Addrs { addrs: vec![own] });
    sim.run_for(SEC);
    assert_eq!(score(&sim, peer), Some(0));
    assert!(sim.engines[0].addr_book().get("127.2.0.1:18331").is_some());
    // still only once, and only one address
    let again = string_to_peer_addr("127.3.0.1:18331", T0).unwrap();
    sim.hostile_send(peer, Message::Addrs { addrs: vec![again] });
    sim.run_for(SEC);
    assert_eq!(score(&sim, peer), Some(20));
    assert!(sim.engines[0].addr_book().get("127.3.0.1:18331").is_none());
}

#[test]
fn a_peer_we_asked_answers_once_and_a_second_answer_is_unsolicited() {
    let rigs = SimRig::rigs("secondanswer", 1);
    let l_addr = "72.1.0.1:8333";
    let mut c = cfg(1, &[]);
    c.seeds = vec![l_addr.to_string()];
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), c);
    let l = sim.add_hostile_listener(l_addr);
    let first: Vec<PeerAddr> = (1..=3u8)
        .map(|i| string_to_peer_addr(&v4(60 + i, 1, 1, 1, 8333), T0).unwrap())
        .collect();
    let mut script = Script::new();
    for _ in 0..20 {
        sim.run_for(SEC);
        script.tick(&mut sim, &rigs[0], &[(l, first.clone())]);
    }
    assert_eq!(sim.engines[0].addr_book().len(), 4);
    assert_eq!(score(&sim, l), Some(0));
    // the same peer sends another batch that nobody asked for
    let second: Vec<PeerAddr> = (1..=3u8)
        .map(|i| string_to_peer_addr(&v4(70 + i, 1, 1, 1, 8333), T0).unwrap())
        .collect();
    sim.hostile_send(l, Message::Addrs { addrs: second });
    sim.run_for(SEC);
    assert_eq!(score(&sim, l), Some(20));
    assert_eq!(sim.engines[0].addr_book().len(), 4, "none of it was taken");
}

#[test]
fn a_dial_in_progress_is_not_started_again_even_when_the_minimum_gap_is_off() {
    let rigs = SimRig::rigs("noduplicatedial", 1);
    let mut c = cfg(3, &[]); // wants more peers than the single address can give
    c.seeds = seeds_in_groups(1);
    c.addrbook.min_redial_ms = 0;
    let mut e = engine_on(&rigs[0], c);
    let t = T0 * 1000;
    assert_eq!(connects(&e.handle(t + 1000, Event::Tick)).len(), 1);
    // the target wants more peers than the one dial will give: the same address must still not be dialled twice
    assert!(connects(&e.handle(t + 2000, Event::Tick)).is_empty());
    assert_eq!(e.connecting_count(), 1);
}

#[test]
fn a_connection_we_dialled_is_refused_when_we_are_full_and_never_treated_as_a_visitor() {
    let rigs = SimRig::rigs("fullout", 1);
    let mut c = cfg(1, &[]);
    c.max_peers = 1;
    c.max_addr_only = 4;
    let mut e = engine_on(&rigs[0], c);
    let t = T0 * 1000;
    e.handle(
        t,
        Event::PeerConnected {
            peer: 1,
            addr: "60.1.1.1:41000".into(),
            inbound: true,
        },
    );
    let actions = e.handle(
        t,
        Event::PeerConnected {
            peer: 2,
            addr: "61.1.1.1:8333".into(),
            inbound: false,
        },
    );
    assert_eq!(disconnected(&actions), vec![(2, "full".to_string())]);
    assert_eq!(e.addr_only_count(), 0);
    // an inbound stranger over the limit is a visitor
    e.handle(
        t,
        Event::PeerConnected {
            peer: 3,
            addr: "62.1.1.1:41000".into(),
            inbound: true,
        },
    );
    assert_eq!(e.addr_only_count(), 1);
}

// ---- a full node makes room by dropping an inbound peer that has done nothing ----------------------------

const MIN: u64 = 60 * SEC;
const IDLE: &str = "idle: slot needed";

/// A node that holds two inbound peers and no more: peers 1 and 2, connected and greeted at `T0 * 1000`, with eviction after 10 minutes.
fn full_of_two_inbound(rigs: &[SimRig], max_addr_only: usize) -> Engine<'_> {
    let mut c = cfg(1, &[]);
    c.max_inbound = 2;
    c.max_addr_only = max_addr_only;
    c.idle_evict_after_ms = 10 * MIN;
    let mut e = engine_on(&rigs[0], c);
    let t = T0 * 1000;
    for i in 1..=2u64 {
        open(&mut e, i, &format!("6{i}.1.1.1:41000"), true, t);
        say_hello(&mut e, &rigs[0], i, 10 + i, t);
    }
    assert_eq!(e.inbound_count(), 2);
    e
}

fn newcomer(e: &mut Engine<'_>, peer: u64, at: u64) -> Vec<Action> {
    e.handle(
        at,
        Event::PeerConnected {
            peer,
            addr: format!("7{peer}.1.1.1:41000"),
            inbound: true,
        },
    )
}

#[test]
fn a_full_node_drops_the_inbound_peer_that_has_been_quiet_longest_for_a_newcomer() {
    let rigs = SimRig::rigs("idle1", 1);
    let mut e = full_of_two_inbound(&rigs, 0);
    let t = T0 * 1000;
    // both have said nothing for 11 minutes: the tie goes to the lower id, and the newcomer is a real peer, not a visitor
    let actions = newcomer(&mut e, 3, t + 11 * MIN);
    assert_eq!(disconnected(&actions), vec![(1, IDLE.to_string())]);
    assert_eq!(e.inbound_count(), 2);
    assert_eq!(e.addr_only_count(), 0);
    // dropping it is not a ban
    assert!(!actions.iter().any(|a| matches!(a, Action::Ban { .. })));
}

#[test]
fn a_peer_that_asks_for_things_is_kept_and_a_peer_that_only_pings_is_not() {
    let rigs = SimRig::rigs("idle2", 1);
    let mut e = full_of_two_inbound(&rigs, 0);
    let t = T0 * 1000;
    // peer 1 announces a transaction at 3 minutes (activity); peer 2 only pings at 9 minutes (not activity)
    e.handle(
        t + 3 * MIN,
        Event::Message {
            peer: 1,
            msg: Message::NewTx { ids: vec![[1; 32]] },
        },
    );
    e.handle(
        t + 9 * MIN,
        Event::Message {
            peer: 2,
            msg: Message::Ping(7),
        },
    );
    // at 12 minutes peer 1 has been quiet for 9 (under the limit) and peer 2 for 12: peer 2 goes
    let actions = newcomer(&mut e, 3, t + 12 * MIN);
    assert_eq!(disconnected(&actions), vec![(2, IDLE.to_string())]);
}

#[test]
fn nobody_is_dropped_while_every_peer_has_been_quiet_for_less_than_the_limit() {
    let rigs = SimRig::rigs("idle3", 1);
    let mut e = full_of_two_inbound(&rigs, 4);
    let t = T0 * 1000;
    // 5 minutes in, neither has done anything, but a connection made 5 minutes ago is not yet an idle one: the newcomer is a visitor
    let actions = newcomer(&mut e, 3, t + 5 * MIN);
    assert!(disconnected(&actions).is_empty(), "{actions:?}");
    assert_eq!(e.addr_only_count(), 1);
    assert_eq!(e.inbound_count(), 3);
}

#[test]
fn a_connection_we_dialled_never_makes_a_full_node_drop_anybody() {
    let rigs = SimRig::rigs("idle4", 1);
    let mut c = cfg(1, &[]);
    c.max_peers = 2;
    c.max_addr_only = 0;
    c.idle_evict_after_ms = 10 * MIN;
    let mut e = engine_on(&rigs[0], c);
    let t = T0 * 1000;
    for i in 1..=2u64 {
        open(&mut e, i, &format!("6{i}.1.1.1:41000"), true, t);
        say_hello(&mut e, &rigs[0], i, 10 + i, t);
    }
    let actions = e.handle(
        t + 11 * MIN,
        Event::PeerConnected {
            peer: 3,
            addr: "73.1.1.1:8333".into(),
            inbound: false,
        },
    );
    assert_eq!(disconnected(&actions), vec![(3, "full".to_string())]);
    assert_eq!(e.inbound_count(), 2);
}

#[test]
fn eviction_can_be_turned_off() {
    let rigs = SimRig::rigs("idle5", 1);
    let mut c = cfg(1, &[]);
    c.max_inbound = 2;
    c.max_addr_only = 0;
    c.idle_evict_after_ms = 0;
    let mut e = engine_on(&rigs[0], c);
    let t = T0 * 1000;
    for i in 1..=2u64 {
        open(&mut e, i, &format!("6{i}.1.1.1:41000"), true, t);
        say_hello(&mut e, &rigs[0], i, 10 + i, t);
    }
    let actions = newcomer(&mut e, 3, t + 60 * MIN);
    assert_eq!(disconnected(&actions), vec![(3, "full".to_string())]);
    assert_eq!(e.inbound_count(), 2);
}

#[test]
fn a_visitor_given_addresses_is_never_the_one_dropped_to_make_room() {
    let rigs = SimRig::rigs("idle6", 1);
    let mut c = cfg(1, &[]);
    c.max_inbound = 1;
    c.max_addr_only = 4;
    c.idle_evict_after_ms = 10 * MIN;
    let mut e = engine_on(&rigs[0], c);
    let t = T0 * 1000;
    open(&mut e, 1, "61.1.1.1:41000", true, t);
    say_hello(&mut e, &rigs[0], 1, 11, t);
    // a visitor (over the limit), long connected
    open(&mut e, 2, "62.1.1.1:41000", true, t);
    assert_eq!(e.addr_only_count(), 1);
    // a newcomer an hour later: the real peer 1 goes, the visitor stays (it is not a candidate), the newcomer is a real peer
    let actions = newcomer(&mut e, 3, t + 60 * MIN);
    assert_eq!(disconnected(&actions), vec![(1, IDLE.to_string())]);
    assert_eq!(e.addr_only_count(), 1);
}

// ---- a node does not stay on its seed --------------------------------------------------------------------

const THE_SEED: &str = "60.1.1.1:8333";
const LEFT_SEED: &str = "seed: enough other peers";
const REFRESH_SEED: &str = "seed: refreshing addresses";

/// A node past its first-start bootstrap with one seed (`THE_SEED`) and these pinned peers, connected to the seed (peer 1) and to the pinned peers
/// (peers 2, 3, ...), all greeted at `T0 * 1000`; they are outbound peers we chose that are not seeds.
fn on_a_seed_with_peers<'a>(
    rigs: &'a [SimRig],
    leave_min: usize,
    refresh_ms: u64,
    others: &[&str],
) -> Engine<'a> {
    let mut c = cfg(8, &[]);
    c.seeds = vec![THE_SEED.to_string()];
    c.bootstrap_wait_ms = 0;
    c.seed_leave_min_peers = leave_min;
    c.addr_refresh_ms = refresh_ms;
    c.trusted = others.iter().map(|s| s.to_string()).collect();
    let mut e = engine_on(&rigs[0], c);
    let t = T0 * 1000;
    open(&mut e, 1, THE_SEED, false, t);
    say_hello(&mut e, &rigs[0], 1, 21, t);
    for (i, a) in others.iter().enumerate() {
        let peer = 2 + i as u64;
        open(&mut e, peer, a, false, t);
        say_hello(&mut e, &rigs[0], peer, 21 + peer, t);
    }
    e
}

#[test]
fn a_node_with_enough_peers_of_its_own_leaves_its_seed_and_does_not_go_back() {
    let rigs = SimRig::rigs("leave1", 1);
    let mut e = on_a_seed_with_peers(&rigs, 2, 0, &["61.1.1.1:8333", "62.1.1.1:8333"]);
    let t = T0 * 1000;
    let actions = e.handle(t + 1000, Event::Tick);
    assert_eq!(disconnected(&actions), vec![(1, LEFT_SEED.to_string())]);
    assert!(
        !actions.iter().any(|a| matches!(a, Action::Ban { .. })),
        "leaving a seed is not a ban"
    );
    // not dialled again in the same step either (the seed is in its book, due, and it wants more peers)
    assert!(
        !connects(&actions).iter().any(|a| a == THE_SEED),
        "{actions:?}"
    );
    // it wants more peers (8 outbound), the seed is in its book and due, and it still does not dial it: it has enough of its own. (Only
    // the first 30 seconds are looked at: after that the test's silent pinned peers would be dropped for not answering pings, and the node
    // would rightly be back below its minimum.)
    for k in 2..30u64 {
        let dials = connects(&e.handle(t + k * SEC, Event::Tick));
        assert!(
            !dials.iter().any(|a| a == THE_SEED),
            "dialled the seed again: {dials:?}"
        );
    }
    assert_eq!(e.peer_count(), 2, "the two pinned peers are still there");
}

#[test]
fn a_node_that_has_lost_its_own_peers_goes_back_to_its_seed() {
    let rigs = SimRig::rigs("leave1b", 1);
    let mut e = on_a_seed_with_peers(&rigs, 2, 0, &["61.1.1.1:8333", "62.1.1.1:8333"]);
    let t = T0 * 1000;
    e.handle(t + 1000, Event::Tick); // leaves the seed
    e.handle(t + 2000, Event::PeerDisconnected { peer: 2 });
    e.handle(t + 2000, Event::PeerDisconnected { peer: 3 });
    // no peers at all now: the seed is dialled again (the book still holds it)
    let dials = connects(&e.handle(t + 40 * SEC, Event::Tick));
    assert!(dials.iter().any(|a| a == THE_SEED), "{dials:?}");
}

#[test]
fn a_node_with_fewer_peers_of_its_own_than_the_limit_keeps_its_seed() {
    let rigs = SimRig::rigs("leave2", 1);
    let mut e = on_a_seed_with_peers(&rigs, 3, 0, &["61.1.1.1:8333", "62.1.1.1:8333"]);
    let t = T0 * 1000;
    let actions = e.handle(t + 1000, Event::Tick);
    assert!(disconnected(&actions).is_empty(), "{actions:?}");
}

#[test]
fn a_peer_that_dialled_us_does_not_count_as_one_of_our_own() {
    // the peers that chose to connect to us were not chosen by us: ten of them do not let a node leave its seed
    let rigs = SimRig::rigs("leave3", 1);
    let mut e = on_a_seed_with_peers(&rigs, 2, 0, &[]);
    let t = T0 * 1000;
    for i in 0..10u64 {
        open(&mut e, 10 + i, &format!("7{i}.1.1.1:41000"), true, t);
        say_hello(&mut e, &rigs[0], 10 + i, 100 + i, t);
    }
    let actions = e.handle(t + 1000, Event::Tick);
    assert!(
        !disconnected(&actions)
            .iter()
            .any(|(_, why)| why == LEFT_SEED),
        "{actions:?}"
    );
}

#[test]
fn a_node_that_only_has_its_seed_makes_the_connection_again_after_a_while_to_hear_of_new_nodes() {
    let rigs = SimRig::rigs("leave4", 1);
    let mut e = on_a_seed_with_peers(&rigs, 3, 30 * MIN, &[]);
    let t = T0 * 1000;
    // 29 minutes: nothing to do yet
    let before = e.handle(t + 29 * MIN, Event::Tick);
    assert!(
        !disconnected(&before)
            .iter()
            .any(|(_, why)| why == REFRESH_SEED),
        "{before:?}"
    );
    // 30 minutes: the seed connection is closed (no ban) and made again, in the same step
    let actions = e.handle(t + 30 * MIN, Event::Tick);
    assert_eq!(disconnected(&actions), vec![(1, REFRESH_SEED.to_string())]);
    assert!(!actions.iter().any(|a| matches!(a, Action::Ban { .. })));
    assert_eq!(connects(&actions), vec![THE_SEED.to_string()]);
}

/// Like `on_a_seed_with_peers`, but the seed (peer 1) is the one that connected to US, from an arbitrary port: what happens to a reachable node
/// as soon as its seed learns its address and dials it (and the node's own link to the seed is then dropped as a duplicate).
fn dialled_by_a_seed<'a>(
    rigs: &'a [SimRig],
    leave_min: usize,
    refresh_ms: u64,
    others: &[&str],
) -> (Engine<'a>, Vec<Action>) {
    let mut c = cfg(8, &[]);
    c.seeds = vec![THE_SEED.to_string()];
    c.bootstrap_wait_ms = 0;
    c.seed_leave_min_peers = leave_min;
    c.addr_refresh_ms = refresh_ms;
    c.trusted = others.iter().map(|s| s.to_string()).collect();
    let mut e = engine_on(&rigs[0], c);
    let t = T0 * 1000;
    open(&mut e, 1, "60.1.1.1:55555", true, t);
    let hello_actions = say_hello(&mut e, &rigs[0], 1, 21, t);
    for (i, a) in others.iter().enumerate() {
        let peer = 2 + i as u64;
        open(&mut e, peer, a, false, t);
        say_hello(&mut e, &rigs[0], peer, 21 + peer, t);
    }
    (e, hello_actions)
}

fn asked_for_addresses(actions: &[Action], peer: u64) -> bool {
    actions
        .iter()
        .any(|a| matches!(a, Action::Send { peer: p, msg: Message::GetAddrs } if *p == peer))
}

#[test]
fn a_node_asks_a_seed_that_dialled_it_for_addresses_but_not_a_stranger_that_did() {
    let rigs = SimRig::rigs("ask1", 1);
    let (mut e, hello) = dialled_by_a_seed(&rigs, 3, 30 * MIN, &[]);
    assert!(
        asked_for_addresses(&hello, 1),
        "no GetAddrs to the seed: {hello:?}"
    );
    // its answer is taken (it was asked for), and the peer is not punished for it
    let t = T0 * 1000;
    e.handle(
        t,
        Event::Message {
            peer: 1,
            msg: Message::Addrs {
                addrs: vec![string_to_peer_addr(&v4(70, 1, 1, 1, 8333), T0).unwrap()],
            },
        },
    );
    assert!(e.addr_book().get(&v4(70, 1, 1, 1, 8333)).is_some());
    assert_eq!(e.peer_count(), 1);
    // a stranger that connected to us is not asked: we did not choose it
    open(&mut e, 9, "61.1.1.1:41000", true, t);
    let stranger = say_hello(&mut e, &rigs[0], 9, 99, t);
    assert!(
        !asked_for_addresses(&stranger, 9),
        "asked a stranger: {stranger:?}"
    );
}

#[test]
fn a_seed_link_that_the_seed_made_is_refreshed_too_when_the_node_has_too_few_peers_of_its_own() {
    let rigs = SimRig::rigs("ask2", 1);
    let (mut e, _) = dialled_by_a_seed(&rigs, 3, 30 * MIN, &[]);
    let t = T0 * 1000;
    let before = e.handle(t + 29 * MIN, Event::Tick);
    assert!(
        !disconnected(&before)
            .iter()
            .any(|(_, why)| why == REFRESH_SEED),
        "{before:?}"
    );
    // 30 minutes: the link is closed (no ban) and the node dials the seed itself, in the same step
    let actions = e.handle(t + 30 * MIN, Event::Tick);
    assert_eq!(disconnected(&actions), vec![(1, REFRESH_SEED.to_string())]);
    assert!(!actions.iter().any(|a| matches!(a, Action::Ban { .. })));
    assert_eq!(connects(&actions), vec![THE_SEED.to_string()]);
}

#[test]
fn a_seed_link_that_the_seed_made_is_left_alone_when_the_node_has_enough_peers_of_its_own() {
    // it is not ours to close: a seed that dialled us would only dial again
    let rigs = SimRig::rigs("ask3", 1);
    let (mut e, _) = dialled_by_a_seed(&rigs, 2, 30 * MIN, &["61.1.1.1:8333", "62.1.1.1:8333"]);
    let t = T0 * 1000;
    let actions = e.handle(t + 1000, Event::Tick);
    assert!(disconnected(&actions).is_empty(), "{actions:?}");
    let later = e.handle(t + 30 * MIN, Event::Tick);
    assert!(
        !disconnected(&later).iter().any(|(id, _)| *id == 1),
        "{later:?}"
    );
}

#[test]
fn leaving_a_seed_and_refreshing_it_can_be_turned_off() {
    let rigs = SimRig::rigs("leave5", 1);
    let mut e = on_a_seed_with_peers(&rigs, 0, 0, &["61.1.1.1:8333", "62.1.1.1:8333"]);
    let t = T0 * 1000;
    let actions = e.handle(t + 60 * MIN, Event::Tick);
    assert!(
        !disconnected(&actions)
            .iter()
            .any(|(_, why)| why == LEFT_SEED || why == REFRESH_SEED),
        "{actions:?}"
    );
}

#[test]
fn when_it_dials_it_prefers_an_address_that_is_not_a_seed() {
    // one dial wanted; the book holds the seed and nine other addresses. Whatever the order the book shuffles into, the seed is not chosen.
    for book_seed in 1..=24u64 {
        let rigs = SimRig::rigs("prefer", 1);
        let mut c = cfg(1, &[]);
        c.bootstrap_wait_ms = 0;
        c.seed_leave_min_peers = 0; // so that only the ordering is in play
        c.addrbook.seed = book_seed;
        c.addrbook.min_redial_ms = 0;
        let mut e = engine_with_book_and_seeds(&rigs[0], 9, c, vec![THE_SEED.to_string()]);
        let dials = connects(&e.handle(T0 * 1000 + 5000, Event::Tick));
        assert_eq!(dials.len(), 1, "{dials:?}");
        assert_ne!(dials[0], THE_SEED, "book seed {book_seed}: {dials:?}");
    }
}

#[test]
fn a_network_that_starts_from_two_seeds_ends_up_connected_to_itself_and_not_to_the_seeds() {
    let n = 30;
    let rigs = SimRig::rigs("offseed", n);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), cfg(8, &[0, 1]));
    // ten minutes in: the nodes have found each other through the seeds, and most already have three peers of their own
    sim.run_for(600 * SEC);
    // an hour in: the seed connections that are not needed have been closed, and the ones that are (a node that has fewer than three
    // peers of its own) are only refreshed now and then
    sim.run_for(3000 * SEC);
    let ordinary: Vec<usize> = (2..n).collect();
    let on_a_seed: Vec<usize> = ordinary
        .iter()
        .copied()
        .filter(|&i| sim.engines[i].seed_peer_count() > 0)
        .collect();
    let own: Vec<usize> = ordinary
        .iter()
        .map(|&i| sim.engines[i].outbound_count() - sim.engines[i].seed_peer_count())
        .collect();
    eprintln!(
        "{} of {} ordinary nodes are connected to a seed; outbound peers that are not seeds: min {} max {}",
        on_a_seed.len(),
        ordinary.len(),
        own.iter().min().unwrap(),
        own.iter().max().unwrap()
    );
    // every ordinary node chose peers of its own (at least three, the limit for leaving a seed) ...
    for &i in &ordinary {
        let mine = sim.engines[i].outbound_count() - sim.engines[i].seed_peer_count();
        assert!(mine >= 3, "node {i} has only {mine} peers of its own");
        // ... and so none of them stays on a seed
        assert_eq!(
            sim.engines[i].seed_peer_count(),
            0,
            "node {i} is still connected to a seed"
        );
        assert_eq!(
            sim.engines[i].stats.bans, 0,
            "node {i} banned an honest peer"
        );
    }
    // it is still one working network: a block from anywhere reaches everyone, with no node on a seed
    sim.mine(17, None);
    assert!(sim.run_until(120 * SEC, |s| s.all_agree()));
}

// ---- what a GetAddrs answer reveals --------------------------------------------------------------------

/// An engine whose address book holds exactly `n` fresh routable addresses, learned the way a node learns them: from
/// answers (60 at a time, each source within its per-source limit). The sources are disconnected afterwards.
fn engine_with_book(rig: &SimRig, n: usize, c: EngineConfig) -> Engine<'_> {
    engine_with_book_and_seeds(rig, n, c, vec![])
}

/// `engine_with_book`, with these configured seeds as well (they are in the book, so it holds `n` learned addresses plus the seeds).
fn engine_with_book_and_seeds(
    rig: &SimRig,
    n: usize,
    mut c: EngineConfig,
    seeds: Vec<String>,
) -> Engine<'_> {
    let in_book = seeds.len();
    c.seeds = seeds;
    let mut e = engine_on(rig, c);
    let t = T0 * 1000;
    let mut made = 0usize;
    let mut source = 0u8;
    while made < n {
        let peer = 1000 + u64::from(source);
        open(&mut e, peer, &v4(150 + source, 1, 1, 1, 8333), false, t);
        say_hello(&mut e, rig, peer, 5000 + u64::from(source), t);
        let take = (n - made).min(60);
        let addrs: Vec<PeerAddr> = (0..take)
            .map(|k| string_to_peer_addr(&v4(30 + source, 1 + k as u8, 1, 1, 8333), T0).unwrap())
            .collect();
        e.handle(
            t,
            Event::Message {
                peer,
                msg: Message::Addrs { addrs },
            },
        );
        e.handle(t, Event::PeerDisconnected { peer });
        made += take;
        source += 1;
    }
    assert_eq!(
        e.addr_book().len(),
        n + in_book,
        "the book is what the test says it is"
    );
    e
}

/// What a peer connecting from `addr` is told when it asks for addresses, `t` milliseconds after the start.
fn ask_for_addrs(e: &mut Engine<'_>, rig: &SimRig, peer: u64, addr: &str, t: u64) -> Vec<PeerAddr> {
    let t = T0 * 1000 + t;
    open(e, peer, addr, true, t);
    say_hello(e, rig, peer, 9000 + peer, t);
    let actions = e.handle(
        t,
        Event::Message {
            peer,
            msg: Message::GetAddrs,
        },
    );
    let mut answers = actions.into_iter().filter_map(|a| match a {
        Action::Send {
            msg: Message::Addrs { addrs },
            ..
        } => Some(addrs),
        _ => None,
    });
    let first = answers.next().expect("an answer");
    assert!(answers.next().is_none(), "exactly one");
    first
}

#[test]
fn an_answer_is_a_share_of_the_book_with_a_floor_and_a_ceiling() {
    let rigs = SimRig::rigs("share", 1);
    // (book size, expected answer): 23 percent, but at least 20 (or the whole book if smaller), and at most 100
    for (book, want) in [
        (10usize, 10usize),
        (50, 20),
        (87, 20),
        (100, 23),
        (400, 92),
        (600, 100),
        (1000, 100),
    ] {
        let mut e = engine_with_book(&rigs[0], book, cfg(1, &[]));
        let answer = ask_for_addrs(&mut e, &rigs[0], 1, "70.1.1.1:5000", 10);
        assert_eq!(answer.len(), want, "a book of {book}");
    }
}

#[test]
fn a_network_group_that_asks_again_gets_the_same_answer_until_it_expires() {
    let rigs = SimRig::rigs("stable", 1);
    let ttl = 3_600_000;
    let c = EngineConfig {
        addr_answer_ttl_ms: ttl,
        ..cfg(1, &[])
    };
    let mut e = engine_with_book(&rigs[0], 400, c);
    let first = ask_for_addrs(&mut e, &rigs[0], 1, "70.1.1.1:5000", 10);
    e.handle(T0 * 1000 + 20, Event::PeerDisconnected { peer: 1 });
    // the same /16 from another host and port: the same answer
    let same = ask_for_addrs(&mut e, &rigs[0], 2, "70.1.9.9:6000", 1000);
    assert_eq!(same, first);
    // another group: its own answer
    let other = ask_for_addrs(&mut e, &rigs[0], 3, "71.1.1.1:5000", 1000);
    assert_ne!(other, first);
    // the last moment before it expires, and the first after
    let still = ask_for_addrs(&mut e, &rigs[0], 4, "70.1.2.2:7000", 10 + ttl - 1);
    assert_eq!(still, first);
    let fresh = ask_for_addrs(&mut e, &rigs[0], 5, "70.1.3.3:7000", 10 + ttl);
    assert_ne!(fresh, first, "a new sample once the old one has expired");
}

#[test]
fn a_visitor_over_the_limit_is_given_the_same_answer_as_a_peer_of_its_group() {
    let rigs = SimRig::rigs("stable-visitor", 1);
    let c = EngineConfig {
        max_peers: 1,
        max_addr_only: 4,
        ..cfg(1, &[])
    };
    let mut e = engine_with_book(&rigs[0], 400, c);
    let first = ask_for_addrs(&mut e, &rigs[0], 1, "70.1.1.1:5000", 10);
    // the one slot is taken: these are visitors
    let t = T0 * 1000 + 50;
    open(&mut e, 2, "70.1.2.2:6000", true, t);
    assert_eq!(e.addr_only_count(), 1);
    say_hello(&mut e, &rigs[0], 2, 9002, t);
    let actions = e.handle(
        t,
        Event::Message {
            peer: 2,
            msg: Message::GetAddrs,
        },
    );
    let visitor: Vec<PeerAddr> = actions
        .iter()
        .find_map(|a| match a {
            Action::Send {
                msg: Message::Addrs { addrs },
                ..
            } => Some(addrs.clone()),
            _ => None,
        })
        .expect("the visitor is answered");
    assert_eq!(visitor, first);
    assert_eq!(disconnected(&actions).len(), 1, "and sent away");
    // a visitor from a group not seen before gets a sample of its own, which is then the group's
    open(&mut e, 3, "71.1.1.1:6000", true, t);
    say_hello(&mut e, &rigs[0], 3, 9003, t);
    let a = e.handle(
        t,
        Event::Message {
            peer: 3,
            msg: Message::GetAddrs,
        },
    );
    let other: Vec<PeerAddr> = a
        .iter()
        .find_map(|x| match x {
            Action::Send {
                msg: Message::Addrs { addrs },
                ..
            } => Some(addrs.clone()),
            _ => None,
        })
        .unwrap();
    assert_ne!(other, first);
}

#[test]
fn only_so_many_answers_are_remembered_and_the_oldest_goes_first() {
    let rigs = SimRig::rigs("stable-bound", 1);
    let c = EngineConfig {
        addr_answer_cache: 2,
        ..cfg(1, &[])
    };
    let mut e = engine_with_book(&rigs[0], 400, c);
    let a70 = ask_for_addrs(&mut e, &rigs[0], 1, "70.1.1.1:5000", 10);
    let _a71 = ask_for_addrs(&mut e, &rigs[0], 2, "71.1.1.1:5000", 20);
    let a72 = ask_for_addrs(&mut e, &rigs[0], 3, "72.1.1.1:5000", 30);
    // two are kept: the newest two (71 and 72); 70, the oldest, was forgotten
    let again72 = ask_for_addrs(&mut e, &rigs[0], 4, "72.1.2.2:5000", 40);
    assert_eq!(again72, a72);
    let again70 = ask_for_addrs(&mut e, &rigs[0], 5, "70.1.2.2:5000", 50);
    assert_ne!(again70, a70, "forgotten, so a fresh sample");
}
