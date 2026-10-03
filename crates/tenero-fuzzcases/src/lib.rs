//! The bodies of the coverage-guided fuzz targets (`fuzz/fuzz_targets/*.rs`, run by libFuzzer through `cargo-fuzz`; M9). Each is an ordinary
//! function from bytes to nothing that **panics when a rule is broken**, so that:
//!
//! * the fuzz targets are one line each and the real work compiles, and is tested, on every platform (libFuzzer itself needs a nightly
//!   Rust and a Linux or similar machine, which the owner's Windows setup is not);
//! * a crash libFuzzer finds (a file under `fuzz/artifacts/`) is reproduced by calling the function with it, in an ordinary test or
//!   debugger, and kept as a regression test (`tests/regressions.rs`).
//!
//! What each checks (the same rules as the proptest tests that already exist, now with a fuzzer that sees which branches an input reached):
//! * [`decode_v2`]: no panic, hang or wild allocation decoding any of the nine version 2 objects, and what decodes re-encodes to the same bytes;
//! * [`wire_stream`]: the peer frame decoder, fed in chunks of any size, never panics, never yields a message after a failure and never holds
//!   more than one frame's worth;
//! * [`control_bodies`]: the control interface's requests and responses decode strictly;
//! * [`wallet_proofs`]: a signature or a payment proof of any shape is refused or checked without a panic, and what parses writes back the same;
//! * [`engine_messages`]: a real protocol engine, given a stream of connections, messages (valid or not, in any order), bad bytes and time,
//!   never panics, never sends to a peer that is gone or just ordered off (the bug the proptest harness found on 2026-10-02), only sends
//!   what the wire can carry, and keeps within its peer limits.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::OnceLock;

use tenero_chain::{ProofsNotChecked, Sha256Pow};
use tenero_core::v2::{
    Block, BlockHeader, Coinbase, CoinbaseOutput, Input, Output, PrunedBlock, PrunedTransaction,
    Transaction, Wire,
};
use tenero_net::sim::{test_chain_id, Sim, SimConfig, SimRig};
use tenero_net::{
    decode_frame, encode, Action, Engine, EngineConfig, Event, FrameDecoder, Hello, Message,
    PeerId, MAX_FRAME, PROTOCOL_VERSION,
};
use tenero_node::{Node, NodeConfig};

// ---- the decoders ------------------------------------------------------------------------------------------------------------------

fn check_wire<T: Wire>(bytes: &[u8], what: &str) {
    if let Ok(x) = T::from_bytes(bytes) {
        let back = x
            .to_bytes()
            .unwrap_or_else(|e| panic!("{what}: decoded but cannot be encoded: {e}"));
        assert_eq!(back.as_slice(), bytes, "{what}: a second encoding");
    }
}

/// Decodes `data` as each of the nine version 2 objects.
pub fn decode_v2(data: &[u8]) {
    check_wire::<Output>(data, "output");
    check_wire::<Input>(data, "input");
    check_wire::<Transaction>(data, "transaction");
    check_wire::<PrunedTransaction>(data, "pruned transaction");
    check_wire::<CoinbaseOutput>(data, "coinbase output");
    check_wire::<Coinbase>(data, "coinbase");
    check_wire::<BlockHeader>(data, "header");
    check_wire::<Block>(data, "block");
    check_wire::<PrunedBlock>(data, "pruned block");
}

// ---- the peer wire ----------------------------------------------------------------------------------------------------------------

