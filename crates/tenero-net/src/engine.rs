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
use tenero_core::v2::{Block, BlockHeader, Transaction};
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
    /// A peer sent bytes, through the encrypted channel, that are not a message (a malformed or oversized
    /// frame): it is broken or hostile, and is banned. (Bytes that fail to DECRYPT are not this: a third party on
    /// the wire could cause those, so the transport only closes the connection.)
    BadBytes {
        peer: PeerId,
        why: String,
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

/// A block the software ships and trusts (**not consensus**): a block id at a height. With it set, a node that is
/// behind it first fetches the headers up to that height, and, only if they link from its own chain and the one at
/// that height is this block, skips the full proof of work and the transaction proofs for the blocks on that path.
/// Everything else (linkage, Merkle roots, the cheap proof-of-work check, emission, key images, ring membership, fees)
/// is still checked. It is a trust decision: see `docs/M8_PLAN.md`, M8.3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AssumeValid {
    pub height: u64,
    pub id: [u8; 32],
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
    /// Off (`None`) unless the operator turns it on.
    pub assume_valid: Option<AssumeValid>,
    /// An answer to `GetAddrs` holds at most this percentage of the address book (and never more than
    /// `limits.max_addrs`), so one request cannot read the whole book...
    pub addr_share_percent: u64,
    /// ...except that a small book may be given out in this many addresses or fewer, whatever the percentage
    /// says: a young network must let a newcomer learn enough addresses to get started, and a book this small
    /// reveals little.
    pub addr_answer_floor: usize,
    /// A network group that asks again within this time gets the SAME answer, so reconnecting does not draw a
    /// fresh sample each time (which would let one host read the whole book a slice at a time).
    pub addr_answer_ttl_ms: u64,
    /// How many groups' answers are remembered.
    pub addr_answer_cache: usize,
    /// When the tip moves, the proof of work is asked to prepare for blocks this far ahead (a dataset for the next
    /// epoch is built in the background when that many blocks from the end of an epoch), so the first block of a
    /// new epoch is not checked after a wait of several seconds.
    pub pow_prefetch_blocks: u64,
    /// The most bytes of blocks put in one `blocks` reply; a request for blocks that are larger together is answered in
    /// several replies (the wire's frame ceiling is 16 MiB, and a reply over it cannot be sent at all).
    pub blocks_reply_bytes: usize,
    /// How many outbound peers are remembered across a restart and dialled first when the node starts again (`anchors.rs`).
    /// 0 turns anchors off.
    pub anchor_count: usize,
    /// An outbound peer must have been connected this long before it may be an anchor: a connection made a moment ago says
    /// little about who is on the other end.
    pub anchor_min_age_ms: u64,
    /// No new tip for this long (10 target block intervals on the 60-second chain) and the node suspects it is cut off from
    /// the real network (an eclipse, or a partition): it dials extra outbound peers from network groups it has none in
    /// (threat model C1). 0 turns it off.
    pub stale_tip_ms: u64,
    /// How many extra outbound peers to dial each time, and the least time between two such attempts.
    pub stale_extra_outbound: usize,
    pub stale_retry_ms: u64,
    /// Peers the operator pinned (`ip:port`, got out of band): dialled first, again whenever they are not connected (at
    /// most once per `trusted_retry_ms` each), and exempt from the per-network-group limit. Never put in the address book, so
    /// never passed on to other nodes. They are still validated like any peer, and still banned if they misbehave.
    pub trusted: Vec<String>,
    pub trusted_retry_ms: u64,
    /// A node with no tried address (a first start) dials ONLY its configured seeds until every seed group has answered its
    /// address request, or this long has passed (a dead seed costs a first start this wait, once). Without this, whichever seed
    /// answers first decides who the node dials first, and a hostile seed can answer first; waiting for a QUOTA of seeds does not
    /// help, because hostile seeds fill the quota before the honest ones answer (`docs/SEED_POLICY.md` has the measurement).
    /// 0 turns it off (threat model C1).
    pub bootstrap_wait_ms: u64,
    /// A feeler connection (`docs/SEED_POLICY.md`, threat model C1): every this many milliseconds the node dials ONE address it has
    /// never connected to, reads its tip and work from its `hello`, and hangs up. It never counts as an outbound peer, an anchor or a
    /// sync peer. It gives independent samples of the wider network (an eclipsed node's peers all say the same thing), and it moves
    /// addresses that work into the "tried" part of the book. 0 turns it off.
    pub feeler_interval_ms: u64,
    /// How long a condition must last before it is an alarm (`Engine::health`): a peer reporting more work than we have, feeler samples
    /// of the same, too few outbound peers. 0 turns the alarms off.
    pub alarm_after_ms: u64,
    /// Fewer outbound peers than this (once the bootstrap is over) is an alarm after `alarm_after_ms`.
    pub min_outbound_peers: usize,
    /// Outbound peers in fewer network groups than this is an alarm (not on a private network, where all addresses are one group).
    pub min_outbound_groups: usize,
    /// Feeler samples older than this are forgotten.
    pub sample_window_ms: u64,
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
            assume_valid: None,
            addr_share_percent: 23,
            addr_answer_floor: 20,
            addr_answer_ttl_ms: 24 * 3600 * 1000,
            addr_answer_cache: 1024,
            pow_prefetch_blocks: 10,
            blocks_reply_bytes: crate::wire::BLOCKS_REPLY_BYTES,
            anchor_count: 2,
            anchor_min_age_ms: 10 * 60 * 1000,
            stale_tip_ms: 10 * 60 * 1000,
            stale_extra_outbound: 2,
            stale_retry_ms: 5 * 60 * 1000,
            trusted: Vec::new(),
            trusted_retry_ms: 30 * 1000,
            bootstrap_wait_ms: 20 * 1000,
            feeler_interval_ms: 2 * 60 * 1000,
            alarm_after_ms: 5 * 60 * 1000,
            min_outbound_peers: 2,
            min_outbound_groups: 2,
            sample_window_ms: 30 * 60 * 1000,
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
    /// Blocks applied without their full proof of work and proofs, because assume-valid vouched for them.
    pub assumed_blocks: u64,
    /// Replies that came after the request they answered had timed out, and were forgiven ("slow is not hostile").
    pub late_replies_forgiven: u64,
    /// Anchor peers dialled first after a restart.
    pub anchors_dialled: u64,
    /// Times the tip went stale (no new block for `stale_tip_ms`), and the extra outbound peers dialled because of it.
    pub stale_tip_events: u64,
    pub stale_extra_dials: u64,
    /// Pinned peers dialled.
    pub trusted_dialled: u64,
    /// First-start bootstrap (see `EngineConfig::bootstrap_wait_ms`): begun, finished because every seed group answered, and
    /// finished because the wait ran out.
    pub bootstrap_started: u64,
    pub bootstrap_done: u64,
    pub bootstrap_timeouts: u64,
    /// Feeler connections: dialled, and read (their tip and work noted).
    pub feelers_dialled: u64,
    pub feelers_sampled: u64,
}

