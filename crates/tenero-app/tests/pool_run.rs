//! The pool on real sockets: a miner that works for it, the handshake and its pinned key, and what hostile clients can and cannot do. The rules of
//! each share are in `pool_server.rs`; here it is the connections.

use std::io::Write as _;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tenero_app::client::BlockVerdict;
use tenero_app::control::Template;
use tenero_app::pool::{Hello, MinerMessage, PoolMessage};
use tenero_app::pool_core::{nonce_in_prefix, Accounts};
use tenero_app::pool_miner::{PoolMiner, PoolMinerConfig};
use tenero_app::pool_net::{self, read_message, write_message, ReadHalf, WriteHalf};
use tenero_app::pool_server::*;
use tenero_chain::Sha256Pow;
use tenero_core::u256::U256;
use tenero_core::v2::ids::PowKind;
use tenero_core::v3::ids;
use tenero_core::v3::{Block, BlockHeader, Coinbase, CoinbaseOutput, VERSION};
use tenero_miner::{Miner, Sha256Backend};
use tenero_net::noise::NodeKey;
use tenero_wallet::{coinbase_payout_to_keys, Address, Network, Wallet};

/// The Janus anchor the made-up node makes its outputs with.
const ANCHOR: [u8; 16] = [0x5a; 16];

fn address(n: u8) -> Address {
    Wallet::from_seed(&[n; 32], Network::Test, 0).address()
}

struct FakeNode {
    height: AtomicU64,
    submitted: Mutex<Vec<Block>>,
    /// The coinbase of every template made (a header names its template by the root).
    coinbases: Mutex<Vec<Coinbase>>,
}

impl FakeNode {
    fn tip_id(&self) -> [u8; 32] {
        [self.height.load(Ordering::SeqCst) as u8; 32]
    }
}

