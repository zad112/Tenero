//! The control protocol: its encoding, and the server and client on real sockets with a real node, including a
//! wallet paying through it. A test chain (SHA-256 proof of work) with the real proof check. **Not a real chain.**

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rand_core::OsRng;
use tenero_app::client::{create_cookie, read_cookie, RemoteNode};
use tenero_app::control::{
    frame, frame_len, read_frame, ControlError, NodeInfo, NodeKind, Request, Response, K_ERROR,
    MAX_FRAME, MAX_NAME, MAX_TEXT,
};
use tenero_app::server::{start, start_with, ControlConfig, ControlHook, Meta};
use tenero_chain::Sha256Pow;
use tenero_core::v3::{
    Coinbase, CoinbaseOutput, Input, Output, Prunable, Transaction, TxPrefix, VERSION,
};
use tenero_net::sim::{test_chain_params, LABEL};
use tenero_net::transport::Hooks;
use tenero_net::{Engine, EngineConfig, Event};
use tenero_node::{Node, NodeConfig};
use tenero_store::{Store, TreeState};
use tenero_tree::PathBytes;
use tenero_wallet::testing::{test_block_to, READY};
use tenero_wallet::{Address, ChainView, Network, Rules, ScanBlock, SpendPaths, Wallet};

const T0: u64 = 1_700_000_000;

// ------------------------------------------------------------------------------------------------
// the encoding
// ------------------------------------------------------------------------------------------------

fn sample_tx() -> Transaction {
    Transaction {
        prefix: TxPrefix {
            version: VERSION,
            inputs: vec![Input { key_image: [1; 32] }],
            outputs: vec![
                Output {
                    onetime_address: [2; 32],
                    amount_commitment: [3; 32],
                    amount_enc: [4; 8],
                    view_tag: [5; 3],
                    anchor_enc: [7; 16],
                };
                2
            ],
            ephemeral_pubkeys: vec![[6; 32]],
            fee: 12345,
            encrypted_payment_id: [9; 8],
        },
        prunable: Prunable {
            reference_height: 70,
            proof_data: vec![8; 10],
        },
    }
}

fn sample_scan_block() -> ScanBlock {
    ScanBlock {
        height: 77,
        id: [0xab; 32],
        first_output_index: 1000,
        coinbase: Coinbase {
            version: VERSION,
            height: 77,
            outputs: vec![CoinbaseOutput {
                onetime_address: [1; 32],
                amount: 5,
                view_tag: [2; 3],
                ephemeral_pubkey: [3; 32],
                anchor_enc: [4; 16],
            }],
            extra: vec![],
        },
        txs: vec![sample_tx().prefix],
    }
}

fn sample_block() -> tenero_core::v3::Block {
    tenero_core::v3::Block {
        header: tenero_core::v3::BlockHeader {
            version: VERSION,
            prev_id: [1; 32],
            timestamp: 5,
            tx_root: [2; 32],
            nonce: 6,
            mix: [7; 64],
        },
        coinbase: sample_scan_block().coinbase,
        transactions: vec![sample_tx()],
    }
}

fn sample_paths() -> SpendPaths {
    SpendPaths {
        reference_height: 70,
        tree: TreeState {
            n_leaves: 99,
            n_layers: 2,
            root: [5; 32],
        },
        paths: vec![
            Some(PathBytes {
                position: 7,
                leaves: vec![([1; 32], [2; 32]); 9],
                layers: vec![vec![[3; 32]; 2]],
            }),
            None,
        ],
    }
}

fn sample_info() -> NodeInfo {
    NodeInfo {
        height: 9,
        tip_id: [5; 32],
        peers: 3,
        inbound: 1,
        pruned_below: 4,
        mempool_txs: 2,
        syncing: true,
        kind: NodeKind::Pruned,
        network: "test".into(),
        version: "0.0.0".into(),
    }
}

fn requests() -> Vec<Request> {
    vec![
        Request::Auth { cookie: [9; 32] },
        Request::Tip,
        Request::Block { height: u64::MAX },
        Request::KeyImageSpent { key_image: [8; 32] },
        Request::KeyImagesSpent {
            key_images: vec![[8; 32], [9; 32]],
        },
        Request::SpendPaths {
            indexes: vec![3, 1, 4],
        },
        Request::CheckPow {
            height: 9,
            header: sample_block().header,
        },
        Request::Rules,
        Request::SubmitTx(sample_tx()),
        Request::Info,
        Request::Stop,
        Request::Blocks { from: 5, count: 64 },
        Request::BlockTemplate {
            spend_pubkey: [1; 32],
            view_pubkey: [2; 32],
            max_weight: 1_000_000,
        },
        Request::SubmitBlock(sample_block()),
        Request::Headers { from: 0, count: 1 },
        Request::Mempool,
        Request::ChainStats,
    ]
}

fn responses() -> Vec<Response> {
    vec![
        Response::Authed,
        Response::Tip {
            height: 5,
            id: [1; 32],
        },
        Response::Block(None),
        Response::Block(Some(sample_scan_block())),
        Response::SpendPaths(sample_paths()),
        Response::PowChecked(true),
        Response::PowChecked(false),
        Response::Spent(true),
        Response::Spent(false),
        Response::Rules(Rules {
            chain_id: [7; 32],
            next_height: 11,
            reward: 2_000_000_000,
            median: 150_000,
            tree_layers: 3,
        }),
        Response::Rules(Rules {
            chain_id: [8; 32],
            next_height: 1,
            reward: 2_000_000_000,
            median: 150_000,
            tree_layers: 0,
        }),
        Response::TxAccepted { id: [3; 32] },
        Response::Info(sample_info()),
        Response::Info(NodeInfo {
            kind: NodeKind::Archive,
            syncing: false,
            ..sample_info()
        }),
        Response::Stopping,
        Response::Template(tenero_app::control::Template {
            header: sample_block().header,
            coinbase: sample_block().coinbase,
            tx_ids: vec![[3; 32], [4; 32]],
            height: 9,
            target: [0xaa; 32],
            anchor: [0x5a; 16],
        }),
        Response::BlockSubmitted {
            id: [8; 32],
            in_chain: true,
        },
        Response::BlockSubmitted {
            id: [9; 32],
            in_chain: false,
        },
        Response::Blocks(vec![]),
        Response::Blocks(vec![sample_scan_block(), sample_scan_block()]),
        Response::Error("no".into()),
    ]
}

