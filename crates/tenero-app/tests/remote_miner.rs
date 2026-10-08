//! The miner in its own process: the node's new control messages (`block_template`, `submit_block`) against a real node,
//! and `RemoteMiner` against a real node and against a scripted fake one (for the cases a real node will not produce on
//! demand: a tip that moves between two questions, a block that loses a race, a connection that drops). The test chain
//! (SHA-256 proof of work) with the real proof check. **Not a real chain.**

use std::io::Write;
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rand_core::OsRng;
use tenero_app::client::{BlockVerdict, RemoteNode};
use tenero_app::control::{frame, read_frame, NodeInfo, NodeKind, Request, Response, Template};
use tenero_app::remote_miner::{RemoteMiner, RemoteMinerConfig};
use tenero_app::server::{start, ControlHook, Meta};
use tenero_chain::Sha256Pow;
use tenero_core::u256::U256;
use tenero_core::v2::ids::PowKind;
use tenero_core::v3::{BlockHeader, Coinbase, CoinbaseOutput, VERSION};
use tenero_miner::{
    Backend, Counters, Job, Miner, MinerEvent, Sha256Backend, Solution, WalletPayout,
};
use tenero_net::sim::{test_chain_params, LABEL};
use tenero_net::transport::Hooks;
use tenero_net::{Engine, EngineConfig, Event, Hello, Message, PROTOCOL_VERSION};
use tenero_node::{Node, NodeConfig};
use tenero_store::Store;
use tenero_wallet::testing::test_block_to;
use tenero_wallet::{coinbase_payout_to_keys, Address, Network, Wallet};

const T0: u64 = 1_700_000_000;
const COOKIE: [u8; 32] = [0x42; 32];

fn address() -> Address {
    Wallet::from_seed(&[1; 32], Network::Test, 0).address()
}

// ------------------------------------------------------------------------------------------------
// a real node
// ------------------------------------------------------------------------------------------------

struct Rig {
    path: PathBuf,
    store: Store,
    params: tenero_chain::ChainParams,
}

