//! Network health, the work-comparison alarm and feeler connections (M9, threat model C1). One engine is driven directly with the time and
//! the replies under the test's control. (An eclipse cannot be PROVED absent from inside; these tests are about what the node shows and
//! does, not about catching an attacker.)

use tenero_chain::{ProofsNotChecked, Sha256Pow};
use tenero_net::addrbook::string_to_peer_addr;
use tenero_net::engine::Alarm;
use tenero_net::sim::{sim_addr, sim_addr_in, SimRig};
use tenero_net::{Action, Engine, EngineConfig, Event, Hello, Message, PeerId, PROTOCOL_VERSION};
use tenero_node::{Node, NodeConfig};

const T0: u64 = 1_700_000_000 * 1000;
const MIN: u64 = 60_000;
const FAR: u64 = u64::MAX / 4;

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

/// Settings in which nothing else happens on its own: no pings, no request timeouts, no first-start wait, and ordinary dialling
/// wants nothing once `outbound` peers are connected.
fn quiet(outbound: usize) -> EngineConfig {
    EngineConfig {
        nonce: 7,
        outbound_target: outbound,
        peer_target: outbound,
        ping_after_ms: FAR,
        pong_timeout_ms: FAR,
        request_timeout_ms: FAR,
        handshake_timeout_ms: FAR,
        bootstrap_wait_ms: 0,
        ..EngineConfig::default()
    }
}

fn work_bytes(n: u8) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[31] = n;
    w
}

fn hello(rig: &SimRig, nonce: u64, work: [u8; 32]) -> Message {
    Message::Hello(Hello {
        version: PROTOCOL_VERSION,
        chain_id: rig.store.chain_id(),
        tip_height: 0,
        cumulative_work: work,
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

fn disconnects(actions: &[Action]) -> Vec<(PeerId, String)> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Disconnect { peer, reason } => Some((*peer, reason.clone())),
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
            msg: hello(rig, 100 + peer, work_bytes(0)),
        },
    );
}

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

