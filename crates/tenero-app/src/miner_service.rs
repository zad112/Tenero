//! The miner service: a second listener on a node, for miners in other processes **on other computers**
//! (`docs/REMOTE_MINING_PLAN.md`). **Experimental and unaudited; nothing on any network it serves has value.**
//!
//! It is NOT the control interface. The control interface (`server.rs`) stays on loopback, behind a cookie, and can read
//! the whole chain, send transactions and stop the node. This listener
//!
//! * accepts only three requests, `info`, `block_template` and `submit_block` (anything else closes the connection with
//!   no answer), so a mistake here cannot reach `stop`, `submit_tx` or the cookie;
//! * speaks over the Noise channel of the peer-to-peer code (`tenero-net`'s `noise.rs`), with the standard `psk3`
//!   pre-shared key when the operator sets one, so only a miner that holds the key can finish the handshake, and the key is
//!   never sent;
//! * limits what a stranger can cost the node: a cap on miners at once, a cap per address, a rate limit per address, a
//!   request size cap, and timeouts on the handshake and on silence;
//! * answers `info` with only what a miner needs (peer counts, the pruning point and the pool size are zeroed).
//!
//! The requests go into the SAME queue as the control interface's, so the node's own thread does the work and needs no
//! locks. **The cost of that:** a flood of miner requests can fill the queue the wallet also uses (the rate limits are the
//! defence; the numbers are not measured yet).
//!
//! **What this does not do:** it does not tell the miner the node is honest (the miner checks what it is given:
//! `remote_miner::check_template`), it is not anonymous (the node sees the miner's address), and without a key anyone can
//! connect (this first version refuses to start without one: `config.rs`).

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tenero_net::noise::{
    handshake_initiator_psk, handshake_responder_psk, NodeKey, SecureReader, SecureWriter,
};

use crate::control::{Request, Response};
use crate::server::Job;

/// What the service says when an address is over its rate limit (the miner recognises it: `remote_miner::is_slow_down`).
pub const RATE_LIMITED: &str = "too many requests: slow down";
/// The most blocks one address may hand in a minute. Handing in a block is limited apart from everything else, so that a miner
/// that has been told to slow down on `info` and `block_template` can still deliver a block it has found; the limit exists
/// because each block handed in costs the node a full check.
pub const SUBMITS_PER_MINUTE: usize = 30;
/// The default port of the miner service (38333 is the peer port, 38332 the control port).
pub const DEFAULT_PORT: u16 = 38334;
/// Bound into the handshake, so the miner service cannot be mistaken for the peer-to-peer protocol.
pub const PROLOGUE: &[u8] = b"tenero miner service v1";
/// The biggest request frame accepted: a block with a full body is under this.
pub const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;
/// How long a miner has to finish the handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// A connection silent for this long is closed (a miner asks every second or so).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a request waits for the node's loop.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(60);

/// The service's limits (the defaults are for a small server; tests use shorter ones).
#[derive(Clone, Copy, Debug)]
pub struct ServiceConfig {
    /// Miners connected at once; more are closed at once.
    pub max_miners: usize,
    /// Connections from one address at once.
    pub per_address: usize,
    /// Requests one address may make in a minute; the rest are answered with an error.
    pub requests_per_minute: usize,
    pub handshake_timeout: Duration,
    pub idle_timeout: Duration,
    /// The key that must be held to connect (`None`: anyone may; the program refuses that for now).
    pub key: Option<[u8; 32]>,
}

impl Default for ServiceConfig {
    fn default() -> ServiceConfig {
        ServiceConfig {
            max_miners: 8,
            per_address: 2,
            requests_per_minute: 120,
            handshake_timeout: HANDSHAKE_TIMEOUT,
            idle_timeout: IDLE_TIMEOUT,
            key: None,
        }
    }
}

/// Keeps the listener running; dropping it stops accepting (open connections end on their own).
pub struct ServiceHandle {
    pub addr: SocketAddr,
    stop: Arc<AtomicBool>,
    active: Arc<AtomicUsize>,
    stats: Arc<Stats>,
}

/// What the service has done, for the status line and for tests.
#[derive(Default)]
pub struct Stats {
    pub accepted: std::sync::atomic::AtomicU64,
    /// Refused at once: too many miners, or too many from one address.
    pub turned_away: std::sync::atomic::AtomicU64,
    /// The handshake failed (no key, the wrong key, another protocol).
    pub handshake_failed: std::sync::atomic::AtomicU64,
    /// A request outside the allowlist, or a frame that is not a request: the connection was closed.
    pub forbidden: std::sync::atomic::AtomicU64,
    /// A request answered with "too many requests".
    pub rate_limited: std::sync::atomic::AtomicU64,
    pub served: std::sync::atomic::AtomicU64,
}

