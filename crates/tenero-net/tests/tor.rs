//! Tor (`docs/TOR_PLAN.md`) over real sockets, with a stand-in for Tor: a small SOCKS5 proxy in the test that delivers
//! a connection for the `.onion` address to the onion listener of node A (as Tor delivers it to an onion service), and a
//! connection for an IP address to that address. Node B dials only through it. Then, in the simulator, a node's own
//! transaction goes to its `.onion` peer first.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tenero_chain::{ProofsNotChecked, Sha256Pow};
use tenero_net::addrbook::AddrBookConfig;
use tenero_net::noise::NodeKey;
use tenero_net::sim::{Sim, SimConfig, SimRig};
use tenero_net::transport::{Hooks, Logger, Net, NetConfig, NoHooks};
use tenero_net::{Engine, EngineConfig, Event};
use tenero_node::{Node, NodeConfig};

const T0: u64 = 1_700_000_000;
const ONION: &str = "2gzyxa5ihm7nsggfxnu52rck2vv4rvmdlkiu3zzui5du4xyclen53wid.onion:38353";

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
    .unwrap();
    Engine::new(node, cfg)
}

fn engine_cfg(seeds: &[&str]) -> EngineConfig {
    EngineConfig {
        addrbook: AddrBookConfig {
            accept_private: true,
            accept_onion: true,
            ..AddrBookConfig::default()
        },
        seeds: seeds.iter().map(|s| s.to_string()).collect(),
        peer_target: 4,
        outbound_target: 2,
        ..EngineConfig::default()
    }
}

#[derive(Clone)]
struct Log(Arc<Mutex<Vec<String>>>);

impl Log {
    fn new() -> Log {
        Log(Arc::new(Mutex::new(Vec::new())))
    }
    fn logger(&self) -> Logger {
        let l = Arc::clone(&self.0);
        Arc::new(move |line| l.lock().unwrap().push(line.to_string()))
    }
    fn dump(&self) -> String {
        self.0.lock().unwrap().join("\n")
    }
}

/// The stand-in for Tor. Every CONNECT it is asked for is recorded (the host as it was sent: a name or an address);
/// one for the onion address goes to `onion_to`, one for an IP address to that address.
fn fake_tor(onion_to: SocketAddr) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let at = l.local_addr().unwrap();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let rec = Arc::clone(&asked);
    thread::spawn(move || {
        for c in l.incoming() {
            let Ok(mut c) = c else { continue };
            let rec = Arc::clone(&rec);
            thread::spawn(move || {
                let mut g = [0u8; 3];
                if c.read_exact(&mut g).is_err() {
                    return;
                }
                c.write_all(&[5, g[2]]).unwrap();
                if g[2] == 2 {
                    let mut v = [0u8; 2];
                    c.read_exact(&mut v).unwrap();
                    let mut skip = vec![0u8; v[1] as usize];
                    c.read_exact(&mut skip).unwrap();
                    c.read_exact(&mut v[..1]).unwrap();
                    let mut skip = vec![0u8; v[0] as usize];
                    c.read_exact(&mut skip).unwrap();
                    c.write_all(&[1, 0]).unwrap();
                }
                let mut head = [0u8; 4];
                c.read_exact(&mut head).unwrap();
                let target = match head[3] {
                    3 => {
                        let mut n = [0u8; 1];
                        c.read_exact(&mut n).unwrap();
                        let mut name = vec![0u8; n[0] as usize];
                        c.read_exact(&mut name).unwrap();
                        String::from_utf8(name).unwrap()
                    }
                    1 => {
                        let mut ip = [0u8; 4];
                        c.read_exact(&mut ip).unwrap();
                        std::net::Ipv4Addr::from(ip).to_string()
                    }
                    _ => panic!("not asked for in this test"),
                };
                let mut port = [0u8; 2];
                c.read_exact(&mut port).unwrap();
                let port = u16::from_be_bytes(port);
                rec.lock().unwrap().push(format!("{target}:{port}"));
                let to = if target.ends_with(".onion") {
                    onion_to
                } else {
                    format!("{target}:{port}").parse().unwrap()
                };
                let Ok(mut up) = TcpStream::connect(to) else {
                    let _ = c.write_all(&[5, 4, 0, 1, 0, 0, 0, 0, 0, 0]);
                    return;
                };
                c.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 1]).unwrap();
                let mut down = c.try_clone().unwrap();
                let mut up2 = up.try_clone().unwrap();
                thread::spawn(move || {
                    let _ = io::copy(&mut down, &mut up2);
                });
                let _ = io::copy(&mut up, &mut c);
            });
        }
    });
    (at, asked)
}

