//! Real sockets under the protocol engine (M8.4): TCP, the Noise channel of [`crate::noise`], and threads.
//!
//! **Shape.** One thread (the caller's) owns the [`Engine`] and runs an event loop: it takes events from a bounded
//! channel, feeds them to [`Engine::handle`], and carries out the [`Action`]s. Every connection has a **reader**
//! thread (decrypt, decode frames, send messages to the loop) and a **writer** thread (take encoded frames from a
//! queue, encrypt, write). Dialling and the handshakes of accepted sockets run in short-lived threads of their own.
//! The engine never sees a socket, a thread or the clock. About two threads per peer, so 50 to 128 peers is a few
//! hundred threads; the loop is the only place that would change if this moved to an async runtime.
//!
//! **What it defends** (and where; the message-level defences are the engine's, `docs/M8_PLAN.md` M8.1):
//! * a socket that does not finish the Noise handshake within a **total** deadline (not a per-read one: a peer
//!   that dribbles a byte at a time is cut off) is closed; at most `max_pending_handshakes` run at once and at most
//!   `max_pending_per_host` from one address, so one host cannot fill the slots;
//! * a host the engine has banned is refused at `accept`, before any cryptography;
//! * a handshake under another chain id or protocol version fails (the prologue), and is logged;
//! * bytes that decrypt but are not a message (a bad or oversized frame) ban the peer; bytes that do not
//!   decrypt only close the connection, since someone on the wire could cause that without the peer's doing;
//! * the queue of frames waiting to be written to a peer is bounded in **bytes**: a peer that does not read is
//!   disconnected rather than allowed to make us hold 16 MiB frames without end;
//! * readers block when the loop's channel is full, so a flood is slowed by TCP back-pressure, not by memory;
//! * the memory the queued frames may hold is bounded in **bytes** (`budget.rs`: a reservation made before the rest of a
//!   frame is read, per-connection and total ceilings, large frames kept apart from small ones), and each connection is
//!   read at no more than a set byte rate (a slower read, never a punishment).
//!
//! **Limits, stated plainly:** real memory can be up to about twice the budget while a frame is being decoded; a peer that
//! declares a large frame and stalls holds its reservation until the engine drops it for not answering a ping; the total
//! byte rate over all peers is not limited (only each peer's); the handshake costs a few Curve25519 operations an attacker can ask for repeatedly, limited only by the pending
//! caps and bans; the addresses it dials must be `ip:port` (no DNS); and it is unreviewed (M9).

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::addrbook::host_of;
use crate::budget::{Gate, GateConfig, Lane, Throttle, Ticket};
use crate::engine::{Action, Engine, Event, PeerId};
use crate::message::{Message, PROTOCOL_VERSION};
use crate::noise::{
    handshake_initiator, handshake_responder, prologue, NodeKey, SecureReader, SecureWriter,
    Secured,
};
use crate::wire::{encode, FrameDecoder};

/// Where log lines go.
pub type Logger = Arc<dyn Fn(&str) + Send + Sync>;

/// A logger that writes each line to standard error with the time.
pub fn stderr_logger() -> Logger {
    Arc::new(|line| {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        eprintln!("[{secs}] {line}");
    })
}

#[derive(Clone)]
pub struct NetConfig {
    /// Accept connections here (`None`: only dial out).
    pub listen: Option<SocketAddr>,
    /// Our long-term Noise key.
    pub key: NodeKey,
    /// The chain we are on: part of the handshake, so a node on another chain never gets as far as the protocol.
    pub chain_id: [u8; 32],
    /// The whole handshake, from the first byte to the last, must finish within this.
    pub handshake_timeout: Duration,
    pub connect_timeout: Duration,
    /// How often the engine is ticked at least.
    pub tick: Duration,
    /// How often the hooks are polled. Separate from `tick` because a hook may answer a program waiting for it (the
    /// wallet's requests), which should not wait a quarter of a second for every answer; the default is `tick`.
    pub hook_tick: Duration,
    /// Handshakes in progress at once, in all and from one address.
    pub max_pending_handshakes: usize,
    pub max_pending_per_host: usize,
    /// Bytes of encoded frames allowed to wait for one peer's socket; past this the peer is disconnected.
    pub max_queued_bytes: usize,
    /// The bound of the channel into the loop (messages). The memory the messages in it may hold is bounded separately, in
    /// bytes, by `memory` (`budget.rs`).
    pub inbound_queue: usize,
    /// How many bytes of received frames may be held at once, in all and per connection.
    pub memory: GateConfig,
    /// The most bytes a second one connection is read at, and the burst it may save up (`budget.rs`; a rate of 0 is no
    /// limit). A connection over its rate is read more slowly, not punished.
    pub peer_bytes_per_sec: u64,
    pub peer_burst_bytes: u64,
    /// Where the address book and ban list are kept (saved atomically), if anywhere.
    pub state_path: Option<PathBuf>,
    pub save_every: Duration,
    pub log: Logger,
}

