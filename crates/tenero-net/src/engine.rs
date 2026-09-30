//! The protocol engine: one node's view of its peers, as a state machine with no I/O.
//!
//! **Events in, actions out.** A transport (the simulator today, sockets in M8.4) reports what happened with
//! [`Engine::handle`] and carries out the [`Action`]s it returns. The engine never blocks, never reads a clock
//! (every call is told the time in milliseconds) and never touches a socket, so its behaviour is a function of
//! the events it is given.
//!
//! **What it does**
//! * a handshake (`Hello`: protocol version and chain id must match, or the peer is dropped);
//! * sync: ask the best peer (most cumulative work) for the ids after our newest common block, fetch the
//!   blocks in chunks, feed them to the node, repeat until the peer has nothing more;
//! * relay: a new tip is announced by id and work to peers that do not know it, and fetched from ONE peer;
//!   transactions likewise (announce ids, fetch what is unknown);
//! * defence: every message is rate-limited; malformed or unsolicited traffic earns a score; an invalid block
//!   is an immediate ban; a peer over the threshold is disconnected and its address banned for a while;
//!   silent or slow peers are dropped (handshake, ping and request timeouts).
//!
//! **What it does not do yet:** find peers (M8.3a), the byte encoding (M8.2), sockets (M8.4).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use tenero_chain::{BlockError, Submitted};
use tenero_core::u256::U256;
use tenero_core::v2::ids::{block_id, tx_id};
use tenero_core::v2::{Block, Transaction};
use tenero_node::{AddOutcome, Node, PoolError};

use crate::addrbook::{
    group_of, host_of, peer_addr_to_string, string_to_peer_addr, AddrBook, AddrBookConfig, BanList,
};
use crate::message::{Hello, Limits, Message, PeerAddr, PROTOCOL_VERSION};

/// A handle for one connection, chosen by the transport.
pub type PeerId = u64;

#[derive(Clone, Debug)]
pub enum Event {
    PeerConnected {
        peer: PeerId,
        addr: String,
        inbound: bool,
    },
    PeerDisconnected {
        peer: PeerId,
    },
    Message {
        peer: PeerId,
        msg: Message,
    },
    /// A connection we asked for (`Action::Connect`) could not be made.
    ConnectFailed {
        addr: String,
    },
    /// Time passed: timeouts, keepalive, retries.
    Tick,
    /// This node mined a block (or was handed one locally).
    LocalBlock(Block),
    /// A transaction from this node's own wallet.
    LocalTx(Transaction),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Send {
        peer: PeerId,
        msg: Message,
    },
    Disconnect {
        peer: PeerId,
        reason: String,
    },
    /// Open an outbound connection to this address. The transport answers with `PeerConnected` (with
    /// `inbound: false` and this address) or `ConnectFailed`.
    Connect {
        addr: String,
    },
    /// Informational: the engine has banned this address until the given time and will refuse it itself.
    Ban {
        addr: String,
        until_ms: u64,
    },
}

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub limits: Limits,
    /// A peer whose score reaches this is disconnected and banned.
    pub ban_threshold: u32,
    pub ban_ms: u64,
    pub max_peers: usize,
    pub max_inbound: usize,
    pub handshake_timeout_ms: u64,
    /// Idle this long and we send a ping.
    pub ping_after_ms: u64,
    pub pong_timeout_ms: u64,
    /// A request unanswered this long counts against the peer and is retried elsewhere.
    pub request_timeout_ms: u64,
    /// Messages per second a peer may send, and the burst it may save up.
    pub msgs_per_sec: u64,
    pub burst: u64,
    /// How often a node repeats its tip announcement to peers behind it (so a lost announcement heals).
    pub reannounce_ms: u64,
    /// Consecutive unanswered requests after which a peer is dropped (not banned: slow is not hostile).
    pub max_timeouts: u32,
    /// How long a peer that failed to sync us is left alone.
    pub sync_cooldown_ms: u64,
    /// Blocks held because they are ahead of our clock.
    pub max_held: usize,
    /// The fewest outbound connections to keep (dialled by us, so a hostile peer cannot have chosen them), even
    /// when inbound peers are plentiful.
    pub outbound_target: usize,
    /// How many peers to keep in all, inbound and outbound: the node dials until it has this many.
    pub peer_target: usize,
    /// Random, chosen once per run, sent in `Hello`: it lets a node recognise a connection to itself, or a
    /// second connection to the same peer. 0 turns that off (do not do that on a real network).
    pub nonce: u64,
    /// At most this many new dials are started in one tick.
    pub max_connect_per_tick: usize,
    /// A dial unanswered this long counts as failed.
    pub connect_timeout_ms: u64,
    /// The most outbound connections to one network group (an IPv4 /16, an IPv6 /32).
    pub max_outbound_per_group: usize,
    /// Addresses to start from when the address book is empty (they are never forgotten).
    pub seeds: Vec<String>,
    /// Our own public address, if we have one: announced once to each peer, never dialled.
    pub advertise: Option<String>,
    pub addrbook: AddrBookConfig,
    /// When full, this many extra inbound connections are still accepted, only to be told addresses and sent
    /// away (so a busy seed node still helps newcomers find peers).
    pub max_addr_only: usize,
}

impl Default for EngineConfig {
    fn default() -> EngineConfig {
        EngineConfig {
            limits: Limits::default(),
            ban_threshold: 100,
            ban_ms: 24 * 3600 * 1000,
            max_peers: 128,
            max_inbound: 64,
            handshake_timeout_ms: 10_000,
            ping_after_ms: 60_000,
            pong_timeout_ms: 30_000,
            request_timeout_ms: 30_000,
            msgs_per_sec: 50,
            burst: 200,
            reannounce_ms: 15_000,
            max_timeouts: 5,
            sync_cooldown_ms: 60_000,
            max_held: 64,
            outbound_target: 8,
            peer_target: 50,
            nonce: 0,
            max_connect_per_tick: 8,
            connect_timeout_ms: 10_000,
            max_outbound_per_group: 2,
            seeds: Vec::new(),
            advertise: None,
            addrbook: AddrBookConfig::default(),
            max_addr_only: 16,
        }
    }
}

/// Counters, for tests and for the operator.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub sent: BTreeMap<&'static str, u64>,
    pub received: BTreeMap<&'static str, u64>,
    pub bans: u64,
    pub disconnects: u64,
    pub blocks_applied: u64,
}