#[test]
fn every_request_and_response_round_trips_exactly() {
    for r in requests() {
        let body = r.to_body().unwrap();
        assert_eq!(body[0], r.kind());
        assert_eq!(Request::from_body(&body).unwrap(), r);
    }
    for r in responses() {
        let body = r.to_body().unwrap();
        assert_eq!(body[0], r.kind());
        assert_eq!(Response::from_body(&body).unwrap(), r);
    }
}

#[test]
fn an_answer_has_its_requests_kind_with_the_top_bit_set() {
    let pairs = [
        (Request::Auth { cookie: [0; 32] }, Response::Authed),
        (
            Request::Tip,
            Response::Tip {
                height: 0,
                id: [0; 32],
            },
        ),
        (Request::Block { height: 0 }, Response::Block(None)),
        (
            Request::KeyImageSpent { key_image: [0; 32] },
            Response::Spent(false),
        ),
        (
            Request::KeyImagesSpent {
                key_images: vec![[0; 32]],
            },
            Response::SpentMany(vec![false]),
        ),
        (
            Request::SpendPaths { indexes: vec![0] },
            Response::SpendPaths(sample_paths()),
        ),
        (
            Request::CheckPow {
                height: 1,
                header: sample_block().header,
            },
            Response::PowChecked(true),
        ),
        (Request::Info, Response::Info(sample_info())),
        (Request::Stop, Response::Stopping),
        (
            Request::Headers { from: 0, count: 1 },
            Response::Headers(vec![]),
        ),
        (
            Request::Mempool,
            Response::Mempool {
                total: 0,
                txs: vec![],
            },
        ),
    ];
    for (q, a) in pairs {
        assert_eq!(a.kind(), q.kind() | 0x80);
    }
    assert_eq!(Response::Error(String::new()).kind(), K_ERROR);
}

#[test]
fn malformed_requests_are_refused_not_guessed() {
    // an empty body, an unknown kind, a short payload, a trailing byte
    assert!(matches!(
        Request::from_body(&[]),
        Err(ControlError::BadLength(0))
    ));
    // 4, 5 and 15 were version 2's requests for ring members: retired, so unknown
    for kind in [0u8, 4, 5, 15, 22, 0x80, 0xFF] {
        assert_eq!(
            Request::from_body(&[kind]),
            Err(ControlError::UnknownKind(kind))
        );
    }
    for r in requests() {
        let body = r.to_body().unwrap();
        // every strict prefix of a message that has a payload is refused
        for cut in 1..body.len() {
            assert!(
                Request::from_body(&body[..cut]).is_err(),
                "{r:?} cut at {cut}"
            );
        }
        let mut longer = body.clone();
        longer.push(0);
        assert_eq!(
            Request::from_body(&longer),
            Err(ControlError::Trailing),
            "{r:?}"
        );
    }
}

#[test]
fn malformed_responses_are_refused_too() {
    for r in responses() {
        let body = r.to_body().unwrap();
        for cut in 1..body.len() {
            assert!(
                Response::from_body(&body[..cut]).is_err(),
                "{r:?} cut at {cut}"
            );
        }
        let mut longer = body.clone();
        longer.push(0);
        assert_eq!(
            Response::from_body(&longer),
            Err(ControlError::Trailing),
            "{r:?}"
        );
    }
    // a flag byte is 0 or 1, nothing else
    for body in [vec![0x80 | 3, 2], vec![0x80 | 6, 2], vec![0x80 | 16, 2]] {
        assert!(Response::from_body(&body).is_err());
    }
    // a node kind is 0 or 1
    let mut info = Response::Info(sample_info()).to_body().unwrap();
    let kind_at = 1 + 8 + 32 + 4 + 4 + 8 + 4 + 1;
    info[kind_at] = 2;
    assert!(Response::from_body(&info).is_err());
    // text must be UTF-8, and has a length cap
    assert!(Response::from_body(&[K_ERROR, 2, 0, 0, 0, 0xff, 0xfe]).is_err());
    assert!(Response::Error("x".repeat(MAX_TEXT + 1)).to_body().is_err());
    assert!(Response::Error("x".repeat(MAX_TEXT)).to_body().is_ok());
    let long_name = NodeInfo {
        network: "x".repeat(MAX_NAME + 1),
        ..sample_info()
    };
    assert!(Response::Info(long_name).to_body().is_err());
}

#[test]
fn frames_are_length_prefixed_and_bounded() {
    let f = frame(&[1, 2, 3]).unwrap();
    assert_eq!(f, vec![3, 0, 0, 0, 1, 2, 3]);
    assert!(frame(&[]).is_err());
    assert!(frame(&vec![0; MAX_FRAME + 1]).is_err());
    assert!(frame(&vec![0; MAX_FRAME]).is_ok());
    assert_eq!(frame_len([0, 0, 0, 0]), Err(ControlError::BadLength(0)));
    assert_eq!(
        frame_len(((MAX_FRAME + 1) as u32).to_le_bytes()),
        Err(ControlError::BadLength(MAX_FRAME + 1))
    );
    assert_eq!(frame_len((MAX_FRAME as u32).to_le_bytes()), Ok(MAX_FRAME));
    // a stream that ends early, and a length that is too big, are errors, not allocations
    let mut short: &[u8] = &[10, 0, 0, 0, 1, 2];
    assert!(read_frame(&mut short).is_err());
    let mut huge: &[u8] = &[0xff, 0xff, 0xff, 0xff];
    assert!(read_frame(&mut huge).is_err());
    let mut ok: &[u8] = &[2, 0, 0, 0, 7, 8, 99];
    assert_eq!(read_frame(&mut ok).unwrap(), vec![7, 8]);
}