fn kinds(e: &Engine<'_>) -> Vec<&'static str> {
    e.health().alarms.iter().map(|a| a.kind()).collect()
}

// ---- health and alarms ----------------------------------------------------------------------------------------------------

#[test]
fn a_node_with_three_peers_in_three_groups_that_agree_with_it_has_no_alarm() {
    let rig = SimRig::new("health-quiet", 0);
    let mut e = engine_on(&rig, quiet(3));
    e.handle(T0, Event::Tick);
    for i in 1..=3u64 {
        connect(&mut e, &rig, i, &sim_addr(i as usize), false, T0 + i);
    }
    connect(&mut e, &rig, 4, &sim_addr(50), true, T0 + 5);
    e.handle(T0 + 4 * MIN, Event::Tick);
    let h = e.health();
    assert_eq!((h.peers, h.outbound, h.inbound), (4, 3, 1));
    assert_eq!(h.outbound_groups, 3);
    assert_eq!((h.peers_ahead, h.behind_for_ms), (0, 0));
    assert!(!h.bootstrapping);
    assert!(h.alarms.is_empty(), "{:?}", h.alarms);
}

#[test]
fn a_peer_that_reports_more_work_than_we_have_and_that_we_do_not_catch_up_to_is_an_alarm_after_five_minutes(
) {
    let rig = SimRig::new("health-behind", 0);
    let mut e = engine_on(&rig, quiet(2));
    e.handle(T0, Event::Tick);
    connect(&mut e, &rig, 1, &sim_addr(1), false, T0 + 1);
    connect(&mut e, &rig, 2, &sim_addr(2), false, T0 + 2);
    // peer 3 says it has far more work, and never gives the blocks
    e.handle(
        T0 + 10,
        Event::PeerConnected {
            peer: 3,
            addr: sim_addr(3),
            inbound: false,
        },
    );
    e.handle(
        T0 + 11,
        Event::Message {
            peer: 3,
            msg: hello(&rig, 103, work_bytes(200)),
        },
    );
    e.handle(T0 + 1000, Event::Tick);
    let h = e.health();
    assert_eq!(h.peers_ahead, 1);
    assert!(!kinds(&e).contains(&"behind-peers"));
    // the clock starts at the first look (T0 + 1000): one millisecond short of five minutes, then five minutes
    e.handle(T0 + 1000 + 5 * MIN - 1, Event::Tick);
    assert!(
        !kinds(&e).contains(&"behind-peers"),
        "{:?}",
        e.health().alarms
    );
    e.handle(T0 + 1000 + 5 * MIN, Event::Tick);
    let alarms = e.health().alarms;
    assert!(
        alarms.contains(&Alarm::Behind { for_ms: 5 * MIN }),
        "{alarms:?}"
    );
    // the peer goes away: the alarm goes with it
    e.handle(T0 + 6 * MIN, Event::PeerDisconnected { peer: 3 });
    e.handle(T0 + 6 * MIN + 1000, Event::Tick);
    assert!(!kinds(&e).contains(&"behind-peers"));
    assert_eq!(e.health().behind_for_ms, 0);
}

#[test]
fn a_peer_that_claims_more_work_than_it_can_back_up_is_forgotten_and_never_raises_the_alarm() {
    let rig = SimRig::new("health-liar", 0);
    let mut e = engine_on(&rig, quiet(1));
    e.handle(T0, Event::Tick);
    e.handle(
        T0 + 10,
        Event::PeerConnected {
            peer: 1,
            addr: sim_addr(1),
            inbound: false,
        },
    );
    e.handle(
        T0 + 11,
        Event::Message {
            peer: 1,
            msg: hello(&rig, 101, work_bytes(200)),
        },
    );
    assert!(e.is_syncing());
    // we ask it for its blocks; it has none to show
    e.handle(
        T0 + 20,
        Event::Message {
            peer: 1,
            msg: Message::BlockIds {
                first_height: 1,
                ids: vec![],
            },
        },
    );
    for k in 1..=12u64 {
        e.handle(T0 + k * MIN, Event::Tick);
        assert!(!kinds(&e).contains(&"behind-peers"), "minute {k}");
    }
    assert_eq!(e.health().peers_ahead, 0);
}

#[test]
fn too_few_outbound_peers_is_an_alarm_after_five_minutes_and_not_during_the_first_start() {
    let rig = SimRig::new("health-few", 0);
    let mut e = engine_on(&rig, quiet(8));
    e.handle(T0, Event::Tick);
    connect(&mut e, &rig, 1, &sim_addr(1), false, T0 + 1);
    // one outbound peer, minimum two: the clock started at the first look (T0, when there were none)
    e.handle(T0 + MIN, Event::Tick);
    assert!(!kinds(&e).contains(&"few-outbound"));
    e.handle(T0 + 5 * MIN - 1, Event::Tick);
    assert!(!kinds(&e).contains(&"few-outbound"));
    e.handle(T0 + 5 * MIN, Event::Tick);
    assert!(
        e.health().alarms.contains(&Alarm::FewOutbound { count: 1 }),
        "{:?}",
        e.health().alarms
    );
    // a second peer ends it
    connect(&mut e, &rig, 2, &sim_addr(2), false, T0 + 7 * MIN);
    e.handle(T0 + 7 * MIN + 1000, Event::Tick);
    assert!(!kinds(&e).contains(&"few-outbound"));
    // a node that is still bootstrapping is not "short": it is dialling its seeds
    let rig2 = SimRig::new("health-few-boot", 0);
    let cfg = EngineConfig {
        seeds: vec![sim_addr(1)],
        bootstrap_wait_ms: 20 * 1000,
        // (an alarm window shorter than the first start, so that counting the first start as "short" would show)
        alarm_after_ms: 5000,
        ..quiet(8)
    };
    let mut b = engine_on(&rig2, cfg);
    b.handle(T0, Event::Tick);
    assert!(b.is_bootstrapping());
    b.handle(T0 + 10_000, Event::Tick);
    assert!(b.health().bootstrapping);
    assert!(!kinds(&b).contains(&"few-outbound"));
}

#[test]
fn outbound_peers_in_one_network_group_are_an_alarm_unless_the_network_is_private() {
    let rig = SimRig::new("health-groups", 0);
    let mut e = engine_on(&rig, quiet(3));
    e.handle(T0, Event::Tick);
    for i in 0..3u64 {
        // three addresses in one /16
        connect(
            &mut e,
            &rig,
            i + 1,
            &sim_addr_in(i as usize, 3),
            false,
            T0 + i,
        );
    }
    e.handle(T0 + 1000, Event::Tick);
    assert!(
        e.health().alarms.contains(&Alarm::FewGroups { groups: 1 }),
        "{:?}",
        e.health().alarms
    );
    // two groups are enough (the least is two)
    connect(&mut e, &rig, 9, &sim_addr(77), false, T0 + 2000);
    e.handle(T0 + 3000, Event::Tick);
    assert!(!kinds(&e).contains(&"few-groups"));
    // a private network (loopback test networks) is one group by nature
    let rig2 = SimRig::new("health-groups-private", 0);
    let mut cfg = quiet(3);
    cfg.addrbook.accept_private = true;
    let mut p = engine_on(&rig2, cfg);
    p.handle(T0, Event::Tick);
    for i in 0..3u64 {
        connect(
            &mut p,
            &rig2,
            i + 1,
            &sim_addr_in(i as usize, 3),
            false,
            T0 + i,
        );
    }
    p.handle(T0 + 1000, Event::Tick);
    assert!(!kinds(&p).contains(&"few-groups"));
}

#[test]
fn a_tip_that_has_not_moved_is_an_alarm_and_the_alarms_can_be_turned_off() {
    let rig = SimRig::new("health-stale", 0);
    let mut e = engine_on(&rig, quiet(1));
    e.handle(T0, Event::Tick);
    connect(&mut e, &rig, 1, &sim_addr(1), false, T0 + 1);
    e.handle(T0 + 10 * MIN, Event::Tick);
    assert!(
        e.health()
            .alarms
            .contains(&Alarm::StaleTip { age_ms: 10 * MIN }),
        "{:?}",
        e.health().alarms
    );
    // the behind, samples, few-outbound and few-groups alarms are off with `alarm_after_ms = 0` (the stale tip has its own switch)
    let rig2 = SimRig::new("health-off", 0);
    let cfg = EngineConfig {
        alarm_after_ms: 0,
        stale_tip_ms: 0,
        ..quiet(8)
    };
    let mut o = engine_on(&rig2, cfg);
    o.handle(T0, Event::Tick);
    o.handle(T0 + 30 * MIN, Event::Tick);
    assert!(o.health().alarms.is_empty(), "{:?}", o.health().alarms);
}

// ---- feelers ---------------------------------------------------------------------------------------------------------------

/// One outbound peer (the only one the node wants) that has told it ten addresses it has never connected to.
fn node_with_ten_untried<'a>(rig: &'a SimRig, cfg: EngineConfig) -> Engine<'a> {
    let mut e = engine_on(rig, cfg);
    e.handle(T0, Event::Tick);
    connect(&mut e, rig, 1, &sim_addr(1), false, T0 + 1);
    answer(&mut e, 1, &list(100, 10), T0 + 2);
    e
}