struct Peer {
    addr: String,
    inbound: bool,
    connected_at: u64,
    /// Set by the peer's `Hello`; a peer is "ready" once it has one.
    hello: Option<Hello>,
    tip_height: u64,
    work: U256,
    score: u32,
    timeouts: u32,
    last_recv: u64,
    ping_sent: Option<u64>,
    milli_tokens: u64,
    last_refill: u64,
    known_blocks: HashSet<[u8; 32]>,
    known_txs: HashSet<[u8; 32]>,
    /// We sent `GetAddrs` and have not had the answer.
    asked_addrs: bool,
    /// We have answered its `GetAddrs` (once per connection).
    answered_addrs: bool,
    /// It has announced its own address (once per connection).
    self_announced: bool,
    /// Accepted over our limit, only to be given addresses: it may say hello and ask for addresses, and is then
    /// sent away. It never becomes a real peer.
    addr_only: bool,
    /// Its handshake was accepted (an address-only peer never gets `hello`).
    greeted: bool,
}

enum Phase {
    /// Waiting for `BlockIds`.
    Ids,
    /// Waiting for these blocks; `remaining` are the ids still to ask for.
    Blocks {
        requested: HashSet<[u8; 32]>,
        remaining: VecDeque<[u8; 32]>,
    },
}

struct Sync {
    peer: PeerId,
    started: u64,
    phase: Phase,
}

struct Req {
    peer: PeerId,
    at: u64,
}

enum Applied {
    /// It is (or became) part of the chain and moved the tip.
    NewTip,
    /// Known, or kept on a side branch.
    Kept,
    Orphan,
    NotYet,
    Invalid,
    /// Our own failure (storage): says nothing about the peer.
    Ours,
}

pub struct Engine<'a> {
    node: Node<'a>,
    cfg: EngineConfig,
    now: u64,
    peers: BTreeMap<PeerId, Peer>,
    bans: BanList,
    book: AddrBook,
    /// Addresses we have asked the transport to dial, and when.
    connecting: BTreeMap<String, u64>,
    syncing: Option<Sync>,
    cooldown: HashMap<PeerId, u64>,
    req_blocks: BTreeMap<[u8; 32], Req>,
    /// Every (peer, block id) we have asked for and not yet had an answer to: a block that arrives from a
    /// peer we asked is never "unsolicited", even if another peer delivered it first.
    asked: BTreeMap<(PeerId, [u8; 32]), u64>,
    announcers: HashMap<[u8; 32], Vec<PeerId>>,
    req_txs: BTreeMap<[u8; 32], Req>,
    rejected_txs: HashSet<[u8; 32]>,
    held: Vec<Block>,
    last_reannounce: u64,
    pub stats: Stats,
}

impl<'a> Engine<'a> {
    pub fn new(node: Node<'a>, cfg: EngineConfig) -> Engine<'a> {
        let mut book = AddrBook::new(cfg.addrbook.clone());
        for seed in &cfg.seeds {
            book.add(seed, 0, "seed", 0);
        }
        Engine {
            node,
            cfg,
            now: 0,
            peers: BTreeMap::new(),
            bans: BanList::new(),
            book,
            connecting: BTreeMap::new(),
            syncing: None,
            cooldown: HashMap::new(),
            req_blocks: BTreeMap::new(),
            asked: BTreeMap::new(),
            announcers: HashMap::new(),
            req_txs: BTreeMap::new(),
            rejected_txs: HashSet::new(),
            held: Vec::new(),
            last_reannounce: 0,
            stats: Stats::default(),
        }
    }

    pub fn node(&self) -> &Node<'a> {
        &self.node
    }

    /// Real peers (address-only visitors are not counted).
    pub fn peer_count(&self) -> usize {
        self.peers.values().filter(|p| !p.addr_only).count()
    }

    /// Strangers accepted over our limit only to be given addresses.
    pub fn addr_only_count(&self) -> usize {
        self.peers.values().filter(|p| p.addr_only).count()
    }

    pub fn ready_peer_count(&self) -> usize {
        self.peers.values().filter(|p| p.hello.is_some()).count()
    }

    pub fn is_banned(&self, addr: &str, now_ms: u64) -> bool {
        self.bans.is_banned(addr, now_ms)
    }

    pub fn is_syncing(&self) -> bool {
        self.syncing.is_some()
    }

    /// A one-line description of what the engine is waiting for, for logs and for debugging.
    pub fn debug_summary(&self) -> String {
        let (h, _, w) = self.tip();
        let peers: Vec<String> = self
            .peers
            .iter()
            .map(|(id, p)| {
                format!(
                    "{id}:{}{}",
                    if p.hello.is_some() { "r" } else { "-" },
                    p.tip_height
                )
            })
            .collect();
        format!(
            "tip {h} work {} syncing {:?} req_blocks {} asked {} announcers {} peers [{}]",
            w.to_dec_string(),
            self.syncing
                .as_ref()
                .map(|s| (s.peer, matches!(s.phase, Phase::Ids))),
            self.req_blocks.len(),
            self.asked.len(),
            self.announcers.len(),
            peers.join(" ")
        )
    }

    pub fn has_peer(&self, peer: PeerId) -> bool {
        self.peers.contains_key(&peer)
    }

    pub fn peer_score(&self, peer: PeerId) -> Option<u32> {
        self.peers.get(&peer).map(|p| p.score)
    }

    /// The single entry point. `now_ms` is the transport's clock (Unix milliseconds).
    pub fn handle(&mut self, now_ms: u64, ev: Event) -> Vec<Action> {
        self.now = now_ms;
        let mut out = Vec::new();
        match ev {
            Event::PeerConnected {
                peer,
                addr,
                inbound,
            } => self.on_connected(peer, addr, inbound, &mut out),
            Event::PeerDisconnected { peer } => {
                if let Some(p) = self.peers.remove(&peer) {
                    // an outbound connection that never got as far as a handshake counts against its address
                    if !p.inbound && p.hello.is_none() {
                        self.book.mark_failure(&p.addr, now_ms);
                    }
                }
                self.cleanup_peer(peer, &mut out);
            }
            Event::ConnectFailed { addr } => {
                if self.connecting.remove(&addr).is_some() {
                    self.book.mark_failure(&addr, now_ms);
                }
            }
            Event::Message { peer, msg } => self.on_message(peer, msg, &mut out),
            Event::Tick => self.on_tick(&mut out),
            Event::LocalBlock(b) => {
                if let Applied::NewTip = self.apply_block(None, &b, &mut out) {
                    self.announce_tip(None, &mut out);
                }
            }
            Event::LocalTx(t) => {
                if let Ok(AddOutcome::Added { id, .. }) = self.node.submit_tx(t) {
                    self.announce_tx(id, &mut out);
                }
            }
        }
        out
    }

    // ---------------------------------------------------------------------------------------------
    // helpers

    fn send(&mut self, peer: PeerId, msg: Message, out: &mut Vec<Action>) {
        if let Message::GetBlocks { ids } = &msg {
            for id in ids {
                self.asked.insert((peer, *id), self.now);
            }
        }
        *self.stats.sent.entry(msg.kind()).or_default() += 1;
        out.push(Action::Send { peer, msg });
    }

    fn secs(&self) -> u64 {
        self.now / 1000
    }

