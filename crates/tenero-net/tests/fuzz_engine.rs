//! Fuzzing the protocol engine's message handlers (M9; proptest, a dev-dependency).
//!
//! A random but *plausible* peer: it connects and disconnects, says hello (honestly or not), sends every kind of message with
//! contents drawn from a real chain (real ids, real headers, real blocks) or made up, and answers the engine's own requests
//! honestly, or honestly with one thing wrong. The clock jumps about. After every event the engine must keep its promises:
//!
//! * it never panics (the test fails by panicking);
//! * every message it sends is one the wire format can carry (`encode` succeeds);
//! * it sends only to peers it was told are connected, and never after it ordered a disconnect;
//! * it dials only `ip:port` addresses;
//! * it never holds more peers than its settings allow, and no peer it still holds has reached the ban score;
//! * the chain's cumulative work never goes down;
//! * what it exports as state stays small;
//! * and the same events always give the same actions (the engine reads no clock and no randomness of its own).
//!
//! Not covered: the transport (sockets and Noise are in `transport.rs` and `noise.rs`), the proof checks (the test chain uses
//! SHA-256 and no signatures), and nothing here is coverage-guided; `cargo-fuzz` is the planned next step.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use proptest::prelude::*;
use tenero_chain::{ProofsNotChecked, Sha256Pow};
use tenero_core::u256::U256;
use tenero_core::v2::{Block, BlockHeader, Transaction, Wire};
use tenero_net::message::PeerAddr;
use tenero_net::sim::{sim_addr, test_chain_id, Sim, SimConfig, SimRig};
use tenero_net::{
    encode, Action, Engine, EngineConfig, Event, Hello, Message, PeerId, PROTOCOL_VERSION,
};
use tenero_node::{Node, NodeConfig};

const T0: u64 = 1_700_000_000;
const CHAIN_LEN: usize = 14;

/// A real chain, mined once, to draw plausible message contents from. `ids[h]` is the id at height `h` (0 is the genesis).
struct Fixture {
    ids: Vec<[u8; 32]>,
    works: Vec<[u8; 32]>,
    headers: Vec<BlockHeader>,
    blocks: Vec<Block>,
    tx: Transaction,
    start_ms: u64,
    /// worked out once: it opens a scratch store each time, and two cases would collide
    chain_id: [u8; 32],
}