impl Rig {
    fn new(tag: &str) -> Rig {
        let path =
            std::env::temp_dir().join(format!("tenero-rm-{}-{tag}.redb", std::process::id()));
        remove(&path);
        let store = Store::open(&path, LABEL, PowKind::Sha256).unwrap();
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

fn meta() -> Meta {
    Meta {
        kind: NodeKind::Archive,
        network: "test".into(),
        version: "0.0.0".into(),
    }
}

/// Mines one block on the engine's tip, paying `to`, the way another miner would.
fn mine(engine: &mut Engine<'_>, to: &Address) {
    let h = engine.node().tip().unwrap().0 + 1;
    let ts = T0 + 60 * h;
    let block = test_block_to(engine.node(), to, ts);
    engine.handle(ts * 1000 + 10_000, Event::LocalBlock(block));
}

/// Makes the engine think it is syncing from a peer with a lot of work.
fn make_syncing(engine: &mut Engine<'_>, rig: &Rig) {
    engine.handle(
        T0 * 1000,
        Event::PeerConnected {
            peer: 77,
            addr: "9.9.9.9:1".into(),
            inbound: true,
        },
    );
    engine.handle(
        T0 * 1000,
        Event::Message {
            peer: 77,
            msg: Message::Hello(Hello {
                version: PROTOCOL_VERSION,
                chain_id: rig.store.chain_id(),
                tip_height: 5000,
                cumulative_work: U256::pow2(200).unwrap().to_be_bytes(),
                tip_id: [9; 32],
                pruned_below: 0,
                nonce: 0,
            }),
        },
    );
    assert!(engine.is_syncing());
}

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

/// Runs `client` on a thread while the main thread serves the control hook against the engine.
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

/// The keys of the miner's address: what a template request carries (the node makes the output from them).
type Keys = ([u8; 32], [u8; 32]);

fn keys() -> Keys {
    let a = address();
    (a.spend_pubkey, a.view_pubkey)
}

/// The Janus anchor the fake node makes its outputs with.
const ANCHOR: [u8; 16] = [0x5a; 16];

// ---- the node's side: templates --------------------------------------------------------------------------------

#[test]
fn a_template_is_a_block_on_the_tip_that_pays_the_miner_and_names_its_target() {
    let rig = Rig::new("template");
    let mut engine = rig.engine();
    let (handle, mut hook) = start(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::new(AtomicBool::new(false)),
        meta(),
    )
    .unwrap();
    for _ in 0..3 {
        mine(&mut engine, &address());
    }
    let (tip_h, tip_id) = engine.node().tip().unwrap();
    let want_target = engine.node().next_block().unwrap().target.to_be_bytes();
    let addr = handle.addr;
    let t = with_client(&mut engine, &mut hook, move || {
        let node = RemoteNode::connect(addr, &COOKIE).unwrap();
        node.block_template(&address(), 1_000_000).unwrap()
    });
    assert_eq!(t.height, tip_h + 1);
    assert_eq!(t.coinbase.height, tip_h + 1);
    assert_eq!(t.header.prev_id, tip_id);
    assert_eq!(t.target, want_target);
    // the reward goes where the miner said, in full: the output the anchor makes for the miner's address and the amount
    let o = &t.coinbase.outputs[0];
    assert!(o.amount > 0);
    let want =
        coinbase_payout_to_keys(&keys().0, &keys().1, t.height, o.amount, &t.anchor).unwrap();
    assert_eq!(
        (
            o.onetime_address,
            o.view_tag,
            o.ephemeral_pubkey,
            o.anchor_enc
        ),
        (
            want.onetime_address,
            want.view_tag,
            want.ephemeral_pubkey,
            want.anchor_enc
        )
    );
    // which is what a miner's check says too
    assert_eq!(
        tenero_app::remote_miner::check_template(&t, tip_h + 1, &tip_id, &address(), T0 + 60 * 500),
        Ok(())
    );
    // and a wallet of that address finds the reward once the block is mined
    let mut wallet = Wallet::from_seed(&[1; 32], Network::Test, 0);
    wallet.sync(engine.node()).unwrap();
    assert_eq!(wallet.owned().len() as u64, tip_h, "the earlier rewards");
    // the nonce and mix are for the miner to fill
    assert_eq!((t.header.nonce, t.header.mix), (0, [0; 64]));
}

#[test]
fn a_node_that_is_catching_up_has_no_template() {
    let rig = Rig::new("tsync");
    let mut engine = rig.engine();
    let (handle, mut hook) = start(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::new(AtomicBool::new(false)),
        meta(),
    )
    .unwrap();
    make_syncing(&mut engine, &rig);
    let addr = handle.addr;
    let e = with_client(&mut engine, &mut hook, move || {
        let node = RemoteNode::connect(addr, &COOKIE).unwrap();
        node.block_template(&address(), 1000).unwrap_err()
    });
    assert!(e.contains("syncing"), "{e}");
}

// ---- the node's side: submitted blocks ----------------------------------------------------------------------------

#[test]
fn a_mined_block_is_taken_and_a_late_one_is_said_to_have_lost_the_race() {
    let rig = Rig::new("submit");
    let mut engine = rig.engine();
    let (handle, mut hook) = start(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::new(AtomicBool::new(false)),
        meta(),
    )
    .unwrap();
    for _ in 0..2 {
        mine(&mut engine, &address());
    }
    // two competing blocks on the same tip: the first extends the chain, the second is on a side branch
    let height = engine.node().tip().unwrap().0 + 1;
    let ts = T0 + 60 * height;
    let first = test_block_to(engine.node(), &address(), ts);
    let second = test_block_to(engine.node(), &address(), ts + 1);
    assert_ne!(first.header, second.header);
    let addr = handle.addr;
    let (a, b) = with_client(&mut engine, &mut hook, move || {
        let node = RemoteNode::connect(addr, &COOKIE).unwrap();
        (
            node.submit_block(first).unwrap(),
            node.submit_block(second).unwrap(),
        )
    });
    assert!(matches!(a, BlockVerdict::InChain(_)), "{a:?}");
    assert!(matches!(b, BlockVerdict::LostRace(_)), "{b:?}");
    assert_eq!(engine.node().tip().unwrap().0, height);
}

/// The nonce that makes a header's id meet the target (the test chain's proof of work).
fn solve(mut h: BlockHeader, target: &[u8; 32]) -> BlockHeader {
    for nonce in 0.. {
        h.nonce = nonce;
        if U256::from_be_bytes(&tenero_core::v3::ids::block_id(&h, PowKind::Sha256))
            < U256::from_be_bytes(target)
        {
            return h;
        }
    }
    unreachable!()
}

#[test]
fn a_block_found_on_a_compact_template_goes_back_as_its_header_and_stale_work_is_said_to_be_stale()
{
    let rig = Rig::new("compact");
    let mut engine = rig.engine();
    let (handle, mut hook) = start(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::new(AtomicBool::new(false)),
        meta(),
    )
    .unwrap();
    for _ in 0..2 {
        mine(&mut engine, &address());
    }
    let addr = handle.addr;
    // a template, solved and handed back as its header: the node puts the block together and it joins the chain
    let (t, verdict) = with_client(&mut engine, &mut hook, move || {
        let node = RemoteNode::connect(addr, &COOKIE).unwrap();
        let t = node.block_template(&address(), u64::MAX).unwrap();
        // the header's root is the root of what the template lists
        assert_eq!(t.tx_root().unwrap(), t.header.tx_root);
        let v = node
            .submit_header(solve(t.header.clone(), &t.target))
            .unwrap();
        (t, v)
    });
    assert!(matches!(verdict, BlockVerdict::InChain(_)), "{verdict:?}");
    assert_eq!(engine.node().tip().unwrap().0, t.height);
    let stored = engine.node().store().get_block(t.height).unwrap().unwrap();
    assert_eq!(stored.coinbase, t.coinbase, "the block is the template's");
    // the tip has moved: that template is gone, and so is any header naming a template the node never gave out
    let (old, unknown) = with_client(&mut engine, &mut hook, move || {
        let node = RemoteNode::connect(addr, &COOKIE).unwrap();
        let old = node
            .submit_header(solve(t.header.clone(), &t.target))
            .unwrap();
        let fresh = node.block_template(&address(), u64::MAX).unwrap();
        let mut forged = fresh.header.clone();
        forged.tx_root = [0x77; 32];
        let unknown = node.submit_header(solve(forged, &fresh.target)).unwrap();
        (old, unknown)
    });
    for v in [old, unknown] {
        assert!(
            matches!(&v, BlockVerdict::Refused(why) if why.contains("stale")),
            "{v:?}"
        );
    }
    assert_eq!(engine.node().tip().unwrap().0, 3);
}

#[test]
fn a_block_that_is_not_valid_is_refused_with_a_reason_and_the_chain_is_unchanged() {
    let rig = Rig::new("refuse");
    let mut engine = rig.engine();
    let (handle, mut hook) = start(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::new(AtomicBool::new(false)),
        meta(),
    )
    .unwrap();
    mine(&mut engine, &address());
    let tip_before = engine.node().tip().unwrap();
    let height = tip_before.0 + 1;
    let good = test_block_to(engine.node(), &address(), T0 + 60 * height);
    // the same block with a nonce that does not meet the target (the chain's target is one in four, so look for one)
    let mut bad = good.clone();
    let target = engine.node().next_block().unwrap().target;
    for nonce in 0.. {
        bad.header.nonce = nonce;
        let id = tenero_core::v3::ids::block_id(&bad.header, PowKind::Sha256);
        if U256::from_be_bytes(&id) >= target {
            break;
        }
    }
    // and one that pays itself too much
    let mut greedy = good.clone();
    greedy.coinbase.outputs[0].amount += 1;
    let addr = handle.addr;
    let (a, b) = with_client(&mut engine, &mut hook, move || {
        let node = RemoteNode::connect(addr, &COOKIE).unwrap();
        (
            node.submit_block(bad).unwrap(),
            node.submit_block(greedy).unwrap(),
        )
    });
    assert!(
        matches!(a, BlockVerdict::Refused(ref why) if !why.is_empty()),
        "{a:?}"
    );
    assert!(matches!(b, BlockVerdict::Refused(_)), "{b:?}");
    assert_eq!(engine.node().tip().unwrap(), tip_before);
}

// ---- the miner against a real node ---------------------------------------------------------------------------------

fn cfg() -> RemoteMinerConfig {
    RemoteMinerConfig::default()
}

/// A configuration that records what the miner tells the screen.
fn cfg_events() -> (RemoteMinerConfig, Arc<Mutex<Vec<MinerEvent>>>) {
    let ev = Arc::new(Mutex::new(Vec::new()));
    let e2 = Arc::clone(&ev);
    (
        RemoteMinerConfig {
            events: Arc::new(move |e| e2.lock().unwrap().push(e)),
            ..RemoteMinerConfig::default()
        },
        ev,
    )
}

fn sha_miner(cfg: RemoteMinerConfig) -> RemoteMiner {
    RemoteMiner::new(
        Miner::spawn(|| Ok(Sha256Backend)),
        WalletPayout::new(address()).unwrap(),
        PowKind::Sha256,
        cfg,
    )
}

#[test]
fn a_miner_in_another_process_mines_blocks_the_node_accepts_and_the_wallet_can_see() {
    let rig = Rig::new("e2e");
    let mut engine = rig.engine();
    let (handle, mut hook) = start(
        "127.0.0.1:0".parse().unwrap(),
        COOKIE,
        Arc::new(AtomicBool::new(false)),
        meta(),
    )
    .unwrap();
    let addr = handle.addr;
    let stop = Arc::new(AtomicBool::new(false));
    let (stats, tip) = thread::scope(|s| {
        let stop2 = Arc::clone(&stop);
        let t = s.spawn(move || {
            let mut rm = sha_miner(RemoteMinerConfig {
                // the node's clock in this test is the pump's, in 2023
                now_secs: Arc::new(|| T0 + 60 * 500),
                ..cfg()
            });
            rm.run(
                || RemoteNode::connect(addr, &COOKIE),
                &stop2,
                Duration::from_millis(5),
            )
            .unwrap();
            rm.stats
        });
        let end = Instant::now() + Duration::from_secs(60);
        while engine.node().tip().unwrap().0 < 10 {
            assert!(Instant::now() < end, "too slow");
            let now = (T0 + 60 * 500) * 1000;
            for ev in hook.poll(&mut engine, now) {
                engine.handle(now, ev);
            }
            thread::sleep(Duration::from_millis(1));
        }
        // let the last verdict be given
        for _ in 0..50 {
            let now = (T0 + 60 * 500) * 1000;
            for ev in hook.poll(&mut engine, now) {
                engine.handle(now, ev);
            }
            thread::sleep(Duration::from_millis(2));
        }
        stop.store(true, Ordering::SeqCst);
        // the miner's last request may be waiting for the hook: keep serving until it is done
        let end = Instant::now() + Duration::from_secs(10);
        while !t.is_finished() && Instant::now() < end {
            let now = (T0 + 60 * 500) * 1000;
            for ev in hook.poll(&mut engine, now) {
                engine.handle(now, ev);
            }
            thread::sleep(Duration::from_millis(2));
        }
        (t.join().unwrap(), engine.node().tip().unwrap())
    });
    assert!(tip.0 >= 10);
    assert!(stats.blocks_found >= 10, "{stats:?}");
    assert_eq!(stats.blocks_refused, 0, "{stats:?}");
    assert!(stats.blocks_accepted >= 9, "{stats:?}");
    assert_eq!(stats.bad_solutions, 0);
    // every reward is the miner's wallet's, readable (the key exchange binds the height, so a template built for the
    // wrong height would not show up here)
    let mut wallet = Wallet::from_seed(&[1; 32], Network::Test, 0);
    wallet.sync(engine.node()).unwrap();
    assert_eq!(
        wallet.owned().len() as u64,
        tip.0,
        "one reward in every block"
    );
    let _ = OsRng;
}

// ------------------------------------------------------------------------------------------------
// a scripted fake node
// ------------------------------------------------------------------------------------------------

type Handler = Box<dyn FnMut(&Request) -> Option<Response> + Send>;

struct Fake {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Speaks the control protocol: authenticates with `COOKIE`, then answers each request with what `handler` says (`None`
/// closes the connection, as a node that goes away would).
fn fake(handler: Handler) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let handler = Arc::new(Mutex::new(handler));
    {
        let (seen, stop) = (Arc::clone(&seen), Arc::clone(&stop));
        thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let (seen, handler) = (Arc::clone(&seen), Arc::clone(&handler));
                        thread::spawn(move || {
                            stream.set_nonblocking(false).unwrap();
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                            let Ok(body) = read_frame(&mut stream) else {
                                return;
                            };
                            if Request::from_body(&body) != Ok(Request::Auth { cookie: COOKIE }) {
                                return;
                            }
                            let _ = stream
                                .write_all(&frame(&Response::Authed.to_body().unwrap()).unwrap());
                            while let Ok(body) = read_frame(&mut stream) {
                                let Ok(req) = Request::from_body(&body) else {
                                    return;
                                };
                                seen.lock().unwrap().push(req.clone());
                                let answer = (handler.lock().unwrap())(&req);
                                let Some(answer) = answer else { return };
                                if stream
                                    .write_all(&frame(&answer.to_body().unwrap()).unwrap())
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        });
                    }
                    Err(_) => thread::sleep(Duration::from_millis(5)),
                }
            }
        });
    }
    Fake { addr, seen, stop }
}