    fn tip(&self) -> (u64, [u8; 32], U256) {
        let (h, i) = self.node.store().tip().expect("the store has a tip");
        (h, i.block_id, U256::from_be_bytes(&i.cumulative_work))
    }

    fn our_hello(&self) -> Hello {
        let (h, i) = self.node.store().tip().expect("the store has a tip");
        Hello {
            version: PROTOCOL_VERSION,
            chain_id: self.node.store().chain_id(),
            tip_height: h,
            cumulative_work: i.cumulative_work,
            tip_id: i.block_id,
            pruned_below: self.node.store().pruned_below().unwrap_or(0),
            nonce: self.cfg.nonce,
        }
    }

    fn on_chain(&self, id: &[u8; 32]) -> bool {
        matches!(self.node.store().height_of(id), Ok(Some(_)))
    }

    fn block_id_of(&self, b: &Block) -> [u8; 32] {
        block_id(&b.header, self.node.store().pow())
    }

    fn penalize(&mut self, peer: PeerId, points: u32, why: &str, out: &mut Vec<Action>) {
        let Some(p) = self.peers.get_mut(&peer) else {
            return;
        };
        p.score = p.score.saturating_add(points);
        if p.score >= self.cfg.ban_threshold {
            self.drop_peer(peer, why, true, out);
        }
    }

    fn drop_peer(&mut self, peer: PeerId, why: &str, ban: bool, out: &mut Vec<Action>) {
        if let Some(p) = self.peers.remove(&peer) {
            if !p.inbound && !ban && p.hello.is_none() {
                self.book.mark_failure(&p.addr, self.now);
            }
            if ban {
                let until = self.now + self.cfg.ban_ms;
                self.bans.ban(&p.addr, until);
                if !p.inbound {
                    self.book.remove(&p.addr);
                }
                self.stats.bans += 1;
                out.push(Action::Ban {
                    addr: p.addr,
                    until_ms: until,
                });
            }
            self.stats.disconnects += 1;
            out.push(Action::Disconnect {
                peer,
                reason: why.to_string(),
            });
        }
        self.cleanup_peer(peer, out);
    }

    /// Forgets everything that waited on `peer`, and looks for another way to get it.
    fn cleanup_peer(&mut self, peer: PeerId, out: &mut Vec<Action>) {
        if self.syncing.as_ref().is_some_and(|s| s.peer == peer) {
            self.syncing = None;
        }
        let lost: Vec<[u8; 32]> = self
            .req_blocks
            .iter()
            .filter(|(_, r)| r.peer == peer)
            .map(|(id, _)| *id)
            .collect();
        for id in lost {
            self.req_blocks.remove(&id);
            self.fetch_from_announcers(id, out);
        }
        self.req_txs.retain(|_, r| r.peer != peer);
        self.asked.retain(|(p, _), _| *p != peer);
        for list in self.announcers.values_mut() {
            list.retain(|p| *p != peer);
        }
        self.maybe_start_sync(out);
    }

    /// Asks the next connected announcer of block `id`, if there is one and we still need it.
    fn fetch_from_announcers(&mut self, id: [u8; 32], out: &mut Vec<Action>) {
        if self.on_chain(&id) || self.req_blocks.contains_key(&id) {
            return;
        }
        let candidate = self
            .announcers
            .get(&id)
            .and_then(|l| l.iter().copied().find(|p| self.peers.contains_key(p)));
        if let Some(peer) = candidate {
            self.req_blocks.insert(id, Req { peer, at: self.now });
            self.send(peer, Message::GetBlocks { ids: vec![id] }, out);
        }
    }

    // ---------------------------------------------------------------------------------------------
    // connections

    fn on_connected(&mut self, peer: PeerId, addr: String, inbound: bool, out: &mut Vec<Action>) {
        if !inbound {
            self.connecting.remove(&addr);
        }
        if self.is_banned(&addr, self.now) {
            out.push(Action::Disconnect {
                peer,
                reason: "banned".into(),
            });
            return;
        }
        let regular = self.peers.values().filter(|p| !p.addr_only);
        let inbound_now = regular.clone().filter(|p| p.inbound).count();
        let mut addr_only = false;
        if regular.count() >= self.cfg.max_peers || (inbound && inbound_now >= self.cfg.max_inbound)
        {
            let extra = self.peers.values().filter(|p| p.addr_only).count();
            if inbound && extra < self.cfg.max_addr_only {
                addr_only = true;
            } else {
                out.push(Action::Disconnect {
                    peer,
                    reason: "full".into(),
                });
                return;
            }
        }
        self.peers.insert(
            peer,
            Peer {
                addr,
                inbound,
                connected_at: self.now,
                hello: None,
                tip_height: 0,
                work: U256::from_be_bytes(&[0; 32]),
                score: 0,
                timeouts: 0,
                last_recv: self.now,
                ping_sent: None,
                milli_tokens: self.cfg.burst * 1000,
                last_refill: self.now,
                known_blocks: HashSet::new(),
                known_txs: HashSet::new(),
                asked_addrs: false,
                answered_addrs: false,
                self_announced: false,
                addr_only,
                greeted: false,
            },
        );
        let hello = self.our_hello();
        self.send(peer, Message::Hello(hello), out);
    }

    fn on_hello(&mut self, peer: PeerId, h: Hello, out: &mut Vec<Action>) {
        if self.peers.get(&peer).is_some_and(|p| p.hello.is_some()) {
            self.penalize(peer, 50, "second hello", out);
            return;
        }
        if h.version != PROTOCOL_VERSION {
            self.drop_peer(peer, "protocol version mismatch", false, out);
            return;
        }
        if h.chain_id != self.node.store().chain_id() {
            // a different chain: no use to us, and cheap to refuse for good
            self.drop_peer(peer, "different chain", true, out);
            return;
        }
        if !self.resolve_duplicate(peer, h.nonce, out) {
            return;
        }
        if let Some(p) = self.peers.get_mut(&peer) {
            p.tip_height = h.tip_height;
            p.work = U256::from_be_bytes(&h.cumulative_work);
            p.known_blocks.insert(h.tip_id);
            p.hello = Some(h);
        }
        self.on_ready(peer, out);
        self.maybe_start_sync(out);
    }