/// The first byte sets the chunk size (1 to 64 bytes at a time); the rest is the stream. The rest is also tried whole as one frame.
pub fn wire_stream(data: &[u8]) {
    let Some((&first, rest)) = data.split_first() else {
        return;
    };
    let size = (first as usize % 64) + 1;
    if let Ok(msg) = decode_frame(rest) {
        let back =
            encode(&msg).unwrap_or_else(|e| panic!("a decoded frame cannot be encoded: {e}"));
        assert_eq!(back.as_slice(), rest, "a second encoding of a frame");
    }
    let mut d = FrameDecoder::new();
    let mut failed = false;
    for chunk in rest.chunks(size) {
        d.push(chunk);
        loop {
            match d.next_message() {
                Ok(Some(_)) => assert!(!failed, "a message after a failure"),
                Ok(None) => break,
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }
        assert!(
            d.buffered() <= MAX_FRAME + 4 + chunk.len(),
            "{} bytes held",
            d.buffered()
        );
        if failed {
            // a failed decoder stays failed and takes nothing more
            let before = d.buffered();
            d.push(&[0u8; 64]);
            assert_eq!(d.buffered(), before, "a failed decoder is still buffering");
            assert!(d.next_message().is_err(), "a failed decoder recovered");
        }
    }
}

// ---- the control interface --------------------------------------------------------------------------------------------------------

/// A control request or response body.
pub fn control_bodies(data: &[u8]) {
    use tenero_app::control::{Request, Response};
    if let Ok(r) = Request::from_body(data) {
        let back = r
            .to_body()
            .unwrap_or_else(|e| panic!("a request cannot be encoded: {e}"));
        assert_eq!(back.as_slice(), data, "a second encoding of a request");
    }
    if let Ok(r) = Response::from_body(data) {
        let back = r
            .to_body()
            .unwrap_or_else(|e| panic!("a response cannot be encoded: {e}"));
        assert_eq!(back.as_slice(), data, "a second encoding of a response");
    }
}

// ---- the wallet's signatures and payment proofs ----------------------------------------------------------------------------------

/// A fixed stream of "random" bytes, so that the output and the seeds below are the same on every run (a corpus made once keeps matching).
struct Det(u64);

impl rand_core::RngCore for Det {
    fn next_u32(&mut self) -> u32 {
        self.next_u64() as u32
    }
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        for b in dest {
            *b = (self.next_u64() >> 24) as u8;
        }
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}
impl rand_core::CryptoRng for Det {}

/// One output made for a known wallet, with honest proofs about it: what the checker is run against, and the seeds.
pub struct ProofFixture {
    pub out: tenero_wallet::proofs::OutputFields,
    pub proofs: Vec<tenero_wallet::proofs::PaymentProof>,
    pub signature: tenero_wallet::proofs::MessageSignature,
    pub address: tenero_wallet::Address,
}

pub fn proof_fixture() -> &'static ProofFixture {
    static F: OnceLock<ProofFixture> = OnceLock::new();
    F.get_or_init(|| {
        use tenero_wallet::proofs::{
            key_proof, prove_received, prove_sent, sign_message, OutputFields,
        };
        let mut rng = Det(0x1234_5678_9abc_def1);
        let keys = tenero_wallet::Keys::from_seed(&[5; 32]);
        let address = keys.address();
        let ctx = tenero_wallet::interim::tx_context(&[6; 32]);
        let e = tenero_wallet::interim::create_enote(&mut rng, &address, 4242, &ctx, 1, false)
            .expect("an output");
        let out = OutputFields {
            onetime_address: e.onetime_address,
            ephemeral_pubkey: e.ephemeral_pubkey,
            amount_commitment: e.amount_commitment,
            amount_enc: e.amount_enc,
            ctx,
            index: 1,
            public_amount: None,
        };
        let proofs = vec![
            prove_received(&keys.view_keys(), &mut rng, 9, 3, &out).expect("a proof"),
            prove_sent(&e.tx_secret, &address, &mut rng, 9, 3, &out).expect("a proof"),
            key_proof(&e.tx_secret, &address, 9, 3, &out).expect("a proof"),
        ];
        let signature = sign_message(&keys, &mut rng, b"fuzz");
        ProofFixture {
            out,
            proofs,
            signature,
            address,
        }
    })
}