fn feeler_cfg() -> EngineConfig {
    EngineConfig {
        feeler_interval_ms: 2 * MIN,
        ..quiet(1)
    }
}

#[test]
fn a_feeler_is_dialled_once_an_interval_one_at_a_time_to_an_address_never_connected_to() {
    let rig = SimRig::new("feeler-dial", 0);
    let mut e = node_with_ten_untried(&rig, feeler_cfg());
    // the clock starts at the first look (T0): one millisecond short of an interval, then the interval
    assert!(connects(&e.handle(T0 + 1000, Event::Tick)).is_empty());
    assert!(connects(&e.handle(T0 + 2 * MIN - 1, Event::Tick)).is_empty());
    let dialled = connects(&e.handle(T0 + 2 * MIN, Event::Tick));
    assert_eq!(dialled.len(), 1, "{dialled:?}");
    assert!(list(100, 10).contains(&dialled[0]), "{dialled:?}");
    assert_eq!(e.stats.feelers_dialled, 1);
}

#[test]
fn only_one_feeler_is_in_progress_at_a_time() {
    let rig = SimRig::new("feeler-one", 0);
    // an interval much shorter than the ten seconds a dial may take, so that only "one at a time" can hold the second back
    let cfg = EngineConfig {
        feeler_interval_ms: 1000,
        ..quiet(1)
    };
    let mut e = node_with_ten_untried(&rig, cfg);
    let first = connects(&e.handle(T0 + 1000, Event::Tick));
    assert_eq!(first.len(), 1);
    for ms in [2000, 3000, 6000, 9000] {
        assert!(connects(&e.handle(T0 + ms, Event::Tick)).is_empty(), "{ms}");
    }
    assert_eq!(e.stats.feelers_dialled, 1);
    // ...and once the dial is declared failed (ten seconds), the next may start
    let after = connects(&e.handle(T0 + 12_000, Event::Tick));
    assert_eq!(after.len(), 1);
    assert_eq!(e.stats.feelers_dialled, 2);
}