    /// The peer at `peer` says its nonce is `nonce`. If that is our own, this is a connection to ourselves; if
    /// another connection shows the same nonce, it is a second link to the same node. Either way exactly one
    /// link survives, and BOTH ends choose the same one: the link dialled by the node with the smaller nonce
    /// (the first one, if both were dialled by the same side). Returns whether `peer` is still connected.
    fn resolve_duplicate(&mut self, peer: PeerId, nonce: u64, out: &mut Vec<Action>) -> bool {
        let ours = self.cfg.nonce;
        if ours == 0 || nonce == 0 {
            return true;
        }
        if nonce == ours {
            self.drop_peer(peer, "connected to ourselves", false, out);
            return false;
        }
        let other = self
            .peers
            .iter()
            .find(|(id, p)| **id != peer && p.hello.as_ref().is_some_and(|h| h.nonce == nonce))
            .map(|(id, p)| (*id, p.inbound));
        let Some((other_id, other_inbound)) = other else {
            return true;
        };
        let new_inbound = self.peers.get(&peer).is_some_and(|p| p.inbound);
        // the nonce of whoever dialled each link
        let dialler = |inbound: bool| if inbound { nonce } else { ours };
        let keep_new = dialler(new_inbound) < dialler(other_inbound);
        if keep_new {
            self.drop_peer(other_id, "duplicate connection", false, out);
            true
        } else {
            self.drop_peer(peer, "duplicate connection", false, out);
            false
        }
    }

    /// The handshake is done: an outbound address has proved itself, and addresses are exchanged.
    fn on_ready(&mut self, peer: PeerId, out: &mut Vec<Action>) {
        let (addr, inbound) = match self.peers.get(&peer) {
            Some(p) => (p.addr.clone(), p.inbound),
            None => return,
        };
        if !inbound {
            self.book.mark_success(&addr, self.secs());
            if let Some(p) = self.peers.get_mut(&peer) {
                p.asked_addrs = true;
            }
            self.send(peer, Message::GetAddrs, out);
        }
        if let Some(own) = self.cfg.advertise.clone() {
            if let Some(a) = string_to_peer_addr(&own, self.secs()) {
                self.send(peer, Message::Addrs { addrs: vec![a] }, out);
            }
        }
    }

    // ---------------------------------------------------------------------------------------------
    // messages

    fn on_message(&mut self, peer: PeerId, msg: Message, out: &mut Vec<Action>) {
        let now = self.now;
        let ready = {
            let Some(p) = self.peers.get_mut(&peer) else {
                return;
            };
            let elapsed = now.saturating_sub(p.last_refill);
            p.last_refill = now;
            p.milli_tokens =
                (p.milli_tokens + elapsed * self.cfg.msgs_per_sec).min(self.cfg.burst * 1000);
            if p.milli_tokens < 1000 {
                None
            } else {
                p.milli_tokens -= 1000;
                p.last_recv = now;
                if matches!(
                    msg,
                    Message::Blocks { .. }
                        | Message::BlockIds { .. }
                        | Message::Txs { .. }
                        | Message::Pong(_)
                        | Message::NotFound { .. }
                ) {
                    p.timeouts = 0; // it answers
                }
                Some(p.hello.is_some())
            }
        };
        let Some(ready) = ready else {
            self.penalize(peer, 20, "too many messages", out);
            return;
        };
        if self.peers.get(&peer).is_some_and(|p| p.addr_only) {
            self.on_addr_only_message(peer, msg, out);
            return;
        }
        *self.stats.received.entry(msg.kind()).or_default() += 1;
        let lim = self.cfg.limits.clone();
        match msg {
            Message::Hello(h) => self.on_hello(peer, h, out),
            _ if !ready => self.penalize(peer, 50, "message before hello", out),
            Message::Ping(n) => self.send(peer, Message::Pong(n), out),
            Message::Pong(n) => {
                let matched = self
                    .peers
                    .get_mut(&peer)
                    .is_some_and(|p| p.ping_sent.take() == Some(n));
                if !matched {
                    self.penalize(peer, 10, "unsolicited pong", out);
                }
            }
            Message::GetBlockIds { locator } => {
                if locator.is_empty() || locator.len() > lim.max_locator {
                    self.penalize(peer, 50, "bad locator", out);
                } else {
                    self.on_get_block_ids(peer, locator, out);
                }
            }
            Message::BlockIds { first_height, ids } => {
                if ids.len() > lim.max_ids {
                    self.penalize(peer, 50, "too many ids", out);
                } else {
                    self.on_block_ids(peer, first_height, ids, out);
                }
            }
            Message::GetBlocks { ids } => {
                if ids.is_empty() || ids.len() > lim.max_blocks {
                    self.penalize(peer, 50, "bad block request", out);
                } else {
                    self.on_get_blocks(peer, ids, out);
                }
            }
            Message::Blocks { blocks } => {
                if blocks.len() > lim.max_blocks {
                    self.penalize(peer, 50, "too many blocks", out);
                } else {
                    self.on_blocks(peer, blocks, out);
                }
            }
            Message::NotFound { ids } => self.on_not_found(peer, ids, out),
            Message::NewBlock {
                id,
                height,
                cumulative_work,
            } => self.on_new_block(peer, id, height, U256::from_be_bytes(&cumulative_work), out),
            Message::NewTx { ids } => {
                if ids.is_empty() || ids.len() > lim.max_txs {
                    self.penalize(peer, 50, "bad transaction announcement", out);
                } else {
                    self.on_new_tx(peer, ids, out);
                }
            }
            Message::GetTxs { ids } => {
                if ids.is_empty() || ids.len() > lim.max_txs {
                    self.penalize(peer, 50, "bad transaction request", out);
                } else {
                    self.on_get_txs(peer, ids, out);
                }
            }
            Message::GetAddrs => self.on_get_addrs(peer, out),
            Message::Addrs { addrs } => {
                if addrs.len() > lim.max_addrs {
                    self.penalize(peer, 50, "too many addresses", out);
                } else {
                    self.on_addrs(peer, addrs, out);
                }
            }
            Message::Txs { txs } => {
                if txs.len() > lim.max_txs {
                    self.penalize(peer, 50, "too many transactions", out);
                } else {
                    self.on_txs(peer, txs, out);
                }
            }
        }
    }

    // ---- serving ----------------------------------------------------------------------------------

    fn on_get_block_ids(&mut self, peer: PeerId, locator: Vec<[u8; 32]>, out: &mut Vec<Action>) {
        let store = self.node.store();
        let common = locator
            .iter()
            .find_map(|id| store.height_of(id).ok().flatten());
        let Some(common) = common else {
            // not even the genesis block: this peer is not on our chain
            self.penalize(peer, 20, "locator shares nothing with our chain", out);
            self.send(
                peer,
                Message::BlockIds {
                    first_height: 1,
                    ids: vec![],
                },
                out,
            );
            return;
        };
        let (tip_h, _, _) = self.tip();
        let last = tip_h.min(common + self.cfg.limits.max_ids as u64);
        let ids: Vec<[u8; 32]> = ((common + 1)..=last)
            .filter_map(|h| self.node.store().block_index(h).ok().flatten())
            .map(|i| i.block_id)
            .collect();
        self.send(
            peer,
            Message::BlockIds {
                first_height: common + 1,
                ids,
            },
            out,
        );
    }