struct StopWhen<F: FnMut(&Engine<'_>) -> bool> {
    done: F,
    stop: Arc<AtomicBool>,
    deadline: Instant,
}

impl<F: FnMut(&Engine<'_>) -> bool> Hooks for StopWhen<F> {
    fn poll(&mut self, engine: &mut Engine<'_>, _now_ms: u64) -> Vec<Event> {
        if (self.done)(engine) || Instant::now() > self.deadline {
            self.stop.store(true, Ordering::SeqCst);
        }
        Vec::new()
    }
}

/// B knows A only by its `.onion` address and dials only through "Tor": it syncs A's chain, the proxy was asked for the
/// onion address BY NAME (B never resolved or dialled it itself), and A saw B come in through its onion service, under a
/// name of its own (not 127.0.0.1, which every Tor peer shares).
#[test]
fn a_node_syncs_from_an_onion_service_through_tor_only() {
    let rigs = SimRig::rigs("tor-sync", 2);
    {
        let mut sim = Sim::new(&rigs, T0, SimConfig::default(), EngineConfig::default());
        sim.mine_chain(0, 30);
    }
    let (log_a, log_b) = (Log::new(), Log::new());
    let mut ca = NetConfig::new(NodeKey::generate(), rigs[0].store.chain_id());
    ca.onion_listen = Some("127.0.0.1:0".parse().unwrap());
    ca.log = log_a.logger();
    ca.tick = Duration::from_millis(100);
    let net_a = Net::bind(ca).unwrap();
    let onion_at = net_a.onion_local_addr().unwrap();
    let (proxy, asked) = fake_tor(onion_at);
    let mut cb = NetConfig::new(NodeKey::generate(), rigs[1].store.chain_id());
    cb.proxy = Some(proxy);
    cb.tor_only = true;
    cb.log = log_b.logger();
    cb.tick = Duration::from_millis(100);
    let net_b = Net::bind(cb).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (inbound_names, tip_b) = thread::scope(|s| {
        let ha = s.spawn(|| {
            let mut cfg = engine_cfg(&[]);
            cfg.advertise_onion = Some(ONION.to_string());
            let mut e = engine_on(&rigs[0], cfg);
            net_a.run(&mut e, Arc::clone(&stop), &mut NoHooks).unwrap();
        });
        let hb = s.spawn(|| {
            let mut cfg = engine_cfg(&[ONION]);
            cfg.tor_only = true;
            let mut e = engine_on(&rigs[1], cfg);
            let mut hooks = StopWhen {
                done: |e: &Engine<'_>| e.node().tip().unwrap().0 == 30,
                stop: Arc::clone(&stop),
                deadline: Instant::now() + Duration::from_secs(60),
            };
            net_b.run(&mut e, Arc::clone(&stop), &mut hooks).unwrap();
            e.node().tip().unwrap().0
        });
        ha.join().unwrap();
        (log_a.dump(), hb.join().unwrap())
    });
    assert_eq!(
        tip_b,
        30,
        "B did not sync\nA:\n{inbound_names}\nB:\n{}",
        log_b.dump()
    );
    let asked = asked.lock().unwrap().clone();
    assert!(asked.iter().all(|a| a == ONION), "{asked:?}");
    assert!(!asked.is_empty());
    assert!(
        inbound_names.contains("onion-in:1"),
        "A names its onion peers on their own:\n{inbound_names}"
    );
    assert!(
        !inbound_names.contains("connected from 127.0.0.1"),
        "{inbound_names}"
    );
}