impl Fake {
    fn count(&self, f: impl Fn(&Request) -> bool) -> usize {
        self.seen.lock().unwrap().iter().filter(|r| f(r)).count()
    }
    fn node(&self) -> RemoteNode {
        RemoteNode::connect(self.addr, &COOKIE).unwrap()
    }
}

fn info(height: u64, tip: u8, syncing: bool) -> Response {
    Response::Info(NodeInfo {
        height,
        tip_id: [tip; 32],
        peers: 1,
        inbound: 0,
        pruned_below: 0,
        mempool_txs: 0,
        syncing,
        kind: NodeKind::Archive,
        network: "test".into(),
        version: "0".into(),
    })
}

/// A template on `tip` for `height` with a target that almost every id meets, as an honest node builds it for the miner
/// that asked: its coinbase pays exactly the output [`ANCHOR`] makes for the keys at that height, the header's transaction
/// root matches the body, and the timestamp is now (`remote_miner::check_template` refuses anything else).
fn template(keys: &Keys, height: u64, tip: u8, target: [u8; 32]) -> Response {
    let p = coinbase_payout_to_keys(&keys.0, &keys.1, height, 1, &ANCHOR).expect("valid keys");
    let coinbase = Coinbase {
        version: VERSION,
        height,
        outputs: vec![CoinbaseOutput {
            onetime_address: p.onetime_address,
            amount: 1,
            view_tag: p.view_tag,
            ephemeral_pubkey: p.ephemeral_pubkey,
            anchor_enc: p.anchor_enc,
        }],
        extra: vec![],
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    Response::Template(Template {
        header: BlockHeader {
            version: VERSION,
            prev_id: [tip; 32],
            timestamp: now,
            tx_root: tenero_core::v3::ids::block_tx_root(&coinbase, &[]).unwrap(),
            nonce: 0,
            mix: [0; 64],
        },
        coinbase,
        tx_ids: vec![],
        height,
        target,
        anchor: ANCHOR,
    })
}

const EASY: [u8; 32] = [0xff; 32];
const IMPOSSIBLE: [u8; 32] = [0; 32];

/// A backend that never finds anything and records the height of each job it is given and whether it was stopped.
type JobRecord = (u64, Arc<AtomicBool>);

#[derive(Clone, Default)]
struct Jobs(Arc<Mutex<Vec<JobRecord>>>);

struct Idle(Jobs);

impl Backend for Idle {
    fn name(&self) -> String {
        "idle".into()
    }
    fn mine(&mut self, job: &Job, _c: &Counters) -> Result<Option<Solution>, String> {
        (self.0)
            .0
            .lock()
            .unwrap()
            .push((job.height, Arc::clone(&job.stale)));
        while !job.stale.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(1));
        }
        Ok(None)
    }
}

/// Drives `step` until `done` or the time is up.
fn drive(
    rm: &mut RemoteMiner,
    node: &RemoteNode,
    secs: u64,
    mut done: impl FnMut(&RemoteMiner) -> bool,
) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        rm.step(node).unwrap();
        if done(rm) {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    false
}

fn idle_miner(jobs: &Jobs, cfg: RemoteMinerConfig) -> RemoteMiner {
    let j = jobs.clone();
    RemoteMiner::new(
        Miner::spawn(move || Ok(Idle(j))),
        WalletPayout::new(address()).unwrap(),
        PowKind::Sha256,
        cfg,
    )
}

fn is_template(r: &Request) -> bool {
    matches!(r, Request::BlockTemplate { .. })
}

fn is_submit(r: &Request) -> bool {
    matches!(r, Request::SubmitHeader(_))
}

// ---- what a node says about a block --------------------------------------------------------------------------------

fn verdict_miner(answer: Response) -> (Fake, RemoteMiner, Arc<Mutex<Vec<MinerEvent>>>) {
    let f = fake(Box::new(move |r| {
        Some(match r {
            Request::Info => info(5, 5, false),
            Request::BlockTemplate {
                spend_pubkey,
                view_pubkey,
                ..
            } => template(&(*spend_pubkey, *view_pubkey), 6, 5, EASY),
            Request::SubmitHeader(_) => answer.clone(),
            _ => return None,
        })
    }));
    let (c, ev) = cfg_events();
    (f, sha_miner(c), ev)
}

