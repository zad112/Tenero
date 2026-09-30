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

use crate::message::{Hello, Limits, Message, PROTOCOL_VERSION};

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
    bans: HashMap<String, u64>,
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
        Engine {
            node,
            cfg,
            now: 0,
            peers: BTreeMap::new(),
            bans: HashMap::new(),
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

    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    pub fn ready_peer_count(&self) -> usize {
        self.peers.values().filter(|p| p.hello.is_some()).count()
    }

    pub fn is_banned(&self, addr: &str, now_ms: u64) -> bool {
        self.bans.get(addr).is_some_and(|&until| until > now_ms)
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
                self.peers.remove(&peer);
                self.cleanup_peer(peer, &mut out);
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
            if ban {
                let until = self.now + self.cfg.ban_ms;
                self.bans.insert(p.addr.clone(), until);
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
        if self.is_banned(&addr, self.now) {
            out.push(Action::Disconnect {
                peer,
                reason: "banned".into(),
            });
            return;
        }
        let inbound_now = self.peers.values().filter(|p| p.inbound).count();
        if self.peers.len() >= self.cfg.max_peers
            || (inbound && inbound_now >= self.cfg.max_inbound)
        {
            out.push(Action::Disconnect {
                peer,
                reason: "full".into(),
            });
            return;
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
        if let Some(p) = self.peers.get_mut(&peer) {
            p.tip_height = h.tip_height;
            p.work = U256::from_be_bytes(&h.cumulative_work);
            p.known_blocks.insert(h.tip_id);
            p.hello = Some(h);
        }
        self.maybe_start_sync(out);
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
        self.maybe_start_sync(out);
    }
}