fn fixture() -> &'static Fixture {
    static FX: OnceLock<Fixture> = OnceLock::new();
    FX.get_or_init(|| {
        let rigs = SimRig::rigs("fuzz-fixture", 1);
        let mut sim = Sim::new(&rigs, T0, SimConfig::default(), EngineConfig::default());
        sim.mine_chain(0, CHAIN_LEN);
        let store = &rigs[0].store;
        let (mut ids, mut works, mut headers, mut blocks) = (vec![], vec![], vec![], vec![]);
        for h in 0..=CHAIN_LEN as u64 {
            let idx = store.block_index(h).unwrap().unwrap();
            ids.push(idx.block_id);
            works.push(idx.cumulative_work);
            if h > 0 {
                headers.push(idx.header.clone());
                blocks.push(store.get_block(h).unwrap().unwrap().into_full().unwrap());
            }
        }
        let v = tenero_core::vectors::load("v2_serialization").unwrap();
        let case = v["valid"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| {
                c["kind"] == "transaction" && c["note"].as_str().unwrap().contains("2 inputs")
            })
            .unwrap();
        let tx = Transaction::from_bytes(
            &tenero_core::vectors::hex(case["hex"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        Fixture {
            ids,
            works,
            headers,
            blocks,
            tx,
            chain_id: test_chain_id(),
            // a little after the chain's last block
            start_ms: (T0 + (CHAIN_LEN as u64 + 5) * 60) * 1000,
        }
    })
}

// ---- what a step can be ----------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum IdSpec {
    Real(usize),
    Rand([u8; 32]),
    Zero,
}

fn id_spec() -> impl Strategy<Value = IdSpec> {
    prop_oneof![
        6 => (0usize..CHAIN_LEN + 1).prop_map(IdSpec::Real),
        2 => any::<[u8; 32]>().prop_map(IdSpec::Rand),
        1 => Just(IdSpec::Zero),
    ]
}

fn ids(max: usize) -> impl Strategy<Value = Vec<IdSpec>> {
    proptest::collection::vec(id_spec(), 0..max)
}

fn resolve(s: &IdSpec) -> [u8; 32] {
    match s {
        IdSpec::Real(i) => fixture().ids[*i % fixture().ids.len()],
        IdSpec::Rand(b) => *b,
        IdSpec::Zero => [0; 32],
    }
}

#[derive(Clone, Debug)]
enum MsgSpec {
    Hello {
        good_version: bool,
        good_chain: bool,
        tip_height: u64,
        work: Result<usize, [u8; 32]>,
        tip: IdSpec,
        pruned_below: u64,
        nonce: u64,
    },
    Ping(u64),
    Pong(u64),
    GetBlockIds(Vec<IdSpec>),
    BlockIds(u64, Vec<IdSpec>),
    GetHeaders(Vec<IdSpec>),
    Headers(u64, Vec<usize>),
    GetBlocks(Vec<IdSpec>),
    Blocks(Vec<usize>, Option<BlockEdit>),
    NotFound(Vec<IdSpec>),
    NewBlock(IdSpec, u64, Result<usize, [u8; 32]>),
    NewTx(Vec<IdSpec>),
    GetTxs(Vec<IdSpec>),
    Txs(usize),
    GetAddrs,
    Addrs(Vec<([u8; 16], u16, u64)>),
}

#[derive(Clone, Copy, Debug)]
enum BlockEdit {
    Nonce,
    Parent,
    Timestamp,
    TxRoot,
    CoinbaseHeight,
    Version,
    AddTransaction,
}

fn work() -> impl Strategy<Value = Result<usize, [u8; 32]>> {
    prop_oneof![
        3 => (0usize..CHAIN_LEN + 1).prop_map(Ok),
        1 => any::<[u8; 32]>().prop_map(Err),
    ]
}

fn msg_spec() -> impl Strategy<Value = MsgSpec> {
    let edit = prop_oneof![
        Just(BlockEdit::Nonce),
        Just(BlockEdit::Parent),
        Just(BlockEdit::Timestamp),
        Just(BlockEdit::TxRoot),
        Just(BlockEdit::CoinbaseHeight),
        Just(BlockEdit::Version),
        Just(BlockEdit::AddTransaction),
    ];
    prop_oneof![
        3 => (
            any::<bool>(),
            any::<bool>(),
            prop_oneof![0u64..20, any::<u64>()],
            work(),
            id_spec(),
            prop_oneof![Just(0u64), 0u64..20, any::<u64>()],
            prop_oneof![Just(0u64), any::<u64>()],
        )
            .prop_map(|(gv, gc, th, w, t, pb, n)| MsgSpec::Hello {
                good_version: gv || n % 7 != 0,
                good_chain: gc || n % 5 != 0,
                tip_height: th,
                work: w,
                tip: t,
                pruned_below: pb,
                nonce: n,
            }),
        1 => any::<u64>().prop_map(MsgSpec::Ping),
        1 => any::<u64>().prop_map(MsgSpec::Pong),
        1 => ids(40).prop_map(MsgSpec::GetBlockIds),
        3 => (prop_oneof![0u64..20, any::<u64>()], ids(600)).prop_map(|(h, i)| MsgSpec::BlockIds(h, i)),
        1 => ids(40).prop_map(MsgSpec::GetHeaders),
        3 => (prop_oneof![0u64..20, any::<u64>()], proptest::collection::vec(0usize..CHAIN_LEN, 0..520))
            .prop_map(|(h, p)| MsgSpec::Headers(h, p)),
        1 => ids(40).prop_map(MsgSpec::GetBlocks),
        4 => (proptest::collection::vec(0usize..CHAIN_LEN, 0..40), proptest::option::of(edit))
            .prop_map(|(p, e)| MsgSpec::Blocks(p, e)),
        2 => ids(70).prop_map(MsgSpec::NotFound),
        3 => (id_spec(), prop_oneof![0u64..20, any::<u64>()], work()).prop_map(|(i, h, w)| MsgSpec::NewBlock(i, h, w)),
        2 => ids(70).prop_map(MsgSpec::NewTx),
        1 => ids(70).prop_map(MsgSpec::GetTxs),
        2 => (0usize..70).prop_map(MsgSpec::Txs),
        1 => Just(MsgSpec::GetAddrs),
        2 => proptest::collection::vec((any::<[u8; 16]>(), any::<u16>(), any::<u64>()), 0..120).prop_map(MsgSpec::Addrs),
    ]
}

fn edit_block(b: &mut Block, e: BlockEdit) {
    match e {
        BlockEdit::Nonce => b.header.nonce = b.header.nonce.wrapping_add(1),
        BlockEdit::Parent => b.header.prev_id[0] ^= 1,
        BlockEdit::Timestamp => b.header.timestamp = b.header.timestamp.wrapping_add(7),
        BlockEdit::TxRoot => b.header.tx_root[0] ^= 1,
        BlockEdit::CoinbaseHeight => b.coinbase.height = b.coinbase.height.wrapping_add(1),
        BlockEdit::Version => b.header.version = b.header.version.wrapping_add(1),
        BlockEdit::AddTransaction => b.transactions.push(fixture().tx.clone()),
    }
}

fn work_of(w: &Result<usize, [u8; 32]>) -> [u8; 32] {
    match w {
        Ok(i) => fixture().works[*i % fixture().works.len()],
        Err(b) => *b,
    }
}

fn build(spec: &MsgSpec) -> Message {
    let fx = fixture();
    match spec {
        MsgSpec::Hello {
            good_version,
            good_chain,
            tip_height,
            work,
            tip,
            pruned_below,
            nonce,
        } => Message::Hello(Hello {
            version: if *good_version {
                PROTOCOL_VERSION
            } else {
                PROTOCOL_VERSION + 1
            },
            chain_id: if *good_chain { fx.chain_id } else { [9; 32] },
            tip_height: *tip_height,
            cumulative_work: work_of(work),
            tip_id: resolve(tip),
            pruned_below: *pruned_below,
            nonce: *nonce,
        }),
        MsgSpec::Ping(n) => Message::Ping(*n),
        MsgSpec::Pong(n) => Message::Pong(*n),
        MsgSpec::GetBlockIds(l) => Message::GetBlockIds {
            locator: l.iter().map(resolve).collect(),
        },
        MsgSpec::BlockIds(h, l) => Message::BlockIds {
            first_height: *h,
            ids: l.iter().map(resolve).collect(),
        },
        MsgSpec::GetHeaders(l) => Message::GetHeaders {
            locator: l.iter().map(resolve).collect(),
        },
        MsgSpec::Headers(h, picks) => Message::Headers {
            first_height: *h,
            headers: picks
                .iter()
                .map(|p| fx.headers[*p % fx.headers.len()].clone())
                .collect(),
        },
        MsgSpec::GetBlocks(l) => Message::GetBlocks {
            ids: l.iter().map(resolve).collect(),
        },
        MsgSpec::Blocks(picks, edit) => {
            let mut blocks: Vec<Block> = picks
                .iter()
                .map(|p| fx.blocks[*p % fx.blocks.len()].clone())
                .collect();
            if let (Some(e), Some(b)) = (edit, blocks.first_mut()) {
                edit_block(b, *e);
            }
            Message::Blocks { blocks }
        }
        MsgSpec::NotFound(l) => Message::NotFound {
            ids: l.iter().map(resolve).collect(),
        },
        MsgSpec::NewBlock(i, h, w) => Message::NewBlock {
            id: resolve(i),
            height: *h,
            cumulative_work: work_of(w),
        },
        MsgSpec::NewTx(l) => Message::NewTx {
            ids: l.iter().map(resolve).collect(),
        },
        MsgSpec::GetTxs(l) => Message::GetTxs {
            ids: l.iter().map(resolve).collect(),
        },
        MsgSpec::Txs(n) => Message::Txs {
            txs: vec![fx.tx.clone(); *n],
        },
        MsgSpec::GetAddrs => Message::GetAddrs,
        MsgSpec::Addrs(a) => Message::Addrs {
            addrs: a
                .iter()
                .map(|(ip, port, t)| PeerAddr {
                    ip: *ip,
                    port: *port,
                    last_seen: *t,
                })
                .collect(),
        },
    }
}

/// One thing wrong with an otherwise honest answer.
#[derive(Clone, Copy, Debug)]
enum Flaw {
    None,
    DropLast,
    DuplicateFirst,
    Reverse,
    ShiftHeight,
    CorruptBlock(BlockEdit),
    Empty,
    Twice,
}

fn flaw() -> impl Strategy<Value = Flaw> {
    prop_oneof![
        8 => Just(Flaw::None),
        1 => Just(Flaw::DropLast),
        1 => Just(Flaw::DuplicateFirst),
        1 => Just(Flaw::Reverse),
        1 => Just(Flaw::ShiftHeight),
        1 => prop_oneof![
            Just(BlockEdit::Nonce),
            Just(BlockEdit::Parent),
            Just(BlockEdit::Timestamp),
            Just(BlockEdit::TxRoot),
            Just(BlockEdit::CoinbaseHeight),
        ]
        .prop_map(Flaw::CorruptBlock),
        1 => Just(Flaw::Empty),
        1 => Just(Flaw::Twice),
    ]
}

#[derive(Clone, Debug)]
enum Step {
    /// A new connection (inbound or outbound) from the address numbered so.
    Connect {
        inbound: bool,
        addr: u8,
    },
    /// The answer to the oldest dial the engine asked for: connected, or failed.
    AnswerDial {
        ok: bool,
    },
    Disconnect {
        slot: usize,
    },
    /// A message from the peer in this slot (or, now and then, from one already gone).
    Msg {
        slot: usize,
        spec: MsgSpec,
    },
    /// A hello claiming the real chain's real tip.
    HonestHello {
        slot: usize,
    },
    /// An answer to the oldest request the engine sent this peer, from the real chain, with perhaps one flaw.
    Respond {
        slot: usize,
        flaw: Flaw,
    },
    Bad {
        slot: usize,
    },
    /// A whole sync with the peer in this slot: an honest hello, then its requests answered from the real chain, round after
    /// round, with `flaw` in answer number `at` (this is what takes the engine into fetching and applying blocks).
    Sync {
        slot: usize,
        at: usize,
        flaw: Flaw,
    },
    Tick {
        ms: u64,
    },
    LocalBlock {
        pick: usize,
        edit: Option<BlockEdit>,
    },
    LocalTx,
    ConnectFailed {
        addr: u8,
    },
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => (any::<bool>(), 0u8..12).prop_map(|(inbound, addr)| Step::Connect { inbound, addr }),
        2 => any::<bool>().prop_map(|ok| Step::AnswerDial { ok }),
        1 => any::<usize>().prop_map(|slot| Step::Disconnect { slot }),
        8 => (any::<usize>(), msg_spec()).prop_map(|(slot, spec)| Step::Msg { slot, spec }),
        5 => any::<usize>().prop_map(|slot| Step::HonestHello { slot }),
        10 => (any::<usize>(), flaw()).prop_map(|(slot, flaw)| Step::Respond { slot, flaw }),
        1 => any::<usize>().prop_map(|slot| Step::Bad { slot }),
        8 => (any::<usize>(), 0usize..8, flaw()).prop_map(|(slot, at, flaw)| Step::Sync { slot, at, flaw }),
        5 => prop_oneof![0u64..2_000, 0u64..120_000, 0u64..200_000_000].prop_map(|ms| Step::Tick { ms }),
        1 => (0usize..CHAIN_LEN, proptest::option::of(prop_oneof![Just(BlockEdit::Nonce), Just(BlockEdit::Parent), Just(BlockEdit::TxRoot)]))
            .prop_map(|(pick, edit)| Step::LocalBlock { pick, edit }),
        1 => Just(Step::LocalTx),
        1 => (0u8..12).prop_map(|addr| Step::ConnectFailed { addr }),
    ]
}