/// With `tor_only`, an IP address is dialled through Tor too (the proxy is asked for it); without a proxy, an onion
/// address cannot be dialled at all, and the dial says why.
#[test]
fn tor_only_sends_ip_addresses_through_tor_and_no_proxy_cannot_reach_an_onion() {
    let rigs = SimRig::rigs("tor-ip", 2);
    let (log_a, log_b) = (Log::new(), Log::new());
    let mut ca = NetConfig::new(NodeKey::generate(), rigs[0].store.chain_id());
    ca.listen = Some("127.0.0.1:0".parse().unwrap());
    ca.log = log_a.logger();
    let net_a = Net::bind(ca).unwrap();
    let a_at = net_a.local_addr().unwrap();
    let (proxy, asked) = fake_tor(a_at);
    let mut cb = NetConfig::new(NodeKey::generate(), rigs[1].store.chain_id());
    cb.proxy = Some(proxy);
    cb.tor_only = true;
    cb.log = log_b.logger();
    cb.tick = Duration::from_millis(100);
    let net_b = Net::bind(cb).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let seed = a_at.to_string();
    thread::scope(|s| {
        s.spawn(|| {
            let mut e = engine_on(&rigs[0], engine_cfg(&[]));
            net_a.run(&mut e, Arc::clone(&stop), &mut NoHooks).unwrap();
        });
        s.spawn(|| {
            let mut cfg = engine_cfg(&[&seed]);
            cfg.tor_only = true;
            let mut e = engine_on(&rigs[1], cfg);
            let mut hooks = StopWhen {
                done: |e: &Engine<'_>| e.peer_count() >= 1,
                stop: Arc::clone(&stop),
                deadline: Instant::now() + Duration::from_secs(30),
            };
            net_b.run(&mut e, Arc::clone(&stop), &mut hooks).unwrap();
        });
    });
    assert_eq!(
        asked.lock().unwrap().clone(),
        vec![seed.clone()],
        "{}",
        log_b.dump()
    );

    // no proxy: the onion seed is not dialled, and the log says why
    let log_c = Log::new();
    let mut cc = NetConfig::new(NodeKey::generate(), rigs[1].store.chain_id());
    cc.log = log_c.logger();
    cc.tick = Duration::from_millis(100);
    let net_c = Net::bind(cc).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let mut e = engine_on(&rigs[1], engine_cfg(&[ONION]));
    let mut hooks = StopWhen {
        done: |_: &Engine<'_>| log_c.dump().contains("needs a Tor proxy"),
        stop: Arc::clone(&stop),
        deadline: Instant::now() + Duration::from_secs(20),
    };
    net_c.run(&mut e, Arc::clone(&stop), &mut hooks).unwrap();
    assert!(
        log_c.dump().contains("needs a Tor proxy"),
        "{}",
        log_c.dump()
    );
}

/// A node's own transaction goes first to a peer reached through Tor (here: one at a `.onion` address), and to no other.
#[test]
fn a_nodes_own_transaction_leaves_through_its_tor_peer() {
    use tenero_core::hash::sha256;
    use tenero_core::v3::ids::tx_id;
    use tenero_core::v3::rules;
    use tenero_core::v3::*;
    use tenero_tree::hash_to_point;
    let rigs = SimRig::rigs("tor-own", 3);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), EngineConfig::default());
    sim.set_node_addr(2, ONION);
    sim.connect(0, 1);
    sim.connect(0, 2);
    sim.run_for(3000);
    while sim.tip(0).0 < 61 {
        sim.mine(0, None);
        assert!(sim.run_until(60_000, |s| s.all_agree()));
        sim.run_for(61_000);
    }
    let next = sim.engines[0].node().next_block().unwrap();
    let pt = |tag: &[u8], n: u8| hash_to_point(sha256(&[tag, &[n]]));
    let out = |n: u8| Output {
        onetime_address: pt(b"addr", n),
        amount_commitment: pt(b"commit", n),
        amount_enc: [n; 8],
        view_tag: [n; 3],
        anchor_enc: [n; 16],
    };
    let mut outputs = vec![out(1), out(2)];
    outputs.sort_by_key(|o| o.onetime_address);
    let mut t = Transaction {
        prefix: TxPrefix {
            version: VERSION,
            inputs: vec![Input {
                key_image: pt(b"key image", 0),
            }],
            outputs,
            ephemeral_pubkeys: vec![pt(b"eph", 0)],
            fee: 0,
            encrypted_payment_id: [0; 8],
        },
        prunable: Prunable {
            reference_height: next.height - 1,
            proof_data: vec![7; 200],
        },
    };
    let size = t.to_bytes().unwrap().len() as u64;
    t.prefix.fee = rules::min_fee(size, next.reward, next.median).unwrap();
    let id = tx_id(&t).unwrap();
    let before = |s: &Sim<'_>, n: usize| {
        s.engines[n]
            .stats
            .received
            .get("stem_tx")
            .copied()
            .unwrap_or(0)
    };
    let (b1, b2) = (before(&sim, 1), before(&sim, 2));
    sim.submit_tx(0, t);
    sim.run_for(300);
    assert_eq!(before(&sim, 2), b2 + 1, "the onion peer got it");
    assert_eq!(before(&sim, 1), b1, "the direct peer did not");
    assert!(sim.run_until(300_000, |s| (0..3)
        .all(|n| s.engines[n].node().pool().contains(&id))));
}