impl ServiceHandle {
    /// Miners connected now.
    pub fn connections(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }
    pub fn stats(&self) -> &Stats {
        &self.stats
    }
}

impl Drop for ServiceHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// The requests a miner may make. Everything else (above all `auth`, `stop`, `submit_tx` and the wallet's questions)
/// closes the connection.
pub fn allowed(req: &Request) -> bool {
    matches!(
        req,
        Request::Info
            | Request::BlockTemplate { .. }
            | Request::SubmitBlock(_)
            | Request::SubmitHeader(_)
    )
}

/// The answer to `info` as a miner is allowed to see it: the node's peer counts, pruning point and pool size are not for
/// strangers.
pub fn trim(r: Response) -> Response {
    match r {
        Response::Info(mut i) => {
            i.peers = 0;
            i.inbound = 0;
            i.pruned_below = 0;
            i.mempool_txs = 0;
            Response::Info(i)
        }
        other => other,
    }
}

/// An encrypted stream: a socket and the two halves of a finished handshake, as `Read` and `Write`, so the frame
/// functions of the control protocol work over it unchanged.
pub struct SecureStream {
    stream: TcpStream,
    reader: SecureReader,
    writer: SecureWriter,
    buf: Vec<u8>,
    pos: usize,
}

impl SecureStream {
    fn new(stream: TcpStream, reader: SecureReader, writer: SecureWriter) -> SecureStream {
        SecureStream {
            stream,
            reader,
            writer,
            buf: Vec::new(),
            pos: 0,
        }
    }

    /// Dials `addr` and finishes the handshake (with `key` if there is one). The server's public key is not checked: no
    /// key is pinned to anyone yet (see `noise.rs`), so a person who can sit in the middle and does not hold the pre-shared
    /// key still cannot; without a key, a man in the middle is not detected.
    pub fn connect(addr: SocketAddr, key: Option<&[u8; 32]>) -> Result<SecureStream, String> {
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
            .map_err(|e| format!("cannot reach the node at {addr}: {e}"))?;
        stream.set_nodelay(true).map_err(|e| e.to_string())?;
        stream
            .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(Duration::from_secs(30)))
            .map_err(|e| e.to_string())?;
        let me = NodeKey::generate();
        let s = handshake_initiator_psk(&mut stream, &me, PROLOGUE, key).map_err(|e| {
            format!("the node at {addr} did not finish the handshake: {e} (is the key right, and is that the miner port?)")
        })?;
        stream
            .set_read_timeout(Some(Duration::from_secs(120)))
            .map_err(|e| e.to_string())?;
        Ok(SecureStream::new(stream, s.reader, s.writer))
    }
}

fn to_io(e: tenero_net::noise::NoiseError) -> io::Error {
    match e {
        tenero_net::noise::NoiseError::Io(e) => e,
        other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
    }
}

impl Read for SecureStream {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.pos >= self.buf.len() {
            self.buf = self.reader.read_chunk(&mut self.stream).map_err(to_io)?;
            self.pos = 0;
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl Write for SecureStream {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.writer
            .write_all(&mut self.stream, data)
            .map_err(to_io)?;
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

/// Requests per address in the last minute.
#[derive(Default)]
struct Limits {
    /// (address, is it a block handed in): the two kinds are counted apart
    requests: HashMap<(IpAddr, bool), Vec<Instant>>,
    connections: HashMap<IpAddr, usize>,
}

impl Limits {
    /// Records a request; false if the address is over its limit for the last minute.
    fn request(&mut self, ip: IpAddr, submit: bool, limit: usize) -> bool {
        let now = Instant::now();
        let v = self.requests.entry((ip, submit)).or_default();
        v.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
        if v.len() >= limit {
            return false;
        }
        v.push(now);
        true
    }
    fn open(&mut self, ip: IpAddr, max: usize) -> bool {
        let c = self.connections.entry(ip).or_default();
        if *c >= max {
            return false;
        }
        *c += 1;
        true
    }
    fn close(&mut self, ip: IpAddr) {
        if let Some(c) = self.connections.get_mut(&ip) {
            *c = c.saturating_sub(1);
            if *c == 0 {
                self.connections.remove(&ip);
            }
        }
        // forget addresses that have been quiet for a while, so the table cannot grow without bound
        if self.requests.len() > 4096 {
            let now = Instant::now();
            self.requests.retain(|_, v| {
                v.last()
                    .is_some_and(|t| now.duration_since(*t) < Duration::from_secs(60))
            });
        }
    }
}

/// Starts listening on `listen` (any address: this is the point). Its requests go into the queue of `control`, the node's
/// control interface, so the node's own thread answers them.
pub fn start(
    listen: SocketAddr,
    control: &crate::server::ControlHandle,
    cfg: ServiceConfig,
) -> io::Result<ServiceHandle> {
    let jobs = control.job_sender();
    let listener = TcpListener::bind(listen)?;
    listener.set_nonblocking(true)?;
    let addr = listener.local_addr()?;
    let stop = Arc::new(AtomicBool::new(false));
    let active = Arc::new(AtomicUsize::new(0));
    let stats = Arc::new(Stats::default());
    let limits = Arc::new(Mutex::new(Limits::default()));
    // the service's own long-term key: made fresh at every start (nothing pins it to anyone, so there is nothing to keep)
    let key = Arc::new(NodeKey::generate());
    {
        let (stop, active, stats) = (Arc::clone(&stop), Arc::clone(&active), Arc::clone(&stats));
        thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, peer)) => {
                        let ip = peer.ip();
                        if active.fetch_add(1, Ordering::SeqCst) >= cfg.max_miners
                            || !limits
                                .lock()
                                .map(|mut l| l.open(ip, cfg.per_address))
                                .unwrap_or(false)
                        {
                            // (the second test only runs if the first passed, which counted this connection)
                            active.fetch_sub(1, Ordering::SeqCst);
                            stats.turned_away.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        stats.accepted.fetch_add(1, Ordering::Relaxed);
                        let (jobs, active, stats, limits, key) = (
                            jobs.clone(),
                            Arc::clone(&active),
                            Arc::clone(&stats),
                            Arc::clone(&limits),
                            Arc::clone(&key),
                        );
                        thread::spawn(move || {
                            let _ = serve(stream, ip, &key, &jobs, &limits, &stats, cfg);
                            if let Ok(mut l) = limits.lock() {
                                l.close(ip);
                            }
                            active.fetch_sub(1, Ordering::SeqCst);
                        });
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(100)),
                }
            }
        });
    }
    Ok(ServiceHandle {
        addr,
        stop,
        active,
        stats,
    })
}