#[test]
fn a_feeler_reads_the_hello_and_hangs_up_and_is_not_an_outbound_peer() {
    let rig = SimRig::new("feeler-read", 0);
    let mut e = node_with_ten_untried(&rig, feeler_cfg());
    e.handle(T0 + 1000, Event::Tick);
    let at = T0 + 2 * MIN;
    let addr = connects(&e.handle(at, Event::Tick)).remove(0);
    assert!(!e.addr_book().get(&addr).unwrap().tried);
    e.handle(
        at + 5,
        Event::PeerConnected {
            peer: 50,
            addr: addr.clone(),
            inbound: false,
        },
    );
    // connected, before its hello: not an outbound peer (the node has its one)
    assert_eq!(e.outbound_count(), 1);
    assert_eq!(e.health().outbound, 1);
    assert_eq!(e.outbound_addrs(), vec![sim_addr(1)]);
    let acts = e.handle(
        at + 10,
        Event::Message {
            peer: 50,
            msg: hello(&rig, 150, work_bytes(3)),
        },
    );
    // it said hello: noted, remembered as working, and sent away
    assert_eq!(disconnects(&acts), vec![(50, "feeler done".to_string())]);
    assert!(
        !acts.iter().any(|a| matches!(
            a,
            Action::Send {
                msg: Message::GetAddrs,
                ..
            }
        )),
        "a feeler is not asked for addresses"
    );
    assert_eq!(e.stats.feelers_sampled, 1);
    assert!(e.addr_book().get(&addr).unwrap().tried);
    assert_eq!(e.health().samples, 1);
    assert_eq!(e.outbound_count(), 1);
    // the next one waits a whole interval from when this one STARTED
    assert!(connects(&e.handle(at + 2 * MIN - 1, Event::Tick)).is_empty());
    let next = connects(&e.handle(at + 2 * MIN, Event::Tick));
    assert_eq!(next.len(), 1);
    assert_ne!(
        next[0], addr,
        "an address that works is not a feeler's again"
    );
}

