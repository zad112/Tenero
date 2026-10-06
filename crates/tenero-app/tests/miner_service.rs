//! The miner service (`docs/REMOTE_MINING_PLAN.md`): a listener for miners on other computers, against a real node on the
//! test chain (SHA-256 proof of work). What is checked: only three requests are answered, a key is needed, the limits hold,
//! `info` is trimmed, and a miner in another process mines blocks the node accepts and the wallet can see, across the
//! encrypted channel. **Not a real chain, and not another computer: every connection here is on this one.**

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tenero_app::client::RemoteNode;
use tenero_app::control::{read_frame, write_frame, NodeKind, Request};
use tenero_app::miner_service::{self, SecureStream, ServiceConfig, MAX_REQUEST_BYTES};
use tenero_app::remote_miner::{RemoteMiner, RemoteMinerConfig};
use tenero_app::server::{start, ControlHandle, ControlHook, Meta};
use tenero_chain::Sha256Pow;
use tenero_core::v2::ids::PowKind;
use tenero_miner::{Miner, Sha256Backend, WalletPayout};
use tenero_net::sim::{test_chain_params, LABEL};
use tenero_net::transport::Hooks;
use tenero_net::{Engine, EngineConfig};
use tenero_node::{Node, NodeConfig, Payout};
use tenero_store::Store;
use tenero_wallet::{Address, Wallet};

const T0: u64 = 1_700_000_000;
const COOKIE: [u8; 32] = [0x42; 32];
const KEY: [u8; 32] = [0x77; 32];
/// The node's clock in these tests (the pump's), in 2023.
const NOW: u64 = T0 + 60 * 500;

fn address() -> Address {
    Wallet::from_seed(&[1; 32], 0).address()
}

struct Rig {
    path: PathBuf,
    store: Store,
    params: tenero_chain::ChainParams,
}

impl Rig {
    fn new(tag: &str) -> Rig {
        let path =
            std::env::temp_dir().join(format!("tenero-ms-{}-{tag}.redb", std::process::id()));
        remove(&path);
        let store = Store::open(&path, LABEL, PowKind::Sha256).unwrap();
        let mut params = test_chain_params();
        params.ring_size = 2;
        params.coinbase_maturity = 1;
        params.spend_maturity = 1;
        Rig {
            path,
            store,
            params,
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

fn meta() -> Meta {
    Meta {
        kind: NodeKind::Archive,
        network: "test".into(),
        version: "0.0.0".into(),
    }
}

fn payout() -> Payout {
    Payout {
        onetime_address: [0x0a; 32],
        view_tag: [0x0b; 3],
        ephemeral_pubkey: [0x0c; 32],
        anchor_enc: [0x0d; 16],
    }
}

fn serve_until(engine: &mut Engine<'_>, hook: &mut ControlHook, done: &AtomicBool) {
    let end = Instant::now() + Duration::from_secs(60);
    while !done.load(Ordering::SeqCst) {
        assert!(Instant::now() < end, "the test took too long");
        for ev in hook.poll(engine, NOW * 1000) {
            engine.handle(NOW * 1000, ev);
        }
        thread::sleep(Duration::from_millis(1));
    }
}

/// Runs `client` on a thread while the main thread serves the node's queue.
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
        serve_until(engine, hook, &done);
        t.join().expect("the client thread")
    })
}

struct Up {
    control: ControlHandle,
    hook: ControlHook,
    shutdown: Arc<AtomicBool>,
}

fn node() -> Up {
    let shutdown = Arc::new(AtomicBool::new(false));
    let (control, hook) = start(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::clone(&shutdown),
        meta(),
    )
    .unwrap();
    Up {
        control,
        hook,
        shutdown,
    }
}

fn cfg() -> ServiceConfig {
    ServiceConfig {
        key: Some(KEY),
        ..ServiceConfig::default()
    }
}

fn service(up: &Up, cfg: ServiceConfig) -> miner_service::ServiceHandle {
    miner_service::start("127.0.0.1:0".parse().unwrap(), &up.control, cfg).unwrap()
}

