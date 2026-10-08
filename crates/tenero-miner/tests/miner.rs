//! The miner against real nodes: blocks found by each backend are validated by the node's own CPU checks; the job
//! follows the tip; a bad answer from a backend is caught or refused and reported.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tenero_chain::{ChainParams, MatmulPow, ProofsNotChecked, Sha256Pow};
use tenero_core::matmulhash::Params;
use tenero_core::u256::U256;
use tenero_core::v2::ids::PowKind;
use tenero_core::v3::ids;
use tenero_miner::{
    Backend, Counters, CpuMatmulBackend, Job, Miner, MinerConfig, MinerEvent, MinerHook,
    PlaceholderPayout, Sha256Backend, Solution, MAX_CORES,
};
use tenero_net::sim::{mine_test_block, SimRig, LABEL};
use tenero_net::transport::Hooks;
use tenero_net::{Engine, EngineConfig, Event};
use tenero_node::{Node, NodeConfig, Payout};
use tenero_store::Store;

const START_MS: u64 = 1_700_000_000_000;

fn sha_engine(rig: &SimRig) -> Engine<'_> {
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
    .unwrap();
    Engine::new(node, EngineConfig::default())
}

struct Lines(Arc<Mutex<Vec<String>>>, Arc<Mutex<Vec<MinerEvent>>>);

impl Lines {
    fn new() -> Lines {
        Lines(
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(Vec::new())),
        )
    }
    fn cfg(&self) -> MinerConfig {
        let lines = Arc::clone(&self.0);
        let events = Arc::clone(&self.1);
        MinerConfig {
            log: Arc::new(move |l| lines.lock().unwrap().push(l.to_string())),
            events: Arc::new(move |e| events.lock().unwrap().push(e)),
            ..MinerConfig::default()
        }
    }
    /// What the miner told the screen, in order.
    fn events(&self) -> Vec<MinerEvent> {
        self.1.lock().unwrap().clone()
    }
    fn contains(&self, needle: &str) -> bool {
        self.0.lock().unwrap().iter().any(|l| l.contains(needle))
    }
    fn dump(&self) -> String {
        self.0.lock().unwrap().join("\n")
    }
}

fn hook_with<B, F>(factory: F, cfg: MinerConfig) -> MinerHook<PlaceholderPayout>
where
    B: Backend,
    F: FnOnce() -> Result<B, String> + Send + 'static,
{
    MinerHook::new(
        Miner::spawn(factory),
        PlaceholderPayout { seed: [7; 32] },
        cfg,
    )
}

/// Polls the hook and feeds what it returns to the engine, a simulated minute passing for each block found, until
/// `until` holds or `limit` passes.
fn drive<P: tenero_miner::PayoutSource>(
    engine: &mut Engine<'_>,
    hook: &mut MinerHook<P>,
    clock: &mut u64,
    until: impl Fn(&Engine<'_>) -> bool,
    limit: Duration,
) -> bool {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        for ev in hook.poll(engine, *clock) {
            let block = matches!(ev, Event::LocalBlock(_));
            engine.handle(*clock, ev);
            if block {
                *clock += 60_000;
            }
        }
        if until(engine) {
            return true;
        }
        thread::sleep(Duration::from_millis(2));
    }
    false
}

fn height(e: &Engine<'_>) -> u64 {
    e.node().tip().unwrap().0
}

/// A backend that never finds anything and records the height of each job it is given.
struct Idle {
    heights: Arc<Mutex<Vec<u64>>>,
    returned: Arc<AtomicU64>,
}

impl Backend for Idle {
    fn name(&self) -> String {
        "idle".into()
    }
    fn mine(&mut self, job: &Job, _c: &Counters) -> Result<Option<Solution>, String> {
        self.heights.lock().unwrap().push(job.height);
        while !job.stale.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(1));
        }
        self.returned.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
}

fn idle() -> (Idle, Arc<Mutex<Vec<u64>>>, Arc<AtomicU64>) {
    let (heights, returned) = (
        Arc::new(Mutex::new(Vec::new())),
        Arc::new(AtomicU64::new(0)),
    );
    (
        Idle {
            heights: Arc::clone(&heights),
            returned: Arc::clone(&returned),
        },
        heights,
        returned,
    )
}

// ---- the SHA-256 test chain, end to end ---------------------------------------------------------------------