/// One request frame, at most [`MAX_REQUEST_BYTES`] (a stranger does not get to make us read 16 MiB).
fn read_request(stream: &mut SecureStream) -> io::Result<Vec<u8>> {
    let mut h = [0u8; 4];
    stream.read_exact(&mut h)?;
    let n = u32::from_le_bytes(h) as usize;
    if n == 0 || n > MAX_REQUEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a request frame of a size not allowed",
        ));
    }
    let mut body = Vec::with_capacity(n.min(64 * 1024));
    let mut left = n;
    let mut buf = [0u8; 16 * 1024];
    while left > 0 {
        let take = left.min(buf.len());
        stream.read_exact(&mut buf[..take])?;
        body.extend_from_slice(&buf[..take]);
        left -= take;
    }
    Ok(body)
}

fn serve(
    stream: TcpStream,
    ip: IpAddr,
    key: &NodeKey,
    jobs: &SyncSender<Job>,
    limits: &Mutex<Limits>,
    stats: &Stats,
    cfg: ServiceConfig,
) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(cfg.handshake_timeout))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let mut s = stream;
    let secured = match handshake_responder_psk(&mut s, key, PROLOGUE, cfg.key.as_ref()) {
        Ok(x) => x,
        Err(_) => {
            stats.handshake_failed.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
    };
    s.set_read_timeout(Some(cfg.idle_timeout))?;
    let mut s = SecureStream::new(s, secured.reader, secured.writer);
    loop {
        let body = read_request(&mut s)?;
        let req = match Request::from_body(&body) {
            Ok(r) if allowed(&r) => r,
            _ => {
                // not a request, or one a miner may not make: hang up, with no answer
                stats.forbidden.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
        };
        let submit = matches!(req, Request::SubmitBlock(_) | Request::SubmitHeader(_));
        let limit = if submit {
            SUBMITS_PER_MINUTE
        } else {
            cfg.requests_per_minute
        };
        let within = limits
            .lock()
            .map(|mut l| l.request(ip, submit, limit))
            .unwrap_or(false);
        let response = if !within {
            stats.rate_limited.fetch_add(1, Ordering::Relaxed);
            Response::Error(RATE_LIMITED.into())
        } else {
            let (reply, wait) = sync_channel(1);
            match jobs.try_send(Job { req, reply }) {
                Ok(()) => match wait.recv_timeout(ANSWER_TIMEOUT) {
                    Ok(r) => trim(r),
                    Err(RecvTimeoutError::Timeout) => {
                        Response::Error("the node did not answer in time".into())
                    }
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                },
                Err(TrySendError::Full(_)) => Response::Error("the node is busy".into()),
                Err(TrySendError::Disconnected(_)) => return Ok(()),
            }
        };
        let body = match response.to_body() {
            Ok(b) => b,
            Err(e) => Response::Error(e.to_string()).to_body().expect("encodes"),
        };
        crate::control::write_frame(&mut s, &body)?;
        stats.served.fetch_add(1, Ordering::Relaxed);
    }
}