// ---- who is answered, and what ---------------------------------------------------------------------------------

#[test]
fn a_miner_with_the_key_gets_info_trimmed_and_a_template() {
    let rig = Rig::new("basic");
    let mut engine = rig.engine();
    let mut up = node();
    let svc = service(&up, cfg());
    let addr = svc.addr;
    let (info, template) = with_client(&mut engine, &mut up.hook, move || {
        let n = RemoteNode::connect_miner_service(addr, Some(&KEY)).unwrap();
        (
            n.info().unwrap(),
            n.block_template(payout(), 1_000_000).unwrap(),
        )
    });
    assert_eq!(info.network, "test");
    // what a miner does not need is not given to strangers
    assert_eq!(
        (
            info.peers,
            info.inbound,
            info.pruned_below,
            info.mempool_txs
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(template.height, info.height + 1);
    assert_eq!(
        template.block.coinbase.outputs[0].onetime_address,
        [0x0a; 32]
    );
}

#[test]
fn every_other_request_closes_the_connection_with_no_answer_and_the_node_goes_on() {
    let rig = Rig::new("allow");
    let mut engine = rig.engine();
    let mut up = node();
    let svc = service(&up, cfg());
    let addr = svc.addr;
    let others: Vec<Request> = vec![
        Request::Auth { cookie: COOKIE },
        Request::Tip,
        Request::Block { height: 0 },
        Request::Blocks { from: 0, count: 1 },
        Request::Output { index: 0 },
        Request::OutputCount,
        Request::KeyImageSpent { key_image: [0; 32] },
        Request::Rules,
        Request::Stop,
    ];
    let n = others.len();
    let results = with_client(&mut engine, &mut up.hook, move || {
        others
            .iter()
            .map(|req| {
                // a new connection for each, since the first of them ends it
                let c = RemoteNode::connect_miner_service(addr, Some(&KEY)).unwrap();
                c.request_raw(req).is_err()
            })
            .collect::<Vec<_>>()
    });
    assert_eq!(
        results,
        vec![true; n],
        "a request outside the allowlist was answered: {results:?}"
    );
    assert!(
        !up.shutdown.load(Ordering::SeqCst),
        "a miner stopped the node"
    );
    assert!(svc.stats().forbidden.load(Ordering::Relaxed) >= n as u64);
}

#[test]
fn what_a_miner_is_told_about_the_node_leaves_out_peers_the_pruning_point_and_the_pool() {
    use tenero_app::control::{NodeInfo, Response};
    let full = NodeInfo {
        height: 7,
        tip_id: [7; 32],
        peers: 5,
        inbound: 3,
        pruned_below: 100,
        mempool_txs: 9,
        syncing: true,
        kind: NodeKind::Pruned,
        network: "alpha".into(),
        version: "v".into(),
    };
    let Response::Info(t) = miner_service::trim(Response::Info(full.clone())) else {
        panic!("not info")
    };
    assert_eq!(
        (t.peers, t.inbound, t.pruned_below, t.mempool_txs),
        (0, 0, 0, 0)
    );
    // what a miner needs is kept
    assert_eq!(
        (t.height, t.tip_id, t.syncing, t.network, t.version),
        (7, [7; 32], true, "alpha".to_string(), "v".to_string())
    );
}

#[test]
fn the_allowlist_is_exactly_info_template_and_submit() {
    assert!(miner_service::allowed(&Request::Info));
    assert!(miner_service::allowed(&Request::BlockTemplate {
        payout: payout(),
        max_body_bytes: 1
    }));
    for r in [
        Request::Tip,
        Request::Stop,
        Request::Rules,
        Request::OutputCount,
        Request::Auth { cookie: [0; 32] },
    ] {
        assert!(!miner_service::allowed(&r), "{r:?} is allowed");
    }
}

#[test]
fn without_the_key_or_with_the_wrong_one_a_miner_gets_nothing() {
    let rig = Rig::new("key");
    let mut engine = rig.engine();
    let mut up = node();
    let svc = service(&up, cfg());
    let addr = svc.addr;
    let (none, wrong, right) = with_client(&mut engine, &mut up.hook, move || {
        // (the dialling side may finish its half of the handshake before the other side says no, so the failure can show
        // at the connection or at the first request: either is a refusal)
        let try_with = |key: Option<&[u8; 32]>| {
            RemoteNode::connect_miner_service(addr, key).and_then(|n| n.info().map(|_| ()))
        };
        (
            try_with(None),
            try_with(Some(&[0x78; 32])),
            try_with(Some(&KEY)),
        )
    });
    assert!(none.is_err(), "no key was accepted");
    assert!(wrong.is_err(), "a wrong key was accepted");
    assert!(right.is_ok(), "{right:?}");
    assert!(svc.stats().handshake_failed.load(Ordering::Relaxed) >= 2);
}

#[test]
fn something_that_is_not_the_protocol_is_dropped_at_the_handshake() {
    use std::io::{Read, Write};
    let up = node();
    let svc = service(&up, cfg());
    let mut s = std::net::TcpStream::connect(svc.addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let _ = s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n");
    let mut buf = [0u8; 16];
    // the node says nothing and closes
    let got = s.read(&mut buf);
    assert!(matches!(got, Ok(0) | Err(_)), "{got:?}");
}

// ---- what a stranger may cost ----------------------------------------------------------------------------------

#[test]
fn an_address_over_its_rate_limit_is_told_so_and_the_node_is_not_asked() {
    let rig = Rig::new("rate");
    let mut engine = rig.engine();
    let mut up = node();
    let svc = service(
        &up,
        ServiceConfig {
            requests_per_minute: 3,
            ..cfg()
        },
    );
    let addr = svc.addr;
    let answers = with_client(&mut engine, &mut up.hook, move || {
        let n = RemoteNode::connect_miner_service(addr, Some(&KEY)).unwrap();
        (0..6).map(|_| n.info().map(|_| ())).collect::<Vec<_>>()
    });
    assert_eq!(
        answers.iter().filter(|a| a.is_ok()).count(),
        3,
        "{answers:?}"
    );
    assert!(answers[3]
        .as_ref()
        .unwrap_err()
        .contains("too many requests"));
    assert_eq!(svc.stats().rate_limited.load(Ordering::Relaxed), 3);
}

#[test]
fn only_so_many_from_one_address_at_once_and_it_may_come_back_when_one_leaves() {
    let rig = Rig::new("cap");
    let mut engine = rig.engine();
    let mut up = node();
    let svc = service(
        &up,
        ServiceConfig {
            per_address: 1,
            ..cfg()
        },
    );
    let addr = svc.addr;
    let (first, second, third) = with_client(&mut engine, &mut up.hook, move || {
        let a = RemoteNode::connect_miner_service(addr, Some(&KEY)).unwrap();
        assert!(a.info().is_ok());
        // a second connection from the same address while the first is open
        let b =
            RemoteNode::connect_miner_service(addr, Some(&KEY)).and_then(|n| n.info().map(|_| ()));
        let still = a.info().map(|_| ());
        drop(a);
        // once the first is gone, the address may connect again (give the node a moment to see it close)
        let mut again = Err("never tried".to_string());
        for _ in 0..50 {
            again = RemoteNode::connect_miner_service(addr, Some(&KEY))
                .and_then(|n| n.info().map(|_| ()));
            if again.is_ok() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        (still, b, again)
    });
    assert!(first.is_ok(), "the first miner was dropped: {first:?}");
    assert!(
        second.is_err(),
        "a second connection from one address was served"
    );
    assert!(third.is_ok(), "the address was not let back in: {third:?}");
    assert!(svc.stats().turned_away.load(Ordering::Relaxed) >= 1);
}

#[test]
fn only_so_many_miners_at_once() {
    let rig = Rig::new("max");
    let mut engine = rig.engine();
    let mut up = node();
    let svc = service(
        &up,
        ServiceConfig {
            max_miners: 1,
            per_address: 8,
            ..cfg()
        },
    );
    let addr = svc.addr;
    let (first, second) = with_client(&mut engine, &mut up.hook, move || {
        let a = RemoteNode::connect_miner_service(addr, Some(&KEY)).unwrap();
        assert!(a.info().is_ok());
        let b =
            RemoteNode::connect_miner_service(addr, Some(&KEY)).and_then(|n| n.info().map(|_| ()));
        (a.info().map(|_| ()), b)
    });
    assert!(first.is_ok(), "{first:?}");
    assert!(second.is_err(), "a miner over the cap was served");
}

#[test]
fn a_request_frame_over_the_size_limit_closes_the_connection() {
    let up = node();
    let svc = service(&up, cfg());
    let mut s = SecureStream::connect(svc.addr, Some(&KEY)).unwrap();
    // a frame that claims to be one byte over the limit: refused before any of it is read
    let claim = (MAX_REQUEST_BYTES as u32 + 1).to_le_bytes();
    std::io::Write::write_all(&mut s, &claim).unwrap();
    std::io::Write::flush(&mut s).unwrap();
    // (a node that waited for the bytes would hold the connection until the client's own timeout, two minutes)
    let began = Instant::now();
    assert!(
        read_frame(&mut s).is_err(),
        "the node answered an oversized frame"
    );
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "the node did not hang up at once on an oversized frame"
    );
}

#[test]
fn a_frame_that_is_not_a_request_closes_the_connection() {
    let up = node();
    let svc = service(&up, cfg());
    let mut s = SecureStream::connect(svc.addr, Some(&KEY)).unwrap();
    write_frame(&mut s, &[0x7e, 1, 2, 3]).unwrap();
    assert!(read_frame(&mut s).is_err());
}

// ---- mining across the channel ---------------------------------------------------------------------------------

#[test]
fn a_miner_on_the_service_mines_blocks_the_node_accepts_and_the_wallet_can_see() {
    let rig = Rig::new("mine");
    let mut engine = rig.engine();
    let mut up = node();
    let svc = service(&up, cfg());
    let addr = svc.addr;
    let stop = Arc::new(AtomicBool::new(false));
    let (stats, tip) = thread::scope(|s| {
        let stop2 = Arc::clone(&stop);
        let t = s.spawn(move || {
            let mut rm = RemoteMiner::new(
                Miner::spawn(|| Ok(Sha256Backend)),
                WalletPayout::new(address()).unwrap(),
                PowKind::Sha256,
                RemoteMinerConfig {
                    now_secs: Arc::new(|| NOW),
                    ..RemoteMinerConfig::default()
                },
            );
            rm.run(
                || RemoteNode::connect_miner_service(addr, Some(&KEY)),
                &stop2,
                Duration::from_millis(5),
            )
            .unwrap();
            rm.stats
        });
        let end = Instant::now() + Duration::from_secs(60);
        while engine.node().tip().unwrap().0 < 8 {
            assert!(Instant::now() < end, "too slow");
            for ev in up.hook.poll(&mut engine, NOW * 1000) {
                engine.handle(NOW * 1000, ev);
            }
            thread::sleep(Duration::from_millis(1));
        }
        stop.store(true, Ordering::SeqCst);
        let end = Instant::now() + Duration::from_secs(10);
        while !t.is_finished() && Instant::now() < end {
            for ev in up.hook.poll(&mut engine, NOW * 1000) {
                engine.handle(NOW * 1000, ev);
            }
            thread::sleep(Duration::from_millis(2));
        }
        (t.join().unwrap(), engine.node().tip().unwrap())
    });
    assert!(tip.0 >= 8);
    assert!(stats.blocks_accepted >= 7, "{stats:?}");
    assert_eq!(stats.blocks_refused, 0, "{stats:?}");
    assert_eq!(stats.refused_templates, 0, "{stats:?}");
    let mut wallet = Wallet::from_seed(&[1; 32], 0);
    wallet.sync(engine.node()).unwrap();
    assert_eq!(
        wallet.owned().len() as u64,
        tip.0,
        "one reward in every block"
    );
    assert!(svc.stats().served.load(Ordering::Relaxed) > 0);
}