#[test]
fn the_cookie_file_round_trips_and_a_bad_one_is_refused() {
    let path = std::env::temp_dir().join(format!("tenero-cookie-{}", std::process::id()));
    let c = create_cookie(&path, &mut OsRng).unwrap();
    assert_eq!(read_cookie(&path).unwrap(), c);
    let c2 = create_cookie(&path, &mut OsRng).unwrap();
    assert_ne!(c, c2, "a new cookie each time");
    assert_eq!(read_cookie(&path).unwrap(), c2);
    for bad in [
        "",
        "abc",
        &"g".repeat(64),
        &"A".repeat(64),
        &"a".repeat(63),
        &"a".repeat(65),
    ] {
        std::fs::write(&path, bad).unwrap();
        assert!(read_cookie(&path).is_err(), "{bad:?}");
    }
    // trailing newline from a text editor is fine
    std::fs::write(&path, format!("{}\n", "ab".repeat(32))).unwrap();
    assert!(read_cookie(&path).is_ok());
    std::fs::remove_file(&path).unwrap();
    assert!(read_cookie(&path).is_err());
}

// ------------------------------------------------------------------------------------------------
// the server, on real sockets
// ------------------------------------------------------------------------------------------------

struct Rig {
    path: PathBuf,
    store: Store,
    params: tenero_chain::ChainParams,
}

impl Rig {
    fn new(tag: &str) -> Rig {
        let path =
            std::env::temp_dir().join(format!("tenero-app-{}-{tag}.redb", std::process::id()));
        remove(&path);
        let store = Store::open(&path, LABEL, tenero_core::v2::ids::PowKind::Sha256).unwrap();
        Rig {
            path,
            store,
            params: test_chain_params(),
        }
    }

    fn engine(&self) -> Engine<'_> {
        let node = Node::new(&self.store, &self.params, &Sha256Pow, NodeConfig::default()).unwrap();
        Engine::new(node, EngineConfig::default())
    }
}

fn remove(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let mut s = path.clone().into_os_string();
    s.push(".segments");
    let _ = std::fs::remove_dir_all(PathBuf::from(s));
}

impl Drop for Rig {
    fn drop(&mut self) {
        remove(&self.path);
    }
}

fn mine(engine: &mut Engine<'_>, to: &Address) {
    let h = engine.node().tip().unwrap().0 + 1;
    let ts = T0 + 60 * h;
    let block = test_block_to(engine.node(), to, ts);
    engine.handle(ts * 1000 + 10_000, Event::LocalBlock(block));
}

fn wallet(tag: u8) -> Wallet {
    Wallet::from_seed(&[tag; 32], Network::Test, 0)
}

fn meta() -> Meta {
    Meta {
        kind: NodeKind::Archive,
        network: "test".into(),
        version: "0.0.0".into(),
    }
}

const COOKIE: [u8; 32] = [0x42; 32];

/// Runs the hook against the engine (as the node's loop does) until `done` is set.
fn pump(engine: &mut Engine<'_>, hook: &mut ControlHook, done: &AtomicBool) {
    let end = Instant::now() + Duration::from_secs(60);
    while !done.load(Ordering::SeqCst) {
        assert!(Instant::now() < end, "the test took too long");
        let now = (T0 + 60 * 500) * 1000;
        for ev in hook.poll(engine, now) {
            engine.handle(now, ev);
        }
        thread::sleep(Duration::from_millis(1));
    }
}

/// Runs `client` on a thread while the main thread serves its requests.
fn with_client<R: Send>(
    engine: &mut Engine<'_>,
    hook: &mut ControlHook,
    client: impl FnOnce() -> R + Send,
) -> R {
    let done = AtomicBool::new(false);
    thread::scope(|s| {
        let t = s.spawn(|| {
            let r = client();
            done.store(true, Ordering::SeqCst);
            r
        });
        pump(engine, hook, &done);
        t.join().expect("the client thread")
    })
}

fn raw_connect(addr: SocketAddr) -> TcpStream {
    let s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s
}

fn send(s: &mut TcpStream, r: &Request) {
    s.write_all(&frame(&r.to_body().unwrap()).unwrap()).unwrap();
}

fn recv(s: &mut TcpStream) -> std::io::Result<Response> {
    let body = read_frame(s)?;
    Ok(Response::from_body(&body).unwrap())
}

/// True when the other side has closed the connection (end of file, or a reset). A read that merely TIMES OUT is
/// not closed: the other side is still holding the connection open.
fn closed(s: &mut TcpStream) -> bool {
    use std::io::ErrorKind::*;
    let mut b = [0u8; 1];
    match s.read(&mut b) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => matches!(
            e.kind(),
            ConnectionReset | ConnectionAborted | BrokenPipe | UnexpectedEof
        ),
    }
}

#[test]
fn a_wallet_pays_another_through_the_control_interface() {
    let rig = Rig::new("wallet");
    let mut engine = rig.engine();
    let shutdown = Arc::new(AtomicBool::new(false));
    let (handle, mut hook) =
        start("127.0.0.1:0".parse().unwrap(), COOKIE, shutdown, meta()).unwrap();
    let (mut alice, mut bob) = (wallet(1), wallet(2));
    for _ in 0..READY {
        mine(&mut engine, &alice.address());
    }
    let addr = handle.addr;
    let bob_address = bob.address();
    // Alice, in another "process" (a thread with its own socket), syncs and pays Bob
    let (alice, built, balance_before) = with_client(&mut engine, &mut hook, move || {
        let mut remote = RemoteNode::connect(addr, &COOKIE).unwrap();
        let report = alice.sync(&remote).unwrap();
        assert_eq!(
            report.blocks_scanned,
            READY + 1,
            "the genesis block and the rest"
        );
        let before = alice.balance(&remote).unwrap();
        assert!(before.spendable > 0);
        let built = alice
            .pay(&mut remote, &mut OsRng, &bob_address, 1_000_000_000)
            .unwrap();
        (alice, built, before)
    });
    assert_eq!(
        engine.node().pool().len(),
        1,
        "the transaction reached the node's pool"
    );
    assert_eq!(built.amount, 1_000_000_000);
    // a block takes it in; Bob finds it, again through the interface
    mine(&mut engine, &alice.address());
    let bob_total = with_client(&mut engine, &mut hook, move || {
        let remote = RemoteNode::connect(addr, &COOKIE).unwrap();
        bob.sync(&remote).unwrap();
        bob.balance(&remote).unwrap().total
    });
    assert_eq!(bob_total, 1_000_000_000);
    assert!(balance_before.total > 0);
    // (a floor: the payment really went through the interface, its coins' paths included)
    assert!(
        hook.answered > 10,
        "{} requests were answered",
        hook.answered
    );
}