/// A signature or a payment proof, as text or bytes, of any shape.
pub fn wallet_proofs(data: &[u8]) {
    use tenero_wallet::proofs::{check, verify_message, MessageSignature, PaymentProof};
    let f = proof_fixture();
    let text = String::from_utf8_lossy(data);
    let _ = MessageSignature::from_text(&text);
    if let Ok(p) = PaymentProof::from_text(&text) {
        assert_eq!(
            PaymentProof::from_text(&p.to_text()).as_ref(),
            Ok(&p),
            "a proof's text did not round-trip"
        );
    }
    if let Ok(p) = PaymentProof::from_bytes(data) {
        assert_eq!(
            p.to_bytes().as_slice(),
            data,
            "a proof's bytes did not round-trip"
        );
        let _ = check(&p, &f.out);
    }
    // a signature made of the first 64 bytes, over the rest as the message, for the honest address and for an address from the input
    if data.len() >= 64 {
        let mut sig = [0u8; 64];
        sig.copy_from_slice(&data[..64]);
        let _ = verify_message(&f.address, &data[64..], &MessageSignature(sig));
        if data.len() >= 128 {
            let a = tenero_wallet::Address {
                spend: data[64..96].try_into().expect("32 bytes"),
                view: data[96..128].try_into().expect("32 bytes"),
            };
            let _ = verify_message(&a, &data[128..], &MessageSignature(sig));
        }
    }
}

// ---- the protocol engine -----------------------------------------------------------------------------------------------------------

const T0: u64 = 1_700_000_000;
const CHAIN_LEN: usize = 14;

/// A real chain, mined once, to build honest messages from: the seeds, and what a record may say.
pub struct Fixture {
    pub ids: Vec<[u8; 32]>,
    pub works: Vec<[u8; 32]>,
    pub headers: Vec<BlockHeader>,
    pub blocks: Vec<Block>,
    pub chain_id: [u8; 32],
    pub start_ms: u64,
}

pub fn fixture() -> &'static Fixture {
    static FX: OnceLock<Fixture> = OnceLock::new();
    FX.get_or_init(|| {
        let rigs = SimRig::rigs("fuzzcases-fixture", 1);
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
        Fixture {
            ids,
            works,
            headers,
            blocks,
            chain_id: test_chain_id(),
            start_ms: (T0 + (CHAIN_LEN as u64 + 5) * 60) * 1000,
        }
    })
}

/// The engine's settings: the first byte of an input picks the defaults or tiny limits (so that the limits are reached).
fn config(variant: u8) -> EngineConfig {
    let base = EngineConfig {
        nonce: 0xfeed,
        ..EngineConfig::default()
    };
    if variant.is_multiple_of(2) {
        base
    } else {
        EngineConfig {
            max_peers: 3,
            max_inbound: 2,
            max_addr_only: 1,
            peer_target: 2,
            outbound_target: 1,
            max_outbound_per_group: 1,
            ..base
        }
    }
}

/// One thing that happens to the node, as a record in the input: `[op][peer][length: u16 little-endian][that many bytes]`.
pub mod op {
    /// A connection (the peer byte's low bit says inbound).
    pub const CONNECT: u8 = 0;
    /// A frame from a peer: the bytes are a frame body, decoded as the wire would (if they do not decode, the engine is told of bad bytes).
    pub const FRAME: u8 = 1;
    /// Time passes: the length, in tenths of a second, then a tick.
    pub const TICK: u8 = 2;
    pub const DISCONNECT: u8 = 3;
    /// An honest hello from the peer (the fixture chain's tip).
    pub const HELLO: u8 = 4;
    pub const BAD_BYTES: u8 = 5;
    /// A dial failed.
    pub const DIAL_FAILED: u8 = 6;
}

/// Builds one record of an input.
pub fn record(op: u8, peer: u8, body: &[u8]) -> Vec<u8> {
    let mut r = vec![op, peer];
    r.extend_from_slice(&(body.len().min(u16::MAX as usize) as u16).to_le_bytes());
    r.extend_from_slice(&body[..body.len().min(u16::MAX as usize)]);
    r
}

struct Harness<'a> {
    engine: Engine<'a>,
    cfg: EngineConfig,
    now_ms: u64,
    next_peer: PeerId,
    connected: BTreeSet<PeerId>,
}