#[test]
fn the_miner_mines_blocks_its_own_node_accepts_one_after_another() {
    let rig = SimRig::rigs("mn-sha", 1);
    let mut engine = sha_engine(&rig[0]);
    let lines = Lines::new();
    let mut hook = hook_with(|| Ok(Sha256Backend), lines.cfg());
    let mut clock = START_MS;
    assert!(
        drive(
            &mut engine,
            &mut hook,
            &mut clock,
            |e| height(e) >= 12,
            Duration::from_secs(30)
        ),
        "{}",
        lines.dump()
    );
    // every block in the chain is the node's own validated one: check a few by id, and the bookkeeping
    drive(
        &mut engine,
        &mut hook,
        &mut clock,
        |_| false,
        Duration::from_millis(100),
    );
    assert!(hook.stats.blocks_found >= 12, "{:?}", hook.stats);
    assert!(hook.stats.blocks_accepted >= 11, "{:?}", hook.stats);
    assert_eq!(hook.stats.blocks_rejected, 0);
    assert_eq!(hook.stats.bad_solutions, 0);
    assert!(
        lines.contains("mining with sha256 test chain"),
        "{}",
        lines.dump()
    );
    assert!(lines.contains("is in the chain"));
    let counters = hook.counters();
    assert!(counters.attempts.load(Ordering::Relaxed) > 0);
    assert!(counters.found.load(Ordering::Relaxed) >= 12);
    // the coinbase of each block pays the placeholder address of its height
    let b5 = rig[0].store.get_block(5).unwrap().unwrap().coinbase;
    assert_eq!(b5.height, 5);
    // the screen is told: what is mining, then each block in the chain, with how long it took and what it pays
    let ev = lines.events();
    assert_eq!(
        ev[0],
        MinerEvent::Started {
            backend: "sha256 test chain (CPU)".to_string()
        },
        "{ev:?}"
    );
    let in_chain: Vec<(u64, f64, u64)> = ev
        .iter()
        .filter_map(|e| match e {
            MinerEvent::InChain {
                height,
                secs,
                reward,
                work,
            } => {
                // what the block is worth: the expected attempts at its target (the test chain's target is easy, but not zero)
                assert!(
                    work.is_finite() && *work > 1.0,
                    "{work}: not the 1.0 of a block that is worth nothing"
                );
                Some((*height, *secs, *reward))
            }
            _ => None,
        })
        .collect();
    assert_eq!(in_chain.len() as u64, hook.stats.blocks_accepted, "{ev:?}");
    assert!(in_chain.len() >= 11);
    for (i, (h, secs, reward)) in in_chain.iter().enumerate() {
        assert_eq!(*h, i as u64 + 1, "blocks come in order: {in_chain:?}");
        assert!(secs.is_finite() && *secs >= 0.0 && *secs < 30.0, "{secs}");
        let paid: u64 = rig[0]
            .store
            .get_block(*h)
            .unwrap()
            .unwrap()
            .coinbase
            .outputs
            .iter()
            .map(|o| o.amount)
            .sum();
        assert!(paid > 0);
        assert_eq!(*reward, paid, "the reward told is what block {h} pays");
    }
    assert!(!ev.iter().any(|e| matches!(
        e,
        MinerEvent::LostRace { .. } | MinerEvent::Refused { .. } | MinerEvent::Paused
    )));
}

// ---- the job follows the tip -------------------------------------------------------------------------------

#[test]
fn a_tip_that_moves_cancels_the_job_and_the_next_one_starts_on_the_new_tip() {
    let rig = SimRig::rigs("mn-tip", 1);
    let mut engine = sha_engine(&rig[0]);
    let (backend, heights, returned) = idle();
    let mut hook = hook_with(move || Ok(backend), MinerConfig::default());
    let mut clock = START_MS;
    drive(
        &mut engine,
        &mut hook,
        &mut clock,
        |_| heights.lock().unwrap().len() == 1,
        Duration::from_secs(5),
    );
    assert_eq!(*heights.lock().unwrap(), vec![1]);
    // a block arrives from elsewhere (here: mined directly on the node): the tip moves
    let payout = Payout {
        onetime_address: [1; 32],
        view_tag: [0; 3],
        ephemeral_pubkey: [2; 32],
        anchor_enc: [0; 16],
    };
    let ts = clock / 1000 + 60;
    let b = mine_test_block(engine.node(), ts, payout);
    engine.handle(clock, Event::LocalBlock(b));
    assert_eq!(height(&engine), 1);
    assert!(drive(
        &mut engine,
        &mut hook,
        &mut clock,
        |_| heights.lock().unwrap().len() == 2,
        Duration::from_secs(5)
    ));
    assert_eq!(
        *heights.lock().unwrap(),
        vec![1, 2],
        "the new job is for the block after the new tip"
    );
    let end = Instant::now() + Duration::from_secs(3);
    while returned.load(Ordering::SeqCst) < 1 && Instant::now() < end {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        returned.load(Ordering::SeqCst),
        1,
        "the job on the old tip was told to stop"
    );
    assert_eq!(hook.stats.templates, 2);
}