#[test]
fn a_block_in_the_chain_is_counted() {
    let (f, mut rm, ev) = verdict_miner(Response::BlockSubmitted {
        id: [1; 32],
        in_chain: true,
    });
    let node = f.node();
    assert!(drive(&mut rm, &node, 20, |m| m.stats.blocks_accepted >= 1));
    assert_eq!(
        rm.stats.blocks_found,
        rm.stats.blocks_accepted + rm.stats.blocks_lost_race + rm.stats.blocks_refused
    );
    assert_eq!((rm.stats.blocks_lost_race, rm.stats.blocks_refused), (0, 0));
    // the screen is told, with the time it took and what the block pays (the reward of the block that was submitted)
    let seen = f.seen.lock().unwrap();
    let Some(Request::SubmitHeader(_)) = seen.iter().find(|r| is_submit(r)) else {
        panic!("no block was submitted")
    };
    // what the fake's template pays
    let paid = 1;
    let ev = ev.lock().unwrap();
    // (stepping the miner by hand: the connection events belong to `run`)
    assert!(matches!(ev[0], MinerEvent::Started { .. }), "{ev:?}");
    assert!(
        ev.iter().any(|e| matches!(e, MinerEvent::Started { .. })),
        "{ev:?}"
    );
    let got = ev.iter().find_map(|e| match e {
        MinerEvent::InChain {
            height,
            secs,
            reward,
            work,
        } => {
            // what the block is worth: the work of the target the node named in the template
            assert_eq!(
                *work,
                tenero_miner::rate::work_of(&U256::from_be_bytes(&EASY))
            );
            Some((*height, *secs, *reward))
        }
        _ => None,
    });
    let (h, secs, reward) = got.expect("an InChain event");
    assert_eq!((h, reward), (6, paid));
    assert!(secs.is_finite() && (0.0..20.0).contains(&secs), "{secs}");
}

#[test]
fn a_block_that_lost_a_race_is_counted_as_that_and_not_as_accepted() {
    let (f, mut rm, ev) = verdict_miner(Response::BlockSubmitted {
        id: [1; 32],
        in_chain: false,
    });
    let node = f.node();
    assert!(drive(&mut rm, &node, 20, |m| m.stats.blocks_lost_race >= 1));
    assert_eq!((rm.stats.blocks_accepted, rm.stats.blocks_refused), (0, 0));
    let ev = ev.lock().unwrap();
    assert!(ev.contains(&MinerEvent::LostRace { height: 6 }), "{ev:?}");
    assert!(!ev
        .iter()
        .any(|e| matches!(e, MinerEvent::InChain { .. } | MinerEvent::Refused { .. })));
}

#[test]
fn a_block_the_node_refuses_is_counted_and_mining_goes_on() {
    let (f, mut rm, ev) = verdict_miner(Response::Error(
        "the node refused the block (it is not valid)".into(),
    ));
    let node = f.node();
    assert!(
        drive(&mut rm, &node, 20, |m| m.stats.blocks_refused >= 2),
        "{:?}",
        rm.stats
    );
    assert_eq!(
        (rm.stats.blocks_accepted, rm.stats.blocks_lost_race),
        (0, 0)
    );
    assert!(rm.failure().is_none());
    let ev = ev.lock().unwrap();
    assert!(ev.contains(&MinerEvent::Refused { height: 6 }), "{ev:?}");
    assert!(!ev.iter().any(|e| matches!(e, MinerEvent::InChain { .. })));
}

#[test]
fn a_found_block_has_the_nonce_the_backend_found_and_meets_the_target() {
    let (f, mut rm, _) = verdict_miner(Response::BlockSubmitted {
        id: [1; 32],
        in_chain: true,
    });
    let node = f.node();
    assert!(drive(&mut rm, &node, 20, |m| m.stats.blocks_found >= 1));
    let seen = f.seen.lock().unwrap();
    // a found block goes back as its header alone: the template's, with the nonce found
    let Some(Request::SubmitHeader(h)) = seen.iter().find(|r| is_submit(r)) else {
        panic!("no block was submitted")
    };
    let id = tenero_core::v3::ids::block_id(h, PowKind::Sha256);
    assert!(U256::from_be_bytes(&id) < U256::from_be_bytes(&EASY));
    assert_eq!(h.prev_id, [5; 32]);
    assert_eq!(h.mix, [0; 64], "the test chain's mix is zero");
}

// ---- when to start, stop and replace a job -----------------------------------------------------------------------

#[test]
fn a_node_that_is_syncing_gets_no_mining() {
    let syncing = Arc::new(AtomicBool::new(false));
    let s2 = Arc::clone(&syncing);
    let f = fake(Box::new(move |r| {
        Some(match r {
            Request::Info => info(5, 5, s2.load(Ordering::SeqCst)),
            Request::BlockTemplate {
                spend_pubkey,
                view_pubkey,
                ..
            } => template(&(*spend_pubkey, *view_pubkey), 6, 5, EASY),
            _ => return None,
        })
    }));
    let jobs = Jobs::default();
    let (c, ev) = cfg_events();
    let mut rm = idle_miner(&jobs, c);
    let node = f.node();
    // syncing from the first look: no template is even asked for
    syncing.store(true, Ordering::SeqCst);
    for _ in 0..10 {
        rm.step(&node).unwrap();
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(f.count(is_template), 0);
    assert!(jobs.0.lock().unwrap().is_empty());
    // in sync: a job starts
    syncing.store(false, Ordering::SeqCst);
    assert!(drive(&mut rm, &node, 10, |_| !jobs
        .0
        .lock()
        .unwrap()
        .is_empty()));
    // and when the node falls behind again the job is stopped
    syncing.store(true, Ordering::SeqCst);
    rm.step(&node).unwrap();
    assert!(
        jobs.0.lock().unwrap()[0].1.load(Ordering::SeqCst),
        "the job was told to stop"
    );
    assert_eq!(rm.stats.paused_syncing, 1);
    let asked = f.count(is_template);
    for _ in 0..5 {
        rm.step(&node).unwrap();
    }
    assert_eq!(f.count(is_template), asked, "no new template while syncing");
    assert_eq!(rm.stats.paused_syncing, 1, "one pause, counted once");
    // the screen is told each change once: paused, resumed, paused
    let changes: Vec<MinerEvent> = ev
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, MinerEvent::Paused | MinerEvent::Resumed))
        .cloned()
        .collect();
    assert_eq!(
        changes,
        vec![MinerEvent::Paused, MinerEvent::Resumed, MinerEvent::Paused]
    );
}

#[test]
fn a_moved_tip_replaces_the_job_and_the_old_one_is_stopped() {
    let tip = Arc::new(AtomicU64::new(5));
    let t2 = Arc::clone(&tip);
    let f = fake(Box::new(move |r| {
        let t = t2.load(Ordering::SeqCst);
        Some(match r {
            Request::Info => info(t, t as u8, false),
            Request::BlockTemplate {
                spend_pubkey,
                view_pubkey,
                ..
            } => template(&(*spend_pubkey, *view_pubkey), t + 1, t as u8, EASY),
            _ => return None,
        })
    }));
    let jobs = Jobs::default();
    let mut rm = idle_miner(&jobs, cfg());
    let node = f.node();
    assert!(drive(&mut rm, &node, 10, |_| jobs.0.lock().unwrap().len() == 1));
    // while the tip stays, no new job
    for _ in 0..10 {
        rm.step(&node).unwrap();
        thread::sleep(Duration::from_millis(3));
    }
    assert_eq!(f.count(is_template), 1, "the same job is kept");
    tip.store(6, Ordering::SeqCst);
    assert!(drive(&mut rm, &node, 10, |_| jobs.0.lock().unwrap().len() == 2));
    let j = jobs.0.lock().unwrap();
    assert_eq!(
        (j[0].0, j[1].0),
        (6, 7),
        "the second job is for the next height"
    );
    assert!(j[0].1.load(Ordering::SeqCst), "the old job was stopped");
    assert!(!j[1].1.load(Ordering::SeqCst));
}

