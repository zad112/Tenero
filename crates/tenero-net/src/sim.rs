//! A deterministic simulated network for the protocol engines.
//!
//! Real [`Engine`]s (each with its own store and mempool) exchange typed messages through an event queue with
//! configurable latency and message loss. **Everything is a function of the seed**: there are no threads and no
//! wall-clock time, so a failing run can be replayed exactly. It can partition and heal the network, and host
//! scripted *hostile peers*: endpoints that are not engines, controlled by a test, which can send anything.
//!
//! Blocks are really mined, on the SHA-256 test chain, and really validated.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet};
use std::path::PathBuf;

use tenero_chain::{ChainParams, ProofsNotChecked, Sha256Pow};
use tenero_core::u256::U256;
use tenero_core::v2::ids::{block_id, PowKind};
use tenero_core::v2::Block;
use tenero_node::{Node, NodeConfig, Payout};
use tenero_store::Store;

use crate::engine::{Action, Engine, EngineConfig, Event, PeerId};
use crate::message::Message;

pub const LABEL: &str = "tenero simulated network";

/// The public-looking address of simulated node `i` when `per_group` consecutive nodes share a network group
/// (an IPv4 /16): unique per node.
pub fn sim_addr_in(i: usize, per_group: usize) -> String {
    let g = i / per_group;
    format!(
        "{}.{}.{}.1:8333",
        20 + g / 200,
        1 + g % 200,
        1 + i % per_group
    )
}

/// The address of simulated node `i` when every node is in its own network group.
pub fn sim_addr(i: usize) -> String {
    sim_addr_in(i, 1)
}

/// A block on `node`'s tip, mined on the SHA-256 test chain (a nonce search that takes a few hashes), with
/// `payout` in its coinbase and the pool's best transactions in its body. **Not for a real chain.**
pub fn mine_test_block(node: &tenero_node::Node<'_>, timestamp: u64, payout: Payout) -> Block {
    let mut b = node
        .block_template(timestamp, 1_000_000, payout)
        .expect("a template");
    let target = node.next_block().expect("next block").target;
    for nonce in 0.. {
        b.header.nonce = nonce;
        if U256::from_be_bytes(&block_id(&b.header, PowKind::Sha256)) < target {
            break;
        }
    }
    b
}

/// The rules of the SHA-256 test chain (a target one hash in four meets, small rings, short maturity): what the
/// simulator, the socket tests and the test-node program all run. **Not a real chain.**
pub fn test_chain_params() -> ChainParams {
    let mut params = ChainParams::version_2(LABEL, PowKind::Sha256, U256::pow2(254).unwrap());
    params.ring_size = 2;
    params.coinbase_maturity = 1;
    params.spend_maturity = 1;
    params
}

/// The id of the test chain (what a handshake is bound to), worked out from a scratch store.
pub fn test_chain_id() -> [u8; 32] {
    let path = std::env::temp_dir().join(format!("tenero-chainid-{}.redb", std::process::id()));
    remove_db(&path);
    let id = Store::open(&path, LABEL, PowKind::Sha256)
        .expect("open a scratch store")
        .chain_id();
    remove_db(&path);
    id
}

/// One node's storage and rules; the simulation's engines borrow from these.
pub struct SimRig {
    path: PathBuf,
    pub store: Store,
    pub params: ChainParams,
}

impl SimRig {
    /// A rig on the SHA-256 test chain (a target one hash in four meets), with small rings and short
    /// maturity so a few blocks are enough to spend.
    pub fn new(tag: &str, index: usize) -> SimRig {
        let path = std::env::temp_dir().join(format!(
            "tenero-sim-{}-{tag}-{index}.redb",
            std::process::id()
        ));
        remove_db(&path);
        let store = Store::open(&path, LABEL, PowKind::Sha256).expect("open a store");
        SimRig {
            path,
            store,
            params: test_chain_params(),
        }
    }

    pub fn rigs(tag: &str, n: usize) -> Vec<SimRig> {
        (0..n).map(|i| SimRig::new(tag, i)).collect()
    }
}