#[test]
fn a_remote_view_of_the_chain_matches_the_nodes_own() {
    let rig = Rig::new("view");
    let mut engine = rig.engine();
    let (handle, mut hook) = start(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::new(AtomicBool::new(false)),
        meta(),
    )
    .unwrap();
    let alice = wallet(1);
    // the first rewards are in the curve tree by block 66
    for _ in 0..66 {
        mine(&mut engine, &alice.address());
    }
    let addr = handle.addr;
    let tip = ChainView::tip(engine.node()).unwrap();
    assert_eq!(tip.0, 66);
    let rules = ChainView::rules(engine.node()).unwrap();
    let blocks: Vec<_> = (0..=67)
        .map(|h| ChainView::block(engine.node(), h).unwrap())
        .collect();
    let wanted: Vec<u64> = (0..12).collect();
    let paths = ChainView::spend_paths(engine.node(), &wanted).unwrap();
    // the tree holds the first rewards (one output each), and an index past them has no path
    let in_tree = paths.paths.iter().filter(|p| p.is_some()).count();
    assert!(in_tree > 0);
    assert_eq!(
        in_tree as u64, paths.tree.n_leaves,
        "every leaf is one of these"
    );
    assert!(paths.paths[..in_tree].iter().all(Option::is_some));
    with_client(&mut engine, &mut hook, move || {
        let remote = RemoteNode::connect(addr, &COOKIE).unwrap();
        assert_eq!(remote.tip().unwrap(), tip);
        assert_eq!(remote.rules().unwrap(), rules);
        assert!(rules.tree_layers >= 1);
        for (h, b) in blocks.iter().enumerate() {
            assert_eq!(&remote.block(h as u64).unwrap(), b, "block {h}");
        }
        // the paths a spend is proven with: the node's own, byte for byte, `None` for an output not in the tree
        assert_eq!(remote.spend_paths(&wanted).unwrap(), paths);
        // more than one request's worth is split by the client, and still comes from one tree
        let many = remote.spend_paths(&vec![2u64; 1200]).unwrap();
        assert_eq!(many.paths.len(), 1200);
        assert_eq!(many.tree, paths.tree);
        assert!(many.paths.iter().all(|p| *p == paths.paths[2]));
        assert!(remote.spend_paths(&[]).is_err(), "nothing to ask for");
        // blocks in a batch: the same as one by one, in order, and fewer than asked at the tip
        let batch = remote.blocks(1, 64).unwrap();
        assert_eq!(batch.len(), 64);
        assert_eq!(
            batch.iter().map(|b| b.height).collect::<Vec<_>>(),
            (1..=64).collect::<Vec<_>>()
        );
        for b in &batch {
            assert_eq!(Some(b), blocks[b.height as usize].as_ref());
        }
        assert_eq!(remote.blocks(0, 2).unwrap().len(), 2);
        assert_eq!(remote.blocks(63, 64).unwrap().len(), 4, "63 to 66");
        assert!(
            remote.blocks(67, 10).unwrap().is_empty(),
            "nothing past the tip"
        );
        assert!(remote.blocks(u64::MAX, 10).unwrap().is_empty());
        assert!(!remote.key_image_spent(&[9; 32]).unwrap());
        // many at once: one answer for each, in order, and more than one request's worth is split by the client
        assert_eq!(
            remote.key_images_spent(&[[9; 32], [10; 32]]).unwrap(),
            vec![false, false]
        );
        assert_eq!(
            remote.key_images_spent(&vec![[7; 32]; 5000]).unwrap().len(),
            5000
        );
        let info = remote.info().unwrap();
        assert_eq!((info.height, info.tip_id), tip);
        assert_eq!(info.network, "test");
        assert_eq!(info.kind, NodeKind::Archive);
        assert!(!info.syncing);
    });
}

#[test]
fn a_transaction_the_node_would_not_take_comes_back_with_the_reason() {
    let rig = Rig::new("badtx");
    let mut engine = rig.engine();
    let (handle, mut hook) = start(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::new(AtomicBool::new(false)),
        meta(),
    )
    .unwrap();
    let alice = wallet(1);
    for _ in 0..3 {
        mine(&mut engine, &alice.address());
    }
    let addr = handle.addr;
    let msg = with_client(&mut engine, &mut hook, move || {
        let remote = RemoteNode::connect(addr, &COOKIE).unwrap();
        remote.request(&Request::SubmitTx(sample_tx())).unwrap_err()
    });
    assert!(!msg.is_empty());
    assert_eq!(engine.node().pool().len(), 0);
}

#[test]
fn the_same_transaction_twice_is_refused_the_second_time() {
    let rig = Rig::new("dup");
    let mut engine = rig.engine();
    let (handle, mut hook) = start(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::new(AtomicBool::new(false)),
        meta(),
    )
    .unwrap();
    let (mut alice, bob) = (wallet(1), wallet(2));
    for _ in 0..READY {
        mine(&mut engine, &alice.address());
    }
    alice.sync(engine.node()).unwrap();
    let built = alice
        .build_payment(engine.node(), &mut OsRng, &bob.address(), 1_000)
        .unwrap();
    let addr = handle.addr;
    let tx = built.tx.clone();
    let (first, second) = with_client(&mut engine, &mut hook, move || {
        let remote = RemoteNode::connect(addr, &COOKIE).unwrap();
        (
            remote.request(&Request::SubmitTx(tx.clone())),
            remote.request(&Request::SubmitTx(tx)),
        )
    });
    assert!(
        matches!(first, Ok(Response::TxAccepted { .. })),
        "{first:?}"
    );
    assert!(
        second.unwrap_err().contains("already"),
        "the node says it has it"
    );
    assert_eq!(engine.node().pool().len(), 1);
}