#[test]
fn an_old_template_is_replaced_even_if_the_tip_has_not_moved() {
    let f = fake(Box::new(|r| {
        Some(match r {
            Request::Info => info(5, 5, false),
            Request::BlockTemplate {
                spend_pubkey,
                view_pubkey,
                ..
            } => template(&(*spend_pubkey, *view_pubkey), 6, 5, EASY),
            _ => return None,
        })
    }));
    let jobs = Jobs::default();
    let mut rm = idle_miner(
        &jobs,
        RemoteMinerConfig {
            refresh_every: Duration::from_millis(150),
            ..cfg()
        },
    );
    let node = f.node();
    assert!(drive(&mut rm, &node, 10, |_| jobs.0.lock().unwrap().len() >= 3));
    assert!(f.count(is_template) >= 3);
}

#[test]
fn a_template_for_the_wrong_height_is_not_mined() {
    // the node's tip moved between "what is your tip" and "give me a template": the reward would be addressed for the
    // wrong height, so the template must be dropped
    let f = fake(Box::new(|r| {
        Some(match r {
            Request::Info => info(5, 5, false),
            Request::BlockTemplate {
                spend_pubkey,
                view_pubkey,
                ..
            } => template(&(*spend_pubkey, *view_pubkey), 7, 6, EASY),
            _ => return None,
        })
    }));
    let jobs = Jobs::default();
    let mut rm = idle_miner(&jobs, cfg());
    let node = f.node();
    assert!(drive(&mut rm, &node, 10, |m| m.stats.stale_templates >= 2));
    assert!(
        jobs.0.lock().unwrap().is_empty(),
        "no job from a stale template"
    );
}

#[test]
fn a_miner_waits_between_blocks_when_told_to() {
    let (f, mut rm) = {
        let f = fake(Box::new(|r| {
            Some(match r {
                Request::Info => info(5, 5, false),
                Request::BlockTemplate {
                    spend_pubkey,
                    view_pubkey,
                    ..
                } => template(&(*spend_pubkey, *view_pubkey), 6, 5, EASY),
                Request::SubmitHeader(_) => Response::BlockSubmitted {
                    id: [1; 32],
                    in_chain: true,
                },
                _ => return None,
            })
        }));
        let rm = sha_miner(RemoteMinerConfig {
            min_block_interval: Duration::from_millis(600),
            ..cfg()
        });
        (f, rm)
    };
    let node = f.node();
    let t = Instant::now();
    assert!(drive(&mut rm, &node, 20, |m| m.stats.blocks_found >= 3));
    assert!(
        t.elapsed() >= Duration::from_millis(1100),
        "three blocks in {:?}",
        t.elapsed()
    );
}

// ---- what a backend may do ---------------------------------------------------------------------------------------

/// A backend that returns a nonce that does not meet the target.
struct Liar;

impl Backend for Liar {
    fn name(&self) -> String {
        "liar".into()
    }
    fn mine(&mut self, job: &Job, _c: &Counters) -> Result<Option<Solution>, String> {
        Ok(Some(Solution {
            job_id: job.id,
            nonce: 1,
            mix: [0; 64],
        }))
    }
}

#[test]
fn a_solution_that_does_not_meet_the_target_is_never_submitted() {
    let f = fake(Box::new(|r| {
        Some(match r {
            Request::Info => info(5, 5, false),
            Request::BlockTemplate {
                spend_pubkey,
                view_pubkey,
                ..
            } => template(&(*spend_pubkey, *view_pubkey), 6, 5, IMPOSSIBLE),
            _ => return None,
        })
    }));
    let mut rm = RemoteMiner::new(
        Miner::spawn(|| Ok(Liar)),
        WalletPayout::new(address()).unwrap(),
        PowKind::Sha256,
        cfg(),
    );
    let node = f.node();
    assert!(drive(&mut rm, &node, 10, |m| m.stats.bad_solutions >= 2));
    assert_eq!(f.count(is_submit), 0);
    assert_eq!(rm.stats.blocks_found, 0);
}

/// A backend that answers job 1 only after it has been replaced (a late solution for a job that is gone).
struct Late;

impl Backend for Late {
    fn name(&self) -> String {
        "late".into()
    }
    fn mine(&mut self, job: &Job, _c: &Counters) -> Result<Option<Solution>, String> {
        while !job.stale.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(1));
        }
        // a perfectly good-looking answer, for a job nobody wants any more
        Ok(Some(Solution {
            job_id: job.id,
            nonce: 5,
            mix: [0; 64],
        }))
    }
}

#[test]
fn a_late_solution_for_a_replaced_job_is_ignored() {
    let tip = Arc::new(AtomicU64::new(5));
    let t2 = Arc::clone(&tip);
    let f = fake(Box::new(move |r| {
        let t = t2.load(Ordering::SeqCst);
        Some(match r {
            Request::Info => info(t, t as u8, false),
            Request::BlockTemplate {
                spend_pubkey,
                view_pubkey,
                ..
            } => template(&(*spend_pubkey, *view_pubkey), t + 1, t as u8, EASY),
            _ => return None,
        })
    }));
    let mut rm = RemoteMiner::new(
        Miner::spawn(|| Ok(Late)),
        WalletPayout::new(address()).unwrap(),
        PowKind::Sha256,
        cfg(),
    );
    let node = f.node();
    assert!(drive(&mut rm, &node, 10, |_| f.count(is_template) == 1));
    tip.store(6, Ordering::SeqCst);
    assert!(drive(&mut rm, &node, 10, |_| f.count(is_template) == 2));
    // let the first job's late answer arrive and be looked at
    for _ in 0..40 {
        rm.step(&node).unwrap();
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        f.count(is_submit),
        0,
        "a block for a job that was replaced must not be submitted"
    );
    assert_eq!(rm.stats.blocks_found, 0);
}

struct Broken;

impl Backend for Broken {
    fn name(&self) -> String {
        "broken".into()
    }
    fn mine(&mut self, _job: &Job, _c: &Counters) -> Result<Option<Solution>, String> {
        Err("no GPU".into())
    }
}