#[test]
fn a_failed_feeler_is_forgotten_and_the_next_one_comes_after_the_interval() {
    let rig = SimRig::new("feeler-fail", 0);
    let mut e = node_with_ten_untried(&rig, feeler_cfg());
    e.handle(T0 + 1000, Event::Tick);
    let at = T0 + 2 * MIN;
    let first = connects(&e.handle(at, Event::Tick)).remove(0);
    e.handle(
        at + 100,
        Event::ConnectFailed {
            addr: first.clone(),
        },
    );
    // not at once...
    assert!(connects(&e.handle(at + 2 * MIN - 1, Event::Tick)).is_empty());
    // ...but after the interval, and not the same address
    let second = connects(&e.handle(at + 2 * MIN, Event::Tick));
    assert_eq!(second.len(), 1);
    assert!(list(100, 10).contains(&second[0]), "{second:?}");
    // (a failed address may be tried again once its backoff is over; it is only forgotten after many failures)
    assert!(e.addr_book().get(&first).unwrap().failures >= 1);
    assert_eq!(e.stats.feelers_dialled, 2);
    assert_eq!(e.stats.feelers_sampled, 0);
}

#[test]
fn a_feeler_that_never_answers_does_not_block_the_next_one_for_ever() {
    let rig = SimRig::new("feeler-stuck", 0);
    let mut e = node_with_ten_untried(&rig, feeler_cfg());
    e.handle(T0 + 1000, Event::Tick);
    let at = T0 + 2 * MIN;
    connects(&e.handle(at, Event::Tick));
    // the dial times out (ten seconds) without any event from the transport
    let later = at + 3 * MIN;
    e.handle(later, Event::Tick);
    let again = connects(&e.handle(later + 2 * MIN, Event::Tick));
    assert_eq!(
        again.len(),
        1,
        "a feeler that timed out blocked every later one"
    );
}

#[test]
fn a_feeler_never_dials_a_host_the_node_is_connected_to() {
    let rig = SimRig::new("feeler-host", 0);
    let mut e = engine_on(&rig, feeler_cfg());
    e.handle(T0, Event::Tick);
    connect(&mut e, &rig, 1, &sim_addr(1), false, T0 + 1);
    // the only untried address is on the host of an inbound peer
    connect(&mut e, &rig, 2, "20.101.1.1:55555", true, T0 + 2);
    answer(
        &mut e,
        1,
        &["20.101.1.1:8333".to_string(), "20.102.1.1:8333".to_string()],
        T0 + 3,
    );
    e.handle(T0 + 1000, Event::Tick);
    let dialled = connects(&e.handle(T0 + 2 * MIN, Event::Tick));
    assert_eq!(dialled, vec!["20.102.1.1:8333".to_string()]);
}

#[test]
fn feelers_can_be_turned_off_and_wait_for_the_first_start_to_finish() {
    let rig = SimRig::new("feeler-off", 0);
    let cfg = EngineConfig {
        feeler_interval_ms: 0,
        // (the stale-tip dials at ten minutes are another feature's)
        stale_tip_ms: 0,
        ..quiet(1)
    };
    let mut e = node_with_ten_untried(&rig, cfg);
    for k in 1..=10u64 {
        assert!(connects(&e.handle(T0 + k * MIN, Event::Tick)).is_empty());
    }
    assert_eq!(e.stats.feelers_dialled, 0);
    // while a fresh node is dialling only its seeds there are no feelers, however short the interval
    let rig2 = SimRig::new("feeler-boot", 0);
    let cfg = EngineConfig {
        feeler_interval_ms: 1000,
        bootstrap_wait_ms: 20 * 1000,
        seeds: vec![sim_addr(1)],
        ..quiet(1)
    };
    let mut b = engine_on(&rig2, cfg);
    b.handle(T0, Event::Tick);
    assert!(b.is_bootstrapping());
    for k in 1..=5u64 {
        b.handle(T0 + k * 3000, Event::Tick);
    }
    assert_eq!(b.stats.feelers_dialled, 0);
}