    fn on_get_blocks(&mut self, peer: PeerId, ids: Vec<[u8; 32]>, out: &mut Vec<Action>) {
        let mut blocks = Vec::new();
        let mut missing = Vec::new();
        for id in ids {
            let full = self
                .node
                .store()
                .height_of(&id)
                .ok()
                .flatten()
                .and_then(|h| self.node.store().get_block(h).ok().flatten())
                .and_then(|sb| sb.into_full());
            match full {
                Some(b) => blocks.push(b),
                None => missing.push(id),
            }
        }
        // a peer we served a block to has it (and its work): remember, so we stop announcing it to that peer
        let served: Vec<[u8; 32]> = blocks.iter().map(|b| self.block_id_of(b)).collect();
        for id in served {
            self.note_peer_has(peer, id);
        }
        if !blocks.is_empty() {
            self.send(peer, Message::Blocks { blocks }, out);
        }
        if !missing.is_empty() {
            self.send(peer, Message::NotFound { ids: missing }, out);
        }
    }

    fn note_peer_has(&mut self, peer: PeerId, id: [u8; 32]) {
        let info = self
            .node
            .store()
            .height_of(&id)
            .ok()
            .flatten()
            .and_then(|h| {
                self.node
                    .store()
                    .block_index(h)
                    .ok()
                    .flatten()
                    .map(|i| (h, U256::from_be_bytes(&i.cumulative_work)))
            });
        if let (Some((height, work)), Some(p)) = (info, self.peers.get_mut(&peer)) {
            p.known_blocks.insert(id);
            if work > p.work {
                p.work = work;
                p.tip_height = height;
            }
        }
    }

    fn on_get_txs(&mut self, peer: PeerId, ids: Vec<[u8; 32]>, out: &mut Vec<Action>) {
        let mut txs = Vec::new();
        let mut missing = Vec::new();
        for id in ids {
            match self.node.pool().get(&id) {
                Some(t) => txs.push(t.clone()),
                None => missing.push(id),
            }
        }
        if !txs.is_empty() {
            self.send(peer, Message::Txs { txs }, out);
        }
        if !missing.is_empty() {
            self.send(peer, Message::NotFound { ids: missing }, out);
        }
    }

    // ---- addresses and connections -----------------------------------------------------------------

    /// The whole protocol an address-only peer gets: say hello, ask for addresses, be answered and sent away.
    /// Anything else, or asking before saying hello, and it is sent away at once (not scored: it is a stranger we
    /// had no room for, not necessarily hostile).
    fn on_addr_only_message(&mut self, peer: PeerId, msg: Message, out: &mut Vec<Action>) {
        *self.stats.received.entry(msg.kind()).or_default() += 1;
        match msg {
            Message::Hello(h) => {
                if h.version != PROTOCOL_VERSION {
                    self.drop_peer(peer, "protocol version mismatch", false, out);
                } else if h.chain_id != self.node.store().chain_id() {
                    self.drop_peer(peer, "different chain", true, out);
                } else if let Some(p) = self.peers.get_mut(&peer) {
                    p.greeted = true;
                }
            }
            Message::GetAddrs => {
                if !self.peers.get(&peer).is_some_and(|p| p.greeted) {
                    self.drop_peer(peer, "busy", false, out);
                    return;
                }
                let n = self.cfg.limits.max_addrs;
                let addrs = self.book.sample(n, self.secs());
                self.send(peer, Message::Addrs { addrs }, out);
                self.drop_peer(peer, "busy: addresses sent", false, out);
            }
            Message::Ping(n) => self.send(peer, Message::Pong(n), out),
            Message::Pong(_) => {}
            _ => self.drop_peer(peer, "busy", false, out),
        }
    }

    fn on_get_addrs(&mut self, peer: PeerId, out: &mut Vec<Action>) {
        let again = match self.peers.get_mut(&peer) {
            Some(p) => std::mem::replace(&mut p.answered_addrs, true),
            None => return,
        };
        if again {
            self.penalize(peer, 10, "asked for addresses twice", out);
            return;
        }
        let n = self.cfg.limits.max_addrs;
        let addrs = self.book.sample(n, self.secs());
        self.send(peer, Message::Addrs { addrs }, out);
    }

    fn on_addrs(&mut self, peer: PeerId, addrs: Vec<PeerAddr>, out: &mut Vec<Action>) {
        let secs = self.secs();
        let (peer_addr, asked, may_announce) = match self.peers.get(&peer) {
            Some(p) => (p.addr.clone(), p.asked_addrs, !p.self_announced),
            None => return,
        };
        let source = group_of(&peer_addr);
        // a peer telling us its own address (once): one entry, at the host it connected from
        if addrs.len() == 1 && may_announce {
            let own = peer_addr_to_string(&addrs[0]);
            if own.as_deref().map(host_of) == Some(host_of(&peer_addr)) {
                if let Some(p) = self.peers.get_mut(&peer) {
                    p.self_announced = true;
                }
                if let Some(a) = own {
                    self.book.add(&a, secs, &source, secs);
                }
                return;
            }
        }
        if asked {
            if let Some(p) = self.peers.get_mut(&peer) {
                p.asked_addrs = false; // one answer to each request
            }
            for a in &addrs {
                if let Some(text) = peer_addr_to_string(a) {
                    self.book.add(&text, a.last_seen, &source, secs);
                }
            }
        } else {
            self.penalize(peer, 20, "addresses nobody asked for", out);
        }
    }

    /// Keeps the outbound connections at their target: dials known addresses, never two in one network group
    /// beyond the limit, never a host we are already connected to, never a banned or backed-off address.
    fn maintain_connections(&mut self, out: &mut Vec<Action>) {
        let now = self.now;
        let late: Vec<String> = self
            .connecting
            .iter()
            .filter(|(_, at)| now.saturating_sub(**at) > self.cfg.connect_timeout_ms)
            .map(|(a, _)| a.clone())
            .collect();
        for a in late {
            self.connecting.remove(&a);
            self.book.mark_failure(&a, now);
        }
        self.bans.expire(now);
        self.book.expire(self.secs());

        let regular = self.peers.values().filter(|p| !p.addr_only);
        let total = regular.clone().count() + self.connecting.len();
        let outbound = regular.filter(|p| !p.inbound).count() + self.connecting.len();
        let want = self
            .cfg
            .outbound_target
            .saturating_sub(outbound)
            .max(self.cfg.peer_target.saturating_sub(total))
            .min(self.cfg.max_connect_per_tick);
        if want == 0 {
            return;
        }
        // a host we are already connected to (in either direction) is not dialled again
        let hosts: HashSet<String> = self.peers.values().map(|p| host_of(&p.addr)).collect();
        let mut groups: HashMap<String, usize> = HashMap::new();
        for p in self.peers.values().filter(|p| !p.inbound) {
            *groups.entry(group_of(&p.addr)).or_default() += 1;
        }
        for a in self.connecting.keys() {
            *groups.entry(group_of(a)).or_default() += 1;
        }
        let per_group = self.cfg.max_outbound_per_group;
        let own = self.cfg.advertise.clone();
        let bans = &self.bans;
        let connecting = &self.connecting;
        let skip = |a: &str| {
            hosts.contains(&host_of(a))
                || connecting.contains_key(a)
                || bans.is_banned(a, now)
                || own.as_deref() == Some(a)
        };
        let full = |g: &str| groups.get(g).copied().unwrap_or(0) >= per_group;
        let candidates = self.book.candidates(now, want * 4, &skip, &full);
        let mut chosen_hosts: HashSet<String> = HashSet::new();
        let mut dialled = 0;
        for a in candidates {
            if dialled >= want {
                break;
            }
            let g = group_of(&a);
            let count = groups.entry(g).or_default();
            if *count >= per_group || !chosen_hosts.insert(host_of(&a)) {
                continue;
            }
            *count += 1;
            self.book.mark_attempt(&a, now);
            self.connecting.insert(a.clone(), now);
            out.push(Action::Connect { addr: a });
            dialled += 1;
        }
    }