#[test]
fn a_backend_that_fails_stops_the_miner_with_its_reason() {
    let f = fake(Box::new(|r| {
        Some(match r {
            Request::Info => info(5, 5, false),
            Request::BlockTemplate {
                spend_pubkey,
                view_pubkey,
                ..
            } => template(&(*spend_pubkey, *view_pubkey), 6, 5, EASY),
            _ => return None,
        })
    }));
    let (bcfg, bev) = cfg_events();
    let mut rm = RemoteMiner::new(
        Miner::spawn(|| Ok(Broken)),
        WalletPayout::new(address()).unwrap(),
        PowKind::Sha256,
        bcfg,
    );
    let addr = f.addr;
    let stop = AtomicBool::new(false);
    let r = rm.run(
        || RemoteNode::connect(addr, &COOKIE),
        &stop,
        Duration::from_millis(5),
    );
    let e = r.unwrap_err();
    assert!(e.contains("no GPU"), "{e}");
    assert!(
        bev.lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, MinerEvent::BackendFailed { why } if why.contains("no GPU"))),
        "{:?}",
        bev.lock().unwrap()
    );
    // and one that cannot even be built
    let mut rm = RemoteMiner::new(
        Miner::spawn(|| -> Result<Broken, String> { Err("no device".into()) }),
        WalletPayout::new(address()).unwrap(),
        PowKind::Sha256,
        cfg(),
    );
    let e = rm
        .run(
            || RemoteNode::connect(addr, &COOKIE),
            &stop,
            Duration::from_millis(5),
        )
        .unwrap_err();
    assert!(e.contains("no device"), "{e}");
}

// ---- the connection ----------------------------------------------------------------------------------------------

#[test]
fn a_miner_whose_node_goes_away_reconnects_and_carries_on() {
    let n = Arc::new(AtomicU64::new(0));
    let n2 = Arc::clone(&n);
    let f = fake(Box::new(move |r| {
        let k = n2.fetch_add(1, Ordering::SeqCst);
        // the third request of the first connection is never answered: the node goes away
        if k == 2 {
            return None;
        }
        Some(match r {
            Request::Info => info(5, 5, false),
            Request::BlockTemplate {
                spend_pubkey,
                view_pubkey,
                ..
            } => template(&(*spend_pubkey, *view_pubkey), 6, 5, EASY),
            Request::SubmitHeader(_) => Response::BlockSubmitted {
                id: [1; 32],
                in_chain: true,
            },
            _ => return None,
        })
    }));
    let addr = f.addr;
    let stop = Arc::new(AtomicBool::new(false));
    let connects = Arc::new(AtomicU64::new(0));
    let (mcfg, ev) = cfg_events();
    let (stats, ()) = thread::scope(|s| {
        let (stop2, c2) = (Arc::clone(&stop), Arc::clone(&connects));
        let t = s.spawn(move || {
            let mut rm = sha_miner(mcfg);
            rm.run(
                || {
                    c2.fetch_add(1, Ordering::SeqCst);
                    RemoteNode::connect(addr, &COOKIE)
                },
                &stop2,
                Duration::from_millis(5),
            )
            .unwrap();
            rm.stats
        });
        let end = Instant::now() + Duration::from_secs(20);
        while connects.load(Ordering::SeqCst) < 2 || f.count(is_submit) < 1 {
            assert!(
                Instant::now() < end,
                "never reconnected: {} connections",
                connects.load(Ordering::SeqCst)
            );
            thread::sleep(Duration::from_millis(20));
        }
        // (the verdict on that block comes back a moment after it is submitted)
        thread::sleep(Duration::from_millis(300));
        stop.store(true, Ordering::SeqCst);
        (t.join().unwrap(), ())
    });
    assert_eq!(stats.connections_lost, 1, "{stats:?}");
    assert!(
        stats.blocks_accepted >= 1,
        "mining resumed after the reconnect: {stats:?}"
    );
    // the screen is told: connected, lost, connected again
    let net: Vec<String> = ev
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            MinerEvent::NodeConnected => Some("connected".to_string()),
            MinerEvent::NodeLost { .. } => Some("lost".to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(net, vec!["connected", "lost", "connected"]);
}

#[test]
fn a_miner_started_before_its_node_waits_for_it() {
    let f = fake(Box::new(|r| {
        Some(match r {
            Request::Info => info(5, 5, false),
            Request::BlockTemplate {
                spend_pubkey,
                view_pubkey,
                ..
            } => template(&(*spend_pubkey, *view_pubkey), 6, 5, EASY),
            Request::SubmitHeader(_) => Response::BlockSubmitted {
                id: [1; 32],
                in_chain: true,
            },
            _ => return None,
        })
    }));
    let addr = f.addr;
    let stop = Arc::new(AtomicBool::new(false));
    let attempts = Arc::new(AtomicU64::new(0));
    let (mcfg, ev) = cfg_events();
    thread::scope(|s| {
        let (stop2, a2) = (Arc::clone(&stop), Arc::clone(&attempts));
        let t = s.spawn(move || {
            let mut rm = sha_miner(mcfg);
            rm.run(
                || {
                    // the node is "not up" for the first two tries
                    if a2.fetch_add(1, Ordering::SeqCst) < 2 {
                        Err("cannot reach the node".to_string())
                    } else {
                        RemoteNode::connect(addr, &COOKIE)
                    }
                },
                &stop2,
                Duration::from_millis(5),
            )
            .unwrap();
        });
        let end = Instant::now() + Duration::from_secs(20);
        while f.count(is_submit) < 1 {
            assert!(
                Instant::now() < end,
                "no block after {} connection attempts",
                attempts.load(Ordering::SeqCst)
            );
            thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Ordering::SeqCst);
        t.join().unwrap();
    });
    assert!(attempts.load(Ordering::SeqCst) >= 3);
    // two failed tries are ONE report, and then the connection
    let net: Vec<MinerEvent> = ev
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, MinerEvent::NodeConnected | MinerEvent::NodeLost { .. }))
        .cloned()
        .collect();
    assert_eq!(net.len(), 2, "{net:?}");
    assert!(
        matches!(&net[0], MinerEvent::NodeLost { why } if why.contains("cannot reach the node"))
    );
    assert_eq!(net[1], MinerEvent::NodeConnected);
}

#[test]
fn the_miner_stops_promptly_when_told_to_even_while_waiting_for_its_node() {
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = Arc::clone(&stop);
    let t = thread::spawn(move || {
        let mut rm = sha_miner(cfg());
        let started = Instant::now();
        rm.run(|| Err("never".to_string()), &s2, Duration::from_millis(5))
            .unwrap();
        started.elapsed()
    });
    thread::sleep(Duration::from_millis(700));
    stop.store(true, Ordering::SeqCst);
    let took = t.join().unwrap();
    assert!(took < Duration::from_secs(3), "{took:?}");
}

/// A backend that finds a nonce at once and says the mix is something other than zero (as the real proof of work does).
struct Mixer;

impl Backend for Mixer {
    fn name(&self) -> String {
        "mixer".into()
    }
    fn mine(&mut self, job: &Job, _c: &Counters) -> Result<Option<Solution>, String> {
        Ok(Some(Solution {
            job_id: job.id,
            nonce: 12345,
            mix: [7; 64],
        }))
    }
}

#[test]
fn the_nonce_and_the_mix_the_backend_found_are_what_is_submitted() {
    let f = fake(Box::new(|r| {
        Some(match r {
            Request::Info => info(5, 5, false),
            Request::BlockTemplate {
                spend_pubkey,
                view_pubkey,
                ..
            } => template(&(*spend_pubkey, *view_pubkey), 6, 5, EASY),
            Request::SubmitHeader(_) => Response::BlockSubmitted {
                id: [1; 32],
                in_chain: true,
            },
            _ => return None,
        })
    }));
    let mut rm = RemoteMiner::new(
        Miner::spawn(|| Ok(Mixer)),
        WalletPayout::new(address()).unwrap(),
        PowKind::Sha256,
        cfg(),
    );
    let node = f.node();
    assert!(drive(&mut rm, &node, 10, |m| m.stats.blocks_found >= 1));
    let seen = f.seen.lock().unwrap();
    let Some(Request::SubmitHeader(h)) = seen.iter().find(|r| is_submit(r)) else {
        panic!("no block was submitted")
    };
    assert_eq!((h.nonce, h.mix), (12345, [7; 64]));
}