/// What a feeler connection read from one address.
#[derive(Clone, Debug)]
struct Sample {
    at: u64,
    addr: String,
    work: U256,
}

/// Something the operator should look at (`Engine::health`). None of them is proof of an attack: each is what an eclipse, a
/// partition or a fork would look like from inside, and so is also what a bad network day looks like.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Alarm {
    /// No new block for `stale_tip_ms`.
    StaleTip { age_ms: u64 },
    /// At least one ready peer has reported more work than we have for this long, and we have not caught up.
    Behind { for_ms: u64 },
    /// At least two nodes we sampled with feeler connections reported more work than we have now, at least `alarm_after_ms` ago: the
    /// wider network seems to be ahead of every peer we are connected to.
    SamplesAhead { count: usize },
    /// Fewer than `min_outbound_peers` outbound peers for `alarm_after_ms`.
    FewOutbound { count: usize },
    /// Outbound peers in fewer than `min_outbound_groups` network groups.
    FewGroups { groups: usize },
}

impl Alarm {
    /// A short name that does not change as the numbers do (to log a change of state, not every look).
    pub fn kind(&self) -> &'static str {
        match self {
            Alarm::StaleTip { .. } => "stale-tip",
            Alarm::Behind { .. } => "behind-peers",
            Alarm::SamplesAhead { .. } => "network-ahead",
            Alarm::FewOutbound { .. } => "few-outbound",
            Alarm::FewGroups { .. } => "few-groups",
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Alarm::StaleTip { age_ms } => format!(
                "no new block for {} minutes: this node may be cut off from the real network",
                age_ms / 60_000
            ),
            Alarm::Behind { for_ms } => format!(
                "peers have reported more work than this node has for {} minutes and it has not caught up: possible eclipse, fork or a node that cannot sync",
                for_ms / 60_000
            ),
            Alarm::SamplesAhead { count } => format!(
                "{count} nodes sampled outside this node's peers reported more work than it has: the wider network seems ahead of every peer it is connected to (possible eclipse)"
            ),
            Alarm::FewOutbound { count } => {
                format!("only {count} outbound peers: this node is not choosing enough of its own peers")
            }
            Alarm::FewGroups { groups } => format!(
                "outbound peers are in only {groups} network group(s): one network range could be all this node hears from"
            ),
        }
    }
}

/// A snapshot of how well connected the node is (`Engine::health`).
#[derive(Clone, Debug)]
pub struct NetHealth {
    pub peers: usize,
    pub inbound: usize,
    pub outbound: usize,
    /// Distinct network groups among the outbound peers.
    pub outbound_groups: usize,
    pub tip_age_ms: u64,
    /// Ready peers that have reported more work than we have.
    pub peers_ahead: usize,
    /// How long that has been so (0 if no peer is ahead).
    pub behind_for_ms: u64,
    /// Feeler samples held, and how many of those are older than `alarm_after_ms` and still ahead of us.
    pub samples: usize,
    pub samples_ahead: usize,
    pub bootstrapping: bool,
    pub alarms: Vec<Alarm>,
}

/// Where a first start is in its bootstrap.
enum Boot {
    NotStarted,
    Active {
        since: u64,
        /// How many seed groups there are to hear from.
        needed: usize,
        /// The seed groups that have answered.
        answered: HashSet<String>,
    },
    Done,
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
    /// A feeler connection: dialled only to read its `hello`, then dropped; never counted as an outbound peer.
    feeler: bool,
}

enum Phase {
    /// Waiting for `Headers` (assume-valid): `ids` are the ids of the headers verified so far, each linked to the
    /// one before, the first to a block of ours; `next_height` is the height the next header must have, and
    /// `last_id` the id it must link to. `next_height` is 0 until the first reply.
    Headers {
        ids: Vec<[u8; 32]>,
        next_height: u64,
        last_id: [u8; 32],
    },
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

/// A reply we have given up waiting for. If it does arrive after all, it is late, not unsolicited: "slow is not
/// hostile". Each timed-out request forgives exactly one reply, from the peer that was slow, for a while.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Late {
    BlockIds,
    Headers,
    Tx([u8; 32]),
    /// The nonce of a ping that went unanswered and was asked again.
    Pong(u64),
}

