//! The rules of the pool, one by one, with no sockets: a made-up node that builds blocks for it, the SHA-256 test proof of work, and a clock the test
//! moves. The network part (the handshake, the frames, the threads) is in `pool_run.rs`.

use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tenero_app::client::BlockVerdict;
use tenero_app::control::Template;
use tenero_app::pool::{Hello, PoolMessage};
use tenero_app::pool_core::{share_target, work_of, Accounts};
use tenero_app::pool_server::*;
use tenero_chain::Sha256Pow;
use tenero_core::u256::U256;
use tenero_core::v2::ids::PowKind;
use tenero_core::v3::ids;
use tenero_core::v3::{Block, BlockHeader, Coinbase, CoinbaseOutput, VERSION};
use tenero_net::noise::NodeKey;
use tenero_wallet::{coinbase_payout_to_keys, Address, Network, Wallet};

const T0: u64 = 1_700_000_000;

/// The Janus anchor the made-up node makes its outputs with.
const ANCHOR: [u8; 16] = [0x5a; 16];

fn address(n: u8) -> Address {
    Wallet::from_seed(&[n; 32], Network::Test, 0).address()
}

/// A node that is made up: the tip and the chain are whatever the test says, and a block handed in is kept.
struct FakeNode {
    state: Mutex<NodeState>,
    /// The id of the block at each height (for settling).
    clock: Arc<AtomicU64>,
}

struct NodeState {
    height: u64,
    tip: [u8; 32],
    syncing: bool,
    target: U256,
    verdict: Option<BlockVerdict>,
    submitted: Vec<Block>,
    chain: std::collections::HashMap<u64, [u8; 32]>,
    templates_made: u64,
    /// How many times the node was asked to check a mix.
    pow_calls: u64,
    fail: bool,
    /// A node that pays someone else with the blocks it builds.
    evil: bool,
}

impl FakeNode {
    fn set_tip(&self, height: u64, tip: [u8; 32]) {
        let mut s = self.state.lock().unwrap();
        s.height = height;
        s.tip = tip;
    }
}

impl PoolNode for FakeNode {
    fn tip(&self) -> Result<(u64, [u8; 32], bool), String> {
        let s = self.state.lock().unwrap();
        if s.fail {
            return Err("the node is down".into());
        }
        Ok((s.height, s.tip, s.syncing))
    }

    fn template(&self, to: &Address, _max: u64) -> Result<Template, String> {
        let mut s = self.state.lock().unwrap();
        s.templates_made += 1;
        let height = s.height + 1;
        // an evil node pays an address of its own
        let to = if s.evil { address(0xEE) } else { *to };
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
        let tx_root = ids::block_tx_root(&coinbase, &[]).unwrap();
        let header = BlockHeader {
            version: VERSION,
            prev_id: s.tip,
            timestamp: self.clock.load(Ordering::SeqCst),
            tx_root,
            nonce: 0,
            mix: [0; 64],
        };
        Ok(Template {
            block: Block {
                header,
                coinbase,
                transactions: vec![],
            },
            height,
            target: s.target.to_be_bytes(),
            anchor: ANCHOR,
        })
    }

    fn submit_block(&self, block: Block) -> Result<BlockVerdict, String> {
        let mut s = self.state.lock().unwrap();
        s.submitted.push(block);
        match s.verdict.clone() {
            Some(v) => Ok(v),
            None => Err("the node did not answer".into()),
        }
    }

    fn block_id_at(&self, height: u64) -> Result<Option<[u8; 32]>, String> {
        Ok(self.state.lock().unwrap().chain.get(&height).copied())
    }

    fn check_pow(&self, header: &BlockHeader, _height: u64) -> Result<bool, String> {
        self.state.lock().unwrap().pow_calls += 1;
        if self.state.lock().unwrap().fail {
            return Err("the node is down".into());
        }
        // the test chain's proof of work: the mix is all zeros
        Ok(header.mix == [0; 64])
    }
}

struct Rig {
    pool: Arc<Pool>,
    node: Arc<FakeNode>,
    clock: Arc<AtomicU64>,
    log: Arc<Mutex<Vec<String>>>,
}