/// A node with an IP address and a `.onion` address never tells one to a peer that came the other way: that would link
/// the two. A peer on a direct connection hears only the IP address; one that came through the onion service only the
/// `.onion` one.
#[test]
fn a_node_never_links_its_ip_and_onion_addresses() {
    use tenero_net::message::PeerHost;
    use tenero_net::{Hello, Message, PROTOCOL_VERSION};
    let rigs = SimRig::rigs("tor-adv", 1);
    let cfg = EngineConfig {
        advertise: Some("20.1.1.1:38353".into()),
        advertise_onion: Some(ONION.into()),
        ..EngineConfig::default()
    };
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), cfg);
    let hello = Message::Hello(Hello {
        version: PROTOCOL_VERSION,
        chain_id: rigs[0].store.chain_id(),
        tip_height: 0,
        cumulative_work: [0; 32],
        tip_id: [9; 32],
        pruned_below: 0,
        nonce: 0,
    });
    let direct = sim.add_hostile(0, "99.1.0.1:5000");
    let tor = sim.add_hostile(0, "onion-in:7");
    for h in [direct, tor] {
        sim.hostile_send(h, hello.clone());
    }
    sim.run_for(2000);
    let told = |h: usize| -> Vec<PeerHost> {
        sim.hostiles[h]
            .inbox
            .iter()
            .filter_map(|m| match m {
                Message::Addrs { addrs } => Some(addrs.iter().map(|a| a.host).collect::<Vec<_>>()),
                _ => None,
            })
            .flatten()
            .collect()
    };
    let (d, t) = (told(direct), told(tor));
    assert!(
        !d.is_empty() && d.iter().all(|h| matches!(h, PeerHost::Ip(_))),
        "{d:?}"
    );
    assert!(
        !t.is_empty() && t.iter().all(|h| matches!(h, PeerHost::Onion(_))),
        "{t:?}"
    );
}

/// A transaction valid at `node`'s tip (its proof is junk: the simulator does not check proofs), spending key image `n`.
fn junk_tx(sim: &Sim<'_>, node: usize, n: u8) -> tenero_core::v3::Transaction {
    use tenero_core::hash::sha256;
    use tenero_core::v3::rules;
    use tenero_core::v3::*;
    use tenero_tree::hash_to_point;
    let next = sim.engines[node].node().next_block().unwrap();
    let pt = |tag: &[u8], k: u8| hash_to_point(sha256(&[tag, &[n, k]]));
    let out = |k: u8| Output {
        onetime_address: pt(b"addr", k),
        amount_commitment: pt(b"commit", k),
        amount_enc: [k; 8],
        view_tag: [k; 3],
        anchor_enc: [k; 16],
    };
    let mut outputs = vec![out(1), out(2)];
    outputs.sort_by_key(|o| o.onetime_address);
    let mut t = Transaction {
        prefix: TxPrefix {
            version: VERSION,
            inputs: vec![Input {
                key_image: pt(b"key image", 0),
            }],
            outputs,
            ephemeral_pubkeys: vec![pt(b"eph", 0)],
            fee: 0,
            encrypted_payment_id: [0; 8],
        },
        prunable: Prunable {
            reference_height: next.height - 1,
            proof_data: vec![7; 200],
        },
    };
    let size = t.to_bytes().unwrap().len() as u64;
    t.prefix.fee = rules::min_fee(size, next.reward, next.median).unwrap();
    t
}

fn hello_with(rig: &SimRig, nonce: u64) -> tenero_net::Message {
    tenero_net::Message::Hello(tenero_net::Hello {
        version: tenero_net::PROTOCOL_VERSION,
        chain_id: rig.store.chain_id(),
        tip_height: 0,
        cumulative_work: [0; 32],
        tip_id: [9; 32],
        pruned_below: 0,
        nonce,
    })
}