// ---- running a case ------------------------------------------------------------------------------------------------

struct Run<'a> {
    engine: Engine<'a>,
    rig: &'a SimRig,
    cfg: EngineConfig,
    now_ms: u64,
    next_peer: PeerId,
    connected: BTreeSet<PeerId>,
    gone: Vec<PeerId>,
    /// requests the engine has sent each peer and not yet had answered by this harness
    pending: BTreeMap<PeerId, Vec<Message>>,
    dials: Vec<String>,
    trace: Vec<Action>,
    last_work: U256,
}

fn new_engine(rig: &SimRig, cfg: EngineConfig) -> Engine<'_> {
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

impl<'a> Run<'a> {
    fn new(rig: &'a SimRig, cfg: EngineConfig) -> Run<'a> {
        Run {
            engine: new_engine(rig, cfg.clone()),
            rig,
            cfg,
            now_ms: fixture().start_ms,
            next_peer: 1,
            connected: BTreeSet::new(),
            gone: vec![],
            pending: BTreeMap::new(),
            dials: vec![],
            trace: vec![],
            last_work: U256::from_be_bytes(&[0; 32]),
        }
    }

    fn peer_in(&self, slot: usize) -> Option<PeerId> {
        if self.connected.is_empty() {
            return None;
        }
        // now and then the peer is one that has already gone: the engine must ignore it
        if !self.gone.is_empty() && slot.is_multiple_of(11) {
            return Some(self.gone[slot % self.gone.len()]);
        }
        self.connected
            .iter()
            .nth(slot % self.connected.len())
            .copied()
    }

    /// Gives the engine one event and checks everything it did and everything it now holds.
    fn feed(&mut self, ev: Event) -> Result<(), TestCaseError> {
        let actions = self.engine.handle(self.now_ms, ev);
        let mut ordered_off: Vec<PeerId> = vec![];
        for a in &actions {
            match a {
                Action::Send { peer, msg } => {
                    prop_assert!(
                        self.connected.contains(peer) && !ordered_off.contains(peer),
                        "a {} to peer {peer}, who is not connected (or was just ordered off)",
                        msg.kind()
                    );
                    prop_assert!(
                        encode(msg).is_ok(),
                        "a {} the wire cannot carry: {:?}",
                        msg.kind(),
                        encode(msg).err()
                    );
                    if matches!(
                        msg,
                        Message::GetBlockIds { .. }
                            | Message::GetHeaders { .. }
                            | Message::GetBlocks { .. }
                            | Message::GetTxs { .. }
                            | Message::GetAddrs
                            | Message::Ping(_)
                    ) {
                        self.pending.entry(*peer).or_default().push(msg.clone());
                    }
                }
                Action::Disconnect { peer, .. } => {
                    prop_assert!(
                        self.connected.contains(peer),
                        "a disconnect of {peer}, who is not connected"
                    );
                    ordered_off.push(*peer);
                }
                Action::Connect { addr } => {
                    prop_assert!(
                        addr.parse::<SocketAddr>().is_ok(),
                        "a dial of {addr:?}, which is not ip:port"
                    );
                    self.dials.push(addr.clone());
                }
                Action::Ban { until_ms, .. } => {
                    prop_assert!(*until_ms >= self.now_ms, "a ban that has already ended");
                }
            }
        }
        self.trace.extend(actions);
        // what the transport does when told to disconnect: the connection closes and the engine is told
        for p in ordered_off {
            if self.connected.remove(&p) {
                self.gone.push(p);
                self.pending.remove(&p);
                self.feed(Event::PeerDisconnected { peer: p })?;
            }
        }
        self.check_state()
    }

    fn check_state(&mut self) -> Result<(), TestCaseError> {
        let e = &self.engine;
        prop_assert!(
            e.peer_count() <= self.cfg.max_peers + self.cfg.max_addr_only,
            "{} peers held, limit {} + {}",
            e.peer_count(),
            self.cfg.max_peers,
            self.cfg.max_addr_only
        );
        prop_assert!(
            e.inbound_count() <= self.cfg.max_inbound + self.cfg.max_addr_only,
            "{} inbound peers held",
            e.inbound_count()
        );
        for p in &self.connected {
            if let Some(score) = e.peer_score(*p) {
                prop_assert!(
                    score < self.cfg.ban_threshold,
                    "peer {p} is held with score {score}"
                );
            }
        }
        let (_, idx) = self.rig.store.tip().unwrap();
        let work = U256::from_be_bytes(&idx.cumulative_work);
        prop_assert!(work >= self.last_work, "the chain's work went down");
        self.last_work = work;
        prop_assert!(
            e.export_state().len() < 4 * 1024 * 1024,
            "{} bytes of exported state",
            e.export_state().len()
        );
        Ok(())
    }

    fn apply(&mut self, step: &Step) -> Result<(), TestCaseError> {
        let fx = fixture();
        match step {
            Step::Connect { inbound, addr } => {
                let peer = self.next_peer;
                self.next_peer += 1;
                self.connected.insert(peer);
                self.feed(Event::PeerConnected {
                    peer,
                    addr: sim_addr(*addr as usize),
                    inbound: *inbound,
                })?;
            }
            Step::AnswerDial { ok } => {
                if self.dials.is_empty() {
                    return Ok(());
                }
                let addr = self.dials.remove(0);
                if *ok {
                    let peer = self.next_peer;
                    self.next_peer += 1;
                    self.connected.insert(peer);
                    self.feed(Event::PeerConnected {
                        peer,
                        addr,
                        inbound: false,
                    })?;
                } else {
                    self.feed(Event::ConnectFailed { addr })?;
                }
            }
            Step::Disconnect { slot } => {
                if let Some(p) = self.peer_in(*slot) {
                    if self.connected.remove(&p) {
                        self.gone.push(p);
                        self.pending.remove(&p);
                    }
                    self.feed(Event::PeerDisconnected { peer: p })?;
                }
            }
            Step::Msg { slot, spec } => {
                if let Some(peer) = self.peer_in(*slot) {
                    self.feed(Event::Message {
                        peer,
                        msg: build(spec),
                    })?;
                }
            }
            Step::HonestHello { slot } => {
                if let Some(peer) = self.peer_in(*slot) {
                    let msg = Message::Hello(Hello {
                        version: PROTOCOL_VERSION,
                        chain_id: fx.chain_id,
                        tip_height: CHAIN_LEN as u64,
                        cumulative_work: fx.works[CHAIN_LEN],
                        tip_id: fx.ids[CHAIN_LEN],
                        pruned_below: 0,
                        nonce: 1000 + peer,
                    });
                    self.feed(Event::Message { peer, msg })?;
                }
            }
            Step::Respond { slot, flaw } => {
                if let Some(peer) = self.peer_in(*slot) {
                    let Some(req) = self.pending.get_mut(&peer).and_then(|v| {
                        if v.is_empty() {
                            None
                        } else {
                            Some(v.remove(0))
                        }
                    }) else {
                        return Ok(());
                    };
                    for msg in answer(&req, *flaw) {
                        self.feed(Event::Message { peer, msg })?;
                    }
                }
            }
            Step::Bad { slot } => {
                if let Some(peer) = self.peer_in(*slot) {
                    self.feed(Event::BadBytes {
                        peer,
                        why: "fuzz".into(),
                    })?;
                }
            }
            Step::Sync { slot, at, flaw } => {
                let Some(peer) = self.peer_in(*slot) else {
                    return Ok(());
                };
                self.apply(&Step::HonestHello { slot: *slot })?;
                for round in 0..14 {
                    // the engine may have dropped the peer meanwhile
                    if !self.connected.contains(&peer) {
                        break;
                    }
                    let Some(req) = self.pending.get_mut(&peer).and_then(|v| {
                        if v.is_empty() {
                            None
                        } else {
                            Some(v.remove(0))
                        }
                    }) else {
                        // nothing asked: let a little time pass, as the engine retries and times out on its own clock
                        self.now_ms += 300;
                        self.feed(Event::Tick)?;
                        continue;
                    };
                    let f = if round == *at { *flaw } else { Flaw::None };
                    for msg in answer(&req, f) {
                        if self.connected.contains(&peer) {
                            self.feed(Event::Message { peer, msg })?;
                        }
                    }
                }
            }
            Step::Tick { ms } => {
                self.now_ms = self.now_ms.saturating_add(*ms);
                self.feed(Event::Tick)?;
            }
            Step::LocalBlock { pick, edit } => {
                let mut b = fx.blocks[*pick % fx.blocks.len()].clone();
                if let Some(e) = edit {
                    edit_block(&mut b, *e);
                }
                self.feed(Event::LocalBlock(b))?;
            }
            Step::LocalTx => self.feed(Event::LocalTx(fx.tx.clone()))?,
            Step::ConnectFailed { addr } => {
                self.feed(Event::ConnectFailed {
                    addr: sim_addr(*addr as usize),
                })?;
            }
        }
        Ok(())
    }
}

/// What an honest peer with the real chain would answer to `req`, with the given flaw.
fn answer(req: &Message, flaw: Flaw) -> Vec<Message> {
    let fx = fixture();
    let height_of = |id: &[u8; 32]| fx.ids.iter().position(|x| x == id);
    let mut msg = match req {
        Message::GetBlockIds { locator } => {
            let from = locator.iter().find_map(height_of).unwrap_or(0);
            Message::BlockIds {
                first_height: from as u64 + 1,
                ids: fx.ids[from + 1..].to_vec(),
            }
        }
        Message::GetHeaders { locator } => {
            let from = locator.iter().find_map(height_of).unwrap_or(0);
            Message::Headers {
                first_height: from as u64 + 1,
                headers: fx.headers[from..].to_vec(),
            }
        }
        Message::GetBlocks { ids } => {
            let blocks: Vec<Block> = ids
                .iter()
                .filter_map(height_of)
                .filter(|h| *h > 0)
                .map(|h| fx.blocks[h - 1].clone())
                .collect();
            Message::Blocks { blocks }
        }
        Message::GetTxs { ids } => Message::NotFound { ids: ids.clone() },
        Message::GetAddrs => Message::Addrs {
            addrs: (0..5u8)
                .map(|i| PeerAddr {
                    ip: ipv4_mapped(sim_addr(i as usize + 20)),
                    port: 18331,
                    last_seen: T0,
                })
                .collect(),
        },
        Message::Ping(n) => Message::Pong(*n),
        other => other.clone(),
    };
    let mut twice = false;
    match (flaw, &mut msg) {
        (Flaw::None, _) => {}
        (Flaw::Empty, Message::BlockIds { ids, .. }) => ids.clear(),
        (Flaw::Empty, Message::Headers { headers, .. }) => headers.clear(),
        (Flaw::Empty, Message::Blocks { blocks }) => blocks.clear(),
        (Flaw::DropLast, Message::BlockIds { ids, .. }) => {
            ids.pop();
        }
        (Flaw::DropLast, Message::Headers { headers, .. }) => {
            headers.pop();
        }
        (Flaw::DropLast, Message::Blocks { blocks }) => {
            blocks.pop();
        }
        (Flaw::DuplicateFirst, Message::BlockIds { ids, .. }) if !ids.is_empty() => {
            ids.insert(0, ids[0])
        }
        (Flaw::DuplicateFirst, Message::Headers { headers, .. }) if !headers.is_empty() => {
            headers.insert(0, headers[0].clone())
        }
        (Flaw::DuplicateFirst, Message::Blocks { blocks }) if !blocks.is_empty() => {
            blocks.insert(0, blocks[0].clone())
        }
        (Flaw::Reverse, Message::BlockIds { ids, .. }) => ids.reverse(),
        (Flaw::Reverse, Message::Headers { headers, .. }) => headers.reverse(),
        (Flaw::Reverse, Message::Blocks { blocks }) => blocks.reverse(),
        (Flaw::ShiftHeight, Message::BlockIds { first_height, .. }) => *first_height += 1,
        (Flaw::ShiftHeight, Message::Headers { first_height, .. }) => {
            *first_height = first_height.saturating_sub(1)
        }
        (Flaw::CorruptBlock(e), Message::Blocks { blocks }) => {
            if let Some(b) = blocks.first_mut() {
                edit_block(b, e);
            }
        }
        (Flaw::Twice, _) => twice = true,
        _ => {}
    }
    if twice {
        vec![msg.clone(), msg]
    } else {
        vec![msg]
    }
}

fn ipv4_mapped(addr: String) -> [u8; 16] {
    let sa: SocketAddr = addr.parse().unwrap();
    match sa.ip() {
        std::net::IpAddr::V4(v4) => v4.to_ipv6_mapped().octets(),
        std::net::IpAddr::V6(v6) => v6.octets(),
    }
}

fn run_case(steps: &[Step], cfg: &EngineConfig) -> Result<Vec<Action>, TestCaseError> {
    run_case_stats(steps, cfg).map(|(t, _)| t)
}

fn run_case_stats(
    steps: &[Step],
    cfg: &EngineConfig,
) -> Result<(Vec<Action>, tenero_net::Stats), TestCaseError> {
    static N: AtomicU64 = AtomicU64::new(0);
    let rig = SimRig::new(&format!("fuzz-{}", N.fetch_add(1, Ordering::SeqCst)), 0);
    let mut run = Run::new(&rig, cfg.clone());
    for s in steps {
        run.apply(s)?;
    }
    let stats = run.engine.stats.clone();
    Ok((run.trace, stats))
}

/// Three settings: the defaults; assume-valid at the real chain's tip (the headers-first sync); and tiny limits (so the
/// limits are reached and the rules about them are exercised).
fn cfg_variant(v: u8) -> EngineConfig {
    let base = EngineConfig {
        nonce: 0xfeed,
        ..EngineConfig::default()
    };
    match v % 3 {
        0 => base,
        1 => EngineConfig {
            assume_valid: Some(tenero_net::AssumeValid {
                height: CHAIN_LEN as u64,
                id: fixture().ids[CHAIN_LEN],
            }),
            ..base
        },
        _ => EngineConfig {
            max_peers: 3,
            max_inbound: 2,
            max_addr_only: 1,
            peer_target: 2,
            outbound_target: 1,
            max_outbound_per_group: 1,
            ..base
        },
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 150, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn no_sequence_of_events_breaks_the_engine_s_promises(variant in 0u8..3, steps in proptest::collection::vec(step(), 1..70)) {
        run_case(&steps, &cfg_variant(variant))?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 60, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn the_same_events_always_give_the_same_actions(variant in 0u8..3, steps in proptest::collection::vec(step(), 1..50)) {
        let a = run_case(&steps, &cfg_variant(variant))?;
        let b = run_case(&steps, &cfg_variant(variant))?;
        prop_assert_eq!(a, b);
    }
}

/// A fuzzer that never gets past the front door proves nothing. Over a few hundred random cases, the engine must have been
/// taken through the deep parts of its work: peers made ready, syncs started, blocks fetched and applied, peers banned,
/// requests timed out, and replies forgiven or punished. (The thresholds are well under what is seen.)
#[test]
fn the_fuzzer_reaches_the_deep_states_it_is_meant_to() {
    use proptest::strategy::{Strategy, ValueTree};
    use proptest::test_runner::{Config, TestRunner};
    let mut runner = TestRunner::new(Config {
        failure_persistence: None,
        ..Config::default()
    });
    let strat = proptest::collection::vec(step(), 1..70);
    let mut total = std::collections::BTreeMap::<String, u64>::new();
    let mut cases_with_blocks = 0;
    let mut cases_with_bans = 0;
    // 240 cases by default; TENERO_FUZZ_CASES runs more (the cases are random each run, so a rare failure needs many)
    let n: u64 = std::env::var("TENERO_FUZZ_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(240);
    for i in 0..n {
        let steps = strat.new_tree(&mut runner).unwrap().current();
        let (_, st) = match run_case_stats(&steps, &cfg_variant((i % 3) as u8)) {
            Ok(r) => r,
            Err(e) => panic!("case {i} failed: {e:?}
steps: {steps:#?}"),
        };
        for (k, v) in st.sent.iter().chain(st.received.iter()) {
            *total.entry(format!("msg {k}")).or_default() += v;
        }
        *total.entry("blocks applied".into()).or_default() += st.blocks_applied;
        *total.entry("bans".into()).or_default() += st.bans;
        *total.entry("disconnects".into()).or_default() += st.disconnects;
        *total.entry("late replies forgiven".into()).or_default() += st.late_replies_forgiven;
        cases_with_blocks += u64::from(st.blocks_applied > 0);
        cases_with_bans += u64::from(st.bans > 0);
    }
    println!("over {n} cases: {total:#?}");
    println!(
        "cases that applied a block: {cases_with_blocks}; that banned a peer: {cases_with_bans}"
    );
    assert!(
        cases_with_blocks >= n / 5,
        "only {cases_with_blocks} cases applied a block"
    );
    assert!(
        cases_with_bans >= n / 4,
        "only {cases_with_bans} cases banned anyone"
    );
    for kind in [
        "get_block_ids",
        "block_ids",
        "get_blocks",
        "blocks",
        "get_headers",
        "headers",
        "new_block",
        "get_addrs",
        "addrs",
        "ping",
        "pong",
        "hello",
    ] {
        assert!(
            total.get(&format!("msg {kind}")).copied().unwrap_or(0) > 0,
            "no {kind} message was ever sent or received"
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 120, failure_persistence: None, ..ProptestConfig::default() })]

    /// `peers.dat` is a file an attacker with the data directory (or a damaged disk) can change: whatever is in it, loading it
    /// must not panic, and what the engine loads, it can save and load again.
    #[test]
    fn a_damaged_state_file_never_breaks_loading(
        steps in proptest::collection::vec(step(), 1..30),
        edits in proptest::collection::vec((any::<usize>(), any::<u8>(), 0u8..4), 0..5),
        junk in proptest::option::of(proptest::collection::vec(any::<u8>(), 0..400)),
    ) {
        static N: AtomicU64 = AtomicU64::new(1_000_000);
        let rig = SimRig::new(&format!("fuzz-state-{}", N.fetch_add(1, Ordering::SeqCst)), 0);
        let mut run = Run::new(&rig, cfg_variant(0));
        for s in &steps {
            run.apply(s)?;
        }
        let mut bytes = junk.unwrap_or_else(|| run.engine.export_state());
        for (p, v, how) in edits {
            if bytes.is_empty() {
                break;
            }
            let n = bytes.len();
            match how {
                0 => bytes[p % n] ^= v | 1,
                1 => bytes[p % n] = v,
                2 => bytes.truncate(p % n),
                _ => bytes.insert(p % n, v),
            }
        }
        let rig2 = SimRig::new(&format!("fuzz-state-{}", N.fetch_add(1, Ordering::SeqCst)), 0);
        let mut e2 = new_engine(&rig2, cfg_variant(0));
        if e2.import_state(&bytes).is_ok() {
            let again = e2.export_state();
            let rig3 = SimRig::new(&format!("fuzz-state-{}", N.fetch_add(1, Ordering::SeqCst)), 0);
            let mut e3 = new_engine(&rig3, cfg_variant(0));
            prop_assert!(e3.import_state(&again).is_ok(), "what an engine saved it cannot load");
        }
    }
}

/// Found by the fuzzer (6,000 cases, once in several thousand): after a failed dial and a disconnect, a peer's honest hello, then ten
/// seconds, then a request for block ids, the engine sent `GetBlockIds` to a peer that was no longer connected.
#[test]
fn a_found_case_a_block_ids_request_to_a_peer_that_is_gone() {
    let steps = vec![
        Step::Connect {
            inbound: false,
            addr: 0,
        },
        Step::Disconnect { slot: 0 },
        Step::AnswerDial { ok: false },
        Step::Connect {
            inbound: false,
            addr: 0,
        },
        Step::Connect {
            inbound: false,
            addr: 0,
        },
        Step::HonestHello {
            slot: 808575776582213578,
        },
        Step::Msg {
            slot: 6636649917422582488,
            spec: MsgSpec::NotFound(vec![]),
        },
        Step::HonestHello {
            slot: 9403484183228689622,
        },
        Step::Tick { ms: 10001 },
        Step::Msg {
            slot: 32872654927256,
            spec: MsgSpec::Pong(0),
        },
        Step::Msg {
            slot: 110681382362,
            spec: MsgSpec::GetBlockIds(vec![IdSpec::Real(1)]),
        },
    ];
    run_case(&steps, &cfg_variant(2)).unwrap();
}