fn rig_with(tweak: impl FnOnce(&mut PoolConfig)) -> Rig {
    let clock = Arc::new(AtomicU64::new(T0));
    let node = Arc::new(FakeNode {
        state: Mutex::new(NodeState {
            height: 10,
            tip: [10; 32],
            syncing: false,
            target: U256::pow2(250).unwrap(),
            verdict: Some(BlockVerdict::InChain([0; 32])),
            submitted: vec![],
            chain: Default::default(),
            templates_made: 0,
            pow_calls: 0,
            fail: false,
            evil: false,
        }),
        clock: Arc::clone(&clock),
    });
    let mut cfg = PoolConfig::new("test", address(200), 5);
    cfg.initial_ratio = 16;
    tweak(&mut cfg);
    let log = Arc::new(Mutex::new(Vec::new()));
    let l = Arc::clone(&log);
    let pool = Pool::new(
        cfg,
        node.clone(),
        Arc::new(Sha256Pow),
        NodeKey::generate(),
        Accounts::new(),
        None,
        Arc::new(move |line| l.lock().unwrap().push(line.to_string())),
    );
    let c = Arc::clone(&clock);
    let pool = pool.with_clock(Arc::new(move || c.load(Ordering::SeqCst)));
    Rig {
        pool,
        node,
        clock,
        log,
    }
}

fn rig() -> Rig {
    rig_with(|_| {})
}

/// A pool that holds no dataset: the cheap check is its own, and the mix is checked by its node (as in the program).
fn rig_node_pow() -> Rig {
    let r = rig();
    let node: Arc<dyn PoolNode> = r.node.clone();
    let pow = Arc::new(NodePow::new(node, PowKind::Sha256));
    let pool = Pool::new(
        r.pool.cfg.clone(),
        r.node.clone(),
        pow,
        NodeKey::generate(),
        Accounts::new(),
        None,
        Arc::new(|_| {}),
    );
    let c = Arc::clone(&r.clock);
    let pool = pool.with_clock(Arc::new(move || c.load(Ordering::SeqCst)));
    Rig { pool, ..r }
}

fn hello(n: u8) -> Hello {
    Hello {
        min_version: 1,
        max_version: 1,
        capabilities: 0,
        network: "test".into(),
        address: address(n).to_text(),
        worker: format!("rig{n}"),
        agent: "test".into(),
    }
}

fn ip(n: u8) -> IpAddr {
    IpAddr::from([10, 0, 0, n])
}

/// The first nonce at or after `start` whose block id is under `target` (or, with `above`, not under it).
fn find(job: &JobRecord, start: u64, target: &U256, above: bool) -> u64 {
    let mut h = job.header.clone();
    let mut n = start;
    loop {
        h.nonce = n;
        let id = U256::from_be_bytes(&ids::block_id(&h, PowKind::Sha256));
        if (id < *target) != above {
            return n;
        }
        n += 1;
    }
}

fn job_of(r: &Rig) -> Arc<JobRecord> {
    r.pool.step_jobs().unwrap();
    r.pool.current_job().expect("a job")
}

// ---- jobs ----------------------------------------------------------------------------------------------------------------

