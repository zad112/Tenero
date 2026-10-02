//! How likely is a brand-new node to end up with only hostile outbound peers, as a function of how many of its seeds are honest
//! and how many are hostile? The REAL engine runs; only the network around it is scripted (`docs/SEED_POLICY.md` has the
//! model, the numbers and what they do not show).
//!
//! The model, stated plainly:
//! * a new node knows only its seeds (no saved address book, no pinned peers, no anchors);
//! * an honest seed (or any honest peer) answers an address request with 100 random addresses from a population of honest
//!   nodes spread over many network groups; a hostile seed answers with 100 addresses the attacker controls, spread over a
//!   few groups, and every hostile address accepts connections and answers like an honest peer would (the worst case: the
//!   node cannot tell them apart by behaviour);
//! * hostile peers answer at once; honest ones answer after `honest_delay` seconds (the attacker's best timing);
//! * seeds hang up after they have answered, and every dial succeeds. The measure is the node's outbound peers two minutes after
//!   it starts, not counting seeds: are ALL of them hostile (an eclipse), and what share of them is.
//!
//! What is NOT modelled: an attacker who is also a large share of the honest population, a seed list that is poisoned in the
//! release itself, an attacker who can block the node's connections to honest addresses, and any difference in how long
//! peers stay up.

use std::collections::{BTreeMap, HashMap, HashSet};

use tenero_chain::{ProofsNotChecked, Sha256Pow};
use tenero_net::addrbook::{AddrBookConfig, XorShift};
use tenero_net::message::PeerAddr;
use tenero_net::sim::SimRig;
use tenero_net::{Action, Engine, EngineConfig, Event, Hello, Message, PROTOCOL_VERSION};
use tenero_node::{Node, NodeConfig};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rules {
    /// The engine as it was before these rules: a node dials whatever it knows.
    Baseline,
    /// The engine's defaults: dial nothing but seeds until every seed group has answered (up to 20 s).
    Wait,
    /// The wait, and addresses reported by two or more sources are preferred.
    WaitAndCorroboration,
}