#[test]
fn a_template_that_gets_old_is_replaced_on_the_same_tip() {
    let rig = SimRig::rigs("mn-refresh", 1);
    let mut engine = sha_engine(&rig[0]);
    let (backend, heights, _) = idle();
    let cfg = MinerConfig {
        refresh_every: Duration::from_millis(150),
        ..MinerConfig::default()
    };
    let mut hook = hook_with(move || Ok(backend), cfg);
    let mut clock = START_MS;
    assert!(drive(
        &mut engine,
        &mut hook,
        &mut clock,
        |_| heights.lock().unwrap().len() >= 3,
        Duration::from_secs(5)
    ));
    assert!(
        heights.lock().unwrap().iter().all(|&h| h == 1),
        "same tip, same height"
    );
    assert!(hook.stats.templates >= 3);
}

#[test]
fn mining_can_be_paused_and_resumed() {
    let rig = SimRig::rigs("mn-pause", 1);
    let mut engine = sha_engine(&rig[0]);
    let (backend, heights, returned) = idle();
    let mut hook = hook_with(move || Ok(backend), MinerConfig::default());
    let switch = hook.enabled();
    let mut clock = START_MS;
    drive(
        &mut engine,
        &mut hook,
        &mut clock,
        |_| heights.lock().unwrap().len() == 1,
        Duration::from_secs(5),
    );
    switch.store(false, Ordering::SeqCst);
    drive(
        &mut engine,
        &mut hook,
        &mut clock,
        |_| false,
        Duration::from_millis(200),
    );
    assert_eq!(heights.lock().unwrap().len(), 1, "no new job while paused");
    assert_eq!(
        returned.load(Ordering::SeqCst),
        1,
        "and the running one was stopped"
    );
    switch.store(true, Ordering::SeqCst);
    assert!(drive(
        &mut engine,
        &mut hook,
        &mut clock,
        |_| heights.lock().unwrap().len() == 2,
        Duration::from_secs(5)
    ));
}

// ---- what a backend can get wrong --------------------------------------------------------------------------

/// What a lying backend answers with the first time.
type Answer = Box<dyn FnOnce(&Job) -> Solution + Send>;

/// Answers first with whatever `first` says, then behaves (the SHA-256 search).
struct Liar {
    first: Option<Answer>,
    honest: Sha256Backend,
}

impl Backend for Liar {
    fn name(&self) -> String {
        "liar".into()
    }
    fn mine(&mut self, job: &Job, c: &Counters) -> Result<Option<Solution>, String> {
        match self.first.take() {
            Some(f) => Ok(Some(f(job))),
            None => self.honest.mine(job, c),
        }
    }
}

fn a_nonce_that_fails(job: &Job) -> u64 {
    let mut h = job.header.clone();
    (0u64..)
        .find(|&n| {
            h.nonce = n;
            U256::from_be_bytes(&ids::block_id(&h, PowKind::Sha256)) >= job.target
        })
        .unwrap()
}

#[test]
fn a_solution_that_does_not_meet_the_target_is_discarded_and_mining_goes_on() {
    let rig = SimRig::rigs("mn-bad", 1);
    let mut engine = sha_engine(&rig[0]);
    let lines = Lines::new();
    let liar = Liar {
        first: Some(Box::new(|job: &Job| Solution {
            job_id: job.id,
            nonce: a_nonce_that_fails(job),
            mix: [0; 64],
        })),
        honest: Sha256Backend,
    };
    let mut hook = hook_with(move || Ok(liar), lines.cfg());
    let mut clock = START_MS;
    assert!(
        drive(
            &mut engine,
            &mut hook,
            &mut clock,
            |e| height(e) >= 3,
            Duration::from_secs(30)
        ),
        "{}",
        lines.dump()
    );
    assert_eq!(hook.stats.bad_solutions, 1);
    assert!(
        lines.contains("does not meet the target"),
        "{}",
        lines.dump()
    );
}

#[test]
fn a_block_the_node_refuses_is_reported_and_mining_goes_on() {
    let rig = SimRig::rigs("mn-refused", 1);
    let mut engine = sha_engine(&rig[0]);
    let lines = Lines::new();
    // a nonce that meets the target, with a mix that is wrong (the SHA-256 chain's mix is zero): it passes the
    // cheap check and fails the full one, like a wrong answer from a GPU would
    let liar = Liar {
        first: Some(Box::new(|job: &Job| {
            let mut h = job.header.clone();
            let nonce = (0u64..)
                .find(|&n| {
                    h.nonce = n;
                    U256::from_be_bytes(&ids::block_id(&h, PowKind::Sha256)) < job.target
                })
                .unwrap();
            Solution {
                job_id: job.id,
                nonce,
                mix: [1; 64],
            }
        })),
        honest: Sha256Backend,
    };
    let mut hook = hook_with(move || Ok(liar), lines.cfg());
    let mut clock = START_MS;
    assert!(
        drive(
            &mut engine,
            &mut hook,
            &mut clock,
            |e| height(e) >= 2,
            Duration::from_secs(30)
        ),
        "{}",
        lines.dump()
    );
    assert_eq!(hook.stats.blocks_rejected, 1, "{}", lines.dump());
    assert!(
        lines.contains("was REFUSED by our own node"),
        "{}",
        lines.dump()
    );
    assert!(hook.stats.blocks_accepted >= 1);
    let ev = lines.events();
    assert!(ev.contains(&MinerEvent::Refused { height: 1 }), "{ev:?}");
    assert!(ev.iter().any(|e| matches!(e, MinerEvent::InChain { .. })));
}

