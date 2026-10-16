//! Limits that one attacker cannot get around by being many connections (the review of 2026-10-16): one IP address may hold
//! only a few inbound slots; every peer that comes through this node's `.onion` service shares one group and one cap, and is
//! never put on the ban list (it is a name for one connection); `.onion` addresses are one network group with a cap of their
//! own for outbound peers and for the address book; and the onion service as a whole slows down when its peers misbehave.

use std::time::{Duration, Instant};

use tenero_chain::{ProofsNotChecked, Sha256Pow};
use tenero_core::u256::U256;
use tenero_net::addrbook::{
    group_of, AddrBook, AddrBookConfig, BanList, ONION_GROUP, ONION_INBOUND_GROUP,
};
use tenero_net::onion::{onion_addr, ONION_INBOUND, ONION_LEN};
use tenero_net::sim::SimRig;
use tenero_net::transport::OnionGate;
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

/// Connects `peer` from `addr` (inbound) and says hello; whether the engine refused it at once.
fn refused(e: &mut Engine<'_>, rig: &SimRig, peer: PeerId, addr: &str) -> bool {
    let actions = e.handle(
        T0 + peer,
        Event::PeerConnected {
            peer,
            addr: addr.to_string(),
            inbound: true,
        },
    );
    let gone = actions
        .iter()
        .any(|a| matches!(a, Action::Disconnect { peer: p, .. } if *p == peer));
    if !gone {
        e.handle(
            T0 + peer,
            Event::Message {
                peer,
                msg: hello(rig, 1000 + peer),
            },
        );
    }
    gone
}

fn onion(n: u8) -> String {
    let mut b = [n; ONION_LEN];
    b[ONION_LEN - 1] = 3;
    onion_addr(&b, 38353)
}

#[test]
fn one_ip_address_holds_only_a_few_inbound_slots() {
    let rig = SimRig::new("zones-host", 0);
    let mut e = engine_on(
        &rig,
        EngineConfig {
            nonce: 7,
            max_inbound_per_host: 4,
            ..EngineConfig::default()
        },
    );
    for p in 1..=4u64 {
        assert!(
            !refused(&mut e, &rig, p, &format!("20.1.1.1:{}", 5000 + p)),
            "{p}"
        );
    }
    assert!(
        refused(&mut e, &rig, 5, "20.1.1.1:5005"),
        "a fifth from one address"
    );
    // another address is welcome
    assert!(!refused(&mut e, &rig, 6, "20.2.1.1:5000"));
    assert_eq!(e.peer_count(), 5);
}

#[test]
fn peers_through_the_onion_service_share_one_cap_and_one_group_and_are_never_on_the_ban_list() {
    let rig = SimRig::new("zones-onion", 0);
    let mut e = engine_on(
        &rig,
        EngineConfig {
            nonce: 7,
            max_onion_inbound: 3,
            ..EngineConfig::default()
        },
    );
    for p in 1..=3u64 {
        assert!(!refused(&mut e, &rig, p, &format!("{ONION_INBOUND}{p}")));
    }
    assert!(refused(&mut e, &rig, 4, &format!("{ONION_INBOUND}4")));
    // an ordinary inbound peer still gets in
    assert!(!refused(&mut e, &rig, 5, "20.3.1.1:5000"));
    assert_eq!(group_of(&format!("{ONION_INBOUND}1")), ONION_INBOUND_GROUP);
    assert_eq!(group_of(&format!("{ONION_INBOUND}99")), ONION_INBOUND_GROUP);
    // a ban of one of them is not kept: it would ban nobody and fill the list
    let mut bans = BanList::new();
    bans.ban(&format!("{ONION_INBOUND}1"), u64::MAX);
    assert!(bans.is_empty());
    bans.ban("20.3.1.1:5000", u64::MAX);
    assert_eq!(bans.len(), 1);
}

#[test]
fn onion_addresses_are_one_group_with_a_cap_on_outbound_peers_and_on_the_address_book() {
    assert_eq!(group_of(&onion(1)), ONION_GROUP);
    assert_eq!(group_of(&onion(2)), ONION_GROUP);
    // the address book keeps at most `max_onion_entries` of them (seeds aside)
    let mut book = AddrBook::new(AddrBookConfig {
        accept_onion: true,
        max_onion_entries: 5,
        max_new_per_source: 1000,
        ..AddrBookConfig::default()
    });
    let now = 1_700_000_000;
    let kept = (1..=20u8)
        .filter(|n| book.add(&onion(*n), now, &format!("v4:20.{n}"), now))
        .count();
    assert_eq!(kept, 5);
    assert!(
        book.add(&onion(30), now, "seed", now),
        "a seed is always kept"
    );

    // outbound: ten onion seeds, room for eight outbound peers, at most `max_outbound_onion` onion dials
    let rig = SimRig::new("zones-onion-out", 0);
    let seeds: Vec<String> = (1..=10u8).map(onion).collect();
    let mut e = engine_on(
        &rig,
        EngineConfig {
            nonce: 7,
            peer_target: 8,
            outbound_target: 8,
            max_connect_per_tick: 8,
            max_outbound_onion: 3,
            feeler_interval_ms: 0,
            bootstrap_wait_ms: 0,
            seeds,
            addrbook: AddrBookConfig {
                accept_onion: true,
                ..AddrBookConfig::default()
            },
            ..EngineConfig::default()
        },
    );
    let dials: Vec<String> = e
        .handle(T0, Event::Tick)
        .into_iter()
        .filter_map(|a| match a {
            Action::Connect { addr } => Some(addr),
            _ => None,
        })
        .collect();
    assert_eq!(dials.len(), 3, "{dials:?}");
}