#[test]
fn a_block_explorer_sees_the_blocks_the_pool_and_the_numbers_as_the_node_has_them() {
    use tenero_core::v3::Wire;
    let rig = Rig::new("explorer");
    let mut engine = rig.engine();
    let (handle, mut hook) = start(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::new(AtomicBool::new(false)),
        meta(),
    )
    .unwrap();
    let (mut alice, bob) = (wallet(1), wallet(2));
    for _ in 0..READY {
        mine(&mut engine, &alice.address());
    }
    alice.sync(engine.node()).unwrap();
    let built = alice
        .build_payment(engine.node(), &mut OsRng, &bob.address(), 1_000)
        .unwrap();
    let tx = built.tx.clone();
    let tx_id = tenero_core::v3::ids::tx_id(&tx).unwrap();
    let addr = handle.addr;
    // the transaction goes in through the interface (so the node's clock stamps it), then the pool is listed
    let (total, listed) = with_client(&mut engine, &mut hook, move || {
        let remote = RemoteNode::connect(addr, &COOKIE).unwrap();
        remote.request(&Request::SubmitTx(tx)).unwrap();
        remote.mempool().unwrap()
    });
    assert_eq!(total, 1);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, tx_id);
    assert_eq!(listed[0].fee, built.tx.prefix.fee);
    assert_eq!(listed[0].size, built.tx.to_bytes().unwrap().len() as u64);
    let tx_weight = tenero_core::v3::rules::tx_weight(&built.tx).unwrap();
    assert_eq!(listed[0].weight, tx_weight);
    assert!(
        listed[0].weight < listed[0].size,
        "the proof counts a quarter"
    );
    // `pump`'s clock, in seconds
    assert_eq!(listed[0].received, T0 + 60 * 500);

    // a block takes it in; the summaries must match the blocks as stored, and a block's weight is its transactions'
    mine(&mut engine, &alice.address());
    let store = engine.node().store();
    let tip = store.tip().unwrap().0;
    assert_eq!(tip, READY + 1);
    let mut want = Vec::new();
    for h in 0..=tip {
        let index = store.block_index(h).unwrap().unwrap();
        let paid: u64 = store
            .coinbase(h)
            .unwrap()
            .map_or(0, |c| c.outputs.iter().map(|o| o.amount).sum());
        want.push((
            h,
            index.block_id,
            index.header.timestamp,
            index.target,
            index.tx_count,
            if h == tip { tx_weight } else { 0 },
            paid,
        ));
    }
    let next = engine.node().next_block().unwrap();
    let emission = rig.params.emission;
    let (summaries, stats, (total_after, _)) = with_client(&mut engine, &mut hook, move || {
        let remote = RemoteNode::connect(addr, &COOKIE).unwrap();
        // in pieces, and nothing past the tip
        let mut s = remote.headers(0, 3).unwrap();
        s.extend(remote.headers(3, 64).unwrap());
        assert!(remote.headers(tip + 1, 5).unwrap().is_empty());
        assert!(remote.headers(u64::MAX, 64).unwrap().is_empty());
        (s, remote.chain_stats().unwrap(), remote.mempool().unwrap())
    });
    let got: Vec<_> = summaries
        .iter()
        .map(|b| {
            (
                b.height,
                b.id,
                b.timestamp,
                b.target,
                b.tx_count,
                b.weight,
                b.coinbase_total,
            )
        })
        .collect();
    assert_eq!(got, want);
    let last = tip as usize;
    assert_eq!(summaries[last].tx_count, 1, "the payment's block");
    assert!(
        summaries[last].coinbase_total > summaries[last - 1].coinbase_total,
        "it paid a fee"
    );
    // the work grows block by block
    for w in summaries.windows(2) {
        assert!(w[1].cumulative_work > w[0].cumulative_work);
    }
    assert_eq!(total_after, 0, "the block emptied the pool");
    assert_eq!(stats.height, tip);
    assert_eq!(stats.next_target, next.target.to_be_bytes());
    assert_eq!(stats.cumulative_work, summaries[last].cumulative_work);
    assert_eq!(stats.next_reward, next.reward);
    assert_eq!(
        stats.emitted,
        (1..=tip).map(|h| emission.reward_at(h)).sum::<u64>()
    );
    assert_eq!(stats.max_supply, emission.max_supply);
    assert_eq!(stats.tail_reward, emission.tail_reward);
    assert_eq!(stats.block_time, rig.params.difficulty.block_time);
}

#[test]
fn stop_asks_the_node_to_shut_down() {
    let rig = Rig::new("stop");
    let mut engine = rig.engine();
    let shutdown = Arc::new(AtomicBool::new(false));
    let (handle, mut hook) = start(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::clone(&shutdown),
        meta(),
    )
    .unwrap();
    let addr = handle.addr;
    with_client(&mut engine, &mut hook, move || {
        RemoteNode::connect(addr, &COOKIE).unwrap().stop().unwrap();
    });
    assert!(shutdown.load(Ordering::SeqCst));
}

// ---- who may talk to it ----------------------------------------------------------------------------------------

#[test]
fn only_a_loopback_address_may_be_listened_on_or_connected_to() {
    let shutdown = Arc::new(AtomicBool::new(false));
    for bad in ["0.0.0.0:0", "192.0.2.1:0", "[::]:0"] {
        assert!(
            start(bad.parse().unwrap(), COOKIE, Arc::clone(&shutdown), meta()).is_err(),
            "{bad}"
        );
    }
    assert!(start(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::clone(&shutdown),
        meta()
    )
    .is_ok());
    assert!(start("127.0.0.2:0".parse().unwrap(), COOKIE, shutdown, meta()).is_ok());
    // the client will not send a cookie anywhere else
    let r = RemoteNode::connect("192.0.2.1:9".parse().unwrap(), &COOKIE);
    assert!(r.err().unwrap().contains("only reachable on this machine"));
}

#[test]
fn a_connection_without_the_right_cookie_gets_nothing() {
    let rig = Rig::new("auth");
    let mut engine = rig.engine();
    let (handle, mut hook) = start(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::new(AtomicBool::new(false)),
        meta(),
    )
    .unwrap();
    let addr = handle.addr;
    with_client(&mut engine, &mut hook, move || {
        // a request before authenticating
        let mut s = raw_connect(addr);
        send(&mut s, &Request::Tip);
        assert!(closed(&mut s), "answered a stranger");
        // the wrong cookie, off by one bit
        let mut wrong = COOKIE;
        wrong[31] ^= 1;
        let mut s = raw_connect(addr);
        send(&mut s, &Request::Auth { cookie: wrong });
        assert!(closed(&mut s));
        // authenticating twice
        let mut s = raw_connect(addr);
        send(&mut s, &Request::Auth { cookie: COOKIE });
        assert_eq!(recv(&mut s).unwrap(), Response::Authed);
        send(&mut s, &Request::Auth { cookie: COOKIE });
        assert!(matches!(recv(&mut s).unwrap(), Response::Error(_)));
        // and the connection still works
        send(&mut s, &Request::Tip);
        assert!(matches!(recv(&mut s).unwrap(), Response::Tip { .. }));
        // the client library refuses with a message
        let e = RemoteNode::connect(addr, &wrong).err().unwrap();
        assert!(e.contains("did not accept") || e.contains("lost"), "{e}");
    });
    assert_eq!(
        hook.answered, 1,
        "only the one request after authenticating reached the node"
    );
}