#[test]
fn a_late_solution_for_a_job_that_was_replaced_is_ignored() {
    let rig = SimRig::rigs("mn-late", 1);
    let mut engine = sha_engine(&rig[0]);
    let lines = Lines::new();
    // answers for a job id that is not the current one
    let liar = Liar {
        first: Some(Box::new(|job: &Job| Solution {
            job_id: job.id + 1000,
            nonce: a_nonce_that_fails(job),
            mix: [0; 64],
        })),
        honest: Sha256Backend,
    };
    let mut hook = hook_with(move || Ok(liar), lines.cfg());
    let mut clock = START_MS;
    assert!(drive(
        &mut engine,
        &mut hook,
        &mut clock,
        |e| height(e) >= 2,
        Duration::from_secs(30)
    ));
    assert_eq!(hook.stats.blocks_rejected, 0);
    assert_eq!(
        hook.stats.bad_solutions, 0,
        "the late answer was used (and found wanting) instead of ignored"
    );
}

#[test]
fn a_backend_that_cannot_start_stops_mining_and_says_so() {
    let rig = SimRig::rigs("mn-nofactory", 1);
    let mut engine = sha_engine(&rig[0]);
    let lines = Lines::new();
    let mut hook = hook_with(
        || Err::<Sha256Backend, String>("no GPU found".into()),
        lines.cfg(),
    );
    let mut clock = START_MS;
    drive(
        &mut engine,
        &mut hook,
        &mut clock,
        |_| hook_failed(),
        Duration::from_millis(1),
    );
    for _ in 0..100 {
        hook.poll(&mut engine, clock);
        if hook.failed() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(hook.failed());
    assert!(lines.contains("no GPU found"), "{}", lines.dump());
    assert!(
        lines.events().contains(&MinerEvent::BackendFailed {
            why: "no GPU found".to_string()
        }),
        "{:?}",
        lines.events()
    );
    assert_eq!(height(&engine), 0);
    // and it stays stopped
    assert!(hook.poll(&mut engine, clock).is_empty());
}

fn hook_failed() -> bool {
    false
}

// ---- searching and cancelling -------------------------------------------------------------------------------

fn job_for(rig: &SimRig, target: U256, id: u64) -> (Job, Arc<AtomicBool>) {
    let engine = sha_engine(rig);
    let payout = tenero_net::sim::test_payout(Payout {
        onetime_address: [9; 32],
        view_tag: [0; 3],
        ephemeral_pubkey: [9; 32],
        anchor_enc: [0; 16],
    });
    let block = engine
        .node()
        .block_template(1_700_000_060, 1000, &|_| payout.clone())
        .unwrap();
    let stale = Arc::new(AtomicBool::new(false));
    (
        Job {
            id,
            header: block.header,
            height: 1,
            target,
            stale: Arc::clone(&stale),
            nonce_start: None,
        },
        stale,
    )
}

#[test]
fn a_search_that_can_never_succeed_stops_promptly_when_told() {
    let rig = SimRig::rigs("mn-cancel", 1);
    let (job, stale) = job_for(&rig[0], U256::ZERO, 1);
    let counters = Arc::new(Counters::default());
    let c = Arc::clone(&counters);
    let t = thread::spawn(move || Sha256Backend.mine(&job, &c));
    thread::sleep(Duration::from_millis(100));
    let told = Instant::now();
    stale.store(true, Ordering::SeqCst);
    assert_eq!(t.join().unwrap().unwrap(), None);
    assert!(told.elapsed() < Duration::from_secs(1));
    assert!(
        counters.attempts.load(Ordering::Relaxed) > 1000,
        "it was searching"
    );
}

#[test]
fn different_jobs_start_at_different_nonces() {
    let rig = SimRig::rigs("mn-nonces", 1);
    let (a, _) = job_for(&rig[0], U256::MAX, 1);
    let (b, _) = job_for(&rig[0], U256::MAX, 2);
    let counters = Counters::default();
    let (sa, sb) = (
        Sha256Backend.mine(&a, &counters).unwrap().unwrap(),
        Sha256Backend.mine(&b, &counters).unwrap().unwrap(),
    );
    // with the easiest possible target the first nonce tried is the answer
    assert_ne!(sa.nonce, sb.nonce);
}

// ---- matmulhash on the CPU, crossing epochs -----------------------------------------------------------------

struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> TempDb {
        let p =
            std::env::temp_dir().join(format!("tenero-miner-{}-{name}.redb", std::process::id()));
        let db = TempDb(p);
        db.remove();
        db
    }
    fn remove(&self) {
        let _ = std::fs::remove_file(&self.0);
        let mut s = self.0.clone().into_os_string();
        s.push(".segments");
        let _ = std::fs::remove_dir_all(PathBuf::from(s));
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        self.remove();
    }
}