/// One whole feeler cycle at `at`: the dial, the connection, a hello with `work`. Returns the address sampled.
fn feel(e: &mut Engine<'_>, rig: &SimRig, peer: PeerId, at: u64, work: u8) -> String {
    let addr = connects(&e.handle(at, Event::Tick)).remove(0);
    e.handle(
        at + 5,
        Event::PeerConnected {
            peer,
            addr: addr.clone(),
            inbound: false,
        },
    );
    e.handle(
        at + 10,
        Event::Message {
            peer,
            msg: hello(rig, 200 + peer, work_bytes(work)),
        },
    );
    addr
}

#[test]
fn two_sampled_nodes_that_report_more_work_than_ours_for_five_minutes_are_an_alarm() {
    let rig = SimRig::new("feeler-ahead", 0);
    let cfg = EngineConfig {
        // (no stale-tip alarm: it is its own and would hide what is being tested)
        stale_tip_ms: 0,
        ..feeler_cfg()
    };
    let mut e = node_with_ten_untried(&rig, cfg);
    e.handle(T0 + 1000, Event::Tick);
    let t1 = T0 + 2 * MIN;
    let a1 = feel(&mut e, &rig, 50, t1, 200);
    let t2 = t1 + 2 * MIN;
    let a2 = feel(&mut e, &rig, 51, t2, 200);
    assert_ne!(a1, a2);
    // the feelers themselves are not "peers ahead": they are gone, and were never counted
    assert_eq!(e.health().peers_ahead, 0);
    assert_eq!(e.health().samples, 2);
    // both are younger than five minutes: nothing yet. The older reaches five minutes at t1 + 5 min, the second at t2 + 5 min.
    e.handle(t1 + 10 + 5 * MIN - 1, Event::Tick);
    assert_eq!(e.health().samples_ahead, 0);
    e.handle(t1 + 10 + 5 * MIN, Event::Tick);
    assert_eq!(e.health().samples_ahead, 1);
    assert!(
        !kinds(&e).contains(&"network-ahead"),
        "one witness is not enough"
    );
    e.handle(t2 + 10 + 5 * MIN - 1, Event::Tick);
    assert!(!kinds(&e).contains(&"network-ahead"));
    e.handle(t2 + 10 + 5 * MIN, Event::Tick);
    assert!(
        e.health()
            .alarms
            .contains(&Alarm::SamplesAhead { count: 2 }),
        "{:?}",
        e.health().alarms
    );
    // samples are forgotten after half an hour
    e.handle(t2 + 10 + 31 * MIN, Event::Tick);
    assert_eq!(e.health().samples, 0);
    assert!(!kinds(&e).contains(&"network-ahead"));
}

#[test]
fn samples_that_do_not_report_more_work_than_ours_are_no_alarm() {
    let rig = SimRig::new("feeler-level", 0);
    let cfg = EngineConfig {
        stale_tip_ms: 0,
        ..feeler_cfg()
    };
    let mut e = node_with_ten_untried(&rig, cfg);
    e.handle(T0 + 1000, Event::Tick);
    let t1 = T0 + 2 * MIN;
    feel(&mut e, &rig, 50, t1, 0);
    feel(&mut e, &rig, 51, t1 + 2 * MIN, 0);
    e.handle(t1 + 10 * MIN, Event::Tick);
    assert_eq!(e.health().samples, 2);
    assert_eq!(e.health().samples_ahead, 0);
    assert!(!kinds(&e).contains(&"network-ahead"));
}

// ---- cases found by injecting faults ----------------------------------------------------------------------------------------

#[test]
fn a_node_with_no_outbound_peers_at_all_has_the_few_outbound_alarm_and_not_the_groups_one() {
    let rig = SimRig::new("health-none", 0);
    let mut e = engine_on(&rig, quiet(8));
    e.handle(T0, Event::Tick);
    e.handle(T0 + 5 * MIN, Event::Tick);
    assert_eq!(kinds(&e), vec!["few-outbound"], "{:?}", e.health().alarms);
}