#[test]
fn a_template_whose_coinbase_is_for_another_height_is_not_mined_either() {
    // the template says it is for the height asked, but the coinbase inside is for the next one
    let f = fake(Box::new(|r| {
        Some(match r {
            Request::Info => info(5, 5, false),
            Request::BlockTemplate {
                spend_pubkey,
                view_pubkey,
                ..
            } => {
                let Response::Template(mut t) =
                    template(&(*spend_pubkey, *view_pubkey), 6, 5, EASY)
                else {
                    unreachable!()
                };
                t.coinbase.height = 7;
                Response::Template(t)
            }
            _ => return None,
        })
    }));
    let jobs = Jobs::default();
    let mut rm = idle_miner(&jobs, cfg());
    let node = f.node();
    assert!(drive(&mut rm, &node, 10, |m| m.stats.stale_templates >= 2));
    assert!(jobs.0.lock().unwrap().is_empty());
}

#[test]
fn a_job_is_stopped_even_when_the_template_that_should_replace_it_is_unusable() {
    let tip = Arc::new(AtomicU64::new(5));
    let wrong = Arc::new(AtomicBool::new(false));
    let (t2, w2) = (Arc::clone(&tip), Arc::clone(&wrong));
    let f = fake(Box::new(move |r| {
        let t = t2.load(Ordering::SeqCst);
        Some(match r {
            Request::Info => info(t, t as u8, false),
            // after the tip moves, the template the node gives is for the wrong height
            Request::BlockTemplate {
                spend_pubkey,
                view_pubkey,
                ..
            } if w2.load(Ordering::SeqCst) => {
                template(&(*spend_pubkey, *view_pubkey), t + 5, t as u8, EASY)
            }
            Request::BlockTemplate {
                spend_pubkey,
                view_pubkey,
                ..
            } => template(&(*spend_pubkey, *view_pubkey), t + 1, t as u8, EASY),
            _ => return None,
        })
    }));
    let jobs = Jobs::default();
    let mut rm = idle_miner(&jobs, cfg());
    let node = f.node();
    assert!(drive(&mut rm, &node, 10, |_| jobs.0.lock().unwrap().len() == 1));
    tip.store(6, Ordering::SeqCst);
    wrong.store(true, Ordering::SeqCst);
    assert!(drive(&mut rm, &node, 10, |m| m.stats.stale_templates >= 1));
    assert!(
        jobs.0.lock().unwrap()[0].1.load(Ordering::SeqCst),
        "the job on the old tip was left running"
    );
}

#[test]
fn a_node_that_cannot_be_reached_is_tried_again_with_a_pause_not_in_a_tight_loop() {
    let attempts = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let (a2, s2) = (Arc::clone(&attempts), Arc::clone(&stop));
    let t = thread::spawn(move || {
        let mut rm = sha_miner(cfg());
        rm.run(
            || {
                a2.fetch_add(1, Ordering::SeqCst);
                Err("down".to_string())
            },
            &s2,
            Duration::from_millis(5),
        )
        .unwrap();
    });
    thread::sleep(Duration::from_millis(1300));
    stop.store(true, Ordering::SeqCst);
    t.join().unwrap();
    let n = attempts.load(Ordering::SeqCst);
    // a first try, then pauses of half a second, a second, ...: three or four tries in 1.3 s, not thousands
    assert!((2..=5).contains(&n), "{n} attempts in 1.3 s");
}

#[test]
fn a_stop_is_heard_in_the_middle_of_a_pause() {
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = Arc::clone(&stop);
    let t = thread::spawn(move || {
        let mut rm = sha_miner(cfg());
        rm.run(|| Err("down".to_string()), &s2, Duration::from_millis(5))
            .unwrap();
        Instant::now()
    });
    // the second pause (a second long) runs from about 0.5 s to 1.5 s: stop in the middle of it
    thread::sleep(Duration::from_millis(900));
    let asked = Instant::now();
    stop.store(true, Ordering::SeqCst);
    let ended = t.join().unwrap();
    assert!(
        ended.duration_since(asked) < Duration::from_millis(300),
        "took {:?} to hear the stop",
        ended.duration_since(asked)
    );
}

// ------------------------------------------------------------------------------------------------
// a node that is not to be trusted (one on another computer): the miner checks what it is given
// ------------------------------------------------------------------------------------------------

use tenero_app::remote_miner::check_template;