fn small() -> Params {
    Params {
        m: 8,
        k: 64,
        nb: 64,
        num_blocks: 8,
    }
}

#[test]
fn the_cpu_matmul_miner_mines_blocks_a_matmul_node_verifies_across_epoch_boundaries() {
    let db = TempDb::new("matmul");
    let store = Store::open(&db.0, LABEL, PowKind::Matmul).unwrap();
    let params = ChainParams::version_3(LABEL, PowKind::Matmul, U256::pow2(253).unwrap());
    // one MatmulPow shared by the node's validation and the miner, so each dataset is built once
    let pow = Arc::new(MatmulPow::new(small(), 4, 1).unwrap());
    let node = Node::with_proof_check(
        &store,
        &params,
        &*pow,
        &ProofsNotChecked,
        NodeConfig {
            allow_unchecked_proofs_for_tests: true,
            ..NodeConfig::default()
        },
    )
    .unwrap();
    let mut engine = Engine::new(node, EngineConfig::default());
    let lines = Lines::new();
    // the miner has a proof of work of its own here, so what it builds is only what IT built
    let miner_pow = Arc::new(MatmulPow::new(small(), 4, 1).unwrap());
    // epochs of 4 blocks; the next dataset is built 2 blocks ahead
    let mut hook = hook_with(
        move || Ok(CpuMatmulBackend::new(miner_pow, 4, 2, 2)),
        lines.cfg(),
    );
    let mut clock = START_MS;
    // blocks 1..=14: epochs 0, 1, 2 and the start of 3
    assert!(
        drive(
            &mut engine,
            &mut hook,
            &mut clock,
            |e| height(e) >= 14,
            Duration::from_secs(120)
        ),
        "{}",
        lines.dump()
    );
    // stop mining, then let the last block's verdict come in
    hook.enabled().store(false, Ordering::SeqCst);
    drive(
        &mut engine,
        &mut hook,
        &mut clock,
        |_| false,
        Duration::from_millis(100),
    );
    assert_eq!(hook.stats.blocks_rejected, 0, "{}", lines.dump());
    assert_eq!(hook.stats.bad_solutions, 0);
    let c = hook.counters();
    let (prefetched, built) = (
        c.prefetches.load(Ordering::Relaxed),
        c.dataset_builds.load(Ordering::Relaxed),
    );
    // epochs 1, 2 and 3 were each built ahead of their first block (a 4th, if a job for height 15 had started)
    assert!(
        (3..=4).contains(&prefetched),
        "{prefetched} datasets were built ahead of time"
    );
    // and the only one built when it was needed, not ahead, was the very first
    assert_eq!(built, prefetched + 1, "{built} datasets built in all");
    // every build was marked as one, so the rate meter leaves its time out (and nothing else was marked)
    assert_eq!(
        c.build_marks.load(Ordering::Relaxed),
        built,
        "builds marked"
    );
    assert!(!c.searching(), "the job is over and nothing is being built");
    assert!(
        lines.contains("matmulhash on 2 CPU thread"),
        "{}",
        lines.dump()
    );
    assert!(c.attempts.load(Ordering::Relaxed) >= 14);
    // the node checked every one of them with the full CPU proof of work: a chain of 14 or more is proof
    assert!(store.tip().unwrap().0 >= 14);
}

#[test]
fn the_cpu_matmul_search_is_cancelled_and_its_threads_are_limited() {
    let pow = Arc::new(MatmulPow::new(small(), 4, 1).unwrap());
    assert_eq!(
        CpuMatmulBackend::new(Arc::clone(&pow), 4, 100, 0).cores(),
        MAX_CORES
    );
    assert_eq!(CpuMatmulBackend::new(Arc::clone(&pow), 4, 0, 0).cores(), 1);
    assert_eq!(CpuMatmulBackend::new(Arc::clone(&pow), 4, 3, 0).cores(), 3);
    let rig = SimRig::rigs("mn-cpucancel", 1);
    let (mut job, stale) = job_for(&rig[0], U256::ZERO, 5);
    job.height = 1;
    let counters = Arc::new(Counters::default());
    let c = Arc::clone(&counters);
    let mut backend = CpuMatmulBackend::new(pow, 4, 2, 0);
    let t = thread::spawn(move || backend.mine(&job, &c));
    thread::sleep(Duration::from_millis(100));
    stale.store(true, Ordering::SeqCst);
    assert_eq!(t.join().unwrap().unwrap(), None);
    assert!(counters.attempts.load(Ordering::Relaxed) >= 2);
}