#[test]
fn seeds_in_one_network_group_are_no_alarm_while_the_first_start_is_still_going() {
    let rig = SimRig::new("health-groups-boot", 0);
    let seeds = vec!["20.5.1.1:8333".to_string(), "20.5.2.1:8333".to_string()];
    let cfg = EngineConfig {
        seeds: seeds.clone(),
        bootstrap_wait_ms: 20 * 1000,
        ..quiet(8)
    };
    let mut e = engine_on(&rig, cfg);
    assert_eq!(connects(&e.handle(T0, Event::Tick)).len(), 2);
    connect(&mut e, &rig, 1, &seeds[0], false, T0 + 10);
    connect(&mut e, &rig, 2, &seeds[1], false, T0 + 20);
    e.handle(T0 + 1000, Event::Tick);
    // two outbound peers, both seeds, in one group, no answer yet: the seeds are only a start
    assert!(e.health().bootstrapping);
    assert_eq!(e.health().outbound, 2);
    assert!(
        !kinds(&e).contains(&"few-groups"),
        "{:?}",
        e.health().alarms
    );
}

#[test]
fn a_feeler_is_dropped_but_the_same_address_dialled_later_as_an_ordinary_peer_stays() {
    // an address a feeler has tried is "tried", so the node may dial it for itself later; that connection must not be mistaken for a
    // feeler (the marker of the finished one has to be gone)
    let rig = SimRig::new("feeler-then-peer", 0);
    let x = "20.150.1.1:8333".to_string();
    let mut e = engine_on(&rig, feeler_cfg());
    e.handle(T0, Event::Tick);
    connect(&mut e, &rig, 1, &sim_addr(1), false, T0 + 1);
    answer(&mut e, 1, std::slice::from_ref(&x), T0 + 2);
    let at = T0 + 2 * MIN;
    assert_eq!(connects(&e.handle(at, Event::Tick)), vec![x.clone()]);
    e.handle(
        at + 5,
        Event::PeerConnected {
            peer: 50,
            addr: x.clone(),
            inbound: false,
        },
    );
    let acts = e.handle(
        at + 10,
        Event::Message {
            peer: 50,
            msg: hello(&rig, 150, work_bytes(0)),
        },
    );
    assert_eq!(disconnects(&acts), vec![(50, "feeler done".to_string())]);
    assert!(e.addr_book().get(&x).unwrap().tried);
    // the node loses its only peer and wants one: the tried address is dialled, by the ordinary rules
    e.handle(at + 1000, Event::PeerDisconnected { peer: 1 });
    let later = at + 40_000;
    let dialled = connects(&e.handle(later, Event::Tick));
    assert_eq!(dialled, vec![x.clone()]);
    e.handle(
        later + 5,
        Event::PeerConnected {
            peer: 60,
            addr: x.clone(),
            inbound: false,
        },
    );
    let acts = e.handle(
        later + 10,
        Event::Message {
            peer: 60,
            msg: hello(&rig, 160, work_bytes(0)),
        },
    );
    assert!(
        disconnects(&acts).is_empty(),
        "an ordinary peer was treated as a feeler: {acts:?}"
    );
    assert_eq!(e.outbound_count(), 1);
    assert_eq!(e.stats.feelers_sampled, 1);
}