impl NetConfig {
    pub fn new(key: NodeKey, chain_id: [u8; 32]) -> NetConfig {
        NetConfig {
            listen: None,
            key,
            chain_id,
            handshake_timeout: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(10),
            tick: Duration::from_millis(250),
            hook_tick: Duration::from_millis(250),
            max_pending_handshakes: 64,
            max_pending_per_host: 4,
            max_queued_bytes: 32 * 1024 * 1024,
            inbound_queue: 1024,
            memory: GateConfig::default(),
            peer_bytes_per_sec: 8 * 1024 * 1024,
            peer_burst_bytes: 32 * 1024 * 1024,
            state_path: None,
            save_every: Duration::from_secs(300),
            log: stderr_logger(),
        }
    }
}

/// Counters, readable from any thread while the node runs.
#[derive(Default)]
pub struct Counters {
    pub accepted: AtomicU64,
    pub refused_banned: AtomicU64,
    pub refused_busy: AtomicU64,
    pub handshake_failures: AtomicU64,
    pub connected: AtomicU64,
    pub dials: AtomicU64,
    pub dial_failures: AtomicU64,
    pub slow_peer_drops: AtomicU64,
    pub bad_bytes: AtomicU64,
    /// Encrypted bytes read from and written to sockets (payload plus chunk headers and tags; not TCP headers).
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    /// Live connection threads (readers and writers).
    pub threads: AtomicUsize,
    /// Milliseconds readers have spent waiting because a connection was over its byte rate.
    pub throttled_ms: AtomicU64,
}

/// The most events the loop reads in a row before it looks at the clock again.
const MAX_DRAIN: usize = 4096;

/// Called from the loop about every tick with the engine itself: a miner's or a test's way to act. The events it
/// returns are fed to the engine in order, and the resulting actions carried out.
pub trait Hooks {
    fn poll(&mut self, engine: &mut Engine<'_>, now_ms: u64) -> Vec<Event>;
}

pub struct NoHooks;

impl Hooks for NoHooks {
    fn poll(&mut self, _engine: &mut Engine<'_>, _now_ms: u64) -> Vec<Event> {
        Vec::new()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A stream whose every read and write is limited by one fixed deadline.
struct Deadline {
    stream: TcpStream,
    end: Instant,
}

impl Deadline {
    fn remaining(&self) -> io::Result<Duration> {
        let left = self.end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the handshake took too long",
            ))
        } else {
            Ok(left)
        }
    }
}

impl Read for Deadline {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.remaining()?;
        self.stream.set_read_timeout(Some(left))?;
        self.stream.read(buf)
    }
}