#[test]
fn the_node_prepares_the_next_epochs_dataset_before_the_first_block_that_needs_it() {
    let db = TempDb::new("nodeprefetch");
    let store = Store::open(&db.0, LABEL, PowKind::Matmul).unwrap();
    let params = ChainParams::version_3(LABEL, PowKind::Matmul, U256::pow2(253).unwrap());
    // the node's own proof of work, and a DIFFERENT one for the miner, so only the node's own prefetch can have
    // built the node's datasets
    let node_pow = Arc::new(MatmulPow::new(small(), 4, 1).unwrap());
    let miner_pow = Arc::new(MatmulPow::new(small(), 4, 1).unwrap());
    let node = Node::with_proof_check(
        &store,
        &params,
        &*node_pow,
        &ProofsNotChecked,
        NodeConfig {
            allow_unchecked_proofs_for_tests: true,
            ..NodeConfig::default()
        },
    )
    .unwrap();
    // epochs of 4 blocks (heights 1 to 4 are epoch 0, 5 to 8 epoch 1); look 2 blocks ahead
    let mut engine = Engine::new(
        node,
        EngineConfig {
            pow_prefetch_blocks: 2,
            ..EngineConfig::default()
        },
    );
    let mut hook = hook_with(
        move || Ok(CpuMatmulBackend::new(miner_pow, 4, 1, 0)),
        Lines::new().cfg(),
    );
    let mut clock = START_MS;
    assert!(drive(
        &mut engine,
        &mut hook,
        &mut clock,
        |e| height(e) >= 2,
        Duration::from_secs(30)
    ));
    hook.enabled().store(false, Ordering::SeqCst); // no block from epoch 1 is checked while we look
    let end = Instant::now() + Duration::from_secs(10);
    while !node_pow.has_dataset(5) && Instant::now() < end {
        drive(
            &mut engine,
            &mut hook,
            &mut clock,
            |_| false,
            Duration::from_millis(20),
        );
    }
    assert!(
        node_pow.has_dataset(5),
        "the next epoch's dataset was not built ahead of time"
    );
    assert!(
        store.tip().unwrap().0 < 5,
        "a block of the next epoch had already been checked"
    );
    assert!(!node_pow.has_dataset(9), "and not the epoch after that");
}

// ---- more rules, each with its own test ---------------------------------------------------------------------

fn bare_job(id: u64) -> (Job, Arc<AtomicBool>) {
    let stale = Arc::new(AtomicBool::new(false));
    (
        Job {
            id,
            header: tenero_core::v2::BlockHeader {
                version: tenero_core::v2::VERSION,
                prev_id: [0; 32],
                timestamp: id,
                tx_root: [0; 32],
                nonce: 0,
                mix: [0; 64],
            },
            height: 1,
            target: U256::ZERO,
            stale: Arc::clone(&stale),
            nonce_start: None,
        },
        stale,
    )
}

/// Reports each job it starts, and when told to stop it waits for a signal before returning, so a test can queue
/// jobs behind it.
struct Gated {
    started: std::sync::mpsc::Sender<u64>,
    gate: std::sync::mpsc::Receiver<()>,
}

impl Backend for Gated {
    fn name(&self) -> String {
        "gated".into()
    }
    fn mine(&mut self, job: &Job, _c: &Counters) -> Result<Option<Solution>, String> {
        let _ = self.started.send(job.id);
        while !job.stale.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(1));
        }
        let _ = self.gate.recv(); // released by the test (or by its sender being dropped)
        Ok(None)
    }
}

#[test]
fn the_miner_thread_stops_the_job_it_is_on_and_starts_only_the_newest_of_those_waiting() {
    let (stx, srx) = std::sync::mpsc::channel();
    let (gtx, grx) = std::sync::mpsc::channel();
    let mut miner = Miner::spawn(move || {
        Ok(Gated {
            started: stx,
            gate: grx,
        })
    });
    let ((j1, s1), (j2, s2), (j3, s3)) = (bare_job(1), bare_job(2), bare_job(3));
    miner.submit(j1);
    assert_eq!(srx.recv_timeout(Duration::from_secs(5)).unwrap(), 1);
    // two more arrive while the first is still being stopped
    miner.submit(j2);
    assert!(
        s1.load(Ordering::SeqCst),
        "submitting a job tells the old one to stop"
    );
    miner.submit(j3);
    assert!(s2.load(Ordering::SeqCst));
    assert!(!s3.load(Ordering::SeqCst));
    gtx.send(()).unwrap(); // the first returns; two are waiting
    assert_eq!(
        srx.recv_timeout(Duration::from_secs(5)).unwrap(),
        3,
        "the one in the middle is not worth starting"
    );
    assert!(srx.try_recv().is_err());
    assert_eq!(
        miner.counters.jobs.load(Ordering::Relaxed),
        2,
        "jobs 1 and 3, not 2"
    );
    drop(gtx);
    drop(miner); // stops job 3 and ends the thread
}

