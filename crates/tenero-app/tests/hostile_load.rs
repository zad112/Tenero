//! What a real `tenerod` process costs in memory and threads when strangers connect and misbehave (B1, B2 and B3 of `docs/THREAT_MODEL.md`).
//! A measurement, `#[ignore]`d: `cargo test --release -p tenero-app --test hostile_load -- --ignored --nocapture`. It starts the real node
//! program as a child process (so the numbers are the node's own, not the test's), opens connections of several hostile kinds, and reads the
//! child's working set, private memory and thread count while they are held and after they are gone.
//!
//! What it does NOT show: a hostile network (everything comes from one machine over loopback, so the connections are as fast as they can be and
//! all from one address), a node with a long chain (this one is empty), or the cost of the proof-of-work check (no blocks are sent).

use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tenero_app::client::{read_cookie, RemoteNode, COOKIE_FILE};
use tenero_app::config::Network;
use tenero_app::daemon::chain_id_of;
use tenero_net::noise::{handshake_initiator, prologue, NodeKey};
use tenero_net::wire::encode;
use tenero_net::{Hello, Message, PROTOCOL_VERSION};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Node {
    child: Child,
    p2p: SocketAddr,
    control: SocketAddr,
    data: std::path::PathBuf,
}

impl Node {
    fn start() -> Node {
        let data = std::env::temp_dir().join(format!("tenero-hostile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let (p2p, control): (SocketAddr, SocketAddr) = (
            format!("127.0.0.1:{}", free_port()).parse().unwrap(),
            format!("127.0.0.1:{}", free_port()).parse().unwrap(),
        );
        let mut c = Command::new(env!("CARGO_BIN_EXE_tenerod"));
        c.args([
            "--data",
            data.to_str().unwrap(),
            "--network",
            "test",
            "--listen",
            &p2p.to_string(),
            "--control",
            &control.to_string(),
            "--quiet",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: nothing opens on the desktop
        }
        let child = c.spawn().expect("tenerod starts");
        let n = Node {
            child,
            p2p,
            control,
            data,
        };
        let end = Instant::now() + Duration::from_secs(30);
        while n.info_ms().is_none() {
            assert!(Instant::now() < end, "the node did not come up");
            std::thread::sleep(Duration::from_millis(200));
        }
        n
    }

    /// The time (ms) a request over the control interface takes, or `None` if it fails.
    fn info_ms(&self) -> Option<u128> {
        let cookie = read_cookie(&self.data.join(COOKIE_FILE)).ok()?;
        let t = Instant::now();
        let c = RemoteNode::connect(self.control, &cookie).ok()?;
        c.info().ok()?;
        Some(t.elapsed().as_millis())
    }

    fn stop(mut self) {
        if let Ok(cookie) = read_cookie(&self.data.join(COOKIE_FILE)) {
            if let Ok(c) = RemoteNode::connect(self.control, &cookie) {
                let _ = c.stop();
            }
        }
        let end = Instant::now() + Duration::from_secs(20);
        while Instant::now() < end {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let _ = self.child.kill(); // only if it did not stop (this is our own child, by handle)
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

/// (working set, private bytes, threads) of a process, in MiB and a count.
#[derive(Clone, Copy, Debug)]
struct Usage {
    working_mib: f64,
    private_mib: f64,
    threads: u64,
}

#[cfg(windows)]
fn usage(pid: u32) -> Option<Usage> {
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("$p = Get-Process -Id {pid}; \"$($p.WorkingSet64) $($p.PrivateMemorySize64) $($p.Threads.Count)\""),
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let v: Vec<f64> = text
        .split_whitespace()
        .filter_map(|x| x.parse().ok())
        .collect();
    (v.len() == 3).then(|| Usage {
        working_mib: v[0] / 1048576.0,
        private_mib: v[1] / 1048576.0,
        threads: v[2] as u64,
    })
}

#[cfg(not(windows))]
fn usage(pid: u32) -> Option<Usage> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let kb = |key: &str| -> Option<f64> {
        s.lines()
            .find(|l| l.starts_with(key))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    };
    Some(Usage {
        working_mib: kb("VmRSS:")? / 1024.0,
        private_mib: kb("RssAnon:").unwrap_or(0.0) / 1024.0,
        threads: s
            .lines()
            .find(|l| l.starts_with("Threads:"))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()?,
    })
}

/// The kinds of stranger.
#[derive(Clone, Copy)]
enum Kind {
    /// Connects and says nothing.
    RawIdle,
    /// Connects and sends a kilobyte of junk.
    RawJunk,
    /// Completes the encrypted handshake and says nothing.
    SilentAfterHandshake,
    /// Handshake, hello, then pings as fast as the socket takes them.
    PingFlood,
    /// Handshake, then sealed junk as fast as the socket takes it (not messages: bytes).
    JunkFlood,
}

fn session(
    addr: SocketAddr,
    kind: Kind,
    chain: [u8; 32],
    stop: &AtomicBool,
    connected: &AtomicUsize,
    sent: &AtomicUsize,
) {
    let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_secs(5)) else {
        return;
    };
    let _ = s.set_nodelay(true);
    let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    match kind {
        Kind::RawIdle => {
            connected.fetch_add(1, Ordering::SeqCst);
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        Kind::RawJunk => {
            let _ = s.write_all(&[0xA5; 1024]);
            connected.fetch_add(1, Ordering::SeqCst);
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        _ => {
            let key = NodeKey::generate();
            let Ok(mut secured) =
                handshake_initiator(&mut s, &key, &prologue(PROTOCOL_VERSION, &chain))
            else {
                return;
            };
            connected.fetch_add(1, Ordering::SeqCst);
            let hello = Hello {
                version: PROTOCOL_VERSION,
                chain_id: chain,
                tip_height: 0,
                cumulative_work: [0; 32],
                tip_id: [0; 32],
                pruned_below: 0,
                nonce: u64::from_le_bytes(key.public()[..8].try_into().unwrap()) | 1,
            };
            match kind {
                Kind::SilentAfterHandshake => {
                    while !stop.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
                Kind::PingFlood => {
                    let send = |s: &mut TcpStream,
                                w: &mut tenero_net::noise::SecureWriter,
                                m: &Message|
                     -> bool {
                        let Ok(b) = encode(m) else { return false };
                        let Ok(sealed) = w.seal(&b) else { return false };
                        s.write_all(&sealed).is_ok()
                    };
                    let _ = send(&mut s, &mut secured.writer, &Message::Hello(hello));
                    let mut n = 0u64;
                    while !stop.load(Ordering::SeqCst) {
                        if !send(&mut s, &mut secured.writer, &Message::Ping(n)) {
                            return; // the node dropped us: that is the defence working
                        }
                        n += 1;
                        sent.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Kind::JunkFlood => {
                    let junk = vec![0x5Au8; 60_000];
                    while !stop.load(Ordering::SeqCst) {
                        let Ok(sealed) = secured.writer.seal(&junk) else {
                            return;
                        };
                        if s.write_all(&sealed).is_err() {
                            return;
                        }
                        sent.fetch_add(60_000, Ordering::Relaxed);
                    }
                }
                _ => unreachable!(),
            }
        }
    }
}

struct Stage {
    label: String,
    attempted: usize,
    connected: usize,
    sent: usize,
    during: Usage,
    peak_threads: u64,
    info_ms_during: Option<u128>,
    after: Usage,
    info_ms_after: Option<u128>,
}

fn stage(node: &Node, label: &str, kind: Kind, n: usize, hold: Duration, chain: [u8; 32]) -> Stage {
    let pid = node.child.id();
    let stop = Arc::new(AtomicBool::new(false));
    let (connected, sent) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let handles: Vec<_> = (0..n)
        .map(|_| {
            let (stop, connected, sent, addr) =
                (stop.clone(), connected.clone(), sent.clone(), node.p2p);
            std::thread::Builder::new()
                .stack_size(256 * 1024)
                .spawn(move || session(addr, kind, chain, &stop, &connected, &sent))
                .unwrap()
        })
        .collect();
    // let the connections come in, then read the node while they are held
    std::thread::sleep(hold / 2);
    let mut peak_threads = 0;
    let mut during = usage(pid).unwrap();
    for _ in 0..3 {
        let u = usage(pid).unwrap();
        peak_threads = peak_threads.max(u.threads);
        if u.working_mib > during.working_mib {
            during = u;
        }
    }
    let info_ms_during = node.info_ms();
    std::thread::sleep(hold / 2);
    stop.store(true, Ordering::SeqCst);
    for h in handles {
        let _ = h.join();
    }
    // and after they are gone
    std::thread::sleep(Duration::from_secs(12));
    let after = usage(pid).unwrap();
    let info_ms_after = node.info_ms();
    Stage {
        label: label.to_string(),
        attempted: n,
        connected: connected.load(Ordering::SeqCst),
        sent: sent.load(Ordering::SeqCst),
        during,
        peak_threads,
        info_ms_during,
        after,
        info_ms_after,
    }
}

#[test]
#[ignore = "a measurement (about 4 minutes): run with --ignored --nocapture"]
fn measure_a_real_node_under_hostile_connections() {
    let chain = chain_id_of(Network::Test).unwrap();
    println!("
=== a real tenerod (test network, empty chain) under hostile connections from this machine over loopback ===");
    println!("  (a fresh node for each kind, because a node bans an address that misbehaves, and all of these come from 127.0.0.1)");
    let plan: Vec<(&str, Kind, usize, u64)> = vec![
        ("400 connections that say nothing", Kind::RawIdle, 400, 14),
        (
            "400 connections that send 1 KiB of junk",
            Kind::RawJunk,
            400,
            14,
        ),
        (
            "200 completed handshakes, then silence",
            Kind::SilentAfterHandshake,
            200,
            14,
        ),
        (
            "100 handshakes, hello, then a flood of pings",
            Kind::PingFlood,
            100,
            14,
        ),
        (
            "50 handshakes, then sealed junk bytes, flat out",
            Kind::JunkFlood,
            50,
            14,
        ),
    ];
    let mut rows = vec![];
    for (label, kind, n, secs) in plan {
        let node = Node::start();
        std::thread::sleep(Duration::from_secs(3));
        let pid = node.child.id();
        let base = usage(pid).unwrap();
        let st = stage(&node, label, kind, n, Duration::from_secs(secs), chain);
        println!(
            "  {:<48} idle {:>4.1} MiB, {} thr | connected {:>3} of {:>3} | during: {:>6.1} MiB working, {:>6.1} private, {:>4} threads, control {:>5} ms | after 12 s: {:>6.1} MiB, {:>4} threads, control {:>4} ms | sent {} KiB or msgs",
            st.label, base.working_mib, base.threads, st.connected, st.attempted, st.during.working_mib, st.during.private_mib, st.peak_threads,
            st.info_ms_during.map_or("FAILED".to_string(), |m| m.to_string()),
            st.after.working_mib, st.after.threads,
            st.info_ms_after.map_or("FAILED".to_string(), |m| m.to_string()),
            st.sent / 1024
        );
        rows.push(st);
        node.stop();
    }
    // what a person can rely on: the node was still there and answering afterwards
    for st in &rows {
        assert!(
            st.info_ms_after.is_some(),
            "{}: the node did not answer afterwards",
            st.label
        );
    }
}
