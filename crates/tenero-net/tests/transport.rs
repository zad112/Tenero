//! The engine over real sockets: TCP on loopback, the Noise channel, real threads and the real clock. Nodes run the
//! SHA-256 test chain; hostile clients are real sockets speaking (or abusing) the real protocol.

use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tenero_chain::{ProofsNotChecked, Sha256Pow};
use tenero_core::u256::U256;
use tenero_net::addrbook::AddrBookConfig;
use tenero_net::noise::{handshake_initiator, prologue, NodeKey};
use tenero_net::sim::{Sim, SimConfig, SimRig};
use tenero_net::transport::{Hooks, Logger, Net, NetConfig, NoHooks};
use tenero_net::{
    encode, Engine, EngineConfig, Event, FrameDecoder, Hello, Message, PROTOCOL_VERSION,
};
use tenero_node::{Node, NodeConfig};

const T0: u64 = 1_700_000_000;

/// Mines `n` blocks on node 0's rig with the simulator (the chain stays in its store).
fn mined(rigs: &[SimRig], n: usize) {
    let mut sim = Sim::new(rigs, T0, SimConfig::default(), EngineConfig::default());
    sim.mine_chain(0, n);
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

/// Engine settings for nodes on loopback: private addresses are accepted, nothing is dialled at random.
fn local_cfg(seeds: &[SocketAddr]) -> EngineConfig {
    EngineConfig {
        addrbook: AddrBookConfig {
            accept_private: true,
            ..AddrBookConfig::default()
        },
        seeds: seeds.iter().map(|s| s.to_string()).collect(),
        peer_target: 8,
        outbound_target: 4,
        ..EngineConfig::default()
    }
}

struct Log(Arc<Mutex<Vec<String>>>);

impl Log {
    fn new() -> Log {
        Log(Arc::new(Mutex::new(Vec::new())))
    }
    fn logger(&self) -> Logger {
        let lines = Arc::clone(&self.0);
        Arc::new(move |l| lines.lock().unwrap().push(l.to_string()))
    }
    fn contains(&self, needle: &str) -> bool {
        self.0.lock().unwrap().iter().any(|l| l.contains(needle))
    }
    fn dump(&self) -> String {
        self.0.lock().unwrap().join("\n")
    }
}

fn net_cfg(listen: Option<&str>, log: &Log) -> NetConfig {
    let mut c = NetConfig::new(NodeKey::generate(), chain_id());
    c.listen = listen.map(|a| a.parse().unwrap());
    c.log = log.logger();
    c.tick = Duration::from_millis(100);
    c
}

/// The test chain's id, worked out once (opening a store for it each time would collide between parallel tests).
fn chain_id() -> [u8; 32] {
    static ID: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    *ID.get_or_init(|| SimRig::rigs("chainid", 1)[0].store.chain_id())
}

/// Stops the node when `done` says so, or after `limit` (so a failing test ends instead of hanging).
struct StopWhen<F: FnMut(&Engine<'_>) -> bool> {
    done: F,
    stop: Vec<Arc<AtomicBool>>,
    deadline: Instant,
}

impl<F: FnMut(&Engine<'_>) -> bool> Hooks for StopWhen<F> {
    fn poll(&mut self, engine: &mut Engine<'_>, _now_ms: u64) -> Vec<Event> {
        if (self.done)(engine) || Instant::now() > self.deadline {
            for s in &self.stop {
                s.store(true, Ordering::SeqCst);
            }
        }
        Vec::new()
    }
}

fn tip_height(e: &Engine<'_>) -> u64 {
    e.node().tip().unwrap().0
}

// ---- two real nodes --------------------------------------------------------------------------------------

#[test]
fn a_fresh_node_syncs_a_chain_from_another_over_real_sockets_and_noise() {
    let rigs = SimRig::rigs("tr-sync", 2);
    mined(&rigs, 60);
    let (log_a, log_b) = (Log::new(), Log::new());
    let net_a = Net::bind(net_cfg(Some("127.0.0.1:0"), &log_a)).unwrap();
    let addr_a = net_a.local_addr().unwrap();
    let net_b = Net::bind(net_cfg(None, &log_b)).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (tip_a, tip_b) = thread::scope(|s| {
        let ha = s.spawn(|| {
            let mut e = engine_on(&rigs[0], local_cfg(&[]));
            net_a.run(&mut e, Arc::clone(&stop), &mut NoHooks).unwrap();
            tip_height(&e)
        });
        let hb = s.spawn(|| {
            let mut e = engine_on(&rigs[1], local_cfg(&[addr_a]));
            let mut hooks = StopWhen {
                done: |e: &Engine<'_>| tip_height(e) == 60,
                stop: vec![Arc::clone(&stop)],
                deadline: Instant::now() + Duration::from_secs(60),
            };
            net_b.run(&mut e, Arc::clone(&stop), &mut hooks).unwrap();
            tip_height(&e)
        });
        (ha.join().unwrap(), hb.join().unwrap())
    });
    assert_eq!(tip_a, 60);
    assert_eq!(
        tip_b,
        60,
        "the new node did not sync\nA:\n{}\nB:\n{}",
        log_a.dump(),
        log_b.dump()
    );
    assert_eq!(
        rigs[0].store.tip().unwrap().1.block_id,
        rigs[1].store.tip().unwrap().1.block_id
    );
    assert!(log_b.contains("connected to"), "{}", log_b.dump());
    assert!(log_a.contains("connected from"), "{}", log_a.dump());
}

// ---- a node under test, and hostile clients --------------------------------------------------------------

#[derive(Clone, Default)]
struct Snap {
    peers: usize,
    inbound: usize,
    tip: u64,
    banned_loopback: bool,
    book: usize,
    outbound: usize,
    connecting: usize,
}

struct Publish {
    snap: Arc<Mutex<Snap>>,
}

impl Hooks for Publish {
    fn poll(&mut self, engine: &mut Engine<'_>, now_ms: u64) -> Vec<Event> {
        *self.snap.lock().unwrap() = Snap {
            peers: engine.peer_count(),
            inbound: engine.inbound_count(),
            tip: tip_height(engine),
            banned_loopback: engine.is_banned("127.0.0.1:1", now_ms),
            book: engine.addr_book().len(),
            outbound: engine.outbound_count(),
            connecting: engine.connecting_count(),
        };
        Vec::new()
    }
}

struct Handle {
    addr: SocketAddr,
    log: Log,
    counters: Arc<tenero_net::transport::Counters>,
    snap: Arc<Mutex<Snap>>,
}

impl Handle {
    fn snap(&self) -> Snap {
        self.snap.lock().unwrap().clone()
    }

    /// Waits (up to 10 s) until `cond` holds of the published state.
    fn wait(&self, what: &str, cond: impl Fn(&Snap) -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end {
            if cond(&self.snap()) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "waited for {what}; peers {}\nlog:\n{}",
            self.snap().peers,
            self.log.dump()
        );
    }

    fn wait_log(&self, needle: &str) {
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end {
            if self.log.contains(needle) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("no log line with {needle:?}; log:\n{}", self.log.dump());
    }
}

/// Runs a listening node on `rig` while `body` does things to it from outside, then shuts it down.
fn with_node<R>(
    rig: &SimRig,
    engine_cfg: EngineConfig,
    tweak: impl FnOnce(&mut NetConfig),
    body: impl FnOnce(&Handle) -> R,
) -> R {
    let log = Log::new();
    let mut cfg = net_cfg(Some("127.0.0.1:0"), &log);
    tweak(&mut cfg);
    let net = Net::bind(cfg).unwrap();
    let handle = Handle {
        addr: net.local_addr().unwrap(),
        log,
        counters: net.counters(),
        snap: Arc::new(Mutex::new(Snap::default())),
    };
    let stop = Arc::new(AtomicBool::new(false));
    thread::scope(|s| {
        let snap = Arc::clone(&handle.snap);
        let stop2 = Arc::clone(&stop);
        let node = s.spawn(move || {
            let mut e = engine_on(rig, engine_cfg);
            net.run(&mut e, stop2, &mut Publish { snap }).unwrap();
        });
        // the node is stopped however the body ends, a panic included (or the scope would wait for it forever)
        struct StopOnDrop(Arc<AtomicBool>);
        impl Drop for StopOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let r = {
            let _stop = StopOnDrop(Arc::clone(&stop));
            body(&handle)
        };
        node.join().unwrap();
        r
    })
}

/// A raw client speaking the real protocol: Noise handshake, then frames.
struct Client {
    stream: TcpStream,
    r: tenero_net::noise::SecureReader,
    w: tenero_net::noise::SecureWriter,
    dec: FrameDecoder,
}

impl Client {
    fn connect_with(addr: SocketAddr, chain: [u8; 32]) -> Result<Client, String> {
        let mut stream =
            TcpStream::connect_timeout(&addr, Duration::from_secs(5)).map_err(|e| e.to_string())?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.set_nodelay(true).unwrap();
        let s = handshake_initiator(
            &mut stream,
            &NodeKey::generate(),
            &prologue(PROTOCOL_VERSION, &chain),
        )
        .map_err(|e| e.to_string())?;
        Ok(Client {
            stream,
            r: s.reader,
            w: s.writer,
            dec: FrameDecoder::new(),
        })
    }

    fn connect(addr: SocketAddr) -> Result<Client, String> {
        Client::connect_with(addr, chain_id())
    }

    fn hello(&self) -> Message {
        Message::Hello(Hello {
            version: PROTOCOL_VERSION,
            chain_id: chain_id(),
            tip_height: 0,
            cumulative_work: U256::from_be_bytes(&[0; 32]).to_be_bytes(),
            tip_id: [9; 32],
            pruned_below: 0,
            nonce: 0,
        })
    }

    fn send(&mut self, m: &Message) {
        self.send_raw(&encode(m).unwrap());
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        use std::io::Write;
        let sealed = self.w.seal(bytes).unwrap();
        let _ = self.stream.write_all(&sealed);
    }

    /// The next message, or `None` on a closed connection or after `wait`.
    fn recv(&mut self, wait: Duration) -> Option<Message> {
        self.stream.set_read_timeout(Some(wait)).unwrap();
        loop {
            if let Ok(Some(m)) = self.dec.next_message() {
                return Some(m);
            }
            match self.r.read_chunk(&mut self.stream) {
                Ok(chunk) => self.dec.push(&chunk),
                Err(_) => return None,
            }
        }
    }

    /// True if the node has closed the connection: reading gives an error or the end of the stream. A read that
    /// merely finds nothing to read (a timeout) is NOT closed.
    fn is_closed(&mut self) -> bool {
        self.stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        loop {
            match self.r.read_chunk(&mut self.stream) {
                Ok(chunk) => self.dec.push(&chunk), // whatever was still on its way
                Err(tenero_net::noise::NoiseError::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    return false
                }
                Err(_) => return true,
            }
        }
    }
}

fn quick() -> impl FnOnce(&mut NetConfig) {
    |c| c.handshake_timeout = Duration::from_millis(600)
}

#[test]
fn a_good_client_gets_a_handshake_and_a_hello_back() {
    let rigs = SimRig::rigs("tr-good", 1);
    with_node(
        &rigs[0],
        local_cfg(&[]),
        |_| {},
        |h| {
            let mut c = Client::connect(h.addr).unwrap();
            c.send(&c.hello());
            assert!(matches!(
                c.recv(Duration::from_secs(3)),
                Some(Message::Hello(_))
            ));
            h.wait("a peer", |s| s.peers == 1 && s.inbound == 1);
        },
    );
}

#[test]
fn garbage_where_a_handshake_belongs_is_refused_and_logged_and_the_node_carries_on() {
    use std::io::Write;
    let rigs = SimRig::rigs("tr-garbage", 1);
    with_node(&rigs[0], local_cfg(&[]), quick(), |h| {
        for junk in [
            vec![0xffu8; 300],
            vec![0, 0],
            vec![0, 50, 1, 2, 3],
            b"GET / HTTP/1.1\r\n\r\n".to_vec(),
        ] {
            let mut s = TcpStream::connect(h.addr).unwrap();
            s.write_all(&junk).unwrap();
            drop(s);
        }
        h.wait_log("failed");
        assert!(h.counters.handshake_failures.load(Ordering::Relaxed) >= 1);
        // and a good client still gets through
        let mut c = Client::connect(h.addr).unwrap();
        c.send(&c.hello());
        assert!(matches!(
            c.recv(Duration::from_secs(3)),
            Some(Message::Hello(_))
        ));
        assert_eq!(h.counters.bad_bytes.load(Ordering::Relaxed), 0);
    });
}

#[test]
fn a_silent_connection_and_a_dribbling_one_are_cut_off_at_the_handshake_deadline() {
    use std::io::{Read, Write};
    let rigs = SimRig::rigs("tr-slow", 1);
    with_node(&rigs[0], local_cfg(&[]), quick(), |h| {
        // says nothing at all
        let mut silent = TcpStream::connect(h.addr).unwrap();
        silent
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let started = Instant::now();
        let mut b = [0u8; 1];
        assert!(
            matches!(silent.read(&mut b), Ok(0) | Err(_)),
            "the node should close it"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        // dribbles a byte every 150 ms: each read is quick, the total is not
        let mut slow = TcpStream::connect(h.addr).unwrap();
        slow.set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let started = Instant::now();
        let mut closed = false;
        for i in 0u8..40 {
            // a length of 32 first, then the 32 bytes that never finish arriving in time
            if slow
                .write_all(&[if i == 0 {
                    0
                } else if i == 1 {
                    32
                } else {
                    1
                }])
                .is_err()
            {
                closed = true;
                break;
            }
            thread::sleep(Duration::from_millis(150));
            if matches!(slow.read(&mut b), Ok(0)) {
                closed = true;
                break;
            }
        }
        assert!(closed, "the dribbling connection was never cut off");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "{:?}",
            started.elapsed()
        );
        h.wait_log("took too long");
    });
}

#[test]
fn a_handshake_for_another_chain_is_refused() {
    let rigs = SimRig::rigs("tr-chain", 1);
    with_node(&rigs[0], local_cfg(&[]), quick(), |h| {
        assert!(Client::connect_with(h.addr, [9; 32]).is_err());
        h.wait_log("handshake with");
        assert_eq!(h.snap().peers, 0);
    });
}

#[test]
fn bytes_that_decrypt_but_are_not_a_message_get_the_peer_banned_before_its_next_handshake() {
    let rigs = SimRig::rigs("tr-badbytes", 1);
    for (name, bytes) in [
        ("an unknown kind", vec![1u8, 0, 0, 0, 0xff]),
        (
            "a length over the cap of its kind",
            vec![0xff, 0xff, 0xff, 0xff, 2],
        ),
        ("a zero length", vec![0, 0, 0, 0, 2]),
        ("a body that is too short", vec![2, 0, 0, 0, 2, 1]),
    ] {
        with_node(
            &rigs[0],
            local_cfg(&[]),
            |_| {},
            |h| {
                let mut c = Client::connect(h.addr).unwrap();
                c.send(&c.hello());
                assert!(
                    matches!(c.recv(Duration::from_secs(3)), Some(Message::Hello(_))),
                    "{name}"
                );
                c.send_raw(&bytes);
                h.wait_log("sent bytes that are not a message");
                assert!(c.is_closed(), "{name}: still connected");
                h.wait("the ban", |s| s.banned_loopback && s.peers == 0);
                assert_eq!(h.counters.bad_bytes.load(Ordering::Relaxed), 1, "{name}");
                // the next connection from that host is turned away before any cryptography
                assert!(Client::connect(h.addr).is_err(), "{name}");
                assert!(
                    h.counters.refused_banned.load(Ordering::Relaxed) >= 1,
                    "{name}"
                );
                assert!(h.log.contains("banned"));
            },
        );
    }
}

#[test]
fn bytes_that_do_not_decrypt_close_the_connection_but_do_not_ban_the_host() {
    let rigs = SimRig::rigs("tr-tamper", 1);
    with_node(
        &rigs[0],
        local_cfg(&[]),
        |_| {},
        |h| {
            let mut c = Client::connect(h.addr).unwrap();
            c.send(&c.hello());
            assert!(matches!(
                c.recv(Duration::from_secs(3)),
                Some(Message::Hello(_))
            ));
            // a chunk of the right shape whose contents are not authenticated
            use std::io::Write;
            let mut forged = vec![0u8, 40];
            forged.extend_from_slice(&[0x5a; 40]);
            h.wait("the peer to be known", |s| s.peers == 1);
            c.stream.write_all(&forged).unwrap();
            h.wait_log("does not authenticate");
            assert!(c.is_closed());
            h.wait("the connection gone", |s| s.peers == 0);
            assert_eq!(h.counters.bad_bytes.load(Ordering::Relaxed), 0);
            assert!(
                !h.snap().banned_loopback,
                "someone on the wire could have caused this: no ban"
            );
            assert!(Client::connect(h.addr).is_ok(), "the host is still welcome");
        },
    );
}

#[test]
fn one_host_cannot_fill_the_handshake_slots() {
    use std::io::Read;
    let rigs = SimRig::rigs("tr-pending", 1);
    with_node(
        &rigs[0],
        local_cfg(&[]),
        |c| {
            c.handshake_timeout = Duration::from_secs(4);
            c.max_pending_per_host = 2;
        },
        |h| {
            let mut held: Vec<TcpStream> = (0..5)
                .map(|_| TcpStream::connect(h.addr).unwrap())
                .collect();
            let end = Instant::now() + Duration::from_secs(3);
            while h.counters.refused_busy.load(Ordering::Relaxed) < 3 && Instant::now() < end {
                thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(
                h.counters.refused_busy.load(Ordering::Relaxed),
                3,
                "{}",
                h.log.dump()
            );
            assert_eq!(h.counters.accepted.load(Ordering::Relaxed), 2);
            // the refused ones are closed at once
            let mut closed = 0;
            for s in held.iter_mut() {
                s.set_read_timeout(Some(Duration::from_millis(300)))
                    .unwrap();
                let mut b = [0u8; 1];
                if matches!(s.read(&mut b), Ok(0)) {
                    closed += 1;
                }
            }
            assert_eq!(closed, 3);
        },
    );
}

// ---- a peer that does not read ---------------------------------------------------------------------------

#[test]
fn a_peer_that_asks_for_a_lot_and_reads_none_of_it_is_disconnected_and_the_node_carries_on() {
    let rigs = SimRig::rigs("tr-slowreader", 1);
    mined(&rigs, 300);
    let ids: Vec<[u8; 32]> = (1..=32u64)
        .map(|h| rigs[0].store.block_index(h).unwrap().unwrap().block_id)
        .collect();
    let cfg = EngineConfig {
        // a peer asking for blocks is not punished for asking a great deal
        msgs_per_sec: 1_000_000,
        burst: 1_000_000_000,
        ..local_cfg(&[])
    };
    with_node(
        &rigs[0],
        cfg,
        |c| c.max_queued_bytes = 256 * 1024,
        |h| {
            let mut c = Client::connect(h.addr).unwrap();
            c.send(&c.hello());
            assert!(matches!(
                c.recv(Duration::from_secs(3)),
                Some(Message::Hello(_))
            ));
            h.wait("the peer to be known", |s| s.peers == 1);
            // ask again and again, and never read an answer
            let request = Message::GetBlocks { ids: ids.clone() };
            let end = Instant::now() + Duration::from_secs(30);
            while h.counters.slow_peer_drops.load(Ordering::Relaxed) == 0 && Instant::now() < end {
                for _ in 0..200 {
                    c.send(&request);
                }
                thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(
                h.counters.slow_peer_drops.load(Ordering::Relaxed),
                1,
                "{}",
                h.log.dump()
            );
            assert!(h.log.contains("is not reading"));
            // the engine has been told it is gone (its count of peers is back to none), and the socket is closed
            // (draining what was already on its way first)
            h.wait("the slow peer gone", |s| s.peers == 0);
            assert!(c.is_closed(), "the slow peer's socket was left open");
            // the node is unharmed, and the drop is not a ban (slow is not hostile)
            assert!(!h.snap().banned_loopback);
            let mut good = Client::connect(h.addr).unwrap();
            good.send(&good.hello());
            assert!(matches!(
                good.recv(Duration::from_secs(3)),
                Some(Message::Hello(_))
            ));
        },
    );
}

// ---- state that survives a restart -----------------------------------------------------------------------

/// Runs an outbound-only node on `rig` (with `state`, if given) until `until` holds of its published state or
/// 15 s have passed; returns what it last published and its log.
fn run_dialler(
    rig: &SimRig,
    seeds: &[SocketAddr],
    state: Option<&std::path::Path>,
    until: impl Fn(&Snap) -> bool,
) -> (Snap, Log) {
    let log = Log::new();
    let mut cfg = net_cfg(None, &log);
    cfg.state_path = state.map(|p| p.to_path_buf());
    let net = Net::bind(cfg).unwrap();
    let snap = Arc::new(Mutex::new(Snap::default()));
    let stop = Arc::new(AtomicBool::new(false));
    thread::scope(|s| {
        let (snap2, stop2) = (Arc::clone(&snap), Arc::clone(&stop));
        let node = s.spawn(move || {
            let mut e = engine_on(rig, local_cfg(seeds));
            net.run(&mut e, stop2, &mut Publish { snap: snap2 })
                .unwrap();
        });
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end && !until(&snap.lock().unwrap().clone()) {
            thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Ordering::SeqCst);
        node.join().unwrap();
    });
    let last = snap.lock().unwrap().clone();
    (last, log)
}

#[test]
fn the_address_book_is_saved_on_shutdown_and_a_restarted_node_finds_its_peer_again_without_seeds() {
    let rigs = SimRig::rigs("tr-state", 2);
    let state = std::env::temp_dir().join(format!("tenero-net-state-{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&state);
    with_node(
        &rigs[0],
        local_cfg(&[]),
        |_| {},
        |h| {
            // first run: a seed, a connection, a clean shutdown
            let (first, log1) = run_dialler(&rigs[1], &[h.addr], Some(&state), |s| s.peers == 1);
            assert_eq!(first.peers, 1, "{}", log1.dump());
            assert!(state.exists(), "no state file was written at shutdown");
            assert!(std::fs::metadata(&state).unwrap().len() > 8);
            // second run: no seeds at all, only the file
            let (second, log2) = run_dialler(&rigs[1], &[], Some(&state), |s| s.peers == 1);
            assert!(log2.contains("loaded the address book"), "{}", log2.dump());
            assert!(second.book >= 1);
            assert_eq!(
                second.peers,
                1,
                "it did not find its way back\n{}",
                log2.dump()
            );
            // third run: a damaged file is ignored, not trusted and not fatal (and the seed still works)
            std::fs::write(&state, b"this is not an address book").unwrap();
            let (third, log3) = run_dialler(&rigs[1], &[h.addr], Some(&state), |s| s.peers == 1);
            assert!(
                log3.contains("ignored a damaged state file"),
                "{}",
                log3.dump()
            );
            assert_eq!(third.peers, 1);
        },
    );
    let _ = std::fs::remove_file(&state);
}

// ---- three real nodes ------------------------------------------------------------------------------------

/// Mines one block on the test chain every so often, until it has mined `left`.
struct Miner {
    left: usize,
    next: Instant,
    every: Duration,
}

impl Hooks for Miner {
    fn poll(&mut self, engine: &mut Engine<'_>, _now_ms: u64) -> Vec<Event> {
        if self.left == 0 || Instant::now() < self.next {
            return Vec::new();
        }
        self.left -= 1;
        self.next = Instant::now() + self.every;
        let tip = engine.node().store().tip().unwrap().1;
        let ts = (tip.header.timestamp + 60).max(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        );
        let payout = tenero_node::Payout {
            onetime_address: [self.left as u8; 32],
            view_tag: [1; 3],
            ephemeral_pubkey: [2; 32],
            anchor_enc: [3; 16],
        };
        vec![Event::LocalBlock(tenero_net::sim::mine_test_block(
            engine.node(),
            ts,
            payout,
        ))]
    }
}

struct Both<A: Hooks, B: Hooks>(A, B);

impl<A: Hooks, B: Hooks> Hooks for Both<A, B> {
    fn poll(&mut self, engine: &mut Engine<'_>, now_ms: u64) -> Vec<Event> {
        let mut v = self.0.poll(engine, now_ms);
        v.extend(self.1.poll(engine, now_ms));
        v
    }
}

#[test]
fn a_block_mined_on_one_node_reaches_the_others_over_real_sockets() {
    let rigs = SimRig::rigs("tr-relay", 3);
    let logs = [Log::new(), Log::new(), Log::new()];
    let nets: Vec<Net> = (0..3)
        .map(|i| {
            Net::bind(net_cfg(
                if i == 0 { Some("127.0.0.1:0") } else { None },
                &logs[i],
            ))
            .unwrap()
        })
        .collect();
    let hub: SocketAddr = nets[0].local_addr().unwrap();
    let snaps: Vec<Arc<Mutex<Snap>>> = (0..3)
        .map(|_| Arc::new(Mutex::new(Snap::default())))
        .collect();
    let stop = Arc::new(AtomicBool::new(false));
    thread::scope(|s| {
        for (i, net) in nets.into_iter().enumerate() {
            let (stop, snap, rig) = (Arc::clone(&stop), Arc::clone(&snaps[i]), &rigs[i]);
            s.spawn(move || {
                let seeds: Vec<SocketAddr> = if i == 0 { vec![] } else { vec![hub] };
                let mut e = engine_on(rig, local_cfg(&seeds));
                // node 1 is the miner: three blocks, half a second apart
                let miner = Miner {
                    left: if i == 1 { 3 } else { 0 },
                    next: Instant::now() + Duration::from_secs(2),
                    every: Duration::from_millis(500),
                };
                net.run(&mut e, stop, &mut Both(Publish { snap }, miner))
                    .unwrap();
            });
        }
        let end = Instant::now() + Duration::from_secs(30);
        while Instant::now() < end && !snaps.iter().all(|s| s.lock().unwrap().tip == 3) {
            thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::SeqCst);
    });
    for (i, s) in snaps.iter().enumerate() {
        assert_eq!(s.lock().unwrap().tip, 3, "node {i}\n{}", logs[i].dump());
    }
    let tips: Vec<[u8; 32]> = rigs
        .iter()
        .map(|r| r.store.tip().unwrap().1.block_id)
        .collect();
    assert!(
        tips.iter().all(|t| *t == tips[0]),
        "the three nodes ended on different tips"
    );
}

// ---- many connections at once ----------------------------------------------------------------------------

/// `n` raw clients (real Noise, real Hello) connect to one node while a real peer syncs from it.
/// `TENERO_STRESS_CLIENTS` changes `n`; `TENERO_STRESS_HOLD_SECS` holds the connections open that long, so a person
/// can look at the process (its memory and threads) with them all up.
#[test]
fn sixty_clients_stay_connected_to_one_node_while_a_real_peer_syncs_from_it() {
    let n: usize = std::env::var("TENERO_STRESS_CLIENTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let hold: u64 = std::env::var("TENERO_STRESS_HOLD_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let rigs = SimRig::rigs("tr-stress", 2);
    mined(&rigs, 200);
    let cfg = EngineConfig {
        max_inbound: n + 50,
        max_peers: n + 100,
        ..local_cfg(&[])
    };
    with_node(
        &rigs[0],
        cfg,
        |c| c.max_pending_per_host = n + 10,
        |h| {
            let stop_clients = Arc::new(AtomicBool::new(false));
            let started = Instant::now();
            let clients: Vec<_> = (0..n)
                .map(|i| {
                    let (addr, stop) = (h.addr, Arc::clone(&stop_clients));
                    thread::spawn(move || {
                        let mut c =
                            Client::connect(addr).unwrap_or_else(|e| panic!("client {i}: {e}"));
                        c.send(&c.hello());
                        // read whatever the node sends until told to stop, so its queue never backs up
                        while !stop.load(Ordering::SeqCst) {
                            let _ = c.recv(Duration::from_millis(200));
                        }
                    })
                })
                .collect();
            h.wait("every client connected", |s| s.inbound >= n);
            let connected_in = started.elapsed();
            // a real peer syncs the whole chain while they are all there
            let (peer, log_peer) = run_dialler(&rigs[1], &[h.addr], None, |s| s.tip == 200);
            assert_eq!(peer.tip, 200, "{}", log_peer.dump());
            let snap = h.snap();
            assert!(
                snap.peers >= n,
                "only {} peers of {n}\n{}",
                snap.peers,
                h.log.dump()
            );
            eprintln!(
                "  {n} clients up in {connected_in:?}; node has {} peers; threads {}; bytes in {}, out {}; handshake failures {}; bans {}",
                snap.peers,
                h.counters.threads.load(Ordering::Relaxed),
                h.counters.bytes_in.load(Ordering::Relaxed),
                h.counters.bytes_out.load(Ordering::Relaxed),
                h.counters.handshake_failures.load(Ordering::Relaxed),
                h.counters.bad_bytes.load(Ordering::Relaxed),
            );
            if hold > 0 {
                eprintln!(
                    "  holding {n} connections for {hold} s (process {})",
                    std::process::id()
                );
                thread::sleep(Duration::from_secs(hold));
            }
            assert_eq!(h.counters.handshake_failures.load(Ordering::Relaxed), 0);
            assert!(!h.snap().banned_loopback);
            stop_clients.store(true, Ordering::SeqCst);
            for c in clients {
                c.join().unwrap();
            }
        },
    );
}

// ---- more rules, each with its own test -------------------------------------------------------------------

/// Runs an outbound-only node until `until` holds of its published state and log, or 15 s.
fn run_node_until(
    rig: &SimRig,
    engine_cfg: EngineConfig,
    tweak: impl FnOnce(&mut NetConfig),
    until: impl Fn(&Snap, &Log) -> bool,
) -> (Snap, Log) {
    let log = Log::new();
    let mut cfg = net_cfg(None, &log);
    tweak(&mut cfg);
    let net = Net::bind(cfg).unwrap();
    let snap = Arc::new(Mutex::new(Snap::default()));
    let stop = Arc::new(AtomicBool::new(false));
    thread::scope(|s| {
        let (snap2, stop2) = (Arc::clone(&snap), Arc::clone(&stop));
        let node = s.spawn(move || {
            let mut e = engine_on(rig, engine_cfg);
            net.run(&mut e, stop2, &mut Publish { snap: snap2 })
                .unwrap();
        });
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end && !until(&snap.lock().unwrap().clone(), &log) {
            thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Ordering::SeqCst);
        node.join().unwrap();
    });
    let last = snap.lock().unwrap().clone();
    (last, log)
}

#[test]
fn a_peer_that_reads_what_it_is_sent_is_never_dropped_however_much_it_is_sent() {
    let rigs = SimRig::rigs("tr-reader", 1);
    mined(&rigs, 300);
    let ids: Vec<[u8; 32]> = (1..=32u64)
        .map(|h| rigs[0].store.block_index(h).unwrap().unwrap().block_id)
        .collect();
    let cfg = EngineConfig {
        msgs_per_sec: 1_000_000,
        burst: 1_000_000_000,
        ..local_cfg(&[])
    };
    with_node(
        &rigs[0],
        cfg,
        // room for a few replies at a time: several megabytes go through, so the count of queued bytes must go
        // back down as each is written
        |c| c.max_queued_bytes = 256 * 1024,
        |h| {
            let mut c = Client::connect(h.addr).unwrap();
            c.send(&c.hello());
            assert!(matches!(
                c.recv(Duration::from_secs(3)),
                Some(Message::Hello(_))
            ));
            let request = Message::GetBlocks { ids: ids.clone() };
            let mut answers = 0;
            for _ in 0..400 {
                c.send(&request);
                while let Some(m) = c.recv(Duration::from_millis(2)) {
                    if matches!(m, Message::Blocks { .. }) {
                        answers += 1;
                    }
                }
            }
            let end = Instant::now() + Duration::from_secs(10);
            while answers < 400 && Instant::now() < end {
                if let Some(Message::Blocks { .. }) = c.recv(Duration::from_millis(200)) {
                    answers += 1;
                }
            }
            assert_eq!(answers, 400, "{}", h.log.dump());
            assert_eq!(h.counters.slow_peer_drops.load(Ordering::Relaxed), 0);
            assert!(h.counters.bytes_out.load(Ordering::Relaxed) > 1_000_000);
        },
    );
}

#[test]
fn a_seed_that_is_not_listening_is_logged_and_counted_and_the_node_carries_on() {
    let rigs = SimRig::rigs("tr-deadseed", 1);
    let dead: SocketAddr = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let (snap, log) = run_node_until(
        &rigs[0],
        local_cfg(&[dead]),
        |_| {},
        |_, log| log.contains("could not connect to"),
    );
    assert!(log.contains("could not connect to"), "{}", log.dump());
    assert_eq!(snap.peers, 0);
}

#[test]
fn the_state_file_is_written_while_the_node_runs_and_not_only_at_shutdown() {
    let rigs = SimRig::rigs("tr-periodic", 2);
    let state =
        std::env::temp_dir().join(format!("tenero-net-periodic-{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&state);
    with_node(
        &rigs[0],
        local_cfg(&[]),
        |_| {},
        |h| {
            let state2 = state.clone();
            let seen_while_running = Arc::new(AtomicBool::new(false));
            let seen = Arc::clone(&seen_while_running);
            let state_for_check = state.clone();
            let (_snap, log) = run_node_until(
                &rigs[1],
                local_cfg(&[h.addr]),
                |c| {
                    c.state_path = Some(state2.clone());
                    c.save_every = Duration::from_millis(300);
                },
                // the node is still running when this looks: the file must already be there (a file written only
                // at shutdown would not be)
                move |s, _| {
                    let there = s.peers == 1 && state_for_check.exists();
                    if there {
                        seen.store(true, Ordering::SeqCst);
                    }
                    there
                },
            );
            assert!(
                seen_while_running.load(Ordering::SeqCst),
                "no state file while the node was running\n{}",
                log.dump()
            );
        },
    );
    let _ = std::fs::remove_file(&state);
}

#[test]
fn every_connection_is_closed_when_the_node_shuts_down() {
    let rigs = SimRig::rigs("tr-shutdown", 1);
    let mut c = with_node(
        &rigs[0],
        local_cfg(&[]),
        |_| {},
        |h| {
            let mut c = Client::connect(h.addr).unwrap();
            c.send(&c.hello());
            assert!(matches!(
                c.recv(Duration::from_secs(3)),
                Some(Message::Hello(_))
            ));
            h.wait("the peer", |s| s.peers == 1);
            c
        },
    );
    assert!(c.is_closed(), "the node stopped but left a connection open");
}

#[test]
fn a_handshake_slot_is_given_back_when_the_handshake_ends() {
    let rigs = SimRig::rigs("tr-slotback", 1);
    with_node(
        &rigs[0],
        local_cfg(&[]),
        |c| {
            c.handshake_timeout = Duration::from_millis(800);
            c.max_pending_per_host = 2;
            c.max_pending_handshakes = 2;
        },
        |h| {
            // two silent connections take both slots (from one host, and in all)
            let _a = TcpStream::connect(h.addr).unwrap();
            let _b = TcpStream::connect(h.addr).unwrap();
            let end = Instant::now() + Duration::from_secs(3);
            while h.counters.accepted.load(Ordering::Relaxed) < 2 && Instant::now() < end {
                thread::sleep(Duration::from_millis(10));
            }
            // a third is turned away while they wait...
            let mut third = TcpStream::connect(h.addr).unwrap();
            third
                .set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
            let mut b = [0u8; 1];
            use std::io::Read;
            assert!(matches!(third.read(&mut b), Ok(0) | Err(_)));
            assert_eq!(h.counters.refused_busy.load(Ordering::Relaxed), 1);
            // ...and once they have timed out, the slots are free again
            h.wait_log("took too long");
            thread::sleep(Duration::from_millis(1200));
            assert!(
                Client::connect(h.addr).is_ok(),
                "the slots were never given back\n{}",
                h.log.dump()
            );
        },
    );
}

#[test]
fn the_ban_is_by_host_so_another_port_of_the_same_address_is_refused_too() {
    let rigs = SimRig::rigs("tr-banhost", 1);
    with_node(
        &rigs[0],
        local_cfg(&[]),
        |_| {},
        |h| {
            let mut c = Client::connect(h.addr).unwrap();
            c.send(&c.hello());
            assert!(matches!(
                c.recv(Duration::from_secs(3)),
                Some(Message::Hello(_))
            ));
            c.send_raw(&[1, 0, 0, 0, 0xee]);
            h.wait("the ban", |s| s.banned_loopback);
            // each new connection comes from a new source port, and each is refused
            for _ in 0..3 {
                assert!(Client::connect(h.addr).is_err());
            }
            assert_eq!(h.counters.refused_banned.load(Ordering::Relaxed), 3);
        },
    );
}

// ---- what the engine is told, and what the node closes ---------------------------------------------------

#[test]
fn the_total_limit_on_handshakes_holds_across_different_hosts() {
    use std::io::Read;
    let rigs = SimRig::rigs("tr-totalcap", 1);
    with_node(
        &rigs[0],
        local_cfg(&[]),
        |c| {
            // listen on every loopback address, so connections can come from different hosts
            c.listen = Some("0.0.0.0:0".parse().unwrap());
            c.handshake_timeout = Duration::from_secs(4);
            c.max_pending_handshakes = 2;
            c.max_pending_per_host = 10;
        },
        |h| {
            let port = h.addr.port();
            let a = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let b = TcpStream::connect(("127.0.0.2", port)).unwrap();
            let end = Instant::now() + Duration::from_secs(3);
            while h.counters.accepted.load(Ordering::Relaxed) < 2 && Instant::now() < end {
                thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(h.counters.accepted.load(Ordering::Relaxed), 2);
            // two hosts hold the two slots; a third host is turned away although it has none of its own
            let mut c = TcpStream::connect(("127.0.0.3", port)).unwrap();
            c.set_read_timeout(Some(Duration::from_millis(800)))
                .unwrap();
            let mut buf = [0u8; 1];
            assert!(matches!(c.read(&mut buf), Ok(0) | Err(_)));
            assert_eq!(
                h.counters.refused_busy.load(Ordering::Relaxed),
                1,
                "{}",
                h.log.dump()
            );
            assert_eq!(h.counters.accepted.load(Ordering::Relaxed), 2);
            drop((a, b));
        },
    );
}

#[test]
fn a_disconnect_the_engine_orders_closes_the_connection() {
    let rigs = SimRig::rigs("tr-orderedclose", 1);
    with_node(
        &rigs[0],
        local_cfg(&[]),
        |_| {},
        |h| {
            // a handshake, then a Hello for another chain: the engine drops and bans the peer by itself
            let mut c = Client::connect(h.addr).unwrap();
            let Message::Hello(mut hello) = c.hello() else {
                unreachable!()
            };
            hello.chain_id = [6; 32];
            c.send(&Message::Hello(hello));
            h.wait_log("disconnecting peer");
            assert!(
                c.is_closed(),
                "the engine dropped the peer but the socket stayed open"
            );
            assert!(h.log.contains("different chain"), "{}", h.log.dump());
        },
    );
}

#[test]
fn a_client_that_hangs_up_is_forgotten_by_the_engine() {
    let rigs = SimRig::rigs("tr-hangup", 1);
    with_node(
        &rigs[0],
        local_cfg(&[]),
        |_| {},
        |h| {
            let mut c = Client::connect(h.addr).unwrap();
            c.send(&c.hello());
            assert!(matches!(
                c.recv(Duration::from_secs(3)),
                Some(Message::Hello(_))
            ));
            h.wait("the peer", |s| s.peers == 1 && s.inbound == 1);
            drop(c);
            h.wait("the peer forgotten", |s| s.peers == 0 && s.inbound == 0);
            assert!(h.log.contains("closed"), "{}", h.log.dump());
        },
    );
}

#[test]
fn a_connection_we_dialled_is_an_outbound_peer() {
    let rigs = SimRig::rigs("tr-outbound", 2);
    with_node(
        &rigs[0],
        local_cfg(&[]),
        |_| {},
        |h| {
            let (snap, log) = run_dialler(&rigs[1], &[h.addr], None, |s| s.peers == 1);
            assert_eq!(
                (snap.peers, snap.outbound, snap.inbound),
                (1, 1, 0),
                "{}",
                log.dump()
            );
            // and the node that was dialled sees it as inbound
            h.wait("the inbound peer", |s| s.inbound <= 1);
        },
    );
}

#[test]
fn a_failed_dial_is_reported_to_the_engine_so_it_stops_waiting_for_it() {
    let rigs = SimRig::rigs("tr-dialfail", 1);
    let dead: SocketAddr = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    // when the failure was logged, and when the engine stopped counting the dial as in progress
    let logged: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let cleared: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let (lg, cl) = (Arc::clone(&logged), Arc::clone(&cleared));
    let (_snap, log) = run_node_until(
        &rigs[0],
        local_cfg(&[dead]),
        |_| {},
        move |s, log| {
            let mut l = lg.lock().unwrap();
            if l.is_none() && log.contains("could not connect to") {
                *l = Some(Instant::now());
            }
            if l.is_some() && s.connecting == 0 {
                *cl.lock().unwrap() = Some(Instant::now());
                return true;
            }
            false
        },
    );
    let (logged, cleared) = (
        logged
            .lock()
            .unwrap()
            .expect("the failure was never logged"),
        cleared
            .lock()
            .unwrap()
            .expect("the engine never stopped waiting"),
    );
    // told at once: the engine's own timeout for a dial that gets no answer is 10 s, which is what a node that was
    // never told would wait for
    assert!(
        cleared.duration_since(logged) < Duration::from_secs(3),
        "{}",
        log.dump()
    );
}