#[test]
fn a_banned_address_is_not_dialled_by_a_feeler() {
    let rig = SimRig::new("feeler-banned", 0);
    let mut e = engine_on(&rig, feeler_cfg());
    e.handle(T0, Event::Tick);
    connect(&mut e, &rig, 1, &sim_addr(1), false, T0 + 1);
    // an inbound peer on 20.101.1.1 misbehaves until it is banned (addresses nobody asked for, five times)
    connect(&mut e, &rig, 2, "20.101.1.1:55555", true, T0 + 2);
    for k in 0..5u64 {
        answer(&mut e, 2, &list(300 + k as usize, 1), T0 + 10 + k);
    }
    assert!(!e.has_peer(2), "the peer was not banned");
    assert!(e.is_banned("20.101.1.1:8333", T0 + 100));
    // the banned host's address is in the book (another peer told us of it); the feeler takes the other one
    answer(
        &mut e,
        1,
        &["20.101.1.1:8333".to_string(), "20.102.1.1:8333".to_string()],
        T0 + 200,
    );
    e.handle(T0 + 1000, Event::Tick);
    let dialled = connects(&e.handle(T0 + 2 * MIN, Event::Tick));
    assert!(
        !dialled.contains(&"20.101.1.1:8333".to_string()),
        "{dialled:?}"
    );
}

#[test]
fn a_feeler_holds_no_slot_while_it_is_dialled_or_connected() {
    let rig = SimRig::new("feeler-wanted", 0);
    let cfg = EngineConfig {
        feeler_interval_ms: 1000,
        outbound_target: 3,
        peer_target: 3,
        ..quiet(3)
    };
    let mut e = engine_on(&rig, cfg);
    e.handle(T0, Event::Tick);
    connect(&mut e, &rig, 1, &sim_addr(1), false, T0 + 1);
    let book = list(150, 4);
    answer(&mut e, 1, &book, T0 + 2);
    // the first tick: a feeler (first, it is started before the ordinary dials) and, since the node holds one of its three wanted outbound
    // peers, two ordinary dials: the feeler on its way does not count as one of the node's own
    let first = connects(&e.handle(T0 + 1000, Event::Tick));
    assert_eq!(first.len(), 3, "{first:?}");
    let (feeler, ordinary_ok, ordinary_failed) =
        (first[0].clone(), first[1].clone(), first[2].clone());
    let undialled: Vec<&String> = book.iter().filter(|a| !first.contains(a)).collect();
    assert_eq!(undialled.len(), 1);
    // the feeler connects and has not said hello; one ordinary dial connects, the other fails
    e.handle(
        T0 + 1500,
        Event::PeerConnected {
            peer: 50,
            addr: feeler,
            inbound: false,
        },
    );
    connect(&mut e, &rig, 51, &ordinary_ok, false, T0 + 1500);
    e.handle(
        T0 + 1600,
        Event::ConnectFailed {
            addr: ordinary_failed,
        },
    );
    // two real outbound peers of the three wanted: one more is dialled, and it is the address nothing has tried yet
    let second = connects(&e.handle(T0 + 2000, Event::Tick));
    assert_eq!(
        second,
        vec![undialled[0].clone()],
        "the feeler counted as one of the peers the node wanted"
    );
}

#[test]
fn a_feeler_does_not_retry_an_address_that_just_failed_until_its_backoff_is_over() {
    let rig = SimRig::new("feeler-backoff", 0);
    let cfg = EngineConfig {
        feeler_interval_ms: 1000,
        ..quiet(1)
    };
    let mut e = engine_on(&rig, cfg);
    e.handle(T0, Event::Tick);
    connect(&mut e, &rig, 1, &sim_addr(1), false, T0 + 1);
    let x = "20.150.1.1:8333".to_string();
    answer(&mut e, 1, std::slice::from_ref(&x), T0 + 2);
    assert_eq!(connects(&e.handle(T0 + 1000, Event::Tick)), vec![x.clone()]);
    e.handle(T0 + 1100, Event::ConnectFailed { addr: x.clone() });
    // the interval (1 s) is over long before the address's backoff (30 s) is
    assert!(connects(&e.handle(T0 + 5000, Event::Tick)).is_empty());
    assert!(connects(&e.handle(T0 + 29_000, Event::Tick)).is_empty());
    assert_eq!(connects(&e.handle(T0 + 32_000, Event::Tick)), vec![x]);
}