impl Write for Deadline {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let left = self.remaining()?;
        self.stream.set_write_timeout(Some(left))?;
        self.stream.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

enum Ev {
    /// A finished handshake, with the socket.
    Up {
        stream: TcpStream,
        secured: Secured,
        addr: String,
        inbound: bool,
    },
    DialFailed {
        addr: String,
        why: String,
    },
    Msg {
        peer: PeerId,
        msg: Message,
        /// The memory reserved for this message's frame, given back when the loop has dealt with it.
        ticket: Option<Ticket>,
    },
    Bad {
        peer: PeerId,
        why: String,
    },
    Closed {
        peer: PeerId,
        why: String,
    },
}

/// Handshakes in progress, in all and per host.
struct Pending {
    total: AtomicUsize,
    per_host: Mutex<HashMap<String, usize>>,
    max_total: usize,
    max_host: usize,
}

struct PendingGuard {
    pending: Arc<Pending>,
    host: String,
}

impl Pending {
    fn try_acquire(self: &Arc<Pending>, host: &str) -> Option<PendingGuard> {
        let mut map = self.per_host.lock().ok()?;
        if self.total.load(Ordering::SeqCst) >= self.max_total {
            return None;
        }
        let n = map.entry(host.to_string()).or_insert(0);
        if *n >= self.max_host {
            return None;
        }
        *n += 1;
        self.total.fetch_add(1, Ordering::SeqCst);
        Some(PendingGuard {
            pending: Arc::clone(self),
            host: host.to_string(),
        })
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Ok(mut map) = self.pending.per_host.lock() {
            if let Some(n) = map.get_mut(&self.host) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    map.remove(&self.host);
                }
            }
        }
        self.pending.total.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One live connection, as the loop keeps it.
struct Conn {
    stream: TcpStream,
    tx: Sender<Vec<u8>>,
    queued: Arc<AtomicUsize>,
    /// Set when the connection is closed, so a reader waiting for memory or for its byte rate stops waiting.
    closed: Arc<AtomicBool>,
}

impl Conn {
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

/// A bound listener (if any) and the settings; [`Net::run`] runs the node.
pub struct Net {
    cfg: NetConfig,
    listener: Option<TcpListener>,
    local: Option<SocketAddr>,
    counters: Arc<Counters>,
    gate: Arc<Gate>,
}

impl Net {
    pub fn bind(cfg: NetConfig) -> io::Result<Net> {
        let (listener, local) = match cfg.listen {
            Some(addr) => {
                let l = TcpListener::bind(addr)?;
                let local = l.local_addr()?;
                (Some(l), Some(local))
            }
            None => (None, None),
        };
        let gate = Gate::new(cfg.memory.clone());
        Ok(Net {
            cfg,
            listener,
            local,
            counters: Arc::new(Counters::default()),
            gate,
        })
    }

    /// The address actually listened on (useful when the port was 0).
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local
    }

    pub fn counters(&self) -> Arc<Counters> {
        Arc::clone(&self.counters)
    }

    /// The memory gate (`budget.rs`): how much received data is held, and the peaks.
    pub fn gate(&self) -> Arc<Gate> {
        Arc::clone(&self.gate)
    }