#[test]
fn an_id_must_be_strictly_below_the_target_to_meet_it() {
    let at = |last: u8| {
        let mut b = [0u8; 32];
        b[31] = last;
        b
    };
    let target = U256::from_be_bytes(&at(5));
    assert!(tenero_miner::meets_target(&at(4), &target));
    assert!(
        !tenero_miner::meets_target(&at(5), &target),
        "equal to the target is not below it"
    );
    assert!(!tenero_miner::meets_target(&at(6), &target));
}

#[test]
fn the_placeholder_payout_depends_on_the_height_and_the_seed_and_on_nothing_else() {
    use tenero_miner::PayoutSource;
    let mut a = PlaceholderPayout { seed: [1; 32] };
    let mut b = PlaceholderPayout { seed: [2; 32] };
    let p = a.payout(10, 7);
    let again = a.payout(10, 7);
    assert_eq!(
        (
            p.onetime_address,
            p.view_tag,
            p.ephemeral_pubkey,
            p.anchor_enc
        ),
        (
            again.onetime_address,
            again.view_tag,
            again.ephemeral_pubkey,
            again.anchor_enc
        )
    );
    let later = a.payout(11, 7);
    assert_ne!(p.onetime_address, later.onetime_address);
    assert_ne!(p.ephemeral_pubkey, later.ephemeral_pubkey);
    assert_ne!(p.onetime_address, b.payout(10, 7).onetime_address);
    assert_ne!(
        p.onetime_address, p.ephemeral_pubkey,
        "the address and the key are made apart"
    );
}

#[test]
fn a_block_that_loses_a_race_is_reported_as_that_and_not_as_a_refusal() {
    let rig = SimRig::rigs("mn-race", 1);
    let mut engine = sha_engine(&rig[0]);
    let lines = Lines::new();
    let mut hook = hook_with(|| Ok(Sha256Backend), lines.cfg());
    let clock = START_MS;
    // wait for the hook to hand over a block, and hold it back
    let ours = loop {
        if let Some(ev) = hook.poll(&mut engine, clock).into_iter().next() {
            break ev;
        }
        thread::sleep(Duration::from_millis(2));
    };
    // meanwhile another block takes the same place
    let payout = Payout {
        onetime_address: [5; 32],
        view_tag: [0; 3],
        ephemeral_pubkey: [6; 32],
        anchor_enc: [0; 16],
    };
    let rival = mine_test_block(engine.node(), clock / 1000 + 61, payout);
    engine.handle(clock, Event::LocalBlock(rival));
    engine.handle(clock, ours); // a tie with a block that came first: kept aside
    hook.poll(&mut engine, clock); // the verdict
    assert_eq!(
        (
            hook.stats.blocks_lost_race,
            hook.stats.blocks_rejected,
            hook.stats.blocks_accepted
        ),
        (1, 0, 0),
        "{}",
        lines.dump()
    );
    assert!(lines.contains("lost a race"), "{}", lines.dump());
    let ev = lines.events();
    assert!(ev.contains(&MinerEvent::LostRace { height: 1 }), "{ev:?}");
    assert!(!ev
        .iter()
        .any(|e| matches!(e, MinerEvent::Refused { .. } | MinerEvent::InChain { .. })));
}

#[test]
fn a_failed_backend_is_given_no_more_templates() {
    let rig = SimRig::rigs("mn-failed", 1);
    let mut engine = sha_engine(&rig[0]);
    let cfg = MinerConfig {
        refresh_every: Duration::from_millis(5), // a live miner would take a new template every few milliseconds
        ..MinerConfig::default()
    };
    let mut hook = hook_with(|| Err::<Sha256Backend, String>("no GPU found".into()), cfg);
    let end = Instant::now() + Duration::from_secs(5);
    while !hook.failed() && Instant::now() < end {
        hook.poll(&mut engine, START_MS);
        thread::sleep(Duration::from_millis(5));
    }
    assert!(hook.failed());
    let before = hook.stats.templates;
    for _ in 0..30 {
        assert!(hook.poll(&mut engine, START_MS).is_empty());
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        hook.stats.templates, before,
        "templates went on being made for a dead backend"
    );
}