#[test]
fn a_clean_job_comes_when_the_tip_moves_and_every_earlier_job_is_dead() {
    let r = rig();
    assert!(r.pool.current_job().is_none());
    let j1 = job_of(&r);
    assert_eq!((j1.height, j1.tip), (11, [10; 32]));
    assert_eq!(j1.header.nonce, 0);
    assert_eq!(j1.header.mix, [0; 64]);
    // the same tip: no new job until the old one is a refresh old
    r.pool.step_jobs().unwrap();
    assert_eq!(r.pool.current_job().unwrap().id, j1.id);
    assert_eq!(r.node.state.lock().unwrap().templates_made, 1);
    // the tip moves
    r.node.set_tip(11, [11; 32]);
    r.pool.step_jobs().unwrap();
    let j2 = r.pool.current_job().unwrap();
    assert_eq!((j2.height, j2.tip), (12, [11; 32]));
    assert!(j2.id > j1.id);
    // a share for the old job is STALE (not unknown)
    let (mut s, _) = r.pool.open_session(&hello(1), ip(1)).unwrap();
    let o = r.pool.on_share(&mut s, j1.id, 0, [0; 64]);
    assert_eq!((o.accepted, o.reason), (false, R_STALE), "{o:?}");
    // the job message says it is clean and carries the target and a header with nothing in it
    match r.pool.job_message(&j2, true) {
        PoolMessage::Job(m) => {
            assert!(m.clean && m.height == 12 && m.job_id == j2.id);
            assert_eq!(m.block_target, j2.block_target.to_be_bytes());
            assert_eq!((m.header.nonce, m.header.mix), (0, [0; 64]));
            assert!((1..=120).contains(&m.ttl));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_node_that_is_syncing_has_no_jobs_and_the_jobs_it_had_are_dead() {
    let r = rig();
    let j = job_of(&r);
    r.node.state.lock().unwrap().syncing = true;
    r.pool.step_jobs().unwrap();
    assert!(r.pool.current_job().is_none());
    let (mut s, _) = r.pool.open_session(&hello(1), ip(1)).unwrap();
    assert_eq!(r.pool.on_share(&mut s, j.id, 0, [0; 64]).reason, R_STALE);
    r.node.state.lock().unwrap().syncing = false;
    r.pool.step_jobs().unwrap();
    assert!(r.pool.current_job().is_some());
}

#[test]
fn a_template_that_does_not_pay_the_pool_is_never_handed_out() {
    let r = rig();
    r.node.state.lock().unwrap().evil = true;
    let e = r.pool.step_jobs().unwrap_err();
    assert!(e.contains("refused") && e.contains("does not pay"), "{e}");
    assert!(r.pool.current_job().is_none(), "no job was made from it");
    // and when the node is honest again, the pool works
    r.node.state.lock().unwrap().evil = false;
    r.pool.step_jobs().unwrap();
    assert!(r.pool.current_job().is_some());
}

#[test]
fn a_node_that_does_not_answer_is_an_error_not_a_panic() {
    let r = rig();
    r.node.state.lock().unwrap().fail = true;
    assert_eq!(r.pool.step_jobs().unwrap_err(), "the node is down");
    r.node.state.lock().unwrap().fail = false;
    assert!(r.pool.step_jobs().is_ok());
}

// ---- hello ---------------------------------------------------------------------------------------------------------------

#[test]
fn a_hello_is_checked_and_each_miner_gets_its_own_slice_of_the_nonces() {
    let r = rig_with(|c| c.max_miners = 4);
    let (s1, ok1) = r.pool.open_session(&hello(1), ip(1)).unwrap();
    let (s2, _) = r.pool.open_session(&hello(2), ip(2)).unwrap();
    assert_ne!(s1.prefix, s2.prefix);
    assert_eq!((ok1.version, ok1.capabilities, ok1.pays_pool), (1, 0, true));
    assert_eq!(ok1.prefix_bits, 2, "four miners need two bits");
    assert_eq!((ok1.prefix, ok1.session), (s1.prefix, s1.id));
    assert_ne!(ok1.share_target, [0; 32]);
    assert_ne!(ok1.share_target, [0xFF; 32]);
    // the same address again (a second worker) is allowed and is the same account
    let (s1b, _) = r.pool.open_session(&hello(1), ip(1)).unwrap();
    assert_eq!(s1b.address_id, s1.address_id);
    // the fourth fills the slices; a fifth is refused, and a slice comes back when a miner leaves
    let (_s4, _) = r.pool.open_session(&hello(4), ip(4)).unwrap();
    let full = r.pool.open_session(&hello(5), ip(5)).err().unwrap();
    assert!(full.contains("full"), "{full}");
    r.pool.close_session(&s2);
    assert!(r.pool.open_session(&hello(5), ip(5)).is_ok());
}

#[test]
fn a_hello_that_is_wrong_is_refused_with_the_reason() {
    let r = rig();
    let bad = |h: Hello| r.pool.open_session(&h, ip(1)).err().expect("refused");
    assert!(bad(Hello {
        network: "alpha".into(),
        ..hello(1)
    })
    .contains("test network"));
    assert!(bad(Hello {
        min_version: 2,
        max_version: 3,
        ..hello(1)
    })
    .contains("version 1"));
    assert!(bad(Hello {
        max_version: 0,
        min_version: 0,
        ..hello(1)
    })
    .contains("version 1"));
    assert!(bad(Hello {
        address: "tni1nothing".into(),
        ..hello(1)
    })
    .contains("address"));
    assert!(bad(Hello {
        address: String::new(),
        ..hello(1)
    })
    .contains("address"));
    assert!(bad(Hello {
        worker: "a\nb".into(),
        ..hello(1)
    })
    .contains("control"));
    assert!(bad(Hello {
        worker: "w".repeat(33),
        ..hello(1)
    })
    .contains("too long"));
    // nothing was taken for the refused ones
    assert_eq!(r.pool.accounts().addresses(), 0);
}

// ---- shares --------------------------------------------------------------------------------------------------------------

#[test]
fn a_good_share_is_accepted_worth_the_work_of_its_target_and_goes_in_the_window() {
    let r = rig();
    let job = job_of(&r);
    let (mut s, _) = r.pool.open_session(&hello(1), ip(1)).unwrap();
    // the session was made after the job, so its target already fits it: nothing to send
    assert!(r.pool.retarget(&mut s, &job.block_target).is_none());
    let target = s.target;
    assert_eq!(target, share_target(&job.block_target, 16));
    let start = session_first_nonce(s.prefix, s.prefix_bits);
    let nonce = find(&job, start, &target, false);
    let o = r.pool.on_share(&mut s, job.id, nonce, [0; 64]);
    assert_eq!(
        (o.accepted, o.reason, o.close.is_none()),
        (true, R_ACCEPTED, true),
        "{o:?}"
    );
    let a = r.pool.accounts();
    assert_eq!(a.window_len(), 1);
    assert_eq!(a.window_work(), u128::from(work_of(&target)));
    assert_eq!((a.shares_accepted, s.shares), (1, 1));
}

#[test]
fn a_share_outside_the_miners_slice_is_refused_even_if_it_would_be_good() {
    let r = rig();
    let job = job_of(&r);
    let (mut s, _) = r.pool.open_session(&hello(1), ip(1)).unwrap();
    r.pool.retarget(&mut s, &job.block_target);
    // a good nonce, but in the slice of another prefix
    let other = (s.prefix + 1) % (1 << s.prefix_bits);
    let start = session_first_nonce(other, s.prefix_bits);
    let nonce = find(&job, start, &s.target, false);
    let o = r.pool.on_share(&mut s, job.id, nonce, [0; 64]);
    assert_eq!((o.accepted, o.reason), (false, R_LOW));
    assert!(o.text.contains("range"), "{}", o.text);
}

#[test]
fn a_nonce_cannot_be_handed_in_twice_and_a_hard_share_is_not_a_duplicate_of_an_easy_one() {
    let r = rig();
    let job = job_of(&r);
    let (mut s, _) = r.pool.open_session(&hello(1), ip(1)).unwrap();
    r.pool.retarget(&mut s, &job.block_target);
    let nonce = find(
        &job,
        session_first_nonce(s.prefix, s.prefix_bits),
        &s.target,
        false,
    );
    assert!(r.pool.on_share(&mut s, job.id, nonce, [0; 64]).accepted);
    let again = r.pool.on_share(&mut s, job.id, nonce, [0; 64]);
    assert_eq!((again.accepted, again.reason), (false, R_DUPLICATE));
    // and it was not counted twice
    assert_eq!(r.pool.accounts().shares_accepted, 1);
    // a duplicate is not the miner's fault in the way a bad share is: it does not count toward a ban
    assert_eq!(r.pool.stats.shares_duplicate.load(Ordering::Relaxed), 1);
}

#[test]
fn a_share_above_the_target_a_share_for_no_job_and_a_wrong_mix_are_refused_each_for_its_reason() {
    let r = rig();
    let job = job_of(&r);
    let (mut s, _) = r.pool.open_session(&hello(1), ip(1)).unwrap();
    r.pool.retarget(&mut s, &job.block_target);
    let start = session_first_nonce(s.prefix, s.prefix_bits);
    // above the target
    let high = find(&job, start, &s.target, true);
    let o = r.pool.on_share(&mut s, job.id, high, [0; 64]);
    assert_eq!((o.accepted, o.reason), (false, R_LOW));
    // a job that never existed
    let o = r.pool.on_share(&mut s, job.id + 1000, start, [0; 64]);
    assert_eq!((o.accepted, o.reason), (false, R_UNKNOWN_JOB));
    // a good nonce with a mix that is not what the proof of work gives (the test chain's mix is all zeros)
    let good = find(&job, start, &s.target, false);
    let mut mix = [0u8; 64];
    mix[0] = 1;
    let o = r.pool.on_share(&mut s, job.id, good, mix);
    assert_eq!((o.accepted, o.reason), (false, R_BAD_MIX), "{o:?}");
    // and none of them was counted
    assert_eq!(r.pool.accounts().shares_accepted, 0);
    // the nonce is used up by the wrong mix (so nobody can make the pool check one nonce over and over): the miner is told it is a duplicate
    let again = r.pool.on_share(&mut s, job.id, good, [0; 64]);
    assert_eq!((again.accepted, again.reason), (false, R_DUPLICATE));
}

#[test]
fn a_run_of_bad_shares_bans_the_address_and_a_good_share_in_between_starts_the_count_again() {
    let r = rig_with(|c| c.bad_shares_ban = 5);
    let job = job_of(&r);
    let (mut s, _) = r.pool.open_session(&hello(1), ip(1)).unwrap();
    r.pool.retarget(&mut s, &job.block_target);
    let start = session_first_nonce(s.prefix, s.prefix_bits);
    // nine different nonces that are above the target
    let mut bad = Vec::new();
    let mut m = start;
    for _ in 0..9 {
        let x = find(&job, m, &s.target, true);
        bad.push(x);
        m = x + 1;
    }
    for x in &bad[..4] {
        assert!(r.pool.on_share(&mut s, job.id, *x, [0; 64]).close.is_none());
    }
    // a good share: the count starts again
    let good = find(&job, start, &s.target, false);
    assert!(r.pool.on_share(&mut s, job.id, good, [0; 64]).accepted);
    for x in &bad[4..8] {
        assert!(r.pool.on_share(&mut s, job.id, *x, [0; 64]).close.is_none());
    }
    assert_eq!(r.pool.stats.bans.load(Ordering::Relaxed), 0);
    // the fifth in a row: the address is banned and the connection is to be closed
    let o = r.pool.on_share(&mut s, job.id, bad[8], [0; 64]);
    assert_eq!(o.reason, R_NOT_ALLOWED);
    assert!(
        o.close.as_deref().is_some_and(|c| c.contains("bad shares")),
        "{o:?}"
    );
    assert_eq!(r.pool.stats.bans.load(Ordering::Relaxed), 1);
}

#[test]
fn a_connection_that_sends_too_many_shares_a_minute_is_closed() {
    let r = rig_with(|c| c.shares_per_minute = 5);
    let job = job_of(&r);
    let (mut s, _) = r.pool.open_session(&hello(1), ip(1)).unwrap();
    r.pool.retarget(&mut s, &job.block_target);
    let start = session_first_nonce(s.prefix, s.prefix_bits);
    let mut n = start;
    for _ in 0..5 {
        let x = find(&job, n, &s.target, false);
        n = x + 1;
        assert!(r.pool.on_share(&mut s, job.id, x, [0; 64]).accepted);
    }
    let x = find(&job, n, &s.target, false);
    let o = r.pool.on_share(&mut s, job.id, x, [0; 64]);
    assert!(!o.accepted && o.close.is_some(), "{o:?}");
    assert_eq!(r.pool.stats.rate_limited.load(Ordering::Relaxed), 1);
    // a minute later the count has gone down
    r.clock.fetch_add(61, Ordering::SeqCst);
    let x = find(&job, x + 1, &s.target, false);
    assert!(r.pool.on_share(&mut s, job.id, x, [0; 64]).accepted);
}

// ---- difficulty ---------------------------------------------------------------------------------------------------------

#[test]
fn after_a_change_of_target_a_share_that_meets_only_the_old_one_is_taken_for_a_minute_at_its_old_worth(
) {
    let r = rig();
    let job = job_of(&r);
    let (mut s, _) = r.pool.open_session(&hello(1), ip(1)).unwrap();
    r.pool.retarget(&mut s, &job.block_target);
    let easy = s.target;
    // the miner shares too fast: its target becomes harder
    s.vardiff.ratio = 2;
    let bt = job.block_target;
    let new = r.pool.retarget(&mut s, &bt).expect("a new target is sent");
    assert_eq!(U256::from_be_bytes(&new), share_target(&bt, 2));
    assert!(s.target < easy);
    // a nonce under the old target but not under the new one
    let start = session_first_nonce(s.prefix, s.prefix_bits);
    let mut n = start;
    let nonce = loop {
        let x = find(&job, n, &easy, false);
        let mut h = job.header.clone();
        h.nonce = x;
        let id = U256::from_be_bytes(&ids::block_id(&h, PowKind::Sha256));
        if id >= s.target {
            break x;
        }
        n = x + 1;
    };
    let o = r.pool.on_share(&mut s, job.id, nonce, [0; 64]);
    assert!(o.accepted, "{o:?}");
    assert_eq!(
        r.pool.accounts().window_work(),
        u128::from(work_of(&easy)),
        "worth what the old target stood for"
    );
    // two minutes later it would not be
    r.clock.fetch_add(120, Ordering::SeqCst);
    n = nonce + 1;
    let nonce2 = loop {
        let x = find(&job, n, &easy, false);
        let mut h = job.header.clone();
        h.nonce = x;
        let id = U256::from_be_bytes(&ids::block_id(&h, PowKind::Sha256));
        if id >= s.target {
            break x;
        }
        n = x + 1;
    };
    let o = r.pool.on_share(&mut s, job.id, nonce2, [0; 64]);
    assert_eq!((o.accepted, o.reason), (false, R_LOW));
}

#[test]
fn a_new_block_target_changes_the_share_target_by_the_same_ratio() {
    let r = rig();
    let job = job_of(&r);
    let (mut s, _) = r.pool.open_session(&hello(1), ip(1)).unwrap();
    assert!(
        r.pool.retarget(&mut s, &job.block_target).is_none(),
        "nothing changed: nothing to send"
    );
    let harder = U256::pow2(240).unwrap();
    let t = r.pool.retarget(&mut s, &harder).unwrap();
    assert_eq!(U256::from_be_bytes(&t), share_target(&harder, 16));
}

#[test]
fn the_pool_watches_a_miners_rate_and_moves_its_difficulty() {
    let r = rig();
    let job = job_of(&r);
    let (mut s, _) = r.pool.open_session(&hello(1), ip(1)).unwrap();
    r.pool.retarget(&mut s, &job.block_target);
    assert!(r.pool.vardiff_tick(&mut s).is_none(), "too soon");
    // thirty seconds and no share: easier, by 4 times
    r.clock.fetch_add(31, Ordering::SeqCst);
    let t = r.pool.vardiff_tick(&mut s).expect("easier");
    assert_eq!(s.vardiff.ratio, 64);
    assert_eq!(U256::from_be_bytes(&t), share_target(&job.block_target, 64));
}

// ---- blocks ------------------------------------------------------------------------------------------------------------

fn block_nonce(job: &JobRecord, start: u64, share_target: &U256) -> u64 {
    // a nonce that meets the BLOCK target (so it is a share and a block)
    let n = find(job, start, &job.block_target, false);
    let mut h = job.header.clone();
    h.nonce = n;
    assert!(U256::from_be_bytes(&ids::block_id(&h, PowKind::Sha256)) < *share_target);
    n
}

#[test]
fn a_share_that_is_a_block_goes_to_the_node_with_its_nonce_and_the_credits_are_fixed() {
    let r = rig();
    let job = job_of(&r);
    let (mut s, _) = r.pool.open_session(&hello(1), ip(1)).unwrap();
    r.pool.retarget(&mut s, &job.block_target);
    let nonce = block_nonce(
        &job,
        session_first_nonce(s.prefix, s.prefix_bits),
        &s.target,
    );
    let o = r.pool.on_share(&mut s, job.id, nonce, [0; 64]);
    assert!(o.accepted);
    assert_eq!(o.block, Some(BlockOutcome::InChain));
    let submitted = r.node.state.lock().unwrap().submitted.clone();
    assert_eq!(submitted.len(), 1);
    assert_eq!(submitted[0].header.nonce, nonce);
    assert_eq!(submitted[0].coinbase.height, 11);
    assert_eq!(submitted[0].coinbase.outputs[0].amount, 2_000_000_000);
    // who is owed: the one miner, for the whole reward, pending until the block is 5 deep
    let a = r.pool.accounts();
    assert_eq!(a.pending().len(), 1);
    assert_eq!(a.pending()[0].credits, vec![(s.address_id, 2_000_000_000)]);
    assert_eq!(a.balance(s.address_id), 0);
    assert_eq!(r.pool.stats.blocks_in_chain.load(Ordering::Relaxed), 1);
}

#[test]
fn a_block_the_node_does_not_take_or_does_not_answer_about_credits_nobody() {
    for (verdict, expect) in [
        (
            Some(BlockVerdict::LostRace([1; 32])),
            BlockOutcome::LostRace,
        ),
        (
            Some(BlockVerdict::Refused("bad".into())),
            BlockOutcome::Refused("bad".into()),
        ),
        (
            None,
            BlockOutcome::NodeError("the node did not answer".into()),
        ),
    ] {
        let r = rig();
        r.node.state.lock().unwrap().verdict = verdict;
        let job = job_of(&r);
        let (mut s, _) = r.pool.open_session(&hello(1), ip(1)).unwrap();
        r.pool.retarget(&mut s, &job.block_target);
        let nonce = block_nonce(
            &job,
            session_first_nonce(s.prefix, s.prefix_bits),
            &s.target,
        );
        let o = r.pool.on_share(&mut s, job.id, nonce, [0; 64]);
        assert!(
            o.accepted,
            "the share is good whatever the node says of the block"
        );
        assert_eq!(o.block, Some(expect));
        assert!(r.pool.accounts().pending().is_empty());
    }
}

#[test]
fn a_matured_block_credits_the_miners_and_a_replaced_one_does_not() {
    let r = rig();
    let job = job_of(&r);
    let (mut s, _) = r.pool.open_session(&hello(1), ip(1)).unwrap();
    r.pool.retarget(&mut s, &job.block_target);
    let nonce = block_nonce(
        &job,
        session_first_nonce(s.prefix, s.prefix_bits),
        &s.target,
    );
    r.node.state.lock().unwrap().verdict = Some(BlockVerdict::InChain([0xAB; 32]));
    r.pool.on_share(&mut s, job.id, nonce, [0; 64]);
    let id = r.pool.accounts().pending()[0].id;
    // the chain has our block at 11; the tip is 14 (3 deep): not yet
    {
        let mut st = r.node.state.lock().unwrap();
        st.chain.insert(11, id);
        st.height = 14;
        st.tip = [14; 32];
    }
    r.pool.step_jobs().unwrap();
    assert_eq!(r.pool.accounts().balance(s.address_id), 0);
    // 5 deep: credited
    r.node.set_tip(16, [16; 32]);
    r.pool.step_jobs().unwrap();
    assert_eq!(r.pool.accounts().balance(s.address_id), 2_000_000_000);
    assert!(r.pool.accounts().pending().is_empty());
}

#[test]
fn a_block_another_one_replaced_pays_nobody() {
    let r = rig();
    let job = job_of(&r);
    let (mut s, _) = r.pool.open_session(&hello(1), ip(1)).unwrap();
    r.pool.retarget(&mut s, &job.block_target);
    let nonce = block_nonce(
        &job,
        session_first_nonce(s.prefix, s.prefix_bits),
        &s.target,
    );
    r.pool.on_share(&mut s, job.id, nonce, [0; 64]);
    {
        let mut st = r.node.state.lock().unwrap();
        st.chain.insert(11, [0x99; 32]); // not ours
        st.height = 30;
        st.tip = [30; 32];
    }
    r.pool.step_jobs().unwrap();
    assert_eq!(r.pool.accounts().balance(s.address_id), 0);
    assert!(
        r.pool.accounts().pending().is_empty(),
        "it is settled as lost"
    );
    assert!(r.log.lock().unwrap().iter().any(|l| l.contains("1 lost")));
}

#[test]
fn the_pools_fee_stays_with_the_pool() {
    let r = rig_with(|c| c.fee_ppm = 100_000);
    let job = job_of(&r);
    let (mut s, _) = r.pool.open_session(&hello(1), ip(1)).unwrap();
    r.pool.retarget(&mut s, &job.block_target);
    let nonce = block_nonce(
        &job,
        session_first_nonce(s.prefix, s.prefix_bits),
        &s.target,
    );
    r.pool.on_share(&mut s, job.id, nonce, [0; 64]);
    assert_eq!(
        r.pool.accounts().pending()[0].credits,
        vec![(s.address_id, 1_800_000_000)]
    );
}

// ---- a pool that asks its node to check the mix ----------------------------------------------------------------------------

#[test]
fn a_pool_without_a_dataset_asks_its_node_for_the_mix_and_only_for_shares_that_could_be_good() {
    let r = rig_node_pow();
    let job = job_of(&r);
    let (mut s, _) = r.pool.open_session(&hello(1), ip(1)).unwrap();
    r.pool.retarget(&mut s, &job.block_target);
    let start = session_first_nonce(s.prefix, s.prefix_bits);
    let calls = |r: &Rig| r.node.state.lock().unwrap().pow_calls;
    // shares that are above the target, outside the slice or duplicates never cost the node anything
    let high = find(&job, start, &s.target, true);
    assert_eq!(r.pool.on_share(&mut s, job.id, high, [0; 64]).reason, R_LOW);
    assert_eq!(
        r.pool.on_share(&mut s, job.id + 99, start, [0; 64]).reason,
        R_UNKNOWN_JOB
    );
    assert_eq!(calls(&r), 0, "the node was asked about junk");
    // a good share: one question, and it is accepted
    let good = find(&job, start, &s.target, false);
    assert!(r.pool.on_share(&mut s, job.id, good, [0; 64]).accepted);
    assert_eq!(calls(&r), 1);
    // the same nonce again is a duplicate and costs nothing
    assert_eq!(
        r.pool.on_share(&mut s, job.id, good, [0; 64]).reason,
        R_DUPLICATE
    );
    assert_eq!(calls(&r), 1);
    // a wrong mix: the node says no, and the share is refused
    let next = find(&job, good + 1, &s.target, false);
    let mut mix = [0u8; 64];
    mix[5] = 9;
    let o = r.pool.on_share(&mut s, job.id, next, mix);
    assert_eq!((o.accepted, o.reason), (false, R_BAD_MIX));
    assert_eq!(calls(&r), 2);
}

#[test]
fn when_the_node_cannot_check_a_share_the_miner_is_told_to_try_again_and_is_not_blamed() {
    let r = rig_node_pow();
    let job = job_of(&r);
    let (mut s, _) = r.pool.open_session(&hello(1), ip(1)).unwrap();
    r.pool.retarget(&mut s, &job.block_target);
    let start = session_first_nonce(s.prefix, s.prefix_bits);
    let good = find(&job, start, &s.target, false);
    r.node.state.lock().unwrap().fail = true;
    let o = r.pool.on_share(&mut s, job.id, good, [0; 64]);
    assert_eq!((o.accepted, o.reason), (false, R_NOT_ALLOWED));
    assert!(o.text.contains("try again"), "{}", o.text);
    assert!(o.close.is_none(), "the miner did nothing wrong");
    assert_eq!(r.pool.accounts().shares_accepted, 0, "nothing was credited");
    assert_eq!(r.pool.stats.bans.load(Ordering::Relaxed), 0);
}