    /// The address book and the ban list, for saving. `import_state` restores them.
    pub fn export_state(&self) -> Vec<u8> {
        let book = self.book.to_bytes();
        let bans = self.bans.to_bytes();
        let mut out = b"TNS1".to_vec();
        out.extend_from_slice(&(book.len() as u32).to_le_bytes());
        out.extend_from_slice(&book);
        out.extend_from_slice(&(bans.len() as u32).to_le_bytes());
        out.extend_from_slice(&bans);
        out
    }

    /// Loads what `export_state` saved. On any damage nothing is changed and the caller carries on with the
    /// seeds alone.
    pub fn import_state(&mut self, data: &[u8]) -> Result<(), String> {
        if data.len() < 12 || &data[..4] != b"TNS1" {
            return Err("not a saved state".into());
        }
        let mut pos = 4;
        let part = |pos: &mut usize| -> Result<&[u8], String> {
            if data.len() < *pos + 4 {
                return Err("truncated".into());
            }
            let n = u32::from_le_bytes(data[*pos..*pos + 4].try_into().unwrap()) as usize;
            *pos += 4;
            if data.len() < *pos + n {
                return Err("truncated".into());
            }
            let s = &data[*pos..*pos + n];
            *pos += n;
            Ok(s)
        };
        let book_bytes = part(&mut pos)?;
        let ban_bytes = part(&mut pos)?;
        if pos != data.len() {
            return Err("trailing bytes".into());
        }
        let mut book = AddrBook::from_bytes(self.cfg.addrbook.clone(), book_bytes)?;
        let bans = BanList::from_bytes(ban_bytes)?;
        for seed in &self.cfg.seeds {
            book.add(seed, 0, "seed", 0);
        }
        self.book = book;
        self.bans = bans;
        Ok(())
    }

    pub fn addr_book(&self) -> &AddrBook {
        &self.book
    }

    /// The addresses we dialled that are connected now.
    pub fn outbound_addrs(&self) -> Vec<String> {
        self.peers
            .values()
            .filter(|p| !p.inbound)
            .map(|p| p.addr.clone())
            .collect()
    }

    pub fn outbound_count(&self) -> usize {
        self.peers.values().filter(|p| !p.inbound).count()
    }

    pub fn inbound_count(&self) -> usize {
        self.peers.values().filter(|p| p.inbound).count()
    }

    pub fn connecting_count(&self) -> usize {
        self.connecting.len()
    }

    // ---- sync -------------------------------------------------------------------------------------

    fn locator(&self) -> Vec<[u8; 32]> {
        let store = self.node.store();
        let (tip_h, _, _) = self.tip();
        let mut ids = Vec::new();
        let (mut h, mut step) = (tip_h, 1u64);
        loop {
            if let Ok(Some(i)) = store.block_index(h) {
                ids.push(i.block_id);
            }
            if h == 0 {
                break;
            }
            if ids.len() >= 10 {
                step *= 2;
            }
            h = h.saturating_sub(step);
        }
        ids
    }

    /// A request went unanswered. Slow is not hostile: the peer is not scored, but a peer that lets
    /// `max_timeouts` requests in a row go unanswered is dropped (any answer resets the count).
    fn note_timeout(&mut self, peer: PeerId, out: &mut Vec<Action>) {
        let over = match self.peers.get_mut(&peer) {
            Some(p) => {
                p.timeouts += 1;
                p.timeouts >= self.cfg.max_timeouts
            }
            None => false,
        };
        if over {
            self.drop_peer(peer, "too many unanswered requests", false, out);
        }
    }

    fn maybe_start_sync(&mut self, out: &mut Vec<Action>) {
        // one download at a time: while a single-block fetch is in flight, a full sync would fetch it twice
        if self.syncing.is_some() || !self.req_blocks.is_empty() {
            return;
        }
        let (_, _, ours) = self.tip();
        let now = self.now;
        let best = self
            .peers
            .iter()
            .filter(|(id, p)| {
                p.hello.is_some()
                    && p.work > ours
                    && self.cooldown.get(id).is_none_or(|&until| until <= now)
            })
            .max_by(|a, b| a.1.work.cmp(&b.1.work).then(b.0.cmp(a.0)))
            .map(|(id, _)| *id);
        if let Some(peer) = best {
            self.start_sync(peer, out);
        }
    }

    fn start_sync(&mut self, peer: PeerId, out: &mut Vec<Action>) {
        self.syncing = Some(Sync {
            peer,
            started: self.now,
            phase: Phase::Ids,
        });
        let locator = self.locator();
        self.send(peer, Message::GetBlockIds { locator }, out);
    }

    fn abort_sync(&mut self, peer: PeerId, out: &mut Vec<Action>) {
        if self.syncing.as_ref().is_some_and(|s| s.peer == peer) {
            self.syncing = None;
            self.cooldown
                .insert(peer, self.now + self.cfg.sync_cooldown_ms);
        }
        self.maybe_start_sync(out);
    }

    fn on_block_ids(
        &mut self,
        peer: PeerId,
        first_height: u64,
        ids: Vec<[u8; 32]>,
        out: &mut Vec<Action>,
    ) {
        let expecting = self
            .syncing
            .as_ref()
            .is_some_and(|s| s.peer == peer && matches!(s.phase, Phase::Ids));
        if !expecting {
            self.penalize(peer, 20, "unsolicited block ids", out);
            return;
        }
        if first_height == 0 {
            self.penalize(peer, 50, "block ids start at the genesis block", out);
            return;
        }
        let mut wanted: VecDeque<[u8; 32]> = VecDeque::new();
        for id in ids {
            if self.node.chain().is_known_invalid(&id) {
                self.penalize(peer, 50, "offered a block known to be invalid", out);
                return;
            }
            if !self.on_chain(&id) && !self.node.chain().in_side_pool(&id) {
                wanted.push_back(id);
            }
        }
        if wanted.is_empty() {
            // nothing new: this peer has told us all it has
            let (_, _, ours) = self.tip();
            let claimed_more = self.peers.get(&peer).is_some_and(|p| p.work > ours);
            self.syncing = None;
            if claimed_more {
                if let Some(p) = self.peers.get_mut(&peer) {
                    // do not believe the claim again until it announces something new
                    p.work = ours;
                }
                self.penalize(peer, 10, "claimed more work than it has", out);
            }
            self.maybe_start_sync(out);
            return;
        }
        self.request_next_chunk(peer, wanted, out);
    }