impl Harness<'_> {
    fn peer(&self, selector: u8) -> Option<PeerId> {
        if self.connected.is_empty() {
            return None;
        }
        self.connected
            .iter()
            .nth(selector as usize % self.connected.len())
            .copied()
    }

    /// Gives the engine one event and checks what it did and what it now holds.
    fn feed(&mut self, ev: Event) {
        let actions = self.engine.handle(self.now_ms, ev);
        let mut ordered_off: Vec<PeerId> = vec![];
        for a in &actions {
            match a {
                Action::Send { peer, msg } => {
                    assert!(
                        self.connected.contains(peer) && !ordered_off.contains(peer),
                        "a {} to peer {peer}, who is not connected (or was just ordered off)",
                        msg.kind()
                    );
                    assert!(
                        encode(msg).is_ok(),
                        "a {} the wire cannot carry",
                        msg.kind()
                    );
                }
                Action::Disconnect { peer, .. } => {
                    assert!(
                        self.connected.contains(peer),
                        "a disconnect of {peer}, who is not connected"
                    );
                    ordered_off.push(*peer);
                }
                Action::Connect { addr } => {
                    assert!(
                        addr.parse::<SocketAddr>().is_ok(),
                        "a dial of {addr:?}, which is not ip:port"
                    );
                }
                Action::Ban { until_ms, .. } => {
                    assert!(*until_ms >= self.now_ms, "a ban that has already ended");
                }
            }
        }
        // what the transport does when told to disconnect: the connection closes and the engine is told
        for p in ordered_off {
            if self.connected.remove(&p) {
                self.feed(Event::PeerDisconnected { peer: p });
            }
        }
        let e = &self.engine;
        assert!(
            e.peer_count() <= self.cfg.max_peers + self.cfg.max_addr_only,
            "{} peers held, limit {} + {}",
            e.peer_count(),
            self.cfg.max_peers,
            self.cfg.max_addr_only
        );
        assert!(
            e.inbound_count() <= self.cfg.max_inbound + self.cfg.max_addr_only,
            "{} inbound peers held",
            e.inbound_count()
        );
    }

    fn apply(&mut self, op: u8, peer: u8, body: &[u8]) {
        let fx = fixture();
        match op % 8 {
            op::CONNECT => {
                let id = self.next_peer;
                self.next_peer += 1;
                let inbound = peer & 1 == 1;
                self.connected.insert(id);
                self.feed(Event::PeerConnected {
                    peer: id,
                    addr: tenero_net::sim::sim_addr(peer as usize % 12),
                    inbound,
                });
            }
            op::FRAME => {
                if let Some(p) = self.peer(peer) {
                    match decode_frame(body) {
                        Ok(msg) => self.feed(Event::Message { peer: p, msg }),
                        Err(e) => self.feed(Event::BadBytes {
                            peer: p,
                            why: e.to_string(),
                        }),
                    }
                }
            }
            op::TICK => {
                let tenths = body.len() as u64;
                self.now_ms += tenths * 100 * (1 + peer as u64 * 40);
                self.feed(Event::Tick);
            }
            op::DISCONNECT => {
                if let Some(p) = self.peer(peer) {
                    self.connected.remove(&p);
                    self.feed(Event::PeerDisconnected { peer: p });
                }
            }
            op::HELLO => {
                if let Some(p) = self.peer(peer) {
                    let msg = Message::Hello(Hello {
                        version: PROTOCOL_VERSION,
                        chain_id: fx.chain_id,
                        tip_height: CHAIN_LEN as u64,
                        cumulative_work: fx.works[CHAIN_LEN],
                        tip_id: fx.ids[CHAIN_LEN],
                        pruned_below: 0,
                        nonce: 1000 + p,
                    });
                    self.feed(Event::Message { peer: p, msg });
                }
            }
            op::BAD_BYTES => {
                if let Some(p) = self.peer(peer) {
                    self.feed(Event::BadBytes {
                        peer: p,
                        why: "fuzz".into(),
                    });
                }
            }
            op::DIAL_FAILED => {
                self.feed(Event::ConnectFailed {
                    addr: tenero_net::sim::sim_addr(peer as usize % 12),
                });
            }
            _ => {}
        }
    }
}