/// The most forgivable replies remembered at once (they are made only by our own timeouts, so this is a bound, not a
/// limit anyone can reach from outside).
const MAX_FORGIVABLE: usize = 4096;

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
    /// Anchor peers loaded from the saved state, still to be dialled first (`anchors.rs`); each is tried once.
    anchors: Vec<String>,
    /// The tip id last seen and since when (the engine's clock): how long the chain has not moved.
    tip_seen: Option<([u8; 32], u64)>,
    /// Set when a handler sent requests, to the clock it was called with: the requests were really sent when the handler
    /// RETURNED, which can be much later (applying a batch of blocks takes a while), so their timeout clocks are restarted at
    /// the next event (see `restamp`).
    stamp_fresh: Option<u64>,
    boot: Boot,
    /// The address of the feeler connection in progress, if any (one at a time), and when the last one was started.
    feeler: Option<String>,
    last_feeler: Option<u64>,
    /// What feeler connections have read, newest last.
    samples: VecDeque<Sample>,
    /// Since when some ready peer has reported more work than we have, and since when we have had too few outbound peers.
    behind_since: Option<u64>,
    few_since: Option<u64>,
    /// Whether the tip is stale now, and when extra peers were last dialled because of it.
    stale: bool,
    last_stale_action: Option<u64>,
    /// When each pinned peer was last dialled.
    trusted_last: HashMap<String, u64>,
    /// Replies that would be forgiven if they came now, and until when (see [`Late`]).
    forgivable: Vec<(PeerId, Late, u64)>,
    /// The answer last given to each requesting network group, and when it stops being reused.
    addr_answers: HashMap<String, (u64, Vec<PeerAddr>)>,
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
            anchors: Vec::new(),
            tip_seen: None,
            stamp_fresh: None,
            boot: Boot::NotStarted,
            feeler: None,
            last_feeler: None,
            samples: VecDeque::new(),
            behind_since: None,
            few_since: None,
            stale: false,
            last_stale_action: None,
            trusted_last: HashMap::new(),
            forgivable: Vec::new(),
            addr_answers: HashMap::new(),
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
        // requests sent by the last handler were stamped with the time that handler STARTED; it may have taken long, and the
        // peer cannot have seen them before it ended: start their clocks now (once; the next event does not move them again)
        if let Some(old) = self.stamp_fresh.take() {
            if now_ms > old {
                self.restamp(old, now_ms);
            }
        }
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
            Event::BadBytes { peer, why } => {
                let threshold = self.cfg.ban_threshold;
                self.penalize(
                    peer,
                    threshold,
                    &format!("undecodable bytes: {why}"),
                    &mut out,
                );
            }
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

    /// Moves every request timestamp equal to `old` to `new` (see `stamp_fresh`).
    fn restamp(&mut self, old: u64, new: u64) {
        for t in self.asked.values_mut() {
            if *t == old {
                *t = new;
            }
        }
        for r in self
            .req_blocks
            .values_mut()
            .chain(self.req_txs.values_mut())
        {
            if r.at == old {
                r.at = new;
            }
        }
        if let Some(s) = self.syncing.as_mut() {
            if s.started == old {
                s.started = new;
            }
        }
    }

    fn send(&mut self, peer: PeerId, msg: Message, out: &mut Vec<Action>) {
        if matches!(
            msg,
            Message::GetBlockIds { .. }
                | Message::GetHeaders { .. }
                | Message::GetBlocks { .. }
                | Message::GetTxs { .. }
        ) {
            // (not pings: a ping's nonce IS its send time, and it is sent from the quick tick handler)
            self.stamp_fresh = Some(self.now);
        }
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

    /// Neither in our chain nor waiting in a pool: a block worth asking a peer for.
    fn not_held(&self, id: &[u8; 32]) -> bool {
        !self.on_chain(id) && !self.node.chain().holds_block(id)
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
        let feeler = !inbound && self.feeler.as_deref() == Some(addr.as_str());
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
                feeler,
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
        if self.peers.get(&peer).is_some_and(|p| p.feeler) {
            // a feeler: note what it said, remember that the address works, and go
            let work = self
                .peers
                .get(&peer)
                .map(|p| p.work)
                .unwrap_or(U256::from_be_bytes(&[0; 32]));
            self.book.mark_success(&addr, self.secs());
            self.samples.push_back(Sample {
                at: self.now,
                addr,
                work,
            });
            while self.samples.len() > 32 {
                self.samples.pop_front();
            }
            self.stats.feelers_sampled += 1;
            self.drop_peer(peer, "feeler done", false, out);
            return;
        }
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
                        | Message::Headers { .. }
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
                if !matched && !self.forgave(peer, &Late::Pong(n)) {
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
            Message::GetHeaders { locator } => {
                if locator.is_empty() || locator.len() > lim.max_locator {
                    self.penalize(peer, 50, "bad locator", out);
                } else {
                    self.on_get_headers(peer, locator, out);
                }
            }
            Message::Headers {
                first_height,
                headers,
            } => {
                if headers.len() > lim.max_headers {
                    self.penalize(peer, 50, "too many headers", out);
                } else {
                    self.on_headers(peer, first_height, headers, out);
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

    /// Headers after the newest locator entry we know, up to `max_headers`. A pruned node still has every header.
    fn on_get_headers(&mut self, peer: PeerId, locator: Vec<[u8; 32]>, out: &mut Vec<Action>) {
        let store = self.node.store();
        let common = locator
            .iter()
            .find_map(|id| store.height_of(id).ok().flatten());
        let Some(common) = common else {
            self.penalize(peer, 20, "locator shares nothing with our chain", out);
            self.send(
                peer,
                Message::Headers {
                    first_height: 1,
                    headers: vec![],
                },
                out,
            );
            return;
        };
        let (tip_h, _, _) = self.tip();
        let last = tip_h.min(common + self.cfg.limits.max_headers as u64);
        let headers: Vec<BlockHeader> = ((common + 1)..=last)
            .filter_map(|h| self.node.store().block_index(h).ok().flatten())
            .map(|i| i.header)
            .collect();
        self.send(
            peer,
            Message::Headers {
                first_height: common + 1,
                headers,
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
        // as many replies as it takes for each to fit a frame; a block too big for any frame is, to the peer, one we cannot serve
        let (groups, too_big) = crate::wire::split_blocks(blocks, self.cfg.blocks_reply_bytes);
        for b in &too_big {
            missing.push(self.block_id_of(b));
        }
        for blocks in groups {
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
                let addrs = self.addrs_for(peer);
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
        let addrs = self.addrs_for(peer);
        self.send(peer, Message::Addrs { addrs }, out);
    }

    /// The answer to `peer`'s `GetAddrs`: a capped share of the book, and the same one for a network group that
    /// asks again before `addr_answer_ttl_ms` has passed.
    fn addrs_for(&mut self, peer: PeerId) -> Vec<PeerAddr> {
        let Some(addr) = self.peers.get(&peer).map(|p| p.addr.clone()) else {
            return Vec::new();
        };
        let group = group_of(&addr);
        let now = self.now;
        if let Some((until, answer)) = self.addr_answers.get(&group) {
            if *until > now {
                return answer.clone();
            }
        }
        let known = self.book.len() as u64;
        let share =
            (known * self.cfg.addr_share_percent / 100).max(self.cfg.addr_answer_floor as u64);
        let n = share.min(self.cfg.limits.max_addrs as u64) as usize;
        let answer = self.book.sample(n, self.secs());
        // (a group that is already here and unexpired was answered above; an expired one is simply replaced, and
        // when the cache is full the answer that would expire soonest goes, which for such a group is its own)
        if self.addr_answers.len() >= self.cfg.addr_answer_cache {
            // make room: the answer that would expire soonest goes
            if let Some(oldest) = self
                .addr_answers
                .iter()
                .min_by_key(|(_, (until, _))| *until)
                .map(|(k, _)| k.clone())
            {
                self.addr_answers.remove(&oldest);
            }
        }
        self.addr_answers
            .insert(group, (now + self.cfg.addr_answer_ttl_ms, answer.clone()));
        answer
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
            if self
                .book
                .get(&peer_addr)
                .is_some_and(|e| e.source == "seed")
            {
                if let Boot::Active { answered, .. } = &mut self.boot {
                    answered.insert(source);
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
        self.watch_tip(now);
        self.dial_trusted(now, out);
        self.dial_anchors(now, out);
        self.dial_for_stale_tip(now, out);
        let seeds_only = self.update_bootstrap(now);
        self.update_health_timers(now, seeds_only);
        if !seeds_only {
            self.dial_feeler(now, out);
        }

        let regular = self.peers.values().filter(|p| !p.addr_only && !p.feeler);
        let dialling = self.ordinary_dials().count();
        let total = regular.clone().count() + dialling;
        let outbound = regular.filter(|p| !p.inbound).count() + dialling;
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
        for p in self.peers.values().filter(|p| !p.inbound && !p.feeler) {
            *groups.entry(group_of(&p.addr)).or_default() += 1;
        }
        for a in self.ordinary_dials() {
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
        let candidates = self
            .book
            .candidates_with(now, want * 4, &skip, &full, seeds_only);
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

    /// The addresses we have asked the transport to dial, not counting a feeler: it holds no slot, so a feeler on its way must not make
    /// the node want one peer fewer of its own.
    fn ordinary_dials(&self) -> impl Iterator<Item = &String> + '_ {
        self.connecting
            .keys()
            .filter(move |a| self.feeler.as_deref() != Some(a.as_str()))
    }

    /// Starts or ends the clocks behind the alarms (since when some peer has been ahead of us, and since when we have been short of
    /// outbound peers).
    fn update_health_timers(&mut self, now: u64, bootstrapping: bool) {
        let ours = self.tip().2;
        let ahead = self
            .peers
            .values()
            .any(|p| !p.addr_only && !p.feeler && p.hello.is_some() && p.work > ours);
        self.behind_since = match (ahead, self.behind_since) {
            (true, None) => Some(now),
            (true, some) => some,
            (false, _) => None,
        };
        let outbound = self.outbound_count();
        let short = !bootstrapping && outbound < self.cfg.min_outbound_peers;
        self.few_since = match (short, self.few_since) {
            (true, None) => Some(now),
            (true, some) => some,
            (false, _) => None,
        };
    }

    /// Dials one address that has never connected, to read its tip and work (see `EngineConfig::feeler_interval_ms`). One at a
    /// time, never a host we are connected to, never a banned address, never our own.
    fn dial_feeler(&mut self, now: u64, out: &mut Vec<Action>) {
        if self.cfg.feeler_interval_ms == 0 {
            return;
        }
        // forget a feeler that is over (it connected and left, or failed, or timed out)
        if let Some(a) = &self.feeler {
            let alive = self.connecting.contains_key(a)
                || self.peers.values().any(|p| p.feeler && &p.addr == a);
            if alive {
                return;
            }
            self.feeler = None;
        }
        let last = *self.last_feeler.get_or_insert(now);
        if now.saturating_sub(last) < self.cfg.feeler_interval_ms {
            return;
        }
        let hosts: HashSet<String> = self.peers.values().map(|p| host_of(&p.addr)).collect();
        let own = self.cfg.advertise.clone();
        let bans = &self.bans;
        let connecting = &self.connecting;
        let skip = |a: &str| {
            hosts.contains(&host_of(a))
                || connecting.contains_key(a)
                || bans.is_banned(a, now)
                || own.as_deref() == Some(a)
        };
        let Some(addr) = self.book.untried_candidate(now, &skip) else {
            return;
        };
        self.last_feeler = Some(now);
        self.book.mark_attempt(&addr, now);
        self.connecting.insert(addr.clone(), now);
        self.feeler = Some(addr.clone());
        self.stats.feelers_dialled += 1;
        out.push(Action::Connect { addr });
    }

    /// How well connected the node is, and what the operator should look at (`Alarm`).
    pub fn health(&self) -> NetHealth {
        let now = self.now;
        let ours = self.tip().2;
        let regular = self.peers.values().filter(|p| !p.addr_only && !p.feeler);
        let outbound_groups: HashSet<String> = regular
            .clone()
            .filter(|p| !p.inbound)
            .map(|p| group_of(&p.addr))
            .collect();
        let peers_ahead = regular
            .clone()
            .filter(|p| p.hello.is_some() && p.work > ours)
            .count();
        let alarm_after = self.cfg.alarm_after_ms;
        let behind_for_ms = self.behind_since.map_or(0, |s| now.saturating_sub(s));
        let window = self.cfg.sample_window_ms;
        let samples = self
            .samples
            .iter()
            .filter(|s| now.saturating_sub(s.at) <= window)
            .count();
        // (distinct nodes: one node sampled twice is one witness)
        let samples_ahead = self
            .samples
            .iter()
            .filter(|s| {
                let age = now.saturating_sub(s.at);
                age <= window && age >= alarm_after && s.work > ours
            })
            .map(|s| s.addr.as_str())
            .collect::<HashSet<_>>()
            .len();
        let outbound = self.outbound_count();
        let bootstrapping = self.is_bootstrapping();
        let mut alarms = Vec::new();
        if self.is_tip_stale() {
            alarms.push(Alarm::StaleTip {
                age_ms: self.tip_age_ms(),
            });
        }
        if alarm_after != 0 {
            if peers_ahead > 0 && behind_for_ms >= alarm_after {
                alarms.push(Alarm::Behind {
                    for_ms: behind_for_ms,
                });
            }
            if samples_ahead >= 2 {
                alarms.push(Alarm::SamplesAhead {
                    count: samples_ahead,
                });
            }
            if self
                .few_since
                .is_some_and(|s| now.saturating_sub(s) >= alarm_after)
            {
                alarms.push(Alarm::FewOutbound { count: outbound });
            }
            if !bootstrapping
                && !self.cfg.addrbook.accept_private
                && self.cfg.min_outbound_groups > 0
                && outbound >= self.cfg.min_outbound_peers.max(1)
                && outbound_groups.len() < self.cfg.min_outbound_groups
            {
                alarms.push(Alarm::FewGroups {
                    groups: outbound_groups.len(),
                });
            }
        }
        NetHealth {
            peers: regular.clone().count(),
            inbound: regular.filter(|p| p.inbound).count(),
            outbound,
            outbound_groups: outbound_groups.len(),
            tip_age_ms: self.tip_age_ms(),
            peers_ahead,
            behind_for_ms,
            samples,
            samples_ahead,
            bootstrapping,
            alarms,
        }
    }

    /// Where the first-start bootstrap is: returns true while only the configured seeds may be dialled. It starts on the first
    /// call of a node with no tried address (the anchors of a saved state are tried addresses) and at least one seed in its book, and ends when every seed group has
    /// answered or the wait is over.
    fn update_bootstrap(&mut self, now: u64) -> bool {
        match &self.boot {
            Boot::Done => false,
            Boot::NotStarted => {
                let groups: HashSet<String> = self
                    .cfg
                    .seeds
                    .iter()
                    .filter(|s| self.book.get(s).is_some())
                    .map(|s| group_of(s))
                    .collect();
                if self.cfg.bootstrap_wait_ms == 0
                    || groups.is_empty()
                    || self.book.tried_count() > 0
                {
                    self.boot = Boot::Done;
                    return false;
                }
                self.boot = Boot::Active {
                    since: now,
                    needed: groups.len(),
                    answered: HashSet::new(),
                };
                self.stats.bootstrap_started += 1;
                true
            }
            Boot::Active {
                since,
                needed,
                answered,
            } => {
                if answered.len() >= *needed {
                    self.boot = Boot::Done;
                    self.stats.bootstrap_done += 1;
                    false
                } else if now.saturating_sub(*since) >= self.cfg.bootstrap_wait_ms {
                    self.boot = Boot::Done;
                    self.stats.bootstrap_timeouts += 1;
                    false
                } else {
                    true
                }
            }
        }
    }

    /// Whether the first-start bootstrap is still going (only the configured seeds are being dialled).
    pub fn is_bootstrapping(&self) -> bool {
        matches!(self.boot, Boot::Active { .. })
    }

    /// The outbound peers worth remembering across a restart: ready peers WE dialled (so not chosen by whoever connected to us),
    /// connected for at least `anchor_min_age_ms`, the oldest first, at most `anchor_count`, and no two from one network group.
    pub fn current_anchors(&self) -> Vec<String> {
        let want = self.cfg.anchor_count.min(crate::anchors::MAX_ANCHORS);
        if want == 0 {
            return Vec::new();
        }
        let mut peers: Vec<&Peer> = self
            .peers
            .values()
            .filter(|p| {
                !p.inbound
                    && !p.addr_only
                    && p.hello.is_some()
                    && self.now.saturating_sub(p.connected_at) >= self.cfg.anchor_min_age_ms
            })
            .collect();
        peers.sort_by(|a, b| (a.connected_at, &a.addr).cmp(&(b.connected_at, &b.addr)));
        let mut groups: HashSet<String> = HashSet::new();
        let mut out = Vec::new();
        for p in peers {
            if out.len() >= want {
                break;
            }
            if groups.insert(group_of(&p.addr)) {
                out.push(p.addr.clone());
            }
        }
        out
    }

    /// Notes whether the tip moved since the last look (a new block, or a reorganisation).
    fn watch_tip(&mut self, now: u64) {
        let id = match self.node.store().tip() {
            Ok((_, i)) => i.block_id,
            Err(_) => return,
        };
        match self.tip_seen {
            Some((seen, _)) if seen == id => {}
            _ => {
                self.tip_seen = Some((id, now));
                self.stale = false;
            }
        }
    }

    /// How long the tip has been the same, in milliseconds of the engine's clock (0 before the first look).
    pub fn tip_age_ms(&self) -> u64 {
        self.tip_seen
            .map_or(0, |(_, since)| self.now.saturating_sub(since))
    }

    /// Whether no new tip has come for `stale_tip_ms` (never, if that is 0).
    pub fn is_tip_stale(&self) -> bool {
        self.cfg.stale_tip_ms != 0 && self.tip_age_ms() >= self.cfg.stale_tip_ms
    }

    /// When the tip has been the same for too long, dials a few extra outbound peers from network groups none of the current
    /// outbound peers is in, so that one attacker's peers cannot be all that a node hears from. Repeats, not more often than
    /// `stale_retry_ms`, while the tip stays stale; never past `max_peers`.
    fn dial_for_stale_tip(&mut self, now: u64, out: &mut Vec<Action>) {
        if self.cfg.stale_extra_outbound == 0 || !self.is_tip_stale() {
            return;
        }
        if !self.stale {
            self.stale = true;
            self.stats.stale_tip_events += 1;
        }
        if let Some(last) = self.last_stale_action {
            if now.saturating_sub(last) < self.cfg.stale_retry_ms {
                return;
            }
        }
        self.last_stale_action = Some(now);
        let held = self.peers.values().filter(|p| !p.addr_only).count() + self.connecting.len();
        let room = self.cfg.max_peers.saturating_sub(held);
        let k = self.cfg.stale_extra_outbound.min(room);
        if k == 0 {
            return;
        }
        let hosts: HashSet<String> = self.peers.values().map(|p| host_of(&p.addr)).collect();
        let mut used_groups: HashSet<String> = self
            .peers
            .values()
            .filter(|p| !p.inbound && !p.feeler)
            .map(|p| group_of(&p.addr))
            .collect();
        used_groups.extend(self.ordinary_dials().map(|a| group_of(a)));
        let own = self.cfg.advertise.clone();
        let bans = &self.bans;
        let connecting = &self.connecting;
        let skip = |a: &str| {
            hosts.contains(&host_of(a))
                || connecting.contains_key(a)
                || bans.is_banned(a, now)
                || own.as_deref() == Some(a)
        };
        // a group that already has an outbound peer counts as full: only new groups
        let in_use = used_groups.clone();
        let full = |g: &str| in_use.contains(g);
        let candidates = self.book.candidates(now, k * 4, &skip, &full);
        let mut dialled = 0;
        for a in candidates {
            if dialled >= k {
                break;
            }
            if !used_groups.insert(group_of(&a)) {
                continue;
            }
            self.book.mark_attempt(&a, now);
            self.connecting.insert(a.clone(), now);
            self.stats.stale_extra_dials += 1;
            out.push(Action::Connect { addr: a });
            dialled += 1;
        }
    }

    /// Dials the pinned peers that are not connected, not being dialled, not banned and not tried within `trusted_retry_ms`.
    fn dial_trusted(&mut self, now: u64, out: &mut Vec<Action>) {
        if self.cfg.trusted.is_empty() {
            return;
        }
        let hosts: HashSet<String> = self.peers.values().map(|p| host_of(&p.addr)).collect();
        let trusted = self.cfg.trusted.clone();
        for a in trusted {
            let held = self.peers.values().filter(|p| !p.addr_only).count() + self.connecting.len();
            if held >= self.cfg.max_peers
                || hosts.contains(&host_of(&a))
                || self.connecting.contains_key(&a)
                || self.bans.is_banned(&a, now)
                || self.cfg.advertise.as_deref() == Some(a.as_str())
            {
                continue;
            }
            if let Some(&last) = self.trusted_last.get(&a) {
                if now.saturating_sub(last) < self.cfg.trusted_retry_ms {
                    continue;
                }
            }
            self.trusted_last.insert(a.clone(), now);
            self.connecting.insert(a.clone(), now);
            self.stats.trusted_dialled += 1;
            out.push(Action::Connect { addr: a });
        }
    }

    /// The anchors loaded from a saved state that have not been dialled yet.
    pub fn pending_anchors(&self) -> &[String] {
        &self.anchors
    }

    /// Dials the anchors loaded from the saved state, once each, before anything the address book or the seeds offer. One that is
    /// banned, on a host we are already connected to, being dialled, or our own address is skipped.
    fn dial_anchors(&mut self, now: u64, out: &mut Vec<Action>) {
        if self.anchors.is_empty() {
            return;
        }
        let anchors = std::mem::take(&mut self.anchors);
        let hosts: HashSet<String> = self.peers.values().map(|p| host_of(&p.addr)).collect();
        let mut chosen_hosts: HashSet<String> = HashSet::new();
        for a in anchors {
            let skip = hosts.contains(&host_of(&a))
                || self.connecting.contains_key(&a)
                || self.bans.is_banned(&a, now)
                || self.cfg.advertise.as_deref() == Some(a.as_str())
                || !chosen_hosts.insert(host_of(&a));
            if skip {
                continue;
            }
            self.book.mark_attempt(&a, now);
            self.connecting.insert(a.clone(), now);
            self.stats.anchors_dialled += 1;
            out.push(Action::Connect { addr: a });
        }
    }

    /// The address book, the ban list and the anchors, for saving. `import_state` restores them.
    pub fn export_state(&self) -> Vec<u8> {
        let book = self.book.to_bytes();
        let bans = self.bans.to_bytes();
        let anchors = crate::anchors::to_bytes(&self.current_anchors());
        let mut out = b"TNS2".to_vec();
        for part in [&book, &bans, &anchors] {
            out.extend_from_slice(&(part.len() as u32).to_le_bytes());
            out.extend_from_slice(part);
        }
        out
    }

    /// Loads what `export_state` saved. On any damage nothing is changed and the caller carries on with the
    /// seeds alone.
    pub fn import_state(&mut self, data: &[u8]) -> Result<(), String> {
        // TNS1 (before anchors) is still read: it has no anchors
        let has_anchors = match data.get(..4) {
            Some(b"TNS2") => true,
            Some(b"TNS1") => false,
            _ => return Err("not a saved state".into()),
        };
        if data.len() < 12 {
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
        let anchor_bytes = if has_anchors {
            Some(part(&mut pos)?)
        } else {
            None
        };
        if pos != data.len() {
            return Err("trailing bytes".into());
        }
        let mut book = AddrBook::from_bytes(self.cfg.addrbook.clone(), book_bytes)?;
        let bans = BanList::from_bytes(ban_bytes)?;
        let anchors = match anchor_bytes {
            Some(b) => crate::anchors::from_bytes(b)?,
            None => Vec::new(),
        };
        for seed in &self.cfg.seeds {
            book.add(seed, 0, "seed", 0);
        }
        self.book = book;
        self.bans = bans;
        self.anchors = anchors
            .into_iter()
            .take(self.cfg.anchor_count.min(crate::anchors::MAX_ANCHORS))
            .collect();
        Ok(())
    }

    pub fn addr_book(&self) -> &AddrBook {
        &self.book
    }

    /// The addresses we dialled that are connected now.
    pub fn outbound_addrs(&self) -> Vec<String> {
        self.peers
            .values()
            .filter(|p| !p.inbound && !p.feeler)
            .map(|p| p.addr.clone())
            .collect()
    }

    pub fn outbound_count(&self) -> usize {
        self.peers
            .values()
            .filter(|p| !p.inbound && !p.feeler)
            .count()
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
    /// Remembers that a reply of this kind from `peer` would now be late, not unsolicited. It is forgiven for four
    /// request timeouts (as long as a late block delivery is welcome).
    fn forgive_later(&mut self, peer: PeerId, what: Late) {
        if self.forgivable.len() >= MAX_FORGIVABLE {
            self.forgivable.remove(0);
        }
        let until = self
            .now
            .saturating_add(self.cfg.request_timeout_ms.saturating_mul(4));
        self.forgivable.push((peer, what, until));
    }

    /// Is this reply the late answer to a request that timed out? If so it is used up and counted, and the caller
    /// must not punish it.
    fn forgave(&mut self, peer: PeerId, what: &Late) -> bool {
        let now = self.now;
        let found = self
            .forgivable
            .iter()
            .position(|(p, w, until)| *p == peer && w == what && *until > now);
        match found {
            Some(i) => {
                self.forgivable.remove(i);
                self.stats.late_replies_forgiven += 1;
                true
            }
            None => false,
        }
    }

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
                self.can_serve_us(p)
                    && !p.feeler
                    && p.work > ours
                    && self.cooldown.get(id).is_none_or(|&until| until <= now)
            })
            .max_by(|a, b| a.1.work.cmp(&b.1.work).then(b.0.cmp(a.0)))
            .map(|(id, _)| *id);
        if let Some(peer) = best {
            self.start_sync(peer, out);
        }
    }

    /// Has said hello, and has not pruned past the next block we need: a peer that has would only answer
    /// `NotFound` (and be punished for it), so it is never a sync peer.
    fn can_serve_us(&self, p: &Peer) -> bool {
        let (our_height, _, _) = self.tip();
        p.hello
            .as_ref()
            .is_some_and(|h| h.pruned_below <= our_height + 1)
    }

    fn start_sync(&mut self, peer: PeerId, out: &mut Vec<Action>) {
        if !self.peers.get(&peer).is_some_and(|p| self.can_serve_us(p)) {
            return;
        }
        let (our_height, _, _) = self.tip();
        if self.cfg.assume_valid.is_some_and(|a| our_height < a.height) {
            // assume-valid: the headers first, so that the path to the checkpoint is proved before it is trusted
            self.syncing = Some(Sync {
                peer,
                started: self.now,
                phase: Phase::Headers {
                    ids: Vec::new(),
                    next_height: 0,
                    last_id: [0; 32],
                },
            });
            let locator = self.locator();
            self.send(peer, Message::GetHeaders { locator }, out);
            return;
        }
        self.start_id_sync(peer, out);
    }

    fn start_id_sync(&mut self, peer: PeerId, out: &mut Vec<Action>) {
        self.syncing = Some(Sync {
            peer,
            started: self.now,
            phase: Phase::Ids,
        });
        let locator = self.locator();
        self.send(peer, Message::GetBlockIds { locator }, out);
    }

    /// A reply to our `GetHeaders`. Every header must link to the one before (the first to a block of ours), and
    /// the one at the checkpoint's height must be the checkpoint. Only then are the ids on that path assumed valid.
    fn on_headers(
        &mut self,
        peer: PeerId,
        first_height: u64,
        headers: Vec<BlockHeader>,
        out: &mut Vec<Action>,
    ) {
        // without a checkpoint we never ask for headers, so any that arrive were not asked for (and cannot be late)
        let Some(assume) = self.cfg.assume_valid else {
            self.penalize(peer, 20, "unsolicited headers", out);
            return;
        };
        let state = match self.syncing.as_ref() {
            Some(Sync {
                peer: p,
                phase:
                    Phase::Headers {
                        ids,
                        next_height,
                        last_id,
                    },
                ..
            }) if *p == peer => Some((ids.clone(), *next_height, *last_id)),
            _ => None,
        };
        let Some((mut ids, next_height, last_id)) = state else {
            if !self.forgave(peer, &Late::Headers) {
                self.penalize(peer, 20, "unsolicited headers", out);
            }
            return;
        };
        if headers.is_empty() {
            // it has nothing more on the way to the checkpoint: fall back to the ordinary sync, full checks and all
            self.start_id_sync(peer, out);
            return;
        }
        // where the first header must link to
        let (mut height, mut prev) = if ids.is_empty() {
            // (the store has no block above our tip, so a first height beyond it finds no parent either)
            let parent = first_height
                .checked_sub(1)
                .and_then(|h| self.node.store().block_index(h).ok().flatten());
            match parent {
                Some(i) => (first_height, i.block_id),
                None => {
                    self.penalize(peer, 50, "headers that do not start at our chain", out);
                    self.abort_sync(peer, out);
                    return;
                }
            }
        } else {
            if first_height != next_height {
                self.penalize(peer, 50, "headers that do not continue", out);
                self.abort_sync(peer, out);
                return;
            }
            (next_height, last_id)
        };
        let count = headers.len();
        let mut reached = false;
        for h in &headers {
            if h.prev_id != prev {
                self.penalize(peer, 50, "headers that do not link", out);
                self.abort_sync(peer, out);
                return;
            }
            let id = block_id(h, self.node.store().pow());
            ids.push(id);
            if height == assume.height {
                if id != assume.id {
                    self.penalize(peer, 50, "not the checkpoint at the checkpoint height", out);
                    self.abort_sync(peer, out);
                    return;
                }
                reached = true;
                break;
            }
            prev = id;
            height += 1;
        }
        if reached {
            // the path from our chain to the checkpoint is proved: those blocks need not be checked in full
            let set: HashSet<[u8; 32]> = ids.iter().copied().collect();
            self.node.set_assumed(set);
            let wanted: VecDeque<[u8; 32]> =
                ids.into_iter().filter(|id| self.not_held(id)).collect();
            self.request_next_chunk(peer, wanted, out);
        } else if count < self.cfg.limits.max_headers {
            // it ran out before the checkpoint: its chain is shorter than the checkpoint, so nothing is assumed
            self.start_id_sync(peer, out);
        } else {
            let next = height;
            if let Some(Sync { phase, started, .. }) = self.syncing.as_mut() {
                *started = self.now;
                *phase = Phase::Headers {
                    ids,
                    next_height: next,
                    last_id: prev,
                };
            }
            self.send(
                peer,
                Message::GetHeaders {
                    locator: vec![prev],
                },
                out,
            );
        }
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
            if !self.forgave(peer, &Late::BlockIds) {
                self.penalize(peer, 20, "unsolicited block ids", out);
            }
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
            if self.not_held(&id) {
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
        let id = self.block_id_of(b);
        let mut result = self.apply_one(from, b, out);
        // blocks that arrived before this one and were waiting for it are now applied too (and so on down)
        if matches!(result, Applied::NewTip | Applied::Kept) {
            let mut waiting: VecDeque<[u8; 32]> = VecDeque::from([id]);
            while let Some(parent) = waiting.pop_front() {
                for kid in self.node.take_orphans_of(&parent) {
                    let kid_id = self.block_id_of(&kid);
                    match self.apply_one(None, &kid, out) {
                        Applied::NewTip => {
                            result = Applied::NewTip;
                            waiting.push_back(kid_id);
                        }
                        Applied::Kept => waiting.push_back(kid_id),
                        _ => {}
                    }
                }
            }
        }
        result
    }

    fn apply_one(&mut self, from: Option<PeerId>, b: &Block, out: &mut Vec<Action>) -> Applied {
        let was_assumed = self.node.chain().is_assumed(&self.block_id_of(b));
        match self.node.submit_block(b, self.secs()) {
            Ok(Submitted::Extended(_)) | Ok(Submitted::Reorganised { .. }) => {
                self.stats.blocks_applied += 1;
                if was_assumed {
                    self.stats.assumed_blocks += 1;
                }
                self.node
                    .prefetch_proof_of_work(self.cfg.pow_prefetch_blocks);
                // past the checkpoint: everything from here on is checked in full, and nothing stays assumed
                if let Some(a) = self.cfg.assume_valid {
                    if self.tip().0 >= a.height {
                        self.node.clear_assumed();
                    }
                }
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
        if self.on_chain(&id) || self.node.chain().holds_block(&id) {
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
                if self.forgave(peer, &Late::Tx(id)) {
                    continue;
                }
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
        let mut late_pings: Vec<(PeerId, u64)> = Vec::new();
        for (id, p) in &self.peers {
            if p.hello.is_none() {
                if now.saturating_sub(p.connected_at) > self.cfg.handshake_timeout_ms {
                    drop.push((*id, "handshake timeout"));
                }
            } else if let Some(t) = p.ping_sent {
                // an unanswered ping is asked again; only several in a row cost the connection
                if now.saturating_sub(t) > self.cfg.pong_timeout_ms {
                    late_pings.push((*id, t));
                }
            } else if now.saturating_sub(p.last_recv) > self.cfg.ping_after_ms {
                ping.push(*id);
            }
        }
        for (id, why) in drop {
            self.drop_peer(id, why, false, out);
        }
        for (id, nonce) in late_pings {
            // the ping we asked again may still be answered: that answer is late, not unsolicited
            self.forgive_later(id, Late::Pong(nonce));
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
            // what the peer owes us now: ids or headers (blocks are covered by `asked`)
            let owed = match self.syncing.as_ref().map(|s| &s.phase) {
                Some(Phase::Ids) => Some(Late::BlockIds),
                Some(Phase::Headers { .. }) => Some(Late::Headers),
                _ => None,
            };
            if let Some(what) = owed {
                self.forgive_later(peer, what);
            }
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
        let late_txs: Vec<([u8; 32], PeerId)> = self
            .req_txs
            .iter()
            .filter(|(_, r)| now.saturating_sub(r.at) > timeout)
            .map(|(id, r)| (*id, r.peer))
            .collect();
        for (id, peer) in late_txs {
            self.req_txs.remove(&id);
            self.forgive_later(peer, Late::Tx(id));
        }
        // a late reply that has waited too long to be forgiven is forgotten
        self.forgivable.retain(|(_, _, until)| *until > now);

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
