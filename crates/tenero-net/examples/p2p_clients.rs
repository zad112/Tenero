//! Opens many connections to a test node and holds them, so the NODE process can be measured (its memory, its
//! threads) with them up. Each client does a real Noise handshake and a real `Hello`, answers pings, and reads
//! whatever it is sent. Nothing else. **A test tool: it makes a node hold many connections.**
//!
//! ```text
//! cargo run --release -p tenero-net --example p2p_clients -- --connect 127.0.0.1:18331 --count 60 --hold 60
//! ```
//!
//! Options: `--connect IP:PORT` (required), `--count N` (default 50), `--hold SECS` (default 30). The clients
//! connect 40 ms apart.

use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tenero_net::noise::{handshake_initiator, prologue, NodeKey};
use tenero_net::sim::test_chain_id;
use tenero_net::{encode, FrameDecoder, Hello, Message, PROTOCOL_VERSION};

fn main() {
    let mut connect: Option<SocketAddr> = None;
    let (mut count, mut hold) = (50usize, 30u64);
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let v = it.next().unwrap_or_default();
        match flag.as_str() {
            "--connect" => connect = v.parse().ok(),
            "--count" => count = v.parse().unwrap_or(count),
            "--hold" => hold = v.parse().unwrap_or(hold),
            other => {
                eprintln!("unknown option {other}");
                std::process::exit(2);
            }
        }
    }
    let Some(addr) = connect else {
        eprintln!("--connect IP:PORT is required");
        std::process::exit(2);
    };
    let chain = test_chain_id();
    let up = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));
    let end = Instant::now() + Duration::from_secs(hold);
    let started = Instant::now();
    let threads: Vec<_> = (0..count)
        .map(|i| {
            let (up, failed) = (Arc::clone(&up), Arc::clone(&failed));
            std::thread::spawn(move || {
                // staggered: the node allows only a few handshakes at once from one address (and all of these come
                // from one), so a burst would be refused, rightly
                std::thread::sleep(Duration::from_millis(40 * i as u64));
                let mut stream = match TcpStream::connect_timeout(&addr, Duration::from_secs(10)) {
                    Ok(s) => s,
                    Err(_) => {
                        failed.fetch_add(1, Ordering::SeqCst);
                        return;
                    }
                };
                let _ = stream.set_nodelay(true);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                let Ok(s) = handshake_initiator(
                    &mut stream,
                    &NodeKey::generate(),
                    &prologue(PROTOCOL_VERSION, &chain),
                ) else {
                    failed.fetch_add(1, Ordering::SeqCst);
                    return;
                };
                let (mut r, mut w) = (s.reader, s.writer);
                let hello = Message::Hello(Hello {
                    version: PROTOCOL_VERSION,
                    chain_id: chain,
                    tip_height: 0,
                    cumulative_work: [0; 32],
                    tip_id: [9; 32],
                    pruned_below: 0,
                    nonce: 0,
                });
                let _ = w.write_all(&mut stream, &encode(&hello).unwrap());
                up.fetch_add(1, Ordering::SeqCst);
                let mut dec = FrameDecoder::new();
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                while Instant::now() < end {
                    match r.read_chunk(&mut stream) {
                        Ok(chunk) => {
                            dec.push(&chunk);
                            while let Ok(Some(m)) = dec.next_message() {
                                if let Message::Ping(n) = m {
                                    let _ = w.write_all(
                                        &mut stream,
                                        &encode(&Message::Pong(n)).unwrap(),
                                    );
                                }
                            }
                        }
                        Err(tenero_net::noise::NoiseError::Io(e))
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                            ) => {}
                        Err(_) => return,
                    }
                }
            })
        })
        .collect();
    while Instant::now() < end {
        std::thread::sleep(Duration::from_secs(5));
        eprintln!(
            "{} of {count} connected, {} failed, {:?} in",
            up.load(Ordering::SeqCst),
            failed.load(Ordering::SeqCst),
            started.elapsed()
        );
    }
    for t in threads {
        let _ = t.join();
    }
}