#[test]
fn garbage_after_authenticating_ends_the_connection() {
    let rig = Rig::new("garbage");
    let mut engine = rig.engine();
    let (handle, mut hook) = start(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::new(AtomicBool::new(false)),
        meta(),
    )
    .unwrap();
    let addr = handle.addr;
    with_client(&mut engine, &mut hook, move || {
        // an unknown request kind
        let mut s = raw_connect(addr);
        send(&mut s, &Request::Auth { cookie: COOKIE });
        assert_eq!(recv(&mut s).unwrap(), Response::Authed);
        s.write_all(&frame(&[0x77]).unwrap()).unwrap();
        assert!(matches!(recv(&mut s).unwrap(), Response::Error(_)));
        assert!(closed(&mut s));
        // a frame that announces more than the limit
        let mut s = raw_connect(addr);
        send(&mut s, &Request::Auth { cookie: COOKIE });
        assert_eq!(recv(&mut s).unwrap(), Response::Authed);
        s.write_all(&((MAX_FRAME + 1) as u32).to_le_bytes())
            .unwrap();
        assert!(closed(&mut s));
        // a frame of length 0
        let mut s = raw_connect(addr);
        s.write_all(&0u32.to_le_bytes()).unwrap();
        assert!(closed(&mut s));
        // a request cut short and then silence: the connection is closed by the server's timeout, not held forever
    });
}

#[test]
fn a_connection_that_does_not_authenticate_in_time_is_closed() {
    let cfg = ControlConfig {
        auth_timeout: Duration::from_millis(300),
        ..ControlConfig::default()
    };
    let (handle, _hook) = start_with(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::new(AtomicBool::new(false)),
        meta(),
        cfg,
    )
    .unwrap();
    let mut s = raw_connect(handle.addr);
    let t = Instant::now();
    assert!(closed(&mut s));
    assert!(
        t.elapsed() < Duration::from_secs(4),
        "closed after {:?}",
        t.elapsed()
    );
    assert!(
        t.elapsed() >= Duration::from_millis(250),
        "closed too early: {:?}",
        t.elapsed()
    );
}

#[test]
fn an_idle_authenticated_connection_is_closed_after_the_idle_timeout() {
    let cfg = ControlConfig {
        idle_timeout: Duration::from_millis(300),
        ..ControlConfig::default()
    };
    let (handle, _hook) = start_with(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::new(AtomicBool::new(false)),
        meta(),
        cfg,
    )
    .unwrap();
    let mut s = raw_connect(handle.addr);
    send(&mut s, &Request::Auth { cookie: COOKIE });
    assert_eq!(recv(&mut s).unwrap(), Response::Authed);
    let t = Instant::now();
    assert!(closed(&mut s));
    assert!(t.elapsed() >= Duration::from_millis(250) && t.elapsed() < Duration::from_secs(4));
}

#[test]
fn only_so_many_connections_are_served_at_once() {
    let cfg = ControlConfig {
        max_connections: 3,
        ..ControlConfig::default()
    };
    let (handle, _hook) = start_with(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::new(AtomicBool::new(false)),
        meta(),
        cfg,
    )
    .unwrap();
    let mut held = Vec::new();
    for _ in 0..3 {
        let mut s = raw_connect(handle.addr);
        send(&mut s, &Request::Auth { cookie: COOKIE });
        assert_eq!(recv(&mut s).unwrap(), Response::Authed);
        held.push(s);
    }
    assert_eq!(handle.connections(), 3);
    // the fourth is turned away
    let mut extra = raw_connect(handle.addr);
    assert!(closed(&mut extra));
    // and a place frees up when one goes
    drop(held.pop());
    let t = Instant::now();
    while handle.connections() > 2 && t.elapsed() < Duration::from_secs(5) {
        thread::sleep(Duration::from_millis(10));
    }
    let mut again = raw_connect(handle.addr);
    send(&mut again, &Request::Auth { cookie: COOKIE });
    assert_eq!(recv(&mut again).unwrap(), Response::Authed);
}

#[test]
fn a_request_flood_cannot_starve_the_nodes_loop() {
    let rig = Rig::new("flood");
    let mut engine = rig.engine();
    let (handle, mut hook) = start(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::new(AtomicBool::new(false)),
        meta(),
    )
    .unwrap();
    let addr = handle.addr;
    let answered = with_client(&mut engine, &mut hook, move || {
        let mut s = raw_connect(addr);
        send(&mut s, &Request::Auth { cookie: COOKIE });
        assert_eq!(recv(&mut s).unwrap(), Response::Authed);
        let mut n = 0;
        for _ in 0..500 {
            send(&mut s, &Request::Tip);
            if matches!(recv(&mut s).unwrap(), Response::Tip { .. }) {
                n += 1;
            }
        }
        n
    });
    assert_eq!(answered, 500);
}

#[test]
fn a_blocks_answer_stops_at_its_byte_budget_but_always_makes_progress() {
    let rig = Rig::new("budget");
    let mut engine = rig.engine();
    let cfg = ControlConfig {
        blocks_bytes: 1,
        ..ControlConfig::default()
    };
    let (handle, mut hook) = start_with(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::new(AtomicBool::new(false)),
        meta(),
        cfg,
    )
    .unwrap();
    let alice = wallet(1);
    for _ in 0..4 {
        mine(&mut engine, &alice.address());
    }
    let addr = handle.addr;
    with_client(&mut engine, &mut hook, move || {
        let remote = RemoteNode::connect(addr, &COOKIE).unwrap();
        // asked for five, given one (the budget), and the next call goes on from there
        let one = remote.blocks(1, 5).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].height, 1);
        assert_eq!(remote.blocks(2, 5).unwrap()[0].height, 2);
        // a wallet scanning through such a node still gets everything
        let mut w = wallet(1);
        w.sync(&remote).unwrap();
        assert_eq!(w.owned().len(), 4);
    });
}