    /// Asks for the next chunk of `remaining`, or, if nothing remains, for the next batch of ids.
    fn request_next_chunk(
        &mut self,
        peer: PeerId,
        mut remaining: VecDeque<[u8; 32]>,
        out: &mut Vec<Action>,
    ) {
        if remaining.is_empty() {
            let locator = self.locator();
            if let Some(s) = self.syncing.as_mut() {
                s.phase = Phase::Ids;
                s.started = self.now;
            }
            self.send(peer, Message::GetBlockIds { locator }, out);
            return;
        }
        let n = remaining.len().min(self.cfg.limits.max_blocks);
        let chunk: Vec<[u8; 32]> = remaining.drain(..n).collect();
        if let Some(s) = self.syncing.as_mut() {
            s.phase = Phase::Blocks {
                requested: chunk.iter().copied().collect(),
                remaining,
            };
            s.started = self.now;
        }
        self.send(peer, Message::GetBlocks { ids: chunk }, out);
    }

    fn on_blocks(&mut self, peer: PeerId, blocks: Vec<Block>, out: &mut Vec<Action>) {
        let mut new_tip = false;
        let mut orphan = false;
        for b in blocks {
            let id = self.block_id_of(&b);
            // a block is welcome from any peer we asked for it, even if another peer delivered it first
            if self.asked.remove(&(peer, id)).is_none() {
                self.penalize(peer, 20, "a block nobody asked for", out);
                if !self.peers.contains_key(&peer) {
                    return;
                }
                continue;
            }
            self.req_blocks.remove(&id);
            if let Some(Sync {
                peer: sync_peer,
                phase: Phase::Blocks { requested, .. },
                ..
            }) = self.syncing.as_mut()
            {
                if *sync_peer == peer {
                    requested.remove(&id);
                }
            }
            match self.apply_block(Some(peer), &b, out) {
                Applied::NewTip => new_tip = true,
                Applied::Orphan => orphan = true,
                Applied::Invalid => return, // the peer was banned, and its requests forgotten
                _ => {}
            }
        }
        if new_tip {
            self.announce_tip(Some(peer), out);
        }
        // sync progress
        let progress = match self.syncing.as_ref() {
            Some(Sync {
                peer: p,
                phase:
                    Phase::Blocks {
                        requested,
                        remaining,
                    },
                ..
            }) if *p == peer && requested.is_empty() => Some(remaining.clone()),
            _ => None,
        };
        if let Some(remaining) = progress {
            if orphan {
                // we lacked an ancestor: start again from a fresh locator
                self.request_next_chunk(peer, VecDeque::new(), out);
            } else {
                self.request_next_chunk(peer, remaining, out);
            }
        } else if orphan && self.syncing.is_none() {
            self.start_sync(peer, out);
        }
    }

    fn on_not_found(&mut self, peer: PeerId, ids: Vec<[u8; 32]>, out: &mut Vec<Action>) {
        let syncing_here = self.syncing.as_ref().is_some_and(|s| s.peer == peer);
        for id in ids {
            self.req_txs.remove(&id);
            self.asked.remove(&(peer, id));
            if self.req_blocks.get(&id).is_some_and(|r| r.peer == peer) {
                self.req_blocks.remove(&id);
                if let Some(l) = self.announcers.get_mut(&id) {
                    l.retain(|p| *p != peer);
                }
                self.fetch_from_announcers(id, out);
            }
        }
        if syncing_here {
            // it offered blocks it cannot serve (pruned, or gone after a reorganisation of its own)
            self.penalize(peer, 20, "cannot serve blocks it offered", out);
            self.abort_sync(peer, out);
        }
    }

    // ---- blocks -----------------------------------------------------------------------------------

    /// Feeds a block to the node and reports what became of it. An invalid block from a peer bans it.
    fn apply_block(&mut self, from: Option<PeerId>, b: &Block, out: &mut Vec<Action>) -> Applied {
        match self.node.submit_block(b, self.secs()) {
            Ok(Submitted::Extended(_)) | Ok(Submitted::Reorganised { .. }) => {
                self.stats.blocks_applied += 1;
                Applied::NewTip
            }
            Ok(Submitted::SideChain { .. }) | Ok(Submitted::AlreadyKnown) => Applied::Kept,
            Ok(Submitted::Orphan) => Applied::Orphan,
            Ok(Submitted::NotYet) => {
                if self.held.len() < self.cfg.max_held {
                    self.held.push(b.clone());
                }
                Applied::NotYet
            }
            Err(BlockError::Store(_)) => Applied::Ours,
            Err(_) => {
                if let Some(p) = from {
                    self.penalize(p, 100, "sent an invalid block", out);
                }
                Applied::Invalid
            }
        }
    }

    fn on_new_block(
        &mut self,
        peer: PeerId,
        id: [u8; 32],
        height: u64,
        work: U256,
        out: &mut Vec<Action>,
    ) {
        if let Some(p) = self.peers.get_mut(&peer) {
            p.known_blocks.insert(id);
            if work > p.work {
                p.work = work;
                p.tip_height = height;
            }
        }
        if self.node.chain().is_known_invalid(&id) {
            self.penalize(peer, 50, "announced a block known to be invalid", out);
            return;
        }
        if self.on_chain(&id) || self.node.chain().in_side_pool(&id) {
            return;
        }
        if self.announcers.len() > 1024 {
            self.announcers.clear();
        }
        let list = self.announcers.entry(id).or_default();
        if !list.contains(&peer) {
            list.push(peer);
        }
        let (_, _, ours) = self.tip();
        if work > ours && !self.req_blocks.contains_key(&id) {
            self.req_blocks.insert(id, Req { peer, at: self.now });
            self.send(peer, Message::GetBlocks { ids: vec![id] }, out);
        }
    }