/// The first byte picks the settings; the rest is records (see [`op`]). A record cut short ends the input. Returns what the engine did
/// (the fuzz target ignores it; the tests of the seeds use it to check that a seed gets deep into the protocol).
pub fn engine_messages(data: &[u8]) -> tenero_net::Stats {
    let Some((&variant, mut rest)) = data.split_first() else {
        return tenero_net::Stats::default();
    };
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let rig = SimRig::new(
        &format!(
            "fuzzcases-{}",
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ),
        0,
    );
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
    let cfg = config(variant);
    let mut h = Harness {
        engine: Engine::new(node, cfg.clone()),
        cfg,
        now_ms: fixture().start_ms,
        next_peer: 1,
        connected: BTreeSet::new(),
    };
    let mut records = 0;
    while rest.len() >= 4 && records < 400 {
        let (op, peer) = (rest[0], rest[1]);
        let len = u16::from_le_bytes([rest[2], rest[3]]) as usize;
        if rest.len() < 4 + len {
            break;
        }
        h.apply(op, peer, &rest[4..4 + len]);
        rest = &rest[4 + len..];
        records += 1;
    }
    h.engine.stats.clone()
}

// ---- seeds ---------------------------------------------------------------------------------------------------------------------------

/// Inputs worth starting from, by target: the golden vectors for the decoders, honest frames for the wire, and an honest sync for the
/// engine (so that the fuzzer starts deep inside the protocol, where random bytes would never get).
pub fn seeds() -> Vec<(&'static str, String, Vec<u8>)> {
    let mut out = vec![];
    if let Ok(v) = tenero_core::vectors::load("v2_serialization") {
        for (i, c) in v["valid"].as_array().into_iter().flatten().enumerate() {
            if let Some(h) = c["hex"]
                .as_str()
                .and_then(|h| tenero_core::vectors::hex(h).ok())
            {
                out.push(("decode_v2", format!("vector-{i}"), h));
            }
        }
    }
    let pf = proof_fixture();
    for (i, p) in pf.proofs.iter().enumerate() {
        out.push(("wallet_proofs", format!("proof-bytes-{i}"), p.to_bytes()));
        out.push((
            "wallet_proofs",
            format!("proof-text-{i}"),
            p.to_text().into_bytes(),
        ));
    }
    out.push((
        "wallet_proofs",
        "signature-text".into(),
        pf.signature.to_text().into_bytes(),
    ));
    out.push((
        "wallet_proofs",
        "signature-bytes".into(),
        pf.signature.0.to_vec(),
    ));
    let fx = fixture();
    let honest: Vec<(&str, Message)> = vec![
        ("ping", Message::Ping(7)),
        ("get-addrs", Message::GetAddrs),
        (
            "get-block-ids",
            Message::GetBlockIds {
                locator: vec![fx.ids[0]],
            },
        ),
        (
            "block-ids",
            Message::BlockIds {
                first_height: 1,
                ids: fx.ids[1..].to_vec(),
            },
        ),
        (
            "headers",
            Message::Headers {
                first_height: 1,
                headers: fx.headers.clone(),
            },
        ),
        (
            "blocks",
            Message::Blocks {
                blocks: fx.blocks[..4].to_vec(),
            },
        ),
        (
            "new-block",
            Message::NewBlock {
                id: fx.ids[CHAIN_LEN],
                height: CHAIN_LEN as u64,
                cumulative_work: fx.works[CHAIN_LEN],
            },
        ),
    ];
    let mut engine_sync = vec![0u8]; // the default settings
    engine_sync.extend(record(op::CONNECT, 0, &[]));
    engine_sync.extend(record(op::HELLO, 0, &[]));
    for (name, msg) in &honest {
        if let Ok(bytes) = encode(msg) {
            let mut stream = vec![7u8]; // chunks of 8 bytes
            stream.extend_from_slice(&bytes);
            out.push(("wire_stream", name.to_string(), stream));
            let mut one = vec![0u8];
            one.extend(record(op::CONNECT, 0, &[]));
            one.extend(record(op::HELLO, 0, &[]));
            one.extend(record(op::FRAME, 0, &bytes));
            out.push(("engine_messages", name.to_string(), one));
            if matches!(
                msg,
                Message::BlockIds { .. } | Message::Headers { .. } | Message::Blocks { .. }
            ) {
                engine_sync.extend(record(op::FRAME, 0, &bytes));
            }
        }
    }
    engine_sync.extend(record(op::TICK, 3, &[0u8; 20]));
    out.push(("engine_messages", "an honest sync".to_string(), engine_sync));
    out
}