#[test]
fn a_blocks_request_for_none_or_too_many_is_refused() {
    let rig = Rig::new("blockscount");
    let mut engine = rig.engine();
    let (handle, mut hook) = start(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::new(AtomicBool::new(false)),
        meta(),
    )
    .unwrap();
    let addr = handle.addr;
    with_client(&mut engine, &mut hook, move || {
        for count in [0u16, 65, u16::MAX] {
            let mut s = raw_connect(addr);
            send(&mut s, &Request::Auth { cookie: COOKIE });
            assert_eq!(recv(&mut s).unwrap(), Response::Authed);
            let mut body = vec![tenero_app::control::K_BLOCKS];
            body.extend_from_slice(&1u64.to_le_bytes());
            body.extend_from_slice(&count.to_le_bytes());
            s.write_all(&frame(&body).unwrap()).unwrap();
            assert!(
                matches!(recv(&mut s).unwrap(), Response::Error(_)),
                "count {count}"
            );
            assert!(closed(&mut s));
        }
    });
}

#[test]
fn the_loop_answers_a_bounded_number_of_requests_each_time_it_looks() {
    use tenero_app::server::PER_POLL;
    let rig = Rig::new("perpoll");
    let mut engine = rig.engine();
    let cfg = ControlConfig {
        max_connections: 64,
        ..ControlConfig::default()
    };
    let (handle, mut hook) = start_with(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::new(AtomicBool::new(false)),
        meta(),
        cfg,
    )
    .unwrap();
    let mut socks = Vec::new();
    for _ in 0..PER_POLL + 8 {
        let mut s = raw_connect(handle.addr);
        send(&mut s, &Request::Auth { cookie: COOKIE });
        assert_eq!(recv(&mut s).unwrap(), Response::Authed);
        send(&mut s, &Request::Tip);
        socks.push(s);
    }
    thread::sleep(Duration::from_millis(400)); // let every request reach the queue
    hook.poll(&mut engine, 0);
    assert_eq!(
        hook.answered, PER_POLL as u64,
        "one look answers at most {PER_POLL}"
    );
    hook.poll(&mut engine, 0);
    assert_eq!(
        hook.answered,
        (PER_POLL + 8) as u64,
        "the next look answers the rest"
    );
    for s in &mut socks {
        assert!(matches!(recv(s).unwrap(), Response::Tip { .. }));
    }
}

#[test]
fn a_full_queue_is_answered_busy_at_once() {
    use tenero_app::server::QUEUE;
    let cfg = ControlConfig {
        max_connections: 200,
        ..ControlConfig::default()
    };
    let (handle, _hook) = start_with(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::new(AtomicBool::new(false)),
        meta(),
        cfg,
    )
    .unwrap();
    // nobody polls the hook, so nothing is answered: the first QUEUE requests wait, the rest are told "busy"
    let mut socks = Vec::new();
    for _ in 0..QUEUE + 6 {
        let mut s = raw_connect(handle.addr);
        send(&mut s, &Request::Auth { cookie: COOKIE });
        assert_eq!(recv(&mut s).unwrap(), Response::Authed);
        send(&mut s, &Request::Tip);
        s.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        socks.push(s);
    }
    let mut busy = 0;
    let mut waiting = 0;
    for s in &mut socks {
        match recv(s) {
            Ok(Response::Error(m)) => {
                assert!(m.contains("busy"), "{m}");
                busy += 1;
            }
            Ok(other) => panic!("{other:?}"),
            Err(e) => {
                assert!(
                    matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ),
                    "{e}"
                );
                waiting += 1;
            }
        }
    }
    assert_eq!((busy, waiting), (6, QUEUE));
}

#[test]
fn a_transaction_the_pool_will_not_keep_is_not_reported_accepted() {
    let rig = Rig::new("poolrefuse");
    let mut engine = rig.engine();
    let (handle, mut hook) = start(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::new(AtomicBool::new(false)),
        meta(),
    )
    .unwrap();
    let (mut alice, bob) = (wallet(1), wallet(2));
    for _ in 0..READY {
        mine(&mut engine, &alice.address());
    }
    alice.sync(engine.node()).unwrap();
    // two different payments from the same coin: each is valid against the chain, but the pool can hold only one
    let one = alice
        .build_payment(engine.node(), &mut OsRng, &bob.address(), 1_000)
        .unwrap();
    let two = alice
        .build_payment(engine.node(), &mut OsRng, &bob.address(), 2_000)
        .unwrap();
    assert_eq!(one.spends, two.spends);
    let addr = handle.addr;
    let (a, b) = with_client(&mut engine, &mut hook, move || {
        let remote = RemoteNode::connect(addr, &COOKIE).unwrap();
        (
            remote.request(&Request::SubmitTx(one.tx)),
            remote.request(&Request::SubmitTx(two.tx)),
        )
    });
    assert!(matches!(a, Ok(Response::TxAccepted { .. })), "{a:?}");
    let e = b.unwrap_err();
    assert!(e.contains("did not keep"), "{e}");
    assert_eq!(engine.node().pool().len(), 1);
}

#[test]
fn a_remote_blocks_call_asks_for_a_sensible_count_whatever_the_wallet_wants() {
    let rig = Rig::new("clamp");
    let mut engine = rig.engine();
    let (handle, mut hook) = start(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::new(AtomicBool::new(false)),
        meta(),
    )
    .unwrap();
    let alice = wallet(1);
    for _ in 0..70 {
        mine(&mut engine, &alice.address());
    }
    let addr = handle.addr;
    with_client(&mut engine, &mut hook, move || {
        let remote = RemoteNode::connect(addr, &COOKIE).unwrap();
        // far more than the protocol allows: it gets the most the protocol allows, not an error
        assert_eq!(remote.blocks(1, 1_000_000).unwrap().len(), 64);
        // none: it gets one (a request for none is not a thing)
        assert_eq!(remote.blocks(1, 0).unwrap().len(), 1);
    });
}