    /// Tells every ready peer that does not know our tip about it.
    fn announce_tip(&mut self, exclude: Option<PeerId>, out: &mut Vec<Action>) {
        let (height, id, work) = self.tip();
        let targets: Vec<PeerId> = self
            .peers
            .iter()
            .filter(|(pid, p)| {
                Some(**pid) != exclude && p.hello.is_some() && !p.known_blocks.contains(&id)
            })
            .map(|(pid, _)| *pid)
            .collect();
        for pid in targets {
            if let Some(p) = self.peers.get_mut(&pid) {
                if p.known_blocks.len() > 4096 {
                    p.known_blocks.clear();
                }
                p.known_blocks.insert(id);
            }
            self.send(
                pid,
                Message::NewBlock {
                    id,
                    height,
                    cumulative_work: work.to_be_bytes(),
                },
                out,
            );
        }
    }

    // ---- transactions -----------------------------------------------------------------------------

    fn on_new_tx(&mut self, peer: PeerId, ids: Vec<[u8; 32]>, out: &mut Vec<Action>) {
        let mut want = Vec::new();
        for id in ids {
            if let Some(p) = self.peers.get_mut(&peer) {
                if p.known_txs.len() > 4096 {
                    p.known_txs.clear();
                }
                p.known_txs.insert(id);
            }
            if !self.node.pool().contains(&id)
                && !self.rejected_txs.contains(&id)
                && !self.req_txs.contains_key(&id)
            {
                self.req_txs.insert(id, Req { peer, at: self.now });
                want.push(id);
            }
        }
        if !want.is_empty() {
            self.send(peer, Message::GetTxs { ids: want }, out);
        }
    }

    fn on_txs(&mut self, peer: PeerId, txs: Vec<Transaction>, out: &mut Vec<Action>) {
        for t in txs {
            let Ok(id) = tx_id(&t) else {
                self.penalize(peer, 50, "a transaction that does not encode", out);
                continue;
            };
            if self.req_txs.remove(&id).map(|r| r.peer) != Some(peer) {
                self.penalize(peer, 20, "a transaction nobody asked for", out);
                if !self.peers.contains_key(&peer) {
                    return;
                }
                continue;
            }
            match self.node.submit_tx(t) {
                Ok(AddOutcome::Added { id, .. }) => self.announce_tx(id, out),
                Err(PoolError::Invalid(_)) | Err(PoolError::TooLarge { .. }) => {
                    if self.rejected_txs.len() > 8192 {
                        self.rejected_txs.clear();
                    }
                    self.rejected_txs.insert(id);
                    self.penalize(peer, 20, "an invalid transaction", out);
                    if !self.peers.contains_key(&peer) {
                        return;
                    }
                }
                // already known, in conflict, or no room: not the peer's fault
                Err(_) => {}
            }
        }
    }

    /// Tells every ready peer that does not know transaction `id` about it. (The peer we got it from, and
    /// any other that announced it to us, already have it in their known set.)
    fn announce_tx(&mut self, id: [u8; 32], out: &mut Vec<Action>) {
        let targets: Vec<PeerId> = self
            .peers
            .iter()
            .filter(|(_, p)| p.hello.is_some() && !p.known_txs.contains(&id))
            .map(|(pid, _)| *pid)
            .collect();
        for pid in targets {
            if let Some(p) = self.peers.get_mut(&pid) {
                if p.known_txs.len() > 4096 {
                    p.known_txs.clear();
                }
                p.known_txs.insert(id);
            }
            self.send(pid, Message::NewTx { ids: vec![id] }, out);
        }
    }

    // ---- time -------------------------------------------------------------------------------------

    fn on_tick(&mut self, out: &mut Vec<Action>) {
        let now = self.now;
        // handshake, keepalive
        let mut drop: Vec<(PeerId, &'static str)> = Vec::new();
        let mut ping: Vec<PeerId> = Vec::new();
        let mut late_pings: Vec<PeerId> = Vec::new();
        for (id, p) in &self.peers {
            if p.hello.is_none() {
                if now.saturating_sub(p.connected_at) > self.cfg.handshake_timeout_ms {
                    drop.push((*id, "handshake timeout"));
                }
            } else if let Some(t) = p.ping_sent {
                // an unanswered ping is asked again; only several in a row cost the connection
                if now.saturating_sub(t) > self.cfg.pong_timeout_ms {
                    late_pings.push(*id);
                }
            } else if now.saturating_sub(p.last_recv) > self.cfg.ping_after_ms {
                ping.push(*id);
            }
        }
        for (id, why) in drop {
            self.drop_peer(id, why, false, out);
        }
        for id in late_pings {
            self.note_timeout(id, out);
            if self.peers.contains_key(&id) {
                ping.push(id);
            }
        }
        for id in ping {
            if let Some(p) = self.peers.get_mut(&id) {
                p.ping_sent = Some(now);
            }
            self.send(id, Message::Ping(now), out);
        }

        // requests that were never answered
        let timeout = self.cfg.request_timeout_ms;
        if self
            .syncing
            .as_ref()
            .is_some_and(|s| now.saturating_sub(s.started) > timeout)
        {
            let peer = self.syncing.as_ref().map(|s| s.peer).unwrap();
            self.abort_sync(peer, out);
            self.note_timeout(peer, out);
        }
        let late: Vec<([u8; 32], PeerId)> = self
            .req_blocks
            .iter()
            .filter(|(_, r)| now.saturating_sub(r.at) > timeout)
            .map(|(id, r)| (*id, r.peer))
            .collect();
        for (id, peer) in late {
            self.req_blocks.remove(&id);
            if let Some(l) = self.announcers.get_mut(&id) {
                l.retain(|p| *p != peer);
            }
            self.note_timeout(peer, out);
            self.fetch_from_announcers(id, out);
        }
        // asks that will never be answered no longer make a delivery welcome
        self.asked
            .retain(|_, at| now.saturating_sub(*at) <= timeout.saturating_mul(4));
        let late_txs: Vec<[u8; 32]> = self
            .req_txs
            .iter()
            .filter(|(_, r)| now.saturating_sub(r.at) > timeout)
            .map(|(id, _)| *id)
            .collect();
        for id in late_txs {
            self.req_txs.remove(&id);
        }

        // blocks that were ahead of the clock
        let held = std::mem::take(&mut self.held);
        let mut new_tip = false;
        for b in held {
            if let Applied::NewTip = self.apply_block(None, &b, out) {
                new_tip = true;
            }
        }
        if new_tip {
            self.announce_tip(None, out);
        }

        // repeat the tip to peers that are behind, so a lost announcement heals
        if now.saturating_sub(self.last_reannounce) >= self.cfg.reannounce_ms {
            self.last_reannounce = now;
            let (height, id, work) = self.tip();
            let behind: Vec<PeerId> = self
                .peers
                .iter()
                .filter(|(_, p)| p.hello.is_some() && p.work < work)
                .map(|(pid, _)| *pid)
                .collect();
            for pid in behind {
                self.send(
                    pid,
                    Message::NewBlock {
                        id,
                        height,
                        cumulative_work: work.to_be_bytes(),
                    },
                    out,
                );
            }
        }
        self.maintain_connections(out);
        self.maybe_start_sync(out);
    }
}