impl Drop for SimRig {
    fn drop(&mut self) {
        remove_db(&self.path);
    }
}

fn remove_db(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let mut s = path.clone().into_os_string();
    s.push(".segments");
    let _ = std::fs::remove_dir_all(PathBuf::from(s));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum End {
    Node(usize),
    Hostile(usize),
}

#[derive(Clone, Debug)]
pub struct SimConfig {
    pub latency_min_ms: u64,
    pub latency_max_ms: u64,
    /// Messages dropped in transit, per thousand.
    pub drop_permille: u64,
    pub tick_ms: u64,
    pub seed: u64,
    /// Every node announces its own address to its peers.
    pub advertise: bool,
    /// How many consecutive nodes share one network group (an IPv4 /16).
    pub nodes_per_group: usize,
}

impl Default for SimConfig {
    fn default() -> SimConfig {
        SimConfig {
            latency_min_ms: 20,
            latency_max_ms: 200,
            drop_permille: 0,
            tick_ms: 1000,
            advertise: true,
            nodes_per_group: 1,
            seed: 0x2545_f491_4f6c_dd1d,
        }
    }
}

enum Ev {
    Deliver {
        to: End,
        peer: PeerId,
        msg: Message,
    },
    Disc {
        to: End,
        peer: PeerId,
    },
    /// A dial reaches its target.
    Dial {
        from: usize,
        to: usize,
        addr: String,
    },
    /// A dial that never connects.
    DialFailed {
        node: usize,
        addr: String,
    },
    /// A dial that reaches a scripted listener.
    DialHostile {
        from: usize,
        h: usize,
        addr: String,
    },
    Tick,
}

struct Sched {
    at: u64,
    seq: u64,
    ev: Ev,
}

impl PartialEq for Sched {
    fn eq(&self, o: &Self) -> bool {
        (self.at, self.seq) == (o.at, o.seq)
    }
}
impl Eq for Sched {}
impl PartialOrd for Sched {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Sched {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        (self.at, self.seq).cmp(&(o.at, o.seq))
    }
}

/// A scripted endpoint: not an engine, and free to send anything.
pub struct Hostile {
    pub addr: String,
    pub node: usize,
    /// What the node has sent it, in order.
    pub inbox: Vec<Message>,
    /// The node cut the connection (or refused it).
    pub disconnected: bool,
    peer_at_node: Option<PeerId>,
    peer_here: PeerId,
}

impl Hostile {
    /// The id the node uses for this connection (for asking the engine about it).
    pub fn peer_at_node(&self) -> Option<PeerId> {
        self.peer_at_node
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn between(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            lo
        } else {
            lo + self.next() % (hi - lo + 1)
        }
    }
    fn bytes<const N: usize>(&mut self) -> [u8; N] {
        let mut b = [0u8; N];
        for c in b.iter_mut() {
            *c = self.next() as u8;
        }
        b
    }
}

pub struct Sim<'a> {
    pub engines: Vec<Engine<'a>>,
    pub hostiles: Vec<Hostile>,
    cfg: SimConfig,
    now_ms: u64,
    seq: u64,
    queue: BinaryHeap<Reverse<Sched>>,
    /// (end, the peer id that end uses for the link) -> the other end
    links: HashMap<(End, PeerId), (End, PeerId)>,
    next_peer: PeerId,
    /// Each node's listening address, and the reverse map.
    addrs: Vec<String>,
    addr_index: HashMap<String, usize>,
    online: Vec<bool>,
    /// Scripted peers that accept dials, by the address they listen on.
    hostile_listen: HashMap<String, usize>,
    next_ephemeral: u64,
    /// The time of the last message scheduled towards each end of a link (for in-order delivery).
    last_delivery: HashMap<(End, PeerId), u64>,
    /// Which side of a partition each node is on (`None`: no partition).
    groups: Option<Vec<usize>>,
    /// Every link ever made between two nodes, to restore after a partition heals.
    topology: BTreeSet<(usize, usize)>,
    severed: BTreeSet<(usize, usize)>,
    rng: Rng,
    /// Messages sent, by kind, over the whole run (all nodes).
    pub sent_by_kind: BTreeMap<&'static str, u64>,
    /// Messages lost in transit.
    pub dropped: u64,
}