/// A peer that came in through the node's `.onion` service never gets the node's own transaction as a stem (relayed
/// stems go only to peers the node dialled, so one sent to it could only be the node's own, and it would know): the own
/// transaction goes on the ordinary stem instead. (Found by a tester in 0.4.0-gamma.1, where it went to that peer.)
#[test]
fn a_peer_that_came_in_through_the_onion_service_never_gets_the_nodes_own_transaction() {
    use tenero_core::v3::ids::tx_id;
    let rigs = SimRig::rigs("tor-own-in", 2);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), EngineConfig::default());
    sim.connect(0, 1);
    sim.run_for(3000);
    while sim.tip(0).0 < 61 {
        sim.mine(0, None);
        assert!(sim.run_until(60_000, |s| s.all_agree()));
        sim.run_for(61_000);
    }
    let spy = sim.add_hostile(0, "onion-in:7");
    sim.hostile_send(spy, hello_with(&rigs[0], 0));
    sim.run_for(500);
    let t = junk_tx(&sim, 0, 1);
    let id = tx_id(&t).unwrap();
    let stems = |s: &Sim<'_>| {
        s.engines[1]
            .stats
            .received
            .get("stem_tx")
            .copied()
            .unwrap_or(0)
    };
    let before = stems(&sim);
    sim.submit_tx(0, t);
    sim.run_for(300);
    assert!(
        !sim.hostiles[spy]
            .inbox
            .iter()
            .any(|m| matches!(m, tenero_net::Message::StemTx(_))),
        "the onion peer got a stem"
    );
    assert_eq!(stems(&sim), before + 1, "it went on the ordinary stem");
    assert!(sim.engines[0].holds_stem(&id));
}

/// The node shows one nonce on direct connections and another through Tor, and neither of its checks on nonces can be
/// used to tie the two together: our direct nonce sent over the onion service is not "ourselves", and the same nonce on
/// a direct link and an onion link is not a "duplicate" (either answer, a hang-up, would link them). Within one identity
/// both checks still work.
#[test]
fn a_node_shows_another_nonce_through_tor_and_never_matches_across_the_two() {
    use tenero_net::Message;
    let rigs = SimRig::rigs("tor-nonce", 1);
    let cfg = EngineConfig {
        nonce: 111,
        tor_nonce: 222,
        ..EngineConfig::default()
    };
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), cfg);
    let nonce_shown = |sim: &Sim<'_>, h: usize| {
        sim.hostiles[h].inbox.iter().find_map(|m| match m {
            Message::Hello(h) => Some(h.nonce),
            _ => None,
        })
    };
    // what each identity shows
    let direct = sim.add_hostile(0, "99.1.0.1:5000");
    let onion = sim.add_hostile(0, "onion-in:1");
    sim.hostile_send(direct, hello_with(&rigs[0], 5));
    sim.hostile_send(onion, hello_with(&rigs[0], 6));
    sim.run_for(500);
    assert_eq!(nonce_shown(&sim, direct), Some(111));
    assert_eq!(nonce_shown(&sim, onion), Some(222));
    // our DIRECT nonce over the onion service, our TOR nonce directly: neither is taken for ourselves
    let a = sim.add_hostile(0, "onion-in:2");
    let b = sim.add_hostile(0, "99.1.0.2:5000");
    sim.hostile_send(a, hello_with(&rigs[0], 111));
    sim.hostile_send(b, hello_with(&rigs[0], 222));
    // the same nonce on a direct link and an onion link: not a duplicate
    let c = sim.add_hostile(0, "99.1.0.3:5000");
    let d = sim.add_hostile(0, "onion-in:3");
    sim.hostile_send(c, hello_with(&rigs[0], 77));
    sim.hostile_send(d, hello_with(&rigs[0], 77));
    sim.run_for(500);
    for h in [direct, onion, a, b, c, d] {
        assert!(!sim.hostiles[h].disconnected, "peer {h} was dropped");
    }
    // within one identity the checks still work: our Tor nonce over the onion service is ourselves, and a second
    // onion link with nonce 77 is a duplicate
    let e = sim.add_hostile(0, "onion-in:4");
    let f = sim.add_hostile(0, "onion-in:5");
    sim.hostile_send(e, hello_with(&rigs[0], 222));
    sim.hostile_send(f, hello_with(&rigs[0], 6));
    sim.run_for(500);
    assert!(
        sim.hostiles[e].disconnected,
        "our own Tor nonce is ourselves"
    );
    assert!(
        sim.hostiles[f].disconnected || sim.hostiles[onion].disconnected,
        "two onion links with one nonce: one of them goes"
    );
}

/// The transport shows a different Noise key through Tor than directly, and a new one at each start.
#[test]
fn the_tor_key_is_not_the_direct_key_and_is_new_at_each_start() {
    let key = NodeKey::generate();
    let (a, b) = (
        NetConfig::new(key.clone(), [1; 32]),
        NetConfig::new(key, [1; 32]),
    );
    assert_ne!(a.tor_key.public(), a.key.public());
    assert_ne!(a.tor_key.public(), b.tor_key.public());
    assert_eq!(a.key.public(), b.key.public());
}