#[derive(Clone, Copy, Debug)]
pub struct Scenario {
    pub honest_seeds: usize,
    pub hostile_seeds: usize,
    pub honest_delay_s: u64,
    /// The hostile seeds return the SAME list (so every address of it is reported by several sources).
    pub coordinated: bool,
    /// How many network groups the attacker's addresses are spread over.
    pub hostile_groups: usize,
    pub rules: Rules,
    /// Every outbound peer drops at this second (a restart of the other side, a network blip) and the node refills its slots from the
    /// address book it has built by then; the outcome is then about the first 8 dials AFTER that.
    pub churn_at: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Outcome {
    pub outbound: usize,
    pub hostile: usize,
    /// The first (up to) 8 non-seed addresses the node DIALLED, in order: the slots an eclipse has to capture.
    pub first: usize,
    pub first_hostile: usize,
}

const HONEST_POP: usize = 2000;
const HOSTILE_POP: usize = 500;
const ANSWER: usize = 100;
const RUN_S: u64 = 120;

fn addr(a: u8, b: u8, c: u8, d: u8) -> String {
    format!("{a}.{b}.{c}.{d}:8333")
}

fn peer_addr(a: &str, last_seen: u64) -> PeerAddr {
    let sa: std::net::SocketAddr = a.parse().unwrap();
    let std::net::IpAddr::V4(v4) = sa.ip() else {
        unreachable!()
    };
    PeerAddr {
        ip: v4.to_ipv6_mapped().octets(),
        port: sa.port(),
        last_seen,
    }
}

enum Ev {
    Connected(u64, String),
    Message(u64, Message),
    Disconnected(u64),
}

pub fn run_trial(sc: &Scenario, trial: u64, rig: &SimRig) -> Outcome {
    let mut rng = XorShift(0x9e37_79b9 ^ trial.wrapping_mul(0x1234_5679) ^ 0xabcd);
    // the honest population: addresses in many different /16 groups (20.x to 89.x)
    let mut honest: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    while honest.len() < HONEST_POP {
        let a = addr(
            20 + rng.below(70) as u8,
            rng.below(256) as u8,
            rng.below(256) as u8,
            1 + rng.below(250) as u8,
        );
        if seen.insert(a.clone()) {
            honest.push(a);
        }
    }
    // the attacker's: `hostile_groups` /16 groups of 100.x
    let mut hostile: Vec<String> = Vec::new();
    let mut hseen = HashSet::new();
    while hostile.len() < HOSTILE_POP {
        let a = addr(
            100,
            rng.below(sc.hostile_groups.max(1)) as u8,
            rng.below(256) as u8,
            1 + rng.below(250) as u8,
        );
        if hseen.insert(a.clone()) {
            hostile.push(a);
        }
    }
    let hostile_set: HashSet<String> = hostile.iter().cloned().collect();
    // the seeds: each in a network group of its own
    let honest_seeds: Vec<String> = (0..sc.honest_seeds)
        .map(|i| addr(5, i as u8, 0, 1))
        .collect();
    let hostile_seeds: Vec<String> = (0..sc.hostile_seeds)
        .map(|i| addr(6, i as u8, 0, 1))
        .collect();
    let mut seeds: Vec<String> = honest_seeds.iter().chain(&hostile_seeds).cloned().collect();
    rng.shuffle(&mut seeds);
    let seed_set: HashSet<String> = seeds.iter().cloned().collect();
    let hostile_all: HashSet<String> = hostile_set
        .iter()
        .cloned()
        .chain(hostile_seeds.iter().cloned())
        .collect();
    // coordinated hostile seeds all hand out one list
    let shared: Vec<String> = hostile.iter().take(ANSWER).cloned().collect();

    let mut book = AddrBookConfig {
        seed: trial.wrapping_mul(7919) + 1,
        ..AddrBookConfig::default()
    };
    let mut cfg = EngineConfig {
        nonce: 5,
        seeds: seeds.clone(),
        ping_after_ms: u64::MAX / 4,
        pong_timeout_ms: u64::MAX / 4,
        ..EngineConfig::default()
    };
    match sc.rules {
        Rules::Baseline => cfg.bootstrap_wait_ms = 0,
        Rules::Wait => {}
        Rules::WaitAndCorroboration => book.prefer_corroborated = true,
    }
    cfg.addrbook = book;
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
    let mut e = Engine::new(node, cfg);
    let genesis = rig.store.block_index(0).unwrap().unwrap();

    let t0: u64 = 1_700_000_000;
    let mut dialled_order: Vec<String> = Vec::new();
    let mut connected: Vec<u64> = Vec::new();
    let mut queue: BTreeMap<u64, Vec<Ev>> = BTreeMap::new();
    let mut who: HashMap<u64, String> = HashMap::new();
    let mut next_peer: u64 = 1;
    for second in 0..RUN_S {
        let now_ms = (t0 + second) * 1000;
        if sc.churn_at == Some(second) {
            // the peers the node has chosen all go away (the seeds already have); what it dials from now on is the refill
            for id in connected.drain(..) {
                queue.entry(second).or_default().push(Ev::Disconnected(id));
            }
            dialled_order.clear();
        }
        let mut todo: Vec<Event> = vec![Event::Tick];
        let mut due = queue.remove(&second).unwrap_or_default();
        loop {
            for ev in due.drain(..) {
                todo.push(match ev {
                    Ev::Connected(id, a) => Event::PeerConnected {
                        peer: id,
                        addr: a,
                        inbound: false,
                    },
                    Ev::Message(id, msg) => Event::Message { peer: id, msg },
                    Ev::Disconnected(id) => Event::PeerDisconnected { peer: id },
                });
            }
            if todo.is_empty() {
                break;
            }
            let mut again: Vec<Ev> = Vec::new();
            for ev in std::mem::take(&mut todo) {
                let connected_now = match &ev {
                    Event::PeerConnected { peer, addr, .. } => Some((*peer, addr.clone())),
                    _ => None,
                };
                // a seed hangs up once it has given its addresses (as a busy seed does: see `max_addr_only`)
                let seed_answered = match &ev {
                    Event::Message {
                        peer,
                        msg: Message::Addrs { .. },
                    } if who.get(peer).is_some_and(|a| seed_set.contains(a)) => Some(*peer),
                    _ => None,
                };
                for action in e.handle(now_ms, ev) {
                    match action {
                        Action::Connect { addr: a } => {
                            if !seed_set.contains(&a) {
                                dialled_order.push(a.clone());
                            }
                            let id = next_peer;
                            next_peer += 1;
                            who.insert(id, a.clone());
                            again.push(Ev::Connected(id, a));
                        }
                        Action::Send {
                            peer,
                            msg: Message::GetAddrs,
                        } => {
                            let a = who.get(&peer).cloned().unwrap_or_default();
                            let is_hostile = hostile_all.contains(&a);
                            let list: Vec<String> = if is_hostile {
                                if sc.coordinated && seed_set.contains(&a) {
                                    shared.clone()
                                } else {
                                    sample(&hostile, ANSWER, &mut rng)
                                }
                            } else {
                                sample(&honest, ANSWER, &mut rng)
                            };
                            let addrs = list.iter().map(|x| peer_addr(x, t0 + second)).collect();
                            let delay = if is_hostile { 0 } else { sc.honest_delay_s };
                            queue
                                .entry(second + delay)
                                .or_default()
                                .push(Ev::Message(peer, Message::Addrs { addrs }));
                        }
                        _ => {}
                    }
                }
                if let Some(id) = seed_answered {
                    again.push(Ev::Disconnected(id));
                }
                if let Some((id, a)) = &connected_now {
                    if !seed_set.contains(a) {
                        connected.push(*id);
                    }
                }
                if let Some((id, _)) = connected_now {
                    // the peer answers the connection with its own Hello at once
                    again.push(Ev::Message(
                        id,
                        Message::Hello(Hello {
                            version: PROTOCOL_VERSION,
                            chain_id: rig.store.chain_id(),
                            tip_height: 0,
                            cumulative_work: genesis.cumulative_work,
                            tip_id: genesis.block_id,
                            pruned_below: 0,
                            nonce: 1000 + id,
                        }),
                    ));
                }
            }
            due = again;
            // events scheduled for NOW by the actions above (zero-delay answers) are delivered in this same second
            if let Some(more) = queue.remove(&second) {
                due.extend(more);
            }
            if due.is_empty() {
                break;
            }
        }
    }
    // what the node chose from what it was told: the seeds themselves do not count (they hang up, and are only a start)
    let outbound: Vec<String> = e
        .outbound_addrs()
        .into_iter()
        .filter(|a| !seed_set.contains(a))
        .collect();
    let first: Vec<&String> = dialled_order.iter().take(8).collect();
    Outcome {
        outbound: outbound.len(),
        hostile: outbound.iter().filter(|a| hostile_all.contains(*a)).count(),
        first: first.len(),
        first_hostile: first.iter().filter(|a| hostile_all.contains(**a)).count(),
    }
}

fn sample(from: &[String], n: usize, rng: &mut XorShift) -> Vec<String> {
    let mut idx: Vec<usize> = (0..from.len()).collect();
    rng.shuffle(&mut idx);
    idx.into_iter().take(n).map(|i| from[i].clone()).collect()
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Summary {
    pub trials: u64,
    /// Every outbound peer hostile (and at least one outbound peer).
    pub eclipsed: u64,
    /// More than half of the outbound peers hostile.
    pub majority: u64,
    pub mean_hostile_fraction: f64,
    /// The same for the first 8 non-seed addresses dialled.
    pub mean_first_fraction: f64,
    /// Every one of the first 8 hostile.
    pub first_eclipsed: u64,
    pub mean_outbound: f64,
    /// No outbound peer at all after two minutes.
    pub none: u64,
}

pub fn run_many(sc: &Scenario, trials: u64, rig: &SimRig) -> Summary {
    let mut s = Summary {
        trials,
        ..Summary::default()
    };
    let mut frac = 0.0;
    let mut first_frac = 0.0;
    let mut out = 0.0;
    for t in 0..trials {
        let o = run_trial(sc, t, rig);
        out += o.outbound as f64;
        if o.first > 0 {
            first_frac += o.first_hostile as f64 / o.first as f64;
            if o.first_hostile == o.first {
                s.first_eclipsed += 1;
            }
        }
        if o.outbound == 0 {
            s.none += 1;
            continue;
        }
        frac += o.hostile as f64 / o.outbound as f64;
        if o.hostile == o.outbound {
            s.eclipsed += 1;
        }
        if o.hostile * 2 > o.outbound {
            s.majority += 1;
        }
    }
    s.mean_hostile_fraction = frac / trials as f64;
    s.mean_first_fraction = first_frac / trials as f64;
    s.mean_outbound = out / trials as f64;
    s
}

fn scenario(h: usize, x: usize, delay: u64, rules: Rules) -> Scenario {
    Scenario {
        honest_seeds: h,
        hostile_seeds: x,
        honest_delay_s: delay,
        coordinated: std::env::var("TENERO_SIM_COORDINATED").map_or(true, |v| v != "0"),
        hostile_groups: 40,
        rules,
        churn_at: None,
    }
}

const ALL: [(Rules, &str); 3] = [
    (Rules::Baseline, "baseline"),
    (Rules::Wait, "wait"),
    (Rules::WaitAndCorroboration, "wait+corrob"),
];

/// The measurement that the policy is based on. Slow; `cargo test --release -p tenero-net --test eclipse_sim -- --ignored
/// --nocapture`; `TENERO_SIM_TRIALS` sets the number of trials per cell (default 150).
#[test]
#[ignore]
fn measure_the_chance_of_an_eclipse() {
    let trials: u64 = std::env::var("TENERO_SIM_TRIALS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(150);
    let rig = SimRig::new("eclipse-measure", 0);
    eprintln!(
        "
A new node with {trials} trials per cell. Hostile peers answer at once, honest ones after 3 s; the attacker's seeds share          one list. Each cell: the share of the node's FIRST 8 dialled non-seed peers that are hostile (and, in brackets, how often          all 8 are)
"
    );
    let mut header = String::from("honest hostile |");
    for (_, name) in ALL {
        header += &format!(" {name:>15} |");
    }
    eprintln!("{header}");
    for h in [1, 2, 3, 4, 6] {
        for x in [0, 1, 2, 3, 4] {
            let mut line = format!("{h:>6} {x:>7} |");
            for (rules, _) in ALL {
                let c = run_many(&scenario(h, x, 3, rules), trials, &rig);
                line += &format!(
                    " {:>6.1}% ({:>4.1}%) |",
                    100.0 * c.mean_first_fraction,
                    100.0 * c.first_eclipsed as f64 / c.trials as f64
                );
            }
            eprintln!("{line}");
        }
    }
}

/// What the origin limit is for: after the node's peers have all dropped at 60 s, it refills its slots from the address book it
/// built in that minute, and by then hostile peers it connected to have each added their own addresses (every peer of theirs is a new
/// source). `TENERO_SIM_COORDINATED=0` makes the hostile seeds give different lists.
#[test]
#[ignore]
fn measure_the_refill_after_all_peers_drop() {
    let trials: u64 = std::env::var("TENERO_SIM_TRIALS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let rig = SimRig::new("eclipse-churn", 0);
    eprintln!(
        "\nThe share of the first 8 dials AFTER every outbound peer dropped at 60 s that are hostile ({trials} trials per cell)\n"
    );
    eprintln!("honest hostile |        baseline |            wait |");
    for h in [2, 3, 6] {
        for x in [1, 2, 3, 4] {
            let mut line = format!("{h:>6} {x:>7} |");
            for rules in [Rules::Baseline, Rules::Wait] {
                let sc = Scenario {
                    churn_at: Some(60),
                    ..scenario(h, x, 3, rules)
                };
                let c = run_many(&sc, trials, &rig);
                line += &format!(
                    " {:>6.1}% ({:>4.1}%) |",
                    100.0 * c.mean_first_fraction,
                    100.0 * c.first_eclipsed as f64 / c.trials as f64
                );
            }
            eprintln!("{line}");
        }
    }
}

// ---- what the measurements showed, as tests (60 trials a cell: the numbers are deterministic, the margins allow for the smaller sample) ----

fn first_share(sc: &Scenario, rig: &SimRig) -> f64 {
    run_many(sc, 60, rig).mean_first_fraction
}

#[test]
fn with_only_honest_seeds_no_rule_costs_a_node_its_peers_or_lets_a_hostile_one_in() {
    let rig = SimRig::new("eclipse-honest", 0);
    for (rules, _) in ALL {
        let s = run_many(&scenario(4, 0, 3, rules), 20, &rig);
        assert_eq!(s.first_eclipsed, 0, "{rules:?}");
        assert_eq!(s.mean_first_fraction, 0.0, "{rules:?}");
        assert_eq!(s.none, 0, "{rules:?}: a node found no peer at all");
        assert!(s.mean_outbound >= 6.0, "{rules:?}: {s:?}");
    }
}

#[test]
fn one_fast_hostile_seed_takes_every_first_dial_without_the_wait_and_a_minority_with_it() {
    let rig = SimRig::new("eclipse-fast", 0);
    // three honest seeds and one hostile one that answers at once while the honest ones take 3 s (measured: 100% against 27%)
    let base = first_share(&scenario(3, 1, 3, Rules::Baseline), &rig);
    let wait = first_share(&scenario(3, 1, 3, Rules::Wait), &rig);
    assert!(base >= 0.95, "baseline {base}");
    assert!(wait <= 0.45, "wait {wait}");
}

#[test]
fn the_wait_gives_an_attacker_about_his_share_of_the_seed_list_whatever_his_speed_or_lists() {
    let rig = SimRig::new("eclipse-share", 0);
    // three honest and three hostile seeds: the hostile ones are half the list (measured 35% when they all give one list, 51% when
    // each gives its own)
    for coordinated in [true, false] {
        let sc = Scenario {
            coordinated,
            ..scenario(3, 3, 3, Rules::Wait)
        };
        let share = first_share(&sc, &rig);
        assert!(
            (0.25..=0.6).contains(&share),
            "coordinated {coordinated}: {share}"
        );
        let base = first_share(
            &Scenario {
                rules: Rules::Baseline,
                ..sc
            },
            &rig,
        );
        assert!(base >= 0.95, "baseline, coordinated {coordinated}: {base}");
    }
}

#[test]
fn nothing_helps_when_most_of_the_seed_list_is_hostile() {
    // the limit of the policy, stated as a test so that nobody mistakes it for a defence: one honest seed and four hostile ones
    let rig = SimRig::new("eclipse-limit", 0);
    let sc = Scenario {
        coordinated: false,
        ..scenario(1, 4, 3, Rules::Wait)
    };
    let share = first_share(&sc, &rig);
    assert!(
        share >= 0.6,
        "{share}: the rules cannot make a list of mostly hostile seeds safe"
    );
}

#[test]
fn preferring_corroborated_addresses_helps_the_attacker_who_coordinates_his_seeds() {
    // honest seeds give independent random samples, which rarely overlap; coordinated hostile ones give one list, which overlaps
    // completely: so "reported twice" picks out the attacker (measured: 47% against 22%). That is why it is off.
    let rig = SimRig::new("eclipse-corrob", 0);
    let wait = first_share(&scenario(6, 3, 3, Rules::Wait), &rig);
    let corrob = first_share(&scenario(6, 3, 3, Rules::WaitAndCorroboration), &rig);
    assert!(
        corrob >= wait + 0.1,
        "corroborated {corrob}, without {wait}"
    );
}

#[test]
fn the_wait_also_protects_the_refill_after_every_peer_drops() {
    let rig = SimRig::new("eclipse-refill", 0);
    let churn = |rules| Scenario {
        churn_at: Some(60),
        ..scenario(3, 2, 3, rules)
    };
    let base = first_share(&churn(Rules::Baseline), &rig);
    let wait = first_share(&churn(Rules::Wait), &rig);
    assert!(base >= wait + 0.1, "baseline {base}, wait only {wait}");
}