impl<'a> Sim<'a> {
    /// One engine per rig, all configured alike. `start_secs` is the simulated Unix time at the start.
    pub fn new(
        rigs: &'a [SimRig],
        start_secs: u64,
        cfg: SimConfig,
        engine: EngineConfig,
    ) -> Sim<'a> {
        Sim::with_configs(rigs, start_secs, cfg, vec![engine; rigs.len()])
    }

    /// One engine per rig, each with its own configuration (for example, different seeds).
    pub fn with_configs(
        rigs: &'a [SimRig],
        start_secs: u64,
        cfg: SimConfig,
        engines: Vec<EngineConfig>,
    ) -> Sim<'a> {
        assert_eq!(rigs.len(), engines.len());
        let n_nodes = rigs.len();
        let per_group = cfg.nodes_per_group.max(1);
        let engines = rigs
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let node = Node::with_proof_check(
                    &r.store,
                    &r.params,
                    &Sha256Pow,
                    &ProofsNotChecked,
                    NodeConfig {
                        allow_unchecked_proofs_for_tests: true,
                        ..NodeConfig::default()
                    },
                )
                .expect("a test node");
                let mut node_cfg = engines[i].clone();
                // each node chooses among addresses in its own order, and announces itself
                node_cfg.addrbook.seed ^= (i as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15);
                if node_cfg.nonce == 0 {
                    node_cfg.nonce =
                        0xA5A5_0000_0000_0000 ^ (i as u64 + 1).wrapping_mul(0x2545_f491_4f6c_dd1d);
                }
                if cfg.advertise && node_cfg.advertise.is_none() {
                    node_cfg.advertise = Some(sim_addr_in(i, per_group));
                }
                Engine::new(node, node_cfg)
            })
            .collect();
        let mut sim = Sim {
            engines,
            hostiles: Vec::new(),
            now_ms: start_secs * 1000,
            seq: 0,
            queue: BinaryHeap::new(),
            links: HashMap::new(),
            next_peer: 1,
            addrs: (0..n_nodes).map(|i| sim_addr_in(i, per_group)).collect(),
            addr_index: (0..n_nodes)
                .map(|i| (sim_addr_in(i, per_group), i))
                .collect(),
            online: vec![true; n_nodes],
            hostile_listen: HashMap::new(),
            next_ephemeral: 40_000,
            last_delivery: HashMap::new(),
            groups: None,
            topology: BTreeSet::new(),
            severed: BTreeSet::new(),
            rng: Rng(cfg.seed),
            sent_by_kind: BTreeMap::new(),
            dropped: 0,
            cfg,
        };
        sim.schedule(sim.cfg.tick_ms, Ev::Tick);
        sim
    }

    /// Changes the message-loss rate (per thousand) from now on, e.g. after the handshakes are done.
    pub fn set_drop_permille(&mut self, p: u64) {
        self.cfg.drop_permille = p;
    }

    pub fn now_ms(&self) -> u64 {
        self.now_ms
    }

    pub fn node_count(&self) -> usize {
        self.engines.len()
    }

    fn schedule(&mut self, delay: u64, ev: Ev) {
        self.seq += 1;
        self.queue.push(Reverse(Sched {
            at: self.now_ms + delay,
            seq: self.seq,
            ev,
        }));
    }

    /// Schedules a message towards `(to, peer)`, never earlier than the previous one on that link: a link
    /// delivers in order, as TCP does.
    fn deliver(&mut self, to: End, peer: PeerId, msg: Message) {
        let lat = self.latency();
        let earliest = self
            .last_delivery
            .get(&(to, peer))
            .map_or(0, |&t| t.saturating_sub(self.now_ms));
        let delay = lat.max(earliest);
        self.last_delivery.insert((to, peer), self.now_ms + delay);
        self.schedule(delay, Ev::Deliver { to, peer, msg });
    }

    fn latency(&mut self) -> u64 {
        self.rng
            .between(self.cfg.latency_min_ms, self.cfg.latency_max_ms)
    }

    fn separated(&self, a: End, b: End) -> bool {
        match (&self.groups, a, b) {
            (Some(g), End::Node(x), End::Node(y)) => g[x] != g[y],
            _ => false,
        }
    }

    // ---- topology ---------------------------------------------------------------------------------

    /// Connects node `a` (the dialer) to node `b`. Returns false if a partition or an offline node forbids it.
    pub fn connect(&mut self, a: usize, b: usize) -> bool {
        if self.separated(End::Node(a), End::Node(b)) || !self.online[a] || !self.online[b] {
            return false;
        }
        let dialled = self.addrs[b].clone();
        self.establish(a, b, dialled);
        true
    }

    /// The link itself: `a` dialled `dialled` (which is `b`'s listening address).
    fn establish(&mut self, a: usize, b: usize, dialled: String) {
        let (pa, pb) = (self.next_peer, self.next_peer + 1);
        self.next_peer += 2;
        self.links.insert((End::Node(a), pa), (End::Node(b), pb));
        self.links.insert((End::Node(b), pb), (End::Node(a), pa));
        self.topology.insert((a.min(b), a.max(b)));
        let now = self.now_ms;
        let out = self.engines[a].handle(
            now,
            Event::PeerConnected {
                peer: pa,
                addr: dialled,
                inbound: false,
            },
        );
        self.process(End::Node(a), out);
        // the receiving node sees the dialler's IP and an ephemeral port, as on a real socket
        self.next_ephemeral += 1;
        let from_ip = self.addrs[a]
            .rsplit_once(':')
            .map_or("0.0.0.0", |x| x.0)
            .to_string();
        let out = self.engines[b].handle(
            now,
            Event::PeerConnected {
                peer: pb,
                addr: format!("{from_ip}:{}", self.next_ephemeral),
                inbound: true,
            },
        );
        self.process(End::Node(b), out);
    }

    pub fn addr_of(&self, node: usize) -> &str {
        &self.addrs[node]
    }

    /// Changes the address node `node` listens on (before it connects to anything), e.g. to crowd one network
    /// group with many nodes.
    pub fn set_node_addr(&mut self, node: usize, addr: &str) {
        self.addr_index.remove(&self.addrs[node]);
        self.addrs[node] = addr.to_string();
        self.addr_index.insert(addr.to_string(), node);
    }

    pub fn is_online(&self, node: usize) -> bool {
        self.online[node]
    }

    /// Takes a node off the network (its links are cut and dials to it fail) or brings it back. Its state is
    /// kept; when it returns, its own connection manager finds peers again.
    pub fn set_online(&mut self, node: usize, online: bool) {
        self.online[node] = online;
        if online {
            return;
        }
        let cut: Vec<(End, PeerId)> = self
            .links
            .keys()
            .filter(|(e, _)| *e == End::Node(node))
            .copied()
            .collect();
        for (end, peer) in cut {
            if let Some((other, other_peer)) = self.links.remove(&(end, peer)) {
                self.links.remove(&(other, other_peer));
                self.schedule(1, Ev::Disc { to: end, peer });
                self.schedule(
                    1,
                    Ev::Disc {
                        to: other,
                        peer: other_peer,
                    },
                );
            }
        }
    }

    /// Ends a partition without reconnecting anything: the nodes' own connection managers do that.
    pub fn heal_quiet(&mut self) {
        self.groups = None;
        self.severed.clear();
    }

    /// Connects every pair of nodes (a full mesh).
    pub fn connect_all(&mut self) {
        for a in 0..self.engines.len() {
            for b in (a + 1)..self.engines.len() {
                self.connect(a, b);
            }
        }
    }

    /// Splits the network: nodes in different groups cannot exchange messages, and existing links across the
    /// split are cut.
    pub fn partition(&mut self, groups: &[Vec<usize>]) {
        let mut g = vec![usize::MAX; self.engines.len()];
        for (i, members) in groups.iter().enumerate() {
            for &m in members {
                g[m] = i;
            }
        }
        self.groups = Some(g);
        let cut: Vec<(End, PeerId)> = self
            .links
            .iter()
            .filter(|((e, _), (o, _))| self.separated(*e, *o))
            .map(|(k, _)| *k)
            .collect();
        for (end, peer) in cut {
            if let Some((other, other_peer)) = self.links.remove(&(end, peer)) {
                self.links.remove(&(other, other_peer));
                if let (End::Node(x), End::Node(y)) = (end, other) {
                    self.severed.insert((x.min(y), x.max(y)));
                }
                self.schedule(1, Ev::Disc { to: end, peer });
                self.schedule(
                    1,
                    Ev::Disc {
                        to: other,
                        peer: other_peer,
                    },
                );
            }
        }
    }

    /// Ends the partition and reconnects the links it cut.
    pub fn heal(&mut self) {
        self.groups = None;
        let again: Vec<(usize, usize)> = std::mem::take(&mut self.severed).into_iter().collect();
        for (a, b) in again {
            self.connect(a, b);
        }
    }

    // ---- hostile peers ----------------------------------------------------------------------------

    /// A scripted peer connecting to `node` from the address `addr`. It sees the node's messages in
    /// `hostiles[h].inbox`.
    pub fn add_hostile(&mut self, node: usize, addr: &str) -> usize {
        let h = self.hostiles.len();
        self.hostiles.push(Hostile {
            addr: addr.to_string(),
            node,
            inbox: Vec::new(),
            disconnected: false,
            peer_at_node: None,
            peer_here: 0,
        });
        self.hostile_connect(h);
        h
    }

    /// A scripted peer that LISTENS on `addr`: a node that dials that address reaches it (the node is then
    /// `hostiles[h].node`, and this peer sees a dialler, so it can answer `GetAddrs` with anything). Until it
    /// is dialled it is `disconnected`.
    pub fn add_hostile_listener(&mut self, addr: &str) -> usize {
        let h = self.hostiles.len();
        self.hostiles.push(Hostile {
            addr: addr.to_string(),
            node: usize::MAX,
            inbox: Vec::new(),
            disconnected: true,
            peer_at_node: None,
            peer_here: 0,
        });
        self.hostile_listen.insert(addr.to_string(), h);
        h
    }

    /// (Re)connects a hostile peer; the node may refuse it (a banned address), which shows as `disconnected`.
    pub fn hostile_connect(&mut self, h: usize) {
        let (pn, ph) = (self.next_peer, self.next_peer + 1);
        self.next_peer += 2;
        let node = self.hostiles[h].node;
        self.links
            .insert((End::Node(node), pn), (End::Hostile(h), ph));
        self.links
            .insert((End::Hostile(h), ph), (End::Node(node), pn));
        let hs = &mut self.hostiles[h];
        hs.disconnected = false;
        hs.peer_at_node = Some(pn);
        hs.peer_here = ph;
        let addr = hs.addr.clone();
        let now = self.now_ms;
        let out = self.engines[node].handle(
            now,
            Event::PeerConnected {
                peer: pn,
                addr,
                inbound: true,
            },
        );
        self.process(End::Node(node), out);
    }

    /// The hostile peer sends `msg` to its node.
    pub fn hostile_send(&mut self, h: usize, msg: Message) {
        let ph = self.hostiles[h].peer_here;
        if let Some(&(to, peer)) = self.links.get(&(End::Hostile(h), ph)) {
            self.deliver(to, peer, msg);
        }
    }

    // ---- running ----------------------------------------------------------------------------------

    fn process(&mut self, from: End, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::Send { peer, msg } => {
                    *self.sent_by_kind.entry(msg.kind()).or_default() += 1;
                    let Some(&(to, to_peer)) = self.links.get(&(from, peer)) else {
                        continue;
                    };
                    if self.separated(from, to) {
                        self.dropped += 1;
                        continue;
                    }
                    if self.cfg.drop_permille > 0 && self.rng.next() % 1000 < self.cfg.drop_permille
                    {
                        self.dropped += 1;
                        continue;
                    }
                    self.deliver(to, to_peer, msg);
                }
                Action::Disconnect { peer, .. } => {
                    if let Some((to, to_peer)) = self.links.remove(&(from, peer)) {
                        // a close arrives AFTER everything sent before it, as on a real TCP connection: the
                        // far end's side of the link stays until then
                        let lat = self.latency();
                        let earliest = self
                            .last_delivery
                            .get(&(to, to_peer))
                            .map_or(0, |&t| t.saturating_sub(self.now_ms) + 1);
                        self.schedule(lat.max(earliest), Ev::Disc { to, peer: to_peer });
                    } else if let End::Node(_) = from {
                        // refused before a link record existed on the far side: nothing to tell
                    }
                    // a refusal of a hostile peer's connection has no live link on its side yet
                    for h in self.hostiles.iter_mut() {
                        if End::Node(h.node) == from && h.peer_at_node == Some(peer) {
                            h.disconnected = true;
                        }
                    }
                }
                Action::Connect { addr } => {
                    let End::Node(a) = from else { continue };
                    let lat = self.latency();
                    if let Some(&h) = self.hostile_listen.get(&addr) {
                        if self.online[a] && self.hostiles[h].disconnected {
                            self.schedule(2 * lat, Ev::DialHostile { from: a, h, addr });
                        } else {
                            self.schedule(3000, Ev::DialFailed { node: a, addr });
                        }
                        continue;
                    }
                    let target = self.addr_index.get(&addr).copied();
                    match target {
                        Some(b)
                            if b != a
                                && self.online[b]
                                && self.online[a]
                                && !self.separated(End::Node(a), End::Node(b)) =>
                        {
                            self.schedule(
                                2 * lat,
                                Ev::Dial {
                                    from: a,
                                    to: b,
                                    addr,
                                },
                            );
                        }
                        // nobody there (or unreachable): the dial times out
                        _ => self.schedule(3000, Ev::DialFailed { node: a, addr }),
                    }
                }
                Action::Ban { .. } => {}
            }
        }
    }

    fn step(&mut self) -> bool {
        let Some(Reverse(s)) = self.queue.pop() else {
            return false;
        };
        self.now_ms = self.now_ms.max(s.at);
        match s.ev {
            Ev::Tick => {
                for i in 0..self.engines.len() {
                    let out = self.engines[i].handle(self.now_ms, Event::Tick);
                    self.process(End::Node(i), out);
                }
                self.schedule(self.cfg.tick_ms, Ev::Tick);
            }
            Ev::Dial { from, to, addr } => {
                if self.online[from]
                    && self.online[to]
                    && !self.separated(End::Node(from), End::Node(to))
                {
                    self.establish(from, to, addr);
                } else {
                    let out = self.engines[from].handle(self.now_ms, Event::ConnectFailed { addr });
                    self.process(End::Node(from), out);
                }
            }
            Ev::DialHostile { from, h, addr } => {
                if self.online[from] && self.hostiles[h].disconnected {
                    let (pn, ph) = (self.next_peer, self.next_peer + 1);
                    self.next_peer += 2;
                    self.links
                        .insert((End::Node(from), pn), (End::Hostile(h), ph));
                    self.links
                        .insert((End::Hostile(h), ph), (End::Node(from), pn));
                    let hs = &mut self.hostiles[h];
                    hs.node = from;
                    hs.disconnected = false;
                    hs.peer_at_node = Some(pn);
                    hs.peer_here = ph;
                    let out = self.engines[from].handle(
                        self.now_ms,
                        Event::PeerConnected {
                            peer: pn,
                            addr,
                            inbound: false,
                        },
                    );
                    self.process(End::Node(from), out);
                } else {
                    let out = self.engines[from].handle(self.now_ms, Event::ConnectFailed { addr });
                    self.process(End::Node(from), out);
                }
            }
            Ev::DialFailed { node, addr } => {
                let out = self.engines[node].handle(self.now_ms, Event::ConnectFailed { addr });
                self.process(End::Node(node), out);
            }
            Ev::Deliver { to, peer, msg } => {
                // a message on a link that has since been cut is lost
                if !self.links.contains_key(&(to, peer)) {
                    return true;
                }
                match to {
                    End::Node(i) => {
                        let out = self.engines[i].handle(self.now_ms, Event::Message { peer, msg });
                        self.process(End::Node(i), out);
                    }
                    End::Hostile(h) => self.hostiles[h].inbox.push(msg),
                }
            }
            Ev::Disc { to, peer } => {
                self.links.remove(&(to, peer));
                match to {
                    End::Node(i) => {
                        let out =
                            self.engines[i].handle(self.now_ms, Event::PeerDisconnected { peer });
                        self.process(End::Node(i), out);
                    }
                    End::Hostile(h) => self.hostiles[h].disconnected = true,
                }
            }
        }
        true
    }

    /// Runs the simulation for `ms` of simulated time.
    pub fn run_for(&mut self, ms: u64) {
        let until = self.now_ms + ms;
        while self.queue.peek().is_some_and(|Reverse(s)| s.at <= until) {
            self.step();
        }
        self.now_ms = until;
    }

    /// Runs until `done` holds (checked after every event), or `limit_ms` of simulated time has passed.
    /// Returns whether it held.
    pub fn run_until(&mut self, limit_ms: u64, mut done: impl FnMut(&Sim<'a>) -> bool) -> bool {
        let until = self.now_ms + limit_ms;
        while self.queue.peek().is_some_and(|Reverse(s)| s.at <= until) {
            self.step();
            if done(self) {
                return true;
            }
        }
        self.now_ms = until;
        done(self)
    }

    // ---- mining and observing ---------------------------------------------------------------------

    /// Mines a block on `node`'s tip at simulated time `timestamp_secs` (or the sim clock if `None`),
    /// hands it to the node, and returns it. The node announces it to its peers.
    pub fn mine(&mut self, node: usize, timestamp_secs: Option<u64>) -> Block {
        let ts = timestamp_secs.unwrap_or(self.now_ms / 1000);
        let payout = Payout {
            onetime_address: self.rng.bytes(),
            view_tag: self.rng.bytes(),
            ephemeral_pubkey: self.rng.bytes(),
            anchor_enc: self.rng.bytes(),
        };
        let b = mine_test_block(self.engines[node].node(), ts, payout);
        let out = self.engines[node].handle(self.now_ms, Event::LocalBlock(b.clone()));
        self.process(End::Node(node), out);
        b
    }

    /// Mines `n` blocks on `node` with timestamps a minute apart (so the difficulty stays put). The clock
    /// jumps forward to each block's timestamp without running the events in between, so a long chain is
    /// quick to build; queued events then run late, never early.
    pub fn mine_chain(&mut self, node: usize, n: usize) {
        for _ in 0..n {
            let (_, tip) = self.engines[node].node().store().tip().unwrap();
            // a chain keeps its own pace, a minute a block, whatever the other side of a partition did
            let ts = if self.engines[node].node().store().tip().unwrap().0 == 0 {
                self.now_ms / 1000
            } else {
                tip.header.timestamp + 60
            };
            self.now_ms = self.now_ms.max(ts * 1000);
            self.mine(node, Some(ts));
        }
    }

    pub fn tip(&self, node: usize) -> (u64, [u8; 32]) {
        let (h, i) = self.engines[node].node().store().tip().unwrap();
        (h, i.block_id)
    }

    /// Every listed node has the same tip.
    pub fn agree(&self, nodes: &[usize]) -> bool {
        let first = self.tip(nodes[0]).1;
        nodes.iter().all(|&n| self.tip(n).1 == first)
    }

    pub fn all_agree(&self) -> bool {
        let all: Vec<usize> = (0..self.engines.len()).collect();
        self.agree(&all)
    }

    /// Distinct tips among all nodes.
    pub fn distinct_tips(&self) -> usize {
        (0..self.engines.len())
            .map(|n| self.tip(n).1)
            .collect::<HashSet<_>>()
            .len()
    }

    /// Submits a transaction at `node` as its own.
    pub fn submit_tx(&mut self, node: usize, tx: tenero_core::v2::Transaction) {
        let out = self.engines[node].handle(self.now_ms, Event::LocalTx(tx));
        self.process(End::Node(node), out);
    }
}