#[test]
fn a_node_that_is_catching_up_is_not_mined_on_unless_told_to() {
    use tenero_net::{Hello, Message, PROTOCOL_VERSION};
    for (mine_while_syncing, expect_jobs) in [(false, 0usize), (true, 1)] {
        let rig = SimRig::rigs(&format!("mn-sync{expect_jobs}"), 1);
        let mut engine = sha_engine(&rig[0]);
        // a peer that claims far more work than we have: the engine starts to sync from it
        engine.handle(
            START_MS,
            Event::PeerConnected {
                peer: 1,
                addr: "8.8.8.8:8333".into(),
                inbound: false,
            },
        );
        engine.handle(
            START_MS,
            Event::Message {
                peer: 1,
                msg: Message::Hello(Hello {
                    version: PROTOCOL_VERSION,
                    chain_id: rig[0].store.chain_id(),
                    tip_height: 500,
                    cumulative_work: U256::pow2(200).unwrap().to_be_bytes(),
                    tip_id: [9; 32],
                    pruned_below: 0,
                    nonce: 0,
                }),
            },
        );
        assert!(engine.is_syncing());
        let (backend, heights, _) = idle();
        let lines = Lines::new();
        let cfg = MinerConfig {
            mine_while_syncing,
            ..lines.cfg()
        };
        let mut hook = hook_with(move || Ok(backend), cfg);
        for _ in 0..40 {
            hook.poll(&mut engine, START_MS);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            heights.lock().unwrap().len(),
            expect_jobs,
            "mine_while_syncing = {mine_while_syncing}"
        );
        // a miner that waits says so, once; one that does not wait has nothing to say
        let paused = lines
            .events()
            .iter()
            .filter(|e| **e == MinerEvent::Paused)
            .count();
        assert_eq!(paused, 1 - expect_jobs, "{:?}", lines.events());
        if !mine_while_syncing {
            // the peer goes away, the node is no longer syncing, and the miner says it has resumed (once)
            engine.handle(START_MS, Event::PeerDisconnected { peer: 1 });
            assert!(!engine.is_syncing());
            for _ in 0..10 {
                hook.poll(&mut engine, START_MS);
                thread::sleep(Duration::from_millis(5));
            }
            let ev = lines.events();
            assert_eq!(
                ev.iter().filter(|e| **e == MinerEvent::Resumed).count(),
                1,
                "{ev:?}"
            );
            let (p, r) = (
                ev.iter().position(|e| *e == MinerEvent::Paused).unwrap(),
                ev.iter().position(|e| *e == MinerEvent::Resumed).unwrap(),
            );
            assert!(p < r);
        }
    }
}

// ---- paying a wallet ----------------------------------------------------------------------------------------

#[test]
fn the_miner_pays_a_wallet_that_finds_every_reward() {
    use tenero_miner::WalletPayout;
    use tenero_wallet::{Address, Wallet};
    let wallet_seed = [5u8; 32];
    let mut wallet = Wallet::from_seed(&wallet_seed, tenero_wallet::Network::Test, 0);
    let rig = SimRig::rigs("mn-wallet", 1);
    let mut engine = sha_engine(&rig[0]);
    let lines = Lines::new();
    let mut hook = MinerHook::new(
        Miner::spawn(|| Ok(Sha256Backend)),
        WalletPayout::new(wallet.address()).expect("a valid address"),
        lines.cfg(),
    );
    let mut clock = START_MS;
    assert!(
        drive(
            &mut engine,
            &mut hook,
            &mut clock,
            |e| height(e) >= 6,
            Duration::from_secs(30)
        ),
        "{}",
        lines.dump()
    );
    let tip = height(&engine);
    wallet.sync(engine.node()).unwrap();
    // one reward in every block, each its own output, and the amounts are what the blocks paid
    assert_eq!(wallet.owned().len() as u64, tip);
    let paid: u64 = (1..=tip)
        .map(|h| {
            engine
                .node()
                .store()
                .get_block(h)
                .unwrap()
                .unwrap()
                .coinbase
                .outputs[0]
                .amount
        })
        .sum();
    assert_eq!(wallet.balance(engine.node()).unwrap().total, paid);
    // a subaddress cannot be paid a block reward (Carrot pays one to a main address only)
    let sub: Address = wallet.subaddress(3).unwrap();
    assert!(WalletPayout::new(sub).is_none());
}

#[test]
fn a_paced_miner_waits_between_blocks() {
    let rig = SimRig::rigs("mn-pace", 1);
    let mut engine = sha_engine(&rig[0]);
    let lines = Lines::new();
    let cfg = MinerConfig {
        min_block_interval: Duration::from_millis(400),
        ..lines.cfg()
    };
    let mut hook = hook_with(|| Ok(Sha256Backend), cfg);
    let mut clock = START_MS;
    let started = Instant::now();
    assert!(
        drive(
            &mut engine,
            &mut hook,
            &mut clock,
            |e| height(e) >= 4,
            Duration::from_secs(30)
        ),
        "{}",
        lines.dump()
    );
    // four blocks, three gaps of at least 400 ms
    assert!(
        started.elapsed() >= Duration::from_millis(1100),
        "four blocks in {:?}",
        started.elapsed()
    );
}