#[test]
fn the_onion_service_takes_a_burst_then_its_rate_and_pauses_after_a_ban() {
    let gate = OnionGate::new(6);
    let t = Instant::now();
    let taken = (0..20).filter(|_| gate.admit(t)).count();
    assert_eq!(taken, 6, "a minute's worth at once");
    assert!(
        !gate.admit(t + Duration::from_secs(5)),
        "one every ten seconds"
    );
    assert!(gate.admit(t + Duration::from_secs(11)));
    // a ban: nobody for a minute, then the rate again; a second ban soon after pauses twice as long
    gate.banned(t + Duration::from_secs(20));
    assert!(!gate.admit(t + Duration::from_secs(79)));
    assert!(gate.admit(t + Duration::from_secs(91)));
    gate.banned(t + Duration::from_secs(100));
    assert!(!gate.admit(t + Duration::from_secs(219)), "two minutes");
    assert!(gate.admit(t + Duration::from_secs(231)));
    // no limit
    let open = OnionGate::new(0);
    assert!((0..1000).all(|_| open.admit(t)));
}

/// A hello says where pruning stops rounded up to a thousand: the exact height was the same on a node's Tor and direct
/// links and so tied the two identities together. Rounded UP, so a node never claims a block it lacks.
#[test]
fn the_pruning_height_a_hello_shows_is_rounded_up_to_a_thousand() {
    use tenero_net::engine::shown_pruned_below;
    assert_eq!(shown_pruned_below(0), 0);
    assert_eq!(shown_pruned_below(1), 1000);
    assert_eq!(shown_pruned_below(1000), 1000);
    assert_eq!(shown_pruned_below(12_345), 13_000);
    assert_eq!(
        shown_pruned_below(u64::MAX),
        u64::MAX,
        "saturates, never wraps"
    );
}

/// A transaction that is only stale (its reference block aged out, or the minimum fee rose past what it pays) is refused
/// without costing the peer anything: an honest peer's pool can hold one, and five of them banned it. A block of a branch this
/// pruned node cannot switch to is not the peer's fault either. Forged and malformed ones still cost.
#[test]
fn stale_transactions_and_branches_below_the_pruned_part_are_not_held_against_the_peer() {
    use tenero_chain::BlockError;
    use tenero_net::engine::{block_refusal_blames_sender, tx_refusal_blames_sender};
    use tenero_node::PoolError;
    let inv = PoolError::Invalid;
    assert!(!tx_refusal_blames_sender(&inv(BlockError::KeyImageSpent {
        tx: 0,
        key_image: [0; 32]
    })));
    assert!(!tx_refusal_blames_sender(&inv(BlockError::BadReference {
        tx: 0,
        reference_height: 5
    })));
    assert!(!tx_refusal_blames_sender(&inv(BlockError::FeeTooLow {
        tx: 0,
        fee: 1,
        min: 2
    })));
    assert!(tx_refusal_blames_sender(&inv(BlockError::ProofRejected {
        tx: 0,
        reason: "x".into()
    })));
    assert!(tx_refusal_blames_sender(&inv(BlockError::BadKeyImage {
        tx: 0,
        input: 0
    })));
    assert!(tx_refusal_blames_sender(&PoolError::TooLarge {
        weight: 2,
        limit: 1
    }));
    assert!(!tx_refusal_blames_sender(&PoolError::PoolFull));
    assert!(!block_refusal_blames_sender(&BlockError::ReorgTooDeep {
        fork_height: 1,
        pruned_below: 2
    }));
    assert!(!block_refusal_blames_sender(&BlockError::Store(
        "disk".into()
    )));
    assert!(block_refusal_blames_sender(&BlockError::PowTargetNotMet));
}

/// The engine's bounded sets forget their oldest entry, not everything at once.
#[test]
fn a_full_recent_set_forgets_only_its_oldest_entry() {
    use tenero_net::engine::RecentSet;
    let mut r = RecentSet::new(3);
    for i in 1..=4u8 {
        r.insert([i; 32]);
    }
    assert_eq!(r.len(), 3);
    assert!(!r.contains(&[1; 32]));
    assert!((2..=4u8).all(|i| r.contains(&[i; 32])));
    r.insert([3; 32]);
    assert_eq!(r.len(), 3, "an id already held is not added twice");
}