#[test]
fn a_block_with_too_many_transactions_is_refused_both_ways() {
    use tenero_core::v2::MAX_BLOCK_TXS;
    use tenero_core::v3::Wire;
    let mut block = sample_scan_block();
    block.coinbase.outputs.clear();
    block.txs = vec![sample_tx().prefix; MAX_BLOCK_TXS];
    let body = Response::Block(Some(block.clone())).to_body().unwrap();
    assert_eq!(
        Response::from_body(&body).unwrap(),
        Response::Block(Some(block.clone()))
    );
    // one more, when encoding
    let mut over = block.clone();
    over.txs.push(sample_tx().prefix);
    assert!(Response::Block(Some(over)).to_body().is_err());
    // one more, when decoding: the count says so, and the bytes of that many are all there
    let mut forged = body.clone();
    let count_at = 1 + 1 + 8 + 32 + 8 + 2 + 8 + 4 + 4; // after the coinbase (no outputs, no extra)
    assert_eq!(
        &forged[count_at..count_at + 4],
        &(MAX_BLOCK_TXS as u32).to_le_bytes()
    );
    forged[count_at..count_at + 4].copy_from_slice(&(MAX_BLOCK_TXS as u32 + 1).to_le_bytes());
    forged.extend_from_slice(&sample_tx().prefix.to_bytes().unwrap());
    assert!(matches!(
        Response::from_body(&forged),
        Err(ControlError::Decode(_))
    ));
}

#[test]
fn a_tree_deeper_than_a_path_may_be_is_an_error_not_a_wrong_number() {
    let r = |tree_layers| Rules {
        chain_id: [0; 32],
        next_height: 1,
        reward: 1,
        median: 1,
        tree_layers,
    };
    use tenero_app::control::MAX_PATH_LAYERS;
    assert!(Response::Rules(r(MAX_PATH_LAYERS)).to_body().is_ok());
    assert!(Response::Rules(r(MAX_PATH_LAYERS + 1)).to_body().is_err());
    assert!(Response::Rules(r(256)).to_body().is_err());
}

// ---- a web page cannot talk to this port (M9, threat model G2) -----------------------------------------------------------

/// What a browser sends. A web page can make a browser connect to `http://127.0.0.1:<port>/` and send HTTP, or a WebSocket
/// handshake, or (by a form) a body of its choosing, but whatever it sends **starts with the HTTP method**.
fn http_requests(port: u16) -> Vec<(String, Vec<u8>)> {
    let host = format!("127.0.0.1:{port}");
    let headers = format!(
        "Host: {host}\r\nUser-Agent: Mozilla/5.0\r\nAccept: */*\r\nOrigin: https://example.org\r\nConnection: close\r\n"
    );
    let mut v: Vec<(String, Vec<u8>)> = vec![];
    for m in [
        "GET", "HEAD", "POST", "PUT", "DELETE", "OPTIONS", "PATCH", "CONNECT", "TRACE",
    ] {
        v.push((
            format!("{m} /"),
            format!("{m} / HTTP/1.1\r\n{headers}Content-Length: 0\r\n\r\n").into_bytes(),
        ));
    }
    // a POST whose body is as hostile as a page can make it: a valid-looking frame, bytes that look like a request, zeros
    let mut body = Vec::new();
    body.extend_from_slice(&frame(&Request::Auth { cookie: [0; 32] }.to_body().unwrap()).unwrap());
    body.extend_from_slice(&frame(&Request::Stop.to_body().unwrap()).unwrap());
    let mut post = format!(
        "POST /stop HTTP/1.1\r\n{headers}Content-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    post.extend_from_slice(&body);
    v.push(("POST with a frame as its body".into(), post));
    v.push((
        "WebSocket upgrade".into(),
        format!(
            "GET /ws HTTP/1.1\r\n{headers}Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        .into_bytes(),
    ));
    v.push(("a bare newline".into(), b"\r\n\r\n".to_vec()));
    v
}

/// Reads until the other side closes or `limit` passes; says whether it closed and what it sent.
fn read_to_close(s: &mut TcpStream, limit: Duration) -> (bool, Vec<u8>) {
    use std::io::ErrorKind::*;
    s.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let (end, mut got) = (Instant::now() + limit, Vec::new());
    let mut buf = [0u8; 512];
    while Instant::now() < end {
        match s.read(&mut buf) {
            Ok(0) => return (true, got),
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    ConnectionReset | ConnectionAborted | BrokenPipe | UnexpectedEof
                ) =>
            {
                return (true, got)
            }
            Err(_) => {} // a timeout: still open, keep waiting
        }
    }
    (false, got)
}

#[test]
fn what_a_browser_can_send_to_the_control_port_is_refused_at_once_and_does_nothing() {
    // every HTTP method, read as the 4-byte little-endian length that starts a frame, is far over the limit
    for m in [
        "GET ", "HEAD", "POST", "PUT ", "DELE", "OPTI", "PATC", "CONN", "TRAC",
    ] {
        let n = u32::from_le_bytes(m.as_bytes().try_into().unwrap()) as usize;
        assert!(n > MAX_FRAME, "{m} reads as a length of {n}");
        assert!(frame_len(m.as_bytes().try_into().unwrap()).is_err(), "{m}");
    }
    let rig = Rig::new("http");
    let mut engine = rig.engine();
    let shutdown = Arc::new(AtomicBool::new(false));
    let (handle, mut hook) = start(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::clone(&shutdown),
        meta(),
    )
    .unwrap();
    let addr = handle.addr;
    with_client(&mut engine, &mut hook, move || {
        for (what, bytes) in http_requests(addr.port()) {
            let mut s = raw_connect(addr);
            s.write_all(&bytes).unwrap();
            // refused at once (well inside the 10 s a silent connection is given), with no answer of any kind
            let (is_closed, got) = read_to_close(&mut s, Duration::from_secs(3));
            assert!(is_closed, "{what}: the connection was held open");
            assert!(
                got.is_empty(),
                "{what}: answered with {} bytes: {:?}",
                got.len(),
                String::from_utf8_lossy(&got)
            );
        }
        // the port is not harmed: a client with the cookie is served at once, after all of that
        let mut s = raw_connect(addr);
        send(&mut s, &Request::Auth { cookie: COOKIE });
        assert_eq!(recv(&mut s).unwrap(), Response::Authed);
        send(&mut s, &Request::Tip);
        assert!(matches!(recv(&mut s).unwrap(), Response::Tip { .. }));
    });
    assert_eq!(
        hook.answered, 1,
        "only the one request after authenticating reached the node"
    );
    assert!(
        !shutdown.load(Ordering::SeqCst),
        "a request from a web page stopped the node"
    );
}