impl PoolNode for FakeNode {
    fn tip(&self) -> Result<(u64, [u8; 32], bool), String> {
        Ok((self.height.load(Ordering::SeqCst), self.tip_id(), false))
    }
    fn template(&self, to: &Address, _max: u64) -> Result<Template, String> {
        let height = self.height.load(Ordering::SeqCst) + 1;
        let p = coinbase_payout_to_keys(
            &to.spend_pubkey,
            &to.view_pubkey,
            height,
            2_000_000_000,
            &ANCHOR,
        )
        .expect("valid keys");
        let coinbase = Coinbase {
            version: VERSION,
            height,
            outputs: vec![CoinbaseOutput {
                onetime_address: p.onetime_address,
                amount: 2_000_000_000,
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
        let header = BlockHeader {
            version: VERSION,
            prev_id: self.tip_id(),
            timestamp: now,
            tx_root: ids::block_tx_root(&coinbase, &[]).unwrap(),
            nonce: 0,
            mix: [0; 64],
        };
        self.coinbases.lock().unwrap().push(coinbase.clone());
        Ok(Template {
            header,
            coinbase,
            tx_ids: vec![],
            height,
            target: U256::pow2(250).unwrap().to_be_bytes(),
            anchor: ANCHOR,
        })
    }
    fn submit_header(&self, header: BlockHeader) -> Result<BlockVerdict, String> {
        let coinbase = self
            .coinbases
            .lock()
            .unwrap()
            .iter()
            .find(|c| ids::block_tx_root(c, &[]).unwrap() == header.tx_root)
            .cloned()
            .expect("a header of one of this node's templates");
        self.submitted.lock().unwrap().push(Block {
            header,
            coinbase,
            transactions: vec![],
        });
        Ok(BlockVerdict::InChain([9; 32]))
    }
    fn block_id_at(&self, _h: u64) -> Result<Option<[u8; 32]>, String> {
        Ok(None)
    }
    fn check_pow(&self, header: &BlockHeader, _h: u64) -> Result<bool, String> {
        Ok(header.mix == [0; 64])
    }
}

struct Rig {
    handle: ListenHandle,
    node: Arc<FakeNode>,
    key: [u8; 32],
}

impl Rig {
    fn pool(&self) -> &Arc<Pool> {
        self.handle.pool()
    }
}

fn start(tweak: impl FnOnce(&mut PoolConfig)) -> Rig {
    let node = Arc::new(FakeNode {
        height: AtomicU64::new(10),
        submitted: Mutex::new(vec![]),
        coinbases: Mutex::new(vec![]),
    });
    let mut cfg = PoolConfig::new("test", address(200), 5);
    cfg.handshake_timeout = Duration::from_millis(600);
    cfg.idle_timeout = Duration::from_secs(20);
    tweak(&mut cfg);
    let key = NodeKey::generate();
    let public = key.public();
    let pool = Pool::new(
        cfg,
        node.clone(),
        Arc::new(Sha256Pow),
        key,
        Accounts::new(),
        None,
        Arc::new(|_| {}),
    );
    let handle = pool.start("127.0.0.1:0".parse().unwrap()).unwrap();
    // the first job
    let end = Instant::now() + Duration::from_secs(5);
    while pool.current_job().is_none() {
        assert!(Instant::now() < end, "no job");
        thread::sleep(Duration::from_millis(20));
    }
    Rig {
        handle,
        node,
        key: public,
    }
}

fn hello(network: &str, n: u8) -> MinerMessage {
    MinerMessage::Hello(Hello {
        min_version: 1,
        max_version: 1,
        capabilities: 0,
        network: network.into(),
        address: address(n).to_text(),
        worker: "t".into(),
        agent: "test".into(),
    })
}

fn send(w: &mut WriteHalf, m: &MinerMessage) {
    write_message(w, &m.to_body().unwrap()).unwrap();
}

/// The next message from the pool, or `None` at the end of the connection (or after a few seconds of nothing).
fn recv(r: &mut ReadHalf) -> Option<PoolMessage> {
    r.set_timeout(Some(Duration::from_secs(5))).unwrap();
    read_message(r)
        .ok()
        .map(|b| PoolMessage::from_body(&b).expect("the pool speaks the protocol"))
}

fn dial(rig: &Rig) -> (ReadHalf, WriteHalf) {
    pool_net::connect(rig.handle.addr, Some(&rig.key)).unwrap()
}

/// The pool's connection is over: the next read is the end, not a message.
fn ended(r: &mut ReadHalf) -> bool {
    loop {
        match recv(r) {
            None => return true,
            // (the pool may say why before it hangs up)
            Some(PoolMessage::Job(_))
            | Some(PoolMessage::SetShareTarget { .. })
            | Some(PoolMessage::Error(_)) => continue,
            Some(_) => return false,
        }
    }
}

// ---- a miner that works for the pool ----------------------------------------------------------------------------------------

#[test]
fn a_miner_works_for_a_pool_over_a_real_socket_and_shares_and_a_block_are_counted() {
    // a SHA-256 miner on an easy share target can send more than the 600 shares a minute the pool takes before its
    // difficulty catches up (seen once in 25 runs: 602 sent, one refused for the rate); the rate limit is tested elsewhere
    let rig = start(|c| c.shares_per_minute = 1_000_000);
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut cfg = PoolMinerConfig::new("test", &address(7).to_text(), "rig7");
    let events = Arc::new(Mutex::new(Vec::new()));
    let e = Arc::clone(&events);
    cfg.events = Arc::new(move |ev| e.lock().unwrap().push(ev));
    let mut pm = PoolMiner::new(Miner::spawn(|| Ok(Sha256Backend)), PowKind::Sha256, cfg);
    let (addr, key) = (rig.handle.addr, rig.key);
    let s = Arc::clone(&shutdown);
    let t = thread::spawn(move || {
        pm.run(|| tenero_app::pool_miner::connect(addr, Some(key)), &s)
            .unwrap();
        pm
    });
    let end = Instant::now() + Duration::from_secs(60);
    while rig.pool().stats.shares_accepted.load(Ordering::Relaxed) < 8
        || rig.pool().stats.blocks_found.load(Ordering::Relaxed) < 1
    {
        assert!(
            Instant::now() < end,
            "too slow: {:?}",
            rig.pool().accounts().shares_accepted
        );
        thread::sleep(Duration::from_millis(50));
    }
    shutdown.store(true, Ordering::SeqCst);
    let pm = t.join().unwrap();
    // both sides agree
    let accepted_by_pool = rig.pool().stats.shares_accepted.load(Ordering::Relaxed);
    assert!(pm.stats.shares_sent >= accepted_by_pool, "{:?}", pm.stats);
    assert!(
        pm.stats.shares_accepted <= accepted_by_pool && pm.stats.shares_accepted >= 6,
        "{:?}",
        pm.stats
    );
    assert_eq!(pm.stats.bad_solutions, 0);
    assert_eq!(pm.stats.jobs_refused, 0);
    assert_eq!(
        pm.stats.shares_rejected, 0,
        "a well-behaved miner has no refused share: {:?}",
        pm.stats
    );
    // the work is the miner's: credited to its address
    let a = rig.pool().accounts();
    assert_eq!(a.addresses(), 1);
    assert!(a.window_work() > 0);
    // a block was found, and handed to the node with a nonce in this miner's slice
    let blocks = rig.node.submitted.lock().unwrap().clone();
    assert!(!blocks.is_empty());
    assert_eq!(blocks[0].coinbase.height, 11);
    assert_eq!(
        rig.pool().accounts().pending().len() as u64,
        rig.pool().stats.blocks_in_chain.load(Ordering::Relaxed)
    );
    // the screen was told it was connected, and shares counted
    let ev = events.lock().unwrap().clone();
    assert!(
        ev.iter()
            .any(|e| matches!(e, tenero_miner::MinerEvent::PoolConnected { .. })),
        "{ev:?}"
    );
    assert!(ev
        .iter()
        .any(|e| matches!(e, tenero_miner::MinerEvent::ShareAccepted { .. })));
}

#[test]
fn a_miner_that_was_given_the_pools_key_refuses_a_pool_with_another_and_one_given_none_goes_anyway()
{
    let rig = start(|_| {});
    let right = rig.key;
    assert!(pool_net::connect(rig.handle.addr, Some(&right)).is_ok());
    assert!(pool_net::connect(rig.handle.addr, None).is_ok());
    let mut wrong = right;
    wrong[0] ^= 1;
    let e = pool_net::connect(rig.handle.addr, Some(&wrong))
        .err()
        .expect("refused");
    assert!(
        e.contains("different key") && e.contains("Not connecting"),
        "{e}"
    );
}

// ---- the first message and what follows -----------------------------------------------------------------------------------------

#[test]
fn the_pool_answers_a_good_hello_with_its_terms_a_job_and_a_pong() {
    let rig = start(|_| {});
    let (mut r, mut w) = dial(&rig);
    send(&mut w, &hello("test", 3));
    let Some(PoolMessage::HelloOk(ok)) = recv(&mut r) else {
        panic!("no hello_ok")
    };
    assert_eq!((ok.version, ok.capabilities, ok.pays_pool), (1, 0, true));
    assert!(ok.prefix_bits >= 1 && ok.prefix >> ok.prefix_bits == 0);
    assert!(!ok.pool_name.is_empty());
    // then a job (the pool may send a share target first)
    let job = loop {
        match recv(&mut r) {
            Some(PoolMessage::Job(j)) => break j,
            Some(PoolMessage::SetShareTarget { .. }) => continue,
            other => panic!("{other:?}"),
        }
    };
    assert!(job.clean && job.height == 11);
    send(&mut w, &MinerMessage::Ping { token: 77 });
    assert_eq!(recv(&mut r), Some(PoolMessage::Pong { token: 77 }));
}

#[test]
fn a_hello_for_another_network_is_refused_with_the_reason_and_the_connection_ends() {
    let rig = start(|_| {});
    let (mut r, mut w) = dial(&rig);
    send(&mut w, &hello("alpha", 3));
    match recv(&mut r) {
        Some(PoolMessage::Error(why)) => assert!(why.contains("test network"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(ended(&mut r));
}

#[test]
fn a_first_message_that_is_not_a_hello_ends_the_connection() {
    let rig = start(|_| {});
    let (mut r, mut w) = dial(&rig);
    send(&mut w, &MinerMessage::Ping { token: 1 });
    assert!(matches!(recv(&mut r), Some(PoolMessage::Error(_))));
    assert!(ended(&mut r));
}

#[test]
fn a_second_hello_and_the_messages_of_job_declaration_end_the_connection() {
    let rig = start(|_| {});
    for second in [
        hello("test", 3),
        MinerMessage::ProvideTxs {
            decl_id: 1,
            txs: vec![],
        },
    ] {
        let (mut r, mut w) = dial(&rig);
        send(&mut w, &hello("test", 3));
        assert!(matches!(recv(&mut r), Some(PoolMessage::HelloOk(_))));
        // ProvideTxs with no transactions is not even encodable: use a ping-sized stand-in for it
        match second {
            MinerMessage::ProvideTxs { .. } => {
                let _ = write_message(&mut w, &[5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            }
            m => send(&mut w, &m),
        }
        assert!(ended(&mut r));
    }
}

#[test]
fn a_share_for_a_job_that_does_not_exist_is_answered_and_a_malformed_frame_ends_the_connection() {
    let rig = start(|_| {});
    let (mut r, mut w) = dial(&rig);
    send(&mut w, &hello("test", 3));
    assert!(matches!(recv(&mut r), Some(PoolMessage::HelloOk(_))));
    send(
        &mut w,
        &MinerMessage::SubmitShare {
            job_id: 1,
            nonce: 5,
            mix: [0; 64],
        },
    );
    let res = loop {
        match recv(&mut r) {
            Some(PoolMessage::ShareResult {
                job_id,
                accepted,
                reason,
                ..
            }) => break (job_id, accepted, reason),
            Some(PoolMessage::Job(_)) | Some(PoolMessage::SetShareTarget { .. }) => continue,
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(res, (1, false, 5), "unknown job");
    // a frame of a length no message may have
    w.write_all(&[0xFF, 0xFF, 0xFF, 0x7F]).unwrap();
    w.flush().unwrap();
    assert!(ended(&mut r));
    // and a message of a kind that does not exist
    let (mut r, mut w) = dial(&rig);
    send(&mut w, &hello("test", 4));
    assert!(matches!(recv(&mut r), Some(PoolMessage::HelloOk(_))));
    write_message(&mut w, &[0x7E, 1, 2, 3]).unwrap();
    assert!(matches!(recv(&mut r), Some(PoolMessage::Error(_))) || ended(&mut r));
}

#[test]
fn a_new_tip_sends_every_connected_miner_a_clean_job() {
    let rig = start(|_| {});
    let (mut r, mut w) = dial(&rig);
    send(&mut w, &hello("test", 3));
    assert!(matches!(recv(&mut r), Some(PoolMessage::HelloOk(_))));
    let first = loop {
        if let Some(PoolMessage::Job(j)) = recv(&mut r) {
            break j;
        }
    };
    rig.node.height.store(11, Ordering::SeqCst);
    let next = loop {
        match recv(&mut r) {
            Some(PoolMessage::Job(j)) if j.job_id != first.job_id => break j,
            Some(_) => continue,
            None => panic!("the connection ended"),
        }
    };
    assert!(next.clean && next.height == 12, "{next:?}");
    // a share for the first job is now stale
    send(
        &mut w,
        &MinerMessage::SubmitShare {
            job_id: first.job_id,
            nonce: 0,
            mix: [0; 64],
        },
    );
    loop {
        match recv(&mut r) {
            Some(PoolMessage::ShareResult {
                reason, accepted, ..
            }) => {
                assert_eq!((accepted, reason), (false, 1));
                break;
            }
            Some(_) => continue,
            None => panic!("ended"),
        }
    }
}

// ---- hostile clients -----------------------------------------------------------------------------------------------------------------

#[test]
fn something_that_is_not_the_handshake_is_closed_and_counted() {
    let rig = start(|_| {});
    for junk in [
        vec![0xA5u8; 300],
        b"GET / HTTP/1.1\r\nHost: x\r\n\r\n".to_vec(),
        vec![],
    ] {
        let mut s = TcpStream::connect(rig.handle.addr).unwrap();
        let _ = s.write_all(&junk);
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = [0u8; 16];
        // the pool hangs up (a read of 0 bytes, or an error), and never answers with anything useful
        let n = std::io::Read::read(&mut s, &mut buf).unwrap_or(0);
        assert!(n == 0 || n <= 64, "{n}");
    }
    let end = Instant::now() + Duration::from_secs(5);
    while rig.pool().stats.handshake_failed.load(Ordering::Relaxed) < 2 {
        assert!(Instant::now() < end);
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_connection_that_never_says_hello_is_closed_when_the_time_is_up() {
    let rig = start(|_| {});
    let (mut r, _w) = dial(&rig);
    let t0 = Instant::now();
    assert!(ended(&mut r));
    assert!(t0.elapsed() < Duration::from_secs(4), "{:?}", t0.elapsed());
}

#[test]
fn too_many_connections_from_one_address_and_too_many_miners_are_turned_away() {
    let rig = start(|c| {
        c.per_address = 2;
        c.max_miners = 100;
    });
    let held: Vec<_> = (0..2)
        .map(|i| {
            let (mut r, mut w) = dial(&rig);
            send(&mut w, &hello("test", 10 + i));
            assert!(matches!(recv(&mut r), Some(PoolMessage::HelloOk(_))));
            (r, w)
        })
        .collect();
    // the third from the same address never gets as far as a handshake answer that works
    let third = pool_net::connect(rig.handle.addr, Some(&rig.key));
    if let Ok((mut r, mut w)) = third {
        let _ = write_message(&mut w, &hello("test", 12).to_body().unwrap());
        assert!(ended(&mut r) || recv(&mut r).is_none());
    }
    assert!(rig.pool().stats.turned_away.load(Ordering::Relaxed) >= 1);
    drop(held);
}

#[test]
fn the_pool_stops_when_its_handle_is_dropped() {
    let rig = start(|_| {});
    let addr: SocketAddr = rig.handle.addr;
    drop(rig);
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err() {
            break;
        }
        assert!(Instant::now() < end, "still listening");
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn the_nonces_a_miner_finds_stay_in_its_slice() {
    // the miner's slice is the pool's: every share the pool accepted came from a nonce in it
    let rig = start(|_| {});
    let (mut r, mut w) = dial(&rig);
    send(&mut w, &hello("test", 3));
    let Some(PoolMessage::HelloOk(ok)) = recv(&mut r) else {
        panic!()
    };
    let job = loop {
        if let Some(PoolMessage::Job(j)) = recv(&mut r) {
            break j;
        }
    };
    let share_target = U256::from_be_bytes(&ok.share_target);
    let mut h = job.header.clone();
    let mut n = tenero_app::pool_core::first_nonce_of(ok.prefix, ok.prefix_bits);
    let mut sent = 0;
    while sent < 3 {
        h.nonce = n;
        if U256::from_be_bytes(&ids::block_id(&h, PowKind::Sha256)) < share_target {
            assert!(nonce_in_prefix(n, ok.prefix, ok.prefix_bits));
            send(
                &mut w,
                &MinerMessage::SubmitShare {
                    job_id: job.job_id,
                    nonce: n,
                    mix: [0; 64],
                },
            );
            sent += 1;
        }
        n += 1;
    }
    let mut accepted = 0;
    while accepted < 3 {
        match recv(&mut r) {
            Some(PoolMessage::ShareResult { accepted: true, .. }) => accepted += 1,
            Some(PoolMessage::ShareResult {
                accepted: false,
                reason,
                ..
            }) => panic!("refused: {reason}"),
            Some(_) => continue,
            None => panic!("ended"),
        }
    }
    assert_eq!(rig.pool().accounts().shares_accepted, 3);
}

// ---- the conformance tool ---------------------------------------------------------------------------------------------------

use tenero_app::pool::{HelloOk, Job};
use tenero_app::pool_check::{failures, run as run_checks, Options, Verdict};

fn check_options(rig: &Rig) -> Options {
    Options {
        addr: rig.handle.addr,
        pin: Some(rig.key),
        network: "test".into(),
        pow: PowKind::Sha256,
        address: address(9).to_text(),
        wait: Duration::from_secs(5),
    }
}

#[test]
fn this_pool_passes_every_check_of_the_conformance_tool() {
    let rig = start(|_| {});
    let checks = run_checks(&check_options(&rig));
    let report: Vec<String> = checks
        .iter()
        .map(|c| format!("{:?}: {}", c.verdict, c.name))
        .collect();
    assert_eq!(failures(&checks), 0, "{report:#?}");
    // on the test network nothing is skipped, and every check ran
    assert!(
        checks.iter().all(|c| c.verdict == Verdict::Pass),
        "{report:#?}"
    );
    assert!(checks.len() >= 15, "{} checks", checks.len());
}

#[test]
fn the_tool_fails_a_pool_that_does_not_keep_the_rules() {
    // a pool that says yes to everything and never hangs up
    let key = NodeKey::generate();
    let public = key.public();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let key = NodeKey::from_bytes(&key.to_bytes()).unwrap();
            thread::spawn(move || {
                let Ok((mut r, mut w, _)) =
                    pool_net::accept(stream, &key, Duration::from_secs(5), Duration::from_secs(5))
                else {
                    return;
                };
                let send = |w: &mut WriteHalf, m: PoolMessage| {
                    let _ = write_message(w, &m.to_body().unwrap());
                };
                while let Ok(body) = read_message(&mut r) {
                    match MinerMessage::from_body(&body) {
                        Ok(MinerMessage::Hello(_)) => {
                            send(
                                &mut w,
                                PoolMessage::HelloOk(HelloOk {
                                    version: 1,
                                    capabilities: 0,
                                    session: 1,
                                    prefix_bits: 4,
                                    prefix: 3,
                                    share_target: U256::pow2(254).unwrap().to_be_bytes(),
                                    pool_name: "yes pool".into(),
                                    pays_pool: true,
                                }),
                            );
                            let header = BlockHeader {
                                version: VERSION,
                                prev_id: [1; 32],
                                timestamp: 1_700_000_000,
                                tx_root: [2; 32],
                                nonce: 0,
                                mix: [0; 64],
                            };
                            send(
                                &mut w,
                                PoolMessage::Job(Job {
                                    job_id: 1,
                                    height: 5,
                                    clean: true,
                                    header,
                                    block_target: U256::pow2(250).unwrap().to_be_bytes(),
                                    ttl: 60,
                                }),
                            );
                        }
                        Ok(MinerMessage::Ping { token }) => {
                            send(&mut w, PoolMessage::Pong { token })
                        }
                        Ok(MinerMessage::SubmitShare { job_id, .. }) => send(
                            &mut w,
                            PoolMessage::ShareResult {
                                job_id,
                                accepted: true,
                                reason: 0,
                                text: String::new(),
                            },
                        ),
                        _ => {}
                    }
                }
            });
        }
    });
    let o = Options {
        addr,
        pin: Some(public),
        network: "test".into(),
        pow: PowKind::Sha256,
        address: address(9).to_text(),
        wait: Duration::from_millis(700),
    };
    let checks = run_checks(&o);
    let failed: Vec<&str> = checks
        .iter()
        .filter(|c| matches!(c.verdict, Verdict::Fail(_)))
        .map(|c| c.name)
        .collect();
    for must in [
        "a share above the share target is refused (reason 3)",
        "a share for a job the pool never made is refused (reason 5 or 1)",
        "a share outside this miner's slice of the nonces is refused (reason 3)",
        "the same share again is refused as a duplicate (reason 2)",
        "a message of a kind that does not exist ends the connection",
        "a hello for another network is refused with an error",
        "a hello with an address that is not valid is refused with an error",
        "a first message that is not a hello ends the connection",
        "a second hello ends the connection",
    ] {
        assert!(
            failed.contains(&must),
            "`{must}` should have failed: {failed:#?}"
        );
    }
    // and what that pool does right still passes
    assert!(checks
        .iter()
        .any(|c| c.verdict == Verdict::Pass && c.name.starts_with("a ping is answered")));
}

#[test]
fn the_tool_fails_cleanly_when_there_is_no_pool_or_the_key_is_wrong() {
    let rig = start(|_| {});
    let mut o = check_options(&rig);
    let mut wrong = rig.key;
    wrong[3] ^= 0x80;
    o.pin = Some(wrong);
    let checks = run_checks(&o);
    assert_eq!(
        checks.len(),
        1,
        "it stops at the first failure: the rest would say nothing"
    );
    assert!(matches!(&checks[0].verdict, Verdict::Fail(why) if why.contains("different key")));
    // nothing listening
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let mut o = check_options(&rig);
    o.addr = format!("127.0.0.1:{port}").parse().unwrap();
    let checks = run_checks(&o);
    assert_eq!(failures(&checks), 1);
}

// ---- the pool built into the program ---------------------------------------------------------------------------------------

#[test]
fn the_built_in_pool_is_the_authors_gamma_pool_with_its_key_and_no_other_network_has_one() {
    use tenero_app::config::Network;
    use tenero_app::pool_miner::{default_pool, DEFAULT_POOLS};
    let (addr, key) = default_pool(Network::Gamma).expect("gamma has one");
    assert_eq!(addr, "195.26.244.245:38335");
    let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        hex, "4eb53ae8fee5bf1596b3c652896ebf5190c0415d92542c9c536842bd5cfa770a",
        "the key made for the gamma pool (2026-10-08)"
    );
    for n in [Network::Test, Network::Dev] {
        assert!(
            default_pool(n).is_none(),
            "{} has no built-in pool",
            n.name()
        );
    }
    // the list is checked like the seeds': a public ip:port, the default pool port, no network twice
    let mut seen = std::collections::BTreeSet::new();
    for (network, addr, _) in DEFAULT_POOLS {
        assert!(seen.insert(*network), "{network} listed twice");
        let sa: SocketAddr = addr.parse().expect("an ip:port");
        assert!(!sa.ip().is_loopback() && !sa.ip().is_unspecified(), "{sa}");
        assert_eq!(sa.port(), pool_net::DEFAULT_PORT);
    }
}