fn honest(keys: &Keys, height: u64, tip: u8) -> Template {
    let Response::Template(t) = template(keys, height, tip, EASY) else {
        unreachable!()
    };
    t
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[test]
fn an_honest_template_passes_the_check() {
    let t = honest(&keys(), 6, 5);
    assert_eq!(check_template(&t, 6, &[5; 32], &address(), now()), Ok(()));
}

#[test]
fn a_template_that_pays_someone_else_is_refused_even_when_everything_else_is_in_order() {
    // the node puts its own address in the coinbase and recomputes the transaction root: only the payout check sees it
    let mut t = honest(&keys(), 6, 5);
    t.coinbase.outputs[0].onetime_address = [0xee; 32];
    t.header.tx_root = t.tx_root().unwrap();
    let e = check_template(&t, 6, &[5; 32], &address(), now()).unwrap_err();
    assert!(e.contains("does not pay"), "{e}");
    // each of the other fields of the payout
    for i in 0..3 {
        let mut t = honest(&keys(), 6, 5);
        match i {
            0 => t.coinbase.outputs[0].view_tag = [9; 3],
            1 => t.coinbase.outputs[0].ephemeral_pubkey = [9; 32],
            _ => t.coinbase.outputs[0].anchor_enc = [9; 16],
        }
        t.header.tx_root = t.tx_root().unwrap();
        assert!(check_template(&t, 6, &[5; 32], &address(), now()).is_err());
    }
}

#[test]
fn a_template_whose_anchor_or_amount_does_not_make_its_output_is_refused() {
    // the anchor the node hands back must be the one the output was made with
    let mut t = honest(&keys(), 6, 5);
    t.anchor = [0x77; 16];
    assert!(check_template(&t, 6, &[5; 32], &address(), now())
        .unwrap_err()
        .contains("does not pay"));
    // a Carrot coinbase output is bound to its amount: the same output with another amount is someone else's
    let mut t = honest(&keys(), 6, 5);
    t.coinbase.outputs[0].amount = 2;
    t.header.tx_root = t.tx_root().unwrap();
    assert!(check_template(&t, 6, &[5; 32], &address(), now()).is_err());
    // and an output made for another address with the same anchor
    let other = Wallet::from_seed(&[2; 32], Network::Test, 0).address();
    let t = honest(&(other.spend_pubkey, other.view_pubkey), 6, 5);
    assert!(check_template(&t, 6, &[5; 32], &address(), now()).is_err());
    assert_eq!(check_template(&t, 6, &[5; 32], &other, now()), Ok(()));
}

#[test]
fn a_miner_asks_for_a_template_for_a_main_address_only() {
    let mut w = Wallet::from_seed(&[1; 32], Network::Test, 0);
    let sub = w.subaddress(1).unwrap();
    let f = fake(Box::new(|_| None));
    let e = f.node().block_template(&sub, 1000).unwrap_err();
    assert!(e.contains("main address"), "{e}");
    assert_eq!(f.count(is_template), 0, "nothing was asked");
}

#[test]
fn a_coinbase_that_also_pays_a_second_output_or_carries_extra_data_is_refused() {
    let mut t = honest(&keys(), 6, 5);
    let o = t.coinbase.outputs[0].clone();
    t.coinbase.outputs.push(CoinbaseOutput {
        onetime_address: [0xee; 32],
        ..o
    });
    t.header.tx_root = t.tx_root().unwrap();
    assert!(check_template(&t, 6, &[5; 32], &address(), now()).is_err());
    let mut t = honest(&keys(), 6, 5);
    t.coinbase.extra = vec![1, 2, 3];
    t.header.tx_root = t.tx_root().unwrap();
    let e = check_template(&t, 6, &[5; 32], &address(), now()).unwrap_err();
    assert!(e.contains("extra"), "{e}");
}

#[test]
fn a_body_that_does_not_match_the_header_is_refused() {
    let mut t = honest(&keys(), 6, 5);
    t.header.tx_root = [0x99; 32];
    let e = check_template(&t, 6, &[5; 32], &address(), now()).unwrap_err();
    assert!(e.contains("transaction root"), "{e}");
}

#[test]
fn a_template_for_another_height_tip_or_clock_or_with_work_already_in_it_is_refused() {
    let t = honest(&keys(), 6, 5);
    assert!(check_template(&t, 7, &[5; 32], &address(), now()).is_err());
    assert!(check_template(&t, 6, &[4; 32], &address(), now()).is_err());
    // a clock a day away
    assert!(check_template(&t, 6, &[5; 32], &address(), now() + 86_400).is_err());
    let mut t = honest(&keys(), 6, 5);
    t.header.nonce = 1;
    assert!(check_template(&t, 6, &[5; 32], &address(), now()).is_err());
    let mut t = honest(&keys(), 6, 5);
    t.header.mix = [1; 64];
    assert!(check_template(&t, 6, &[5; 32], &address(), now()).is_err());
}

#[test]
fn a_hostile_node_gets_no_hashing_from_the_miner_and_is_never_handed_a_block() {
    // the node answers every template request with a block that pays itself
    let f = fake(Box::new(|r| {
        Some(match r {
            Request::Info => info(5, 5, false),
            Request::BlockTemplate {
                spend_pubkey,
                view_pubkey,
                ..
            } => {
                let Response::Template(mut t) =
                    template(&(*spend_pubkey, *view_pubkey), 6, 5, EASY)
                else {
                    unreachable!()
                };
                t.coinbase.outputs[0].onetime_address = [0xee; 32];
                t.header.tx_root = t.tx_root().unwrap();
                Response::Template(t)
            }
            _ => return None,
        })
    }));
    let jobs = Jobs::default();
    let mut rm = idle_miner(&jobs, cfg());
    let node = f.node();
    assert!(drive(&mut rm, &node, 10, |m| m.stats.refused_templates >= 3));
    assert!(
        jobs.0.lock().unwrap().is_empty(),
        "the miner hashed a block that pays the node"
    );
    assert_eq!(f.count(is_submit), 0);
    assert_eq!(rm.stats.blocks_found, 0);
}

// ------------------------------------------------------------------------------------------------
// a node that asks the miner to slow down (found by running a GPU miner against the miner service: it used to count that as a
// lost connection and reconnect in a loop, dropping its job each time)
// ------------------------------------------------------------------------------------------------

#[test]
fn a_node_that_says_slow_down_keeps_the_connection_and_the_job_and_mining_resumes() {
    let n = Arc::new(AtomicU64::new(0));
    let n2 = Arc::clone(&n);
    let f = fake(Box::new(move |r| {
        Some(match r {
            Request::Info if n2.fetch_add(1, Ordering::SeqCst) < 4 => {
                Response::Error("too many requests: slow down".into())
            }
            Request::Info => info(5, 5, false),
            Request::BlockTemplate {
                spend_pubkey,
                view_pubkey,
                ..
            } => template(&(*spend_pubkey, *view_pubkey), 6, 5, EASY),
            Request::SubmitHeader(_) => Response::BlockSubmitted {
                id: [1; 32],
                in_chain: true,
            },
            _ => return None,
        })
    }));
    let addr = f.addr;
    let stop = Arc::new(AtomicBool::new(false));
    let connects = Arc::new(AtomicU64::new(0));
    let stats = thread::scope(|s| {
        let (stop2, c2) = (Arc::clone(&stop), Arc::clone(&connects));
        let t = s.spawn(move || {
            let mut rm = sha_miner(RemoteMinerConfig {
                slow_down_pause: Duration::from_millis(20),
                ..cfg()
            });
            rm.run(
                || {
                    c2.fetch_add(1, Ordering::SeqCst);
                    RemoteNode::connect(addr, &COOKIE)
                },
                &stop2,
                Duration::from_millis(5),
            )
            .unwrap();
            rm.stats
        });
        let end = Instant::now() + Duration::from_secs(20);
        while f.count(is_submit) < 1 {
            assert!(Instant::now() < end, "mining never resumed");
            thread::sleep(Duration::from_millis(20));
        }
        thread::sleep(Duration::from_millis(300));
        stop.store(true, Ordering::SeqCst);
        t.join().unwrap()
    });
    assert_eq!(
        connects.load(Ordering::SeqCst),
        1,
        "the miner reconnected instead of waiting"
    );
    assert_eq!(stats.connections_lost, 0, "{stats:?}");
    assert!(stats.slowed_down >= 4, "{stats:?}");
    assert!(stats.blocks_accepted >= 1, "{stats:?}");
}

#[test]
fn a_block_the_node_would_not_take_for_being_asked_too_often_is_handed_in_again_not_given_up() {
    let n = Arc::new(AtomicU64::new(0));
    let n2 = Arc::clone(&n);
    let f = fake(Box::new(move |r| {
        Some(match r {
            Request::Info => info(5, 5, false),
            Request::BlockTemplate {
                spend_pubkey,
                view_pubkey,
                ..
            } => template(&(*spend_pubkey, *view_pubkey), 6, 5, EASY),
            Request::SubmitHeader(_) if n2.fetch_add(1, Ordering::SeqCst) < 2 => {
                Response::Error("too many requests: slow down".into())
            }
            Request::SubmitHeader(_) => Response::BlockSubmitted {
                id: [1; 32],
                in_chain: true,
            },
            _ => return None,
        })
    }));
    let jobs = Jobs::default();
    let _ = jobs;
    let mut rm = sha_miner(RemoteMinerConfig {
        slow_down_pause: Duration::from_millis(10),
        ..cfg()
    });
    let node = f.node();
    assert!(drive(&mut rm, &node, 20, |m| m.stats.blocks_accepted >= 1));
    assert_eq!(rm.stats.blocks_refused, 0, "{:?}", rm.stats);
    assert!(rm.stats.slowed_down >= 2, "{:?}", rm.stats);
    assert!(f.count(is_submit) >= 3);
}