    /// Runs the node until `shutdown` is set: accepts and dials, feeds the engine, carries out its actions, saves
    /// its state. Returns when everything has been closed.
    pub fn run<H: Hooks>(
        self,
        engine: &mut Engine<'_>,
        shutdown: Arc<AtomicBool>,
        hooks: &mut H,
    ) -> io::Result<()> {
        let Net {
            cfg,
            listener,
            counters,
            ..
        } = self;
        let log = cfg.log.clone();
        let (tx, rx) = sync_channel::<Ev>(cfg.inbound_queue.max(1));
        let banned: Arc<Mutex<HashMap<String, u64>>> = Arc::new(Mutex::new(HashMap::new()));
        let pro = prologue(PROTOCOL_VERSION, &cfg.chain_id);

        // the state saved by an earlier run
        if let Some(path) = &cfg.state_path {
            match std::fs::read(path) {
                Ok(bytes) => match engine.import_state(&bytes) {
                    Ok(()) => log(&format!(
                        "loaded the address book and bans from {}",
                        path.display()
                    )),
                    Err(e) => log(&format!(
                        "ignored a damaged state file {}: {e}",
                        path.display()
                    )),
                },
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => log(&format!("could not read {}: {e}", path.display())),
            }
        }

        if let Some(listener) = listener {
            listener.set_nonblocking(true)?;
            let acceptor = Acceptor {
                listener,
                key: cfg.key.clone(),
                prologue: pro.clone(),
                timeout: cfg.handshake_timeout,
                tx: tx.clone(),
                shutdown: Arc::clone(&shutdown),
                banned: Arc::clone(&banned),
                pending: Arc::new(Pending {
                    total: AtomicUsize::new(0),
                    per_host: Mutex::new(HashMap::new()),
                    max_total: cfg.max_pending_handshakes,
                    max_host: cfg.max_pending_per_host,
                }),
                counters: Arc::clone(&counters),
                log: log.clone(),
            };
            thread::spawn(move || acceptor.run());
        }

        let mut lp = Loop {
            cfg: &cfg,
            tx,
            banned,
            counters,
            gate: Arc::clone(&self.gate),
            conns: HashMap::new(),
            next_peer: 1,
            pro,
            shutdown: Arc::clone(&shutdown),
        };
        lp.event_loop(engine, &rx, hooks);
        // shutting down: close everything and save
        for conn in lp.conns.values() {
            conn.close();
        }
        lp.save_state(engine);
        Ok(())
    }
}

struct Acceptor {
    listener: TcpListener,
    key: NodeKey,
    prologue: Vec<u8>,
    timeout: Duration,
    tx: SyncSender<Ev>,
    shutdown: Arc<AtomicBool>,
    banned: Arc<Mutex<HashMap<String, u64>>>,
    pending: Arc<Pending>,
    counters: Arc<Counters>,
    log: Logger,
}

impl Acceptor {
    fn run(self) {
        while !self.shutdown.load(Ordering::SeqCst) {
            match self.listener.accept() {
                Ok((stream, peer)) => self.on_accept(stream, peer),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => {
                    (self.log)(&format!("accept failed: {e}"));
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }

    fn on_accept(&self, stream: TcpStream, peer: SocketAddr) {
        let addr = peer.to_string();
        let host = host_of(&addr).to_string();
        let now = now_ms();
        let is_banned = self
            .banned
            .lock()
            .map(|m| m.get(&host).is_some_and(|&until| until > now))
            .unwrap_or(false);
        if is_banned {
            self.counters.refused_banned.fetch_add(1, Ordering::Relaxed);
            (self.log)(&format!("refused {addr}: banned"));
            return; // dropping the stream closes it, before any cryptography
        }
        let Some(guard) = self.pending.try_acquire(&host) else {
            self.counters.refused_busy.fetch_add(1, Ordering::Relaxed);
            (self.log)(&format!("refused {addr}: too many handshakes in progress"));
            return;
        };
        self.counters.accepted.fetch_add(1, Ordering::Relaxed);
        let (key, pro, timeout) = (self.key.clone(), self.prologue.clone(), self.timeout);
        let (tx, counters, log) = (
            self.tx.clone(),
            Arc::clone(&self.counters),
            self.log.clone(),
        );
        thread::spawn(move || {
            let _guard = guard;
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_nodelay(true);
            let mut d = Deadline {
                stream,
                end: Instant::now() + timeout,
            };
            match handshake_responder(&mut d, &key, &pro) {
                Ok(secured) => {
                    let _ = d.stream.set_read_timeout(None);
                    let _ = d.stream.set_write_timeout(None);
                    let _ = tx.send(Ev::Up {
                        stream: d.stream,
                        secured,
                        addr,
                        inbound: true,
                    });
                }
                Err(e) => {
                    counters.handshake_failures.fetch_add(1, Ordering::Relaxed);
                    log(&format!("handshake with {addr} failed: {}", describe(&e)));
                }
            }
        });
    }
}

struct Loop<'c> {
    cfg: &'c NetConfig,
    tx: SyncSender<Ev>,
    banned: Arc<Mutex<HashMap<String, u64>>>,
    counters: Arc<Counters>,
    gate: Arc<Gate>,
    conns: HashMap<PeerId, Conn>,
    next_peer: PeerId,
    pro: Vec<u8>,
    shutdown: Arc<AtomicBool>,
}

impl Loop<'_> {
    fn log(&self, s: &str) {
        (self.cfg.log)(s);
    }

    fn event_loop<H: Hooks>(&mut self, engine: &mut Engine<'_>, rx: &Receiver<Ev>, hooks: &mut H) {
        let mut next_tick = Instant::now();
        let mut next_hook = Instant::now();
        let mut next_save = Instant::now() + self.cfg.save_every;
        while !self.shutdown.load(Ordering::SeqCst) {
            let wait = next_tick
                .min(next_hook)
                .saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait) {
                Ok(ev) => {
                    self.on_event(engine, ev);
                    // Everything already waiting is read BEFORE the clock is looked at. After a stall (this loop was
                    // busy, or the machine was) the answers to our requests are sitting in the queue; ticking first
                    // would declare those requests timed out and then find their answers unwelcome. (Bounded, so a
                    // flood cannot keep the clock from being looked at.)
                    for _ in 0..MAX_DRAIN {
                        match rx.try_recv() {
                            Ok(ev) => self.on_event(engine, ev),
                            Err(_) => break,
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if Instant::now() >= next_tick {
                next_tick = Instant::now() + self.cfg.tick;
                let actions = engine.handle(now_ms(), Event::Tick);
                self.exec(engine, actions);
            }
            if Instant::now() >= next_hook {
                next_hook = Instant::now() + self.cfg.hook_tick;
                for ev in hooks.poll(engine, now_ms()) {
                    let actions = engine.handle(now_ms(), ev);
                    self.exec(engine, actions);
                }
            }
            if Instant::now() >= next_save {
                next_save = Instant::now() + self.cfg.save_every;
                self.save_state(engine);
            }
        }
    }

    fn save_state(&self, engine: &Engine<'_>) {
        let Some(path) = &self.cfg.state_path else {
            return;
        };
        if let Err(e) = write_atomic(path, &engine.export_state()) {
            self.log(&format!("could not save {}: {e}", path.display()));
        }
    }

    fn on_event(&mut self, engine: &mut Engine<'_>, ev: Ev) {
        match ev {
            Ev::Up {
                stream,
                secured,
                addr,
                inbound,
            } => {
                let peer = self.next_peer;
                self.next_peer += 1;
                match self.start_conn(peer, stream, secured, &addr) {
                    Ok(()) => {
                        self.counters.connected.fetch_add(1, Ordering::Relaxed);
                        self.log(&format!(
                            "peer {peer} {} {addr}",
                            if inbound {
                                "connected from"
                            } else {
                                "connected to"
                            }
                        ));
                        let actions = engine.handle(
                            now_ms(),
                            Event::PeerConnected {
                                peer,
                                addr,
                                inbound,
                            },
                        );
                        self.exec(engine, actions);
                    }
                    Err(e) => {
                        self.log(&format!("could not start {addr}: {e}"));
                        if !inbound {
                            let actions = engine.handle(now_ms(), Event::ConnectFailed { addr });
                            self.exec(engine, actions);
                        }
                    }
                }
            }
            Ev::DialFailed { addr, why } => {
                self.counters.dial_failures.fetch_add(1, Ordering::Relaxed);
                self.log(&format!("could not connect to {addr}: {why}"));
                let actions = engine.handle(now_ms(), Event::ConnectFailed { addr });
                self.exec(engine, actions);
            }
            Ev::Msg { peer, msg, ticket } => {
                // (a message from a peer we have already dropped is ignored by the engine, which no longer knows it)
                let actions = engine.handle(now_ms(), Event::Message { peer, msg });
                self.exec(engine, actions);
                // the memory the frame was given is free again once the engine is done with it
                drop(ticket);
            }
            Ev::Bad { peer, why } => {
                if self.conns.contains_key(&peer) {
                    self.counters.bad_bytes.fetch_add(1, Ordering::Relaxed);
                    self.log(&format!(
                        "peer {peer} sent bytes that are not a message: {why}"
                    ));
                    let actions = engine.handle(now_ms(), Event::BadBytes { peer, why });
                    self.exec(engine, actions);
                    // the engine's ban closes it; if it somehow did not, close it here
                    if let Some(c) = self.conns.remove(&peer) {
                        c.close();
                        let actions = engine.handle(now_ms(), Event::PeerDisconnected { peer });
                        self.exec(engine, actions);
                    }
                }
            }
            Ev::Closed { peer, why } => {
                if let Some(c) = self.conns.remove(&peer) {
                    c.close();
                    self.log(&format!("peer {peer} closed: {why}"));
                    let actions = engine.handle(now_ms(), Event::PeerDisconnected { peer });
                    self.exec(engine, actions);
                }
            }
        }
    }

    /// Starts the reader and writer of a new connection.
    fn start_conn(
        &mut self,
        peer: PeerId,
        stream: TcpStream,
        secured: Secured,
        _addr: &str,
    ) -> io::Result<()> {
        let read_half = stream.try_clone()?;
        let write_half = stream.try_clone()?;
        let (wtx, wrx) = channel::<Vec<u8>>();
        let queued = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let Secured { reader, writer, .. } = secured;
        {
            let (tx, counters) = (self.tx.clone(), Arc::clone(&self.counters));
            let input = ReaderInput {
                gate: Arc::clone(&self.gate),
                lane: Lane::new(),
                closed: Arc::clone(&closed),
                throttle: Throttle::new(self.cfg.peer_bytes_per_sec, self.cfg.peer_burst_bytes),
            };
            counters.threads.fetch_add(1, Ordering::Relaxed);
            thread::spawn(move || {
                reader_thread(peer, read_half, reader, tx, &counters, input);
                counters.threads.fetch_sub(1, Ordering::Relaxed);
            });
        }
        {
            let (queued, counters, tx) = (
                Arc::clone(&queued),
                Arc::clone(&self.counters),
                self.tx.clone(),
            );
            counters.threads.fetch_add(1, Ordering::Relaxed);
            thread::spawn(move || {
                writer_thread(peer, write_half, writer, wrx, &queued, tx, &counters);
                counters.threads.fetch_sub(1, Ordering::Relaxed);
            });
        }
        self.conns.insert(
            peer,
            Conn {
                stream,
                tx: wtx,
                queued,
                closed,
            },
        );
        Ok(())
    }

    /// Carries out actions, and the actions that carrying them out causes.
    fn exec(&mut self, engine: &mut Engine<'_>, actions: Vec<Action>) {
        let mut work: VecDeque<Action> = actions.into();
        while let Some(a) = work.pop_front() {
            match a {
                Action::Send { peer, msg } => {
                    let Some(conn) = self.conns.get(&peer) else {
                        continue;
                    };
                    let frame = match encode(&msg) {
                        Ok(f) => f,
                        Err(e) => {
                            self.log(&format!("could not encode a {} message: {e}", msg.kind()));
                            continue;
                        }
                    };
                    let len = frame.len();
                    if conn.queued.load(Ordering::SeqCst) + len > self.cfg.max_queued_bytes {
                        // it is not reading what we send
                        self.counters
                            .slow_peer_drops
                            .fetch_add(1, Ordering::Relaxed);
                        self.log(&format!("peer {peer} is not reading: disconnected"));
                        if let Some(c) = self.conns.remove(&peer) {
                            c.close();
                        }
                        work.extend(engine.handle(now_ms(), Event::PeerDisconnected { peer }));
                        continue;
                    }
                    conn.queued.fetch_add(len, Ordering::SeqCst);
                    if conn.tx.send(frame).is_err() {
                        conn.queued.fetch_sub(len, Ordering::SeqCst);
                    }
                }
                Action::Disconnect { peer, reason } => {
                    if let Some(c) = self.conns.remove(&peer) {
                        self.log(&format!("disconnecting peer {peer}: {reason}"));
                        c.close();
                    }
                }
                Action::Connect { addr } => self.dial(addr),
                Action::Ban { addr, until_ms } => {
                    self.log(&format!("banned {addr}"));
                    if let Ok(mut m) = self.banned.lock() {
                        m.insert(host_of(&addr).to_string(), until_ms);
                    }
                }
            }
        }
    }

    fn dial(&self, addr: String) {
        self.counters.dials.fetch_add(1, Ordering::Relaxed);
        let (tx, key, pro) = (self.tx.clone(), self.cfg.key.clone(), self.pro.clone());
        let (connect_timeout, handshake_timeout) =
            (self.cfg.connect_timeout, self.cfg.handshake_timeout);
        thread::spawn(move || {
            let fail = |why: String| {
                let _ = tx.send(Ev::DialFailed {
                    addr: addr.clone(),
                    why,
                });
            };
            let sock: SocketAddr = match addr.parse() {
                Ok(s) => s,
                Err(_) => return fail("not an ip:port address".into()),
            };
            let stream = match TcpStream::connect_timeout(&sock, connect_timeout) {
                Ok(s) => s,
                Err(e) => return fail(e.to_string()),
            };
            let _ = stream.set_nodelay(true);
            let mut d = Deadline {
                stream,
                end: Instant::now() + handshake_timeout,
            };
            match handshake_initiator(&mut d, &key, &pro) {
                Ok(secured) => {
                    let _ = d.stream.set_read_timeout(None);
                    let _ = d.stream.set_write_timeout(None);
                    let _ = tx.send(Ev::Up {
                        stream: d.stream,
                        secured,
                        addr: addr.clone(),
                        inbound: false,
                    });
                }
                Err(e) => fail(format!("handshake: {}", describe(&e))),
            }
        });
    }
}

/// What a reader needs to keep its connection within bounds.
struct ReaderInput {
    gate: Arc<Gate>,
    lane: Arc<Lane>,
    closed: Arc<AtomicBool>,
    throttle: Throttle,
}

/// Waits `d`, in short pieces, stopping early if the connection is closed.
fn wait_unless_closed(d: Duration, closed: &AtomicBool) {
    let end = Instant::now() + d;
    while !closed.load(Ordering::SeqCst) {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        thread::sleep(left.min(Duration::from_millis(50)));
    }
}

fn reader_thread(
    peer: PeerId,
    stream: TcpStream,
    mut reader: SecureReader,
    tx: SyncSender<Ev>,
    counters: &Counters,
    mut input: ReaderInput,
) {
    let mut stream = BufReader::with_capacity(70_000, stream);
    let mut decoder = FrameDecoder::new();
    // the memory reserved for the frame being assembled (taken as soon as its size is known, before the rest of it is read)
    let mut reserved: Option<Ticket> = None;
    loop {
        let chunk_len: usize;
        match reader.read_chunk(&mut stream) {
            Ok(chunk) => {
                chunk_len = chunk.len();
                counters
                    .bytes_in
                    .fetch_add(chunk.len() as u64 + 18, Ordering::Relaxed);
                decoder.push(&chunk);
                loop {
                    if reserved.is_none() {
                        if let Some(size) = decoder.pending_frame() {
                            match input.gate.acquire(&input.lane, size, &input.closed) {
                                Some(t) => reserved = Some(t),
                                // the connection was closed while we waited
                                None => return,
                            }
                        }
                    }
                    match decoder.next_message() {
                        Ok(Some(msg)) => {
                            let ticket = reserved.take();
                            if tx.send(Ev::Msg { peer, msg, ticket }).is_err() {
                                return;
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            let _ = tx.send(Ev::Bad {
                                peer,
                                why: e.to_string(),
                            });
                            return;
                        }
                    }
                }
            }
            Err(e) => {
                let _ = tx.send(Ev::Closed {
                    peer,
                    why: e.to_string(),
                });
                return;
            }
        }
        // over its byte rate: read more slowly (the sender is slowed by TCP, and nobody is blamed)
        let wait = input.throttle.take(chunk_len, Instant::now());
        if !wait.is_zero() {
            counters
                .throttled_ms
                .fetch_add(wait.as_millis() as u64, Ordering::Relaxed);
            wait_unless_closed(wait, &input.closed);
        }
    }
}

fn writer_thread(
    peer: PeerId,
    mut stream: TcpStream,
    mut writer: SecureWriter,
    rx: Receiver<Vec<u8>>,
    queued: &AtomicUsize,
    tx: SyncSender<Ev>,
    counters: &Counters,
) {
    while let Ok(frame) = rx.recv() {
        let len = frame.len();
        let result = writer
            .seal(&frame)
            .map_err(|e| e.to_string())
            .and_then(|sealed| {
                counters
                    .bytes_out
                    .fetch_add(sealed.len() as u64, Ordering::Relaxed);
                stream.write_all(&sealed).map_err(|e| e.to_string())
            });
        queued.fetch_sub(len, Ordering::SeqCst);
        if let Err(why) = result {
            let _ = tx.send(Ev::Closed { peer, why });
            return;
        }
    }
}

/// An error in words a log reader can use: a timeout says so, rather than giving the operating system's text.
fn describe(e: &crate::noise::NoiseError) -> String {
    match e {
        crate::noise::NoiseError::Io(io)
            if matches!(
                io.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) =>
        {
            "the handshake took too long".to_string()
        }
        other => other.to_string(),
    }
}

/// Writes `bytes` to `path` by writing a file beside it and renaming it over it.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// The hosts (without ports) of these addresses, for tests that compare them.
pub fn hosts_of<'a>(addrs: impl IntoIterator<Item = &'a str>) -> HashSet<String> {
    addrs.into_iter().map(|a| host_of(a).to_string()).collect()
}
