//! The miner (milestone M8.5). **Experimental and unaudited.**
//!
//! **Shape.** The node builds a block template (mempool transactions, a coinbase paying exactly what the rules
//! say, the Merkle root, the target from `Validator::next_block`). A [`Backend`] searches for a nonce (and, for
//! matmulhash, the mix that goes with it) on its own thread, for as long as the template is current. The miner
//! plugs into the node as [`MinerHook`], a [`tenero_net::transport::Hooks`]: each time the node loop polls it, it
//! collects a solution, or notices that the tip moved (or the template is old) and starts again on a fresh
//! template. A solution goes back as [`Event::LocalBlock`], through the node's own validation like any block: **a
//! wrong block from the GPU is refused by the node's CPU check**, and the hook says so in its log.
//!
//! **Backends**
//! * [`Sha256Backend`]: the SHA-256 test chain (a CPU finds a nonce in a few hashes);
//! * [`CpuMatmulBackend`]: the real matmulhash on the CPU, on at most [`MAX_CORES`] threads (CLAUDE.md rule 8).
//!   Only for tests and curiosity: a CPU is about 100 times slower than the GPU;
//! * [`gpu::GpuBackend`]: the real matmulhash on an NVIDIA GPU. **Its speed has not been measured by anyone but
//!   the owner** (CLAUDE.md rule 5): this code compiles anywhere and is only run on the owner's machine.
//!
//! **Epoch switching.** A dataset serves 100 blocks and takes 2.6 s to build on the CPU and about 0.12 s on the GPU.
//! Each backend keeps the last two epochs and, when the template is within `prefetch_blocks` of the end of its
//! epoch, builds the next one between two batches, so the block that crosses the boundary finds it ready.
//!
//! **Not done here** (M8.7): reaching a node that runs in another process (this is in-process: the miner is a hook
//! of the node's loop), a wallet address to pay (the payout is a [`PayoutSource`], and the default one is a
//! placeholder that nobody can spend), and a command-line program.

pub mod gpu;
pub mod gpu_stats;
pub mod rate;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tenero_chain::MatmulPow;
use tenero_core::hash::sha256;
use tenero_core::matmulhash as mh;
use tenero_core::u256::U256;
use tenero_core::v2::ids::{self, PowKind};
use tenero_core::v2::{Block, BlockHeader};
use tenero_net::transport::Hooks;
use tenero_net::{Engine, Event};
use tenero_node::Payout;

/// The most CPU threads the miner uses (CLAUDE.md rule 8).
pub const MAX_CORES: usize = 6;

/// A template to search: the header (nonce and mix empty), the height and the target it must meet.
#[derive(Clone)]
pub struct Job {
    pub id: u64,
    pub header: BlockHeader,
    pub height: u64,
    pub target: U256,
    /// Set when the job is no longer worth working on (the tip moved, or it was replaced): a backend looks at it
    /// between batches and returns.
    pub stale: Arc<AtomicBool>,
}

/// A nonce (and mix) a backend says meets the target. The hook checks it before using it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Solution {
    pub job_id: u64,
    pub nonce: u64,
    pub mix: [u8; 64],
}

/// What a backend has done, readable from any thread.
#[derive(Default)]
pub struct Counters {
    /// Nonces tried (a proof-of-work attempt each).
    pub attempts: AtomicU64,
    /// Jobs started.
    pub jobs: AtomicU64,
    /// Solutions returned.
    pub found: AtomicU64,
    /// Datasets built for the epoch AFTER the one being mined, ahead of time.
    pub prefetches: AtomicU64,
    /// Datasets built in all (the current epoch's, when it was not ready, and the prefetched ones).
    pub dataset_builds: AtomicU64,
    /// Is the backend inside `mine` (a job is being searched)? False when it waits for a job.
    pub in_job: AtomicBool,
    /// Datasets being built right now (see [`Counters::building`]).
    building: std::sync::atomic::AtomicU32,
    /// How many times a build has been marked (one for each dataset built, so it equals `dataset_builds` when the marks are right).
    pub build_marks: AtomicU64,
    /// The blocks the attempts so far should have found (see [`rate::Luck`]).
    luck: std::sync::Mutex<JobLedger>,
}

/// What the attempts at finished jobs were worth, and the job in progress.
#[derive(Default)]
struct JobLedger {
    settled: f64,
    /// (attempts at the start of the job, the work of its target)
    job: Option<(u64, f64)>,
}

impl Counters {
    /// A job begins at a target of this work (see [`rate::work_of`]).
    pub fn begin_job(&self, work: f64) {
        if let Ok(mut l) = self.luck.lock() {
            l.job = Some((self.attempts.load(Ordering::Relaxed), work));
        }
    }

    /// The job is over (found, replaced or dropped): what its attempts were worth is settled.
    pub fn end_job(&self) {
        if let Ok(mut l) = self.luck.lock() {
            if let Some((start, work)) = l.job.take() {
                let n = self.attempts.load(Ordering::Relaxed).saturating_sub(start);
                l.settled += n as f64 / work;
            }
        }
    }

    /// The blocks the attempts so far should have found, the job in progress included.
    pub fn expected_blocks(&self) -> f64 {
        let Ok(l) = self.luck.lock() else { return 0.0 };
        let live = l.job.map_or(0.0, |(start, work)| {
            self.attempts.load(Ordering::Relaxed).saturating_sub(start) as f64 / work
        });
        l.settled + live
    }

    /// Marks a dataset build for as long as the returned guard lives: no attempts are made meanwhile, and the rate meter leaves that
    /// time out.
    pub fn building(&self) -> BuildingGuard<'_> {
        self.building.fetch_add(1, Ordering::SeqCst);
        self.build_marks.fetch_add(1, Ordering::Relaxed);
        BuildingGuard(self)
    }

    /// Is the miner searching for a nonce right now (in a job, and not building a dataset)?
    pub fn searching(&self) -> bool {
        self.in_job.load(Ordering::SeqCst) && self.building.load(Ordering::SeqCst) == 0
    }
}

/// See [`Counters::building`].
pub struct BuildingGuard<'a>(&'a Counters);

impl Drop for BuildingGuard<'_> {
    fn drop(&mut self) {
        self.0.building.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Something that searches a job's nonce space.
pub trait Backend {
    fn name(&self) -> String;

    /// Searches until a nonce meets the target (`Ok(Some)`), or `job.stale` is set (`Ok(None)`), checking it at
    /// least every few batches. An error ends mining.
    fn mine(&mut self, job: &Job, counters: &Counters) -> Result<Option<Solution>, String>;
}

/// A block id meets the target when it is STRICTLY below it (an id equal to the target does not).
pub fn meets_target(id: &[u8; 32], target: &U256) -> bool {
    U256::from_be_bytes(id) < *target
}

pub(crate) fn start_nonce(job_id: u64) -> u64 {
    job_id.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

// ---- the SHA-256 test chain ---------------------------------------------------------------------------------

/// The SHA-256 test chain: the block id is `sha256(header_hash || nonce)` and the mix is zero.
#[derive(Default)]
pub struct Sha256Backend;

impl Backend for Sha256Backend {
    fn name(&self) -> String {
        "sha256 test chain (CPU)".into()
    }

    fn mine(&mut self, job: &Job, counters: &Counters) -> Result<Option<Solution>, String> {
        let mut header = job.header.clone();
        let mut nonce = start_nonce(job.id);
        let (mut tried, mut counted) = (0u64, 0u64);
        loop {
            if tried % 1024 == 0 {
                counters
                    .attempts
                    .fetch_add(tried - counted, Ordering::Relaxed);
                counted = tried;
                if job.stale.load(Ordering::SeqCst) {
                    return Ok(None);
                }
            }
            header.nonce = nonce;
            tried += 1;
            if U256::from_be_bytes(&ids::block_id(&header, PowKind::Sha256)) < job.target {
                counters
                    .attempts
                    .fetch_add(tried - counted, Ordering::Relaxed);
                counters.found.fetch_add(1, Ordering::Relaxed);
                return Ok(Some(Solution {
                    job_id: job.id,
                    nonce,
                    mix: [0; 64],
                }));
            }
            nonce = nonce.wrapping_add(1);
        }
    }
}

// ---- matmulhash on the CPU ----------------------------------------------------------------------------------

/// The real matmulhash on the CPU, on `cores` threads.
pub struct CpuMatmulBackend {
    pow: Arc<MatmulPow>,
    epoch_blocks: u64,
    cores: usize,
    prefetch_blocks: u64,
}

/// What one thread of a round found: a nonce that met the target, and its mix.
type NonceAndMix = Option<(u64, [u8; 64])>;

/// Attempts each thread makes in one round, between looks at `stale`.
const PER_THREAD_ROUND: u64 = 2;

impl CpuMatmulBackend {
    /// `pow` holds (and should be the node's own, so the dataset is built once) the last two epochs' datasets.
    /// `cores` is limited to `1..=MAX_CORES`.
    pub fn new(
        pow: Arc<MatmulPow>,
        epoch_blocks: u64,
        cores: usize,
        prefetch_blocks: u64,
    ) -> CpuMatmulBackend {
        CpuMatmulBackend {
            pow,
            epoch_blocks,
            cores: cores.clamp(1, MAX_CORES),
            prefetch_blocks,
        }
    }

    /// The number of threads in use.
    pub fn cores(&self) -> usize {
        self.cores
    }
}

impl Backend for CpuMatmulBackend {
    fn name(&self) -> String {
        format!("matmulhash on {} CPU thread(s)", self.cores)
    }

    fn mine(&mut self, job: &Job, counters: &Counters) -> Result<Option<Solution>, String> {
        let had = self.pow.has_dataset(job.height);
        let data = {
            let _building = (!had).then(|| counters.building());
            self.pow.dataset_for(job.height)?
        };
        if !had {
            counters.dataset_builds.fetch_add(1, Ordering::Relaxed);
        }
        let hh = ids::header_hash(&job.header);
        let base = start_nonce(job.id);
        let here = mh::epoch_of(job.height, self.epoch_blocks);
        let mut prefetched: Option<u64> = None;
        let mut round = 0u64;
        loop {
            if job.stale.load(Ordering::SeqCst) {
                return Ok(None);
            }
            // the next epoch's dataset, a few blocks before it is needed
            let ahead = mh::epoch_of(job.height + self.prefetch_blocks, self.epoch_blocks);
            if self.prefetch_blocks > 0 && ahead != here && ahead != prefetched {
                let at = job.height + self.prefetch_blocks;
                let had = self.pow.has_dataset(at);
                {
                    let _building = (!had).then(|| counters.building());
                    self.pow.dataset_for(at)?;
                }
                if !had {
                    // (a job restarted near the boundary finds it already built, and counts nothing)
                    counters.prefetches.fetch_add(1, Ordering::Relaxed);
                    counters.dataset_builds.fetch_add(1, Ordering::Relaxed);
                }
                prefetched = ahead;
            }
            let per_round = self.cores as u64 * PER_THREAD_ROUND;
            let first = base.wrapping_add(round * per_round);
            let found: Result<Vec<NonceAndMix>, String> = thread::scope(|s| {
                let handles: Vec<_> = (0..self.cores as u64)
                    .map(|t| {
                        let (data, hh, target) = (&data, &hh, &job.target);
                        s.spawn(move || -> Result<NonceAndMix, String> {
                            for i in 0..PER_THREAD_ROUND {
                                let nonce = first.wrapping_add(t * PER_THREAD_ROUND + i);
                                let a = mh::compute_attempt(data, hh, nonce)?;
                                counters.attempts.fetch_add(1, Ordering::Relaxed);
                                if mh::meets_target(&a.digest, target) {
                                    return Ok(Some((nonce, a.mix)));
                                }
                            }
                            Ok(None)
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| {
                        h.join()
                            .map_err(|_| "a mining thread panicked".to_string())?
                    })
                    .collect()
            });
            // the lowest nonce of the round that met the target
            if let Some((nonce, mix)) = found?.into_iter().flatten().min_by_key(|(n, _)| *n) {
                counters.found.fetch_add(1, Ordering::Relaxed);
                return Ok(Some(Solution {
                    job_id: job.id,
                    nonce,
                    mix,
                }));
            }
            round += 1;
        }
    }
}

// ---- the miner thread ---------------------------------------------------------------------------------------

/// What the miner thread says.
pub enum Msg {
    Ready(String),
    Solved(Solution),
    /// The thread has finished with this job (found something for it, or was told to stop) and is waiting for
    /// another: if it is still the job the hook thinks is current, the hook must start a new one.
    Finished(u64),
    Failed(String),
}

/// A backend running on a thread of its own.
pub struct Miner {
    jobs: Option<Sender<Job>>,
    results: Receiver<Msg>,
    handle: Option<JoinHandle<()>>,
    stale: Option<Arc<AtomicBool>>,
    pub counters: Arc<Counters>,
}

impl Miner {
    /// Starts the thread, which builds its backend with `factory` (so the backend need not be `Send`: a GPU's
    /// handles are made where they are used) and then waits for jobs.
    pub fn spawn<B, F>(factory: F) -> Miner
    where
        B: Backend,
        F: FnOnce() -> Result<B, String> + Send + 'static,
    {
        let (jtx, jrx) = channel::<Job>();
        let (rtx, rrx) = channel::<Msg>();
        let counters = Arc::new(Counters::default());
        let c = Arc::clone(&counters);
        let handle = thread::spawn(move || {
            let mut backend = match factory() {
                Ok(b) => b,
                Err(e) => {
                    let _ = rtx.send(Msg::Failed(e));
                    return;
                }
            };
            let _ = rtx.send(Msg::Ready(backend.name()));
            while let Ok(mut job) = jrx.recv() {
                // only the newest job is worth starting
                while let Ok(newer) = jrx.try_recv() {
                    job = newer;
                }
                c.jobs.fetch_add(1, Ordering::Relaxed);
                c.begin_job(rate::work_of(&job.target));
                c.in_job.store(true, Ordering::SeqCst);
                let result = backend.mine(&job, &c);
                c.in_job.store(false, Ordering::SeqCst);
                c.end_job();
                match result {
                    Ok(Some(sol)) => {
                        let _ = rtx.send(Msg::Solved(sol));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        let _ = rtx.send(Msg::Failed(e));
                        return;
                    }
                }
                let _ = rtx.send(Msg::Finished(job.id));
            }
        });
        Miner {
            jobs: Some(jtx),
            results: rrx,
            handle: Some(handle),
            stale: None,
            counters,
        }
    }

    /// Gives the thread a new job, telling it to drop the one it is on.
    pub fn submit(&mut self, job: Job) {
        self.cancel();
        self.stale = Some(Arc::clone(&job.stale));
        if let Some(tx) = &self.jobs {
            let _ = tx.send(job);
        }
    }

    /// Tells the thread to drop the job it is on.
    pub fn cancel(&mut self) {
        if let Some(s) = self.stale.take() {
            s.store(true, Ordering::SeqCst);
        }
    }

    /// The next message from the thread, if there is one.
    pub fn try_msg(&self) -> Option<Msg> {
        self.results.try_recv().ok()
    }
}

impl Drop for Miner {
    fn drop(&mut self) {
        self.cancel();
        self.jobs = None; // the thread's `recv` ends
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ---- the node hook ------------------------------------------------------------------------------------------

/// Where the coinbase of each block pays.
pub trait PayoutSource {
    fn payout(&mut self, height: u64) -> Payout;
}

/// Pays every block's reward to a wallet's address (the interim output scheme, `tenero-wallet`), with fresh
/// randomness for every block, so the rewards cannot be linked to one another by anyone without the view key.
pub struct WalletPayout {
    address: tenero_wallet::Address,
}

impl WalletPayout {
    /// `None` if the address holds an invalid key (a reward could not be made for it).
    pub fn new(address: tenero_wallet::Address) -> Option<WalletPayout> {
        tenero_wallet::coinbase_payout_random(&address, 0)?;
        Some(WalletPayout { address })
    }
}

impl PayoutSource for WalletPayout {
    fn payout(&mut self, height: u64) -> Payout {
        tenero_wallet::coinbase_payout_random(&self.address, height)
            .expect("the address was checked when this payout was made")
    }
}

/// A placeholder payout derived from a seed: the outputs it makes have no spendable key behind them, so **coins
/// mined to it are unspendable**. For tests and for a test chain; use [`WalletPayout`] to be paid.
pub struct PlaceholderPayout {
    pub seed: [u8; 32],
}

impl PayoutSource for PlaceholderPayout {
    fn payout(&mut self, height: u64) -> Payout {
        let h = |tag: &[u8]| {
            sha256(&[
                b"tenero placeholder payout",
                tag,
                &self.seed,
                &height.to_le_bytes(),
            ])
        };
        let (a, e, t) = (h(b"address"), h(b"ephemeral"), h(b"anchor"));
        let mut view_tag = [0u8; 3];
        view_tag.copy_from_slice(&t[..3]);
        let mut anchor = [0u8; 16];
        anchor.copy_from_slice(&t[3..19]);
        Payout {
            onetime_address: a,
            view_tag,
            ephemeral_pubkey: e,
            anchor_enc: anchor,
        }
    }
}

pub type Logger = Arc<dyn Fn(&str) + Send + Sync>;

/// What a miner tells the program, in a form a screen can use. (The log lines carry the full detail and are unchanged; this is the
/// summary: a block in the chain, a lost race, a pause.)
#[derive(Clone, Debug, PartialEq)]
pub enum MinerEvent {
    /// The backend is ready: what is mining (`matmulhash on 2 CPU thread(s)`).
    Started {
        backend: String,
    },
    /// A block this miner found is in the chain: how long it took from the start of the job, and the reward in units.
    InChain {
        height: u64,
        secs: f64,
        reward: u64,
        /// The work (expected attempts) of the block's target: what the block is worth.
        work: f64,
    },
    /// A block this miner found lost a race: another block took its place.
    LostRace {
        height: u64,
    },
    /// A block this miner found was refused by the node.
    Refused {
        height: u64,
    },
    /// Mining is paused because the node is syncing, and resumed when it is done.
    Paused,
    Resumed,
    /// The backend failed and mining has stopped.
    BackendFailed {
        why: String,
    },
    /// (The separate miner.) Connected to the node, or lost it.
    NodeConnected,
    NodeLost {
        why: String,
    },
}

pub type EventSink = Arc<dyn Fn(MinerEvent) + Send + Sync>;

/// The reward a block pays, in units: its coinbase outputs.
pub fn block_reward(block: &Block) -> u64 {
    block.coinbase.outputs.iter().map(|o| o.amount).sum()
}

#[derive(Clone)]
pub struct MinerConfig {
    /// The most transaction bytes to put in a block.
    pub max_body_bytes: u64,
    /// A template this old is replaced by a fresh one (new transactions, a later timestamp) even if the tip has
    /// not moved.
    pub refresh_every: Duration,
    /// Mine while the node is catching up? No: a block on a tip that is about to be replaced is wasted work.
    pub mine_while_syncing: bool,
    /// The least time between one block found and the start of the next job. Zero (the default) mines as fast as
    /// the backend can; a test chain mined by a CPU needs a pace, or its difficulty runs away.
    pub min_block_interval: Duration,
    pub log: Logger,
    /// Structured events for the screen; nothing by default.
    pub events: EventSink,
}

impl Default for MinerConfig {
    fn default() -> MinerConfig {
        MinerConfig {
            max_body_bytes: 1_000_000,
            refresh_every: Duration::from_secs(60),
            mine_while_syncing: false,
            min_block_interval: Duration::ZERO,
            log: Arc::new(|_| {}),
            events: Arc::new(|_| {}),
        }
    }
}

struct Current {
    job_id: u64,
    block: Block,
    tip: [u8; 32],
    target: U256,
    started: Instant,
}

/// What the hook has seen, for an operator and for tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MinerStats {
    pub templates: u64,
    pub blocks_found: u64,
    pub blocks_accepted: u64,
    /// Found and valid, but another block took its place first: it is on a side branch.
    pub blocks_lost_race: u64,
    /// Found and then refused by our own node as invalid (a wrong proof of work, or a wrong block).
    pub blocks_rejected: u64,
    /// Solutions a backend returned that did not meet the target, or whose job was no longer current.
    pub bad_solutions: u64,
}

/// The miner as a hook of the node's loop.
pub struct MinerHook<P: PayoutSource> {
    miner: Miner,
    payout: P,
    cfg: MinerConfig,
    enabled: Arc<AtomicBool>,
    current: Option<Current>,
    next_id: u64,
    awaiting: Option<([u8; 32], u64)>,
    /// How long the block being waited for took to find, and what it pays.
    awaiting_info: (f64, u64, f64),
    paused: bool,
    last_found: Option<Instant>,
    failed: bool,
    pub stats: MinerStats,
}

impl<P: PayoutSource> MinerHook<P> {
    pub fn new(miner: Miner, payout: P, cfg: MinerConfig) -> MinerHook<P> {
        MinerHook {
            miner,
            payout,
            cfg,
            enabled: Arc::new(AtomicBool::new(true)),
            current: None,
            next_id: 1,
            awaiting: None,
            awaiting_info: (0.0, 0, 0.0),
            paused: false,
            last_found: None,
            failed: false,
            stats: MinerStats::default(),
        }
    }

    /// A switch another thread can flip to pause or resume mining.
    pub fn enabled(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.enabled)
    }

    pub fn counters(&self) -> Arc<Counters> {
        Arc::clone(&self.miner.counters)
    }

    /// The backend failed (for example no GPU): mining has stopped.
    pub fn failed(&self) -> bool {
        self.failed
    }

    fn log(&self, line: &str) {
        (self.cfg.log)(line);
    }

    fn event(&self, e: MinerEvent) {
        (self.cfg.events)(e);
    }

    fn on_solution(&mut self, sol: Solution, pow: PowKind, events: &mut Vec<Event>) {
        let Some(cur) = self.current.take() else {
            self.stats.bad_solutions += 1;
            return;
        };
        if cur.job_id != sol.job_id {
            // for a job that has been replaced: not an error, just late
            self.current = Some(cur);
            return;
        }
        let mut block = cur.block;
        block.header.nonce = sol.nonce;
        block.header.mix = sol.mix;
        let id = ids::block_id(&block.header, pow);
        if !meets_target(&id, &cur.target) {
            self.stats.bad_solutions += 1;
            self.log(&format!(
                "the backend returned nonce {} for height {} but its id does not meet the target: discarded",
                sol.nonce,
                block.coinbase.height
            ));
            return; // self.current stays empty, so the next poll starts a new job
        }
        self.stats.blocks_found += 1;
        self.log(&format!(
            "found a block at height {} (nonce {}), {:.1} s after starting it",
            block.coinbase.height,
            sol.nonce,
            cur.started.elapsed().as_secs_f64()
        ));
        self.awaiting = Some((id, block.coinbase.height));
        self.awaiting_info = (
            cur.started.elapsed().as_secs_f64(),
            block_reward(&block),
            rate::work_of(&cur.target),
        );
        self.last_found = Some(Instant::now());
        events.push(Event::LocalBlock(block));
    }
}

impl<P: PayoutSource> Hooks for MinerHook<P> {
    fn poll(&mut self, engine: &mut Engine<'_>, now_ms: u64) -> Vec<Event> {
        let mut events = Vec::new();
        let pow = engine.node().store().pow();
        // 1. what became of the block we handed to the node at the last poll (the node has dealt with it by now)
        if let Some((id, height)) = self.awaiting.take() {
            if matches!(engine.node().store().height_of(&id), Ok(Some(_))) {
                self.stats.blocks_accepted += 1;
                self.log(&format!("block {height} is in the chain"));
                self.event(MinerEvent::InChain {
                    height,
                    secs: self.awaiting_info.0,
                    reward: self.awaiting_info.1,
                    work: self.awaiting_info.2,
                });
            } else if engine.node().chain().holds_block(&id) {
                self.stats.blocks_lost_race += 1;
                self.log(&format!(
                    "block {height} lost a race: another block took its place and ours is on a side branch"
                ));
                self.event(MinerEvent::LostRace { height });
            } else {
                self.stats.blocks_rejected += 1;
                self.log(&format!(
                    "block {height} was REFUSED by our own node: the block or its proof of work is wrong"
                ));
                self.event(MinerEvent::Refused { height });
            }
        }
        // 2. what the thread has to say
        while let Some(msg) = self.miner.try_msg() {
            match msg {
                Msg::Ready(name) => {
                    self.log(&format!("mining with {name}"));
                    self.event(MinerEvent::Started { backend: name });
                }
                Msg::Solved(sol) => self.on_solution(sol, pow, &mut events),
                Msg::Finished(id) => {
                    // the thread is idle: if that was the job we are waiting on, it needs another
                    if self.current.as_ref().is_some_and(|c| c.job_id == id) {
                        self.current = None;
                    }
                }
                Msg::Failed(e) => {
                    self.log(&format!(
                        "the mining backend failed and mining has stopped: {e}"
                    ));
                    self.failed = true;
                    self.event(MinerEvent::BackendFailed { why: e });
                }
            }
        }
        // 3. a job, if we should be mining
        let syncing = engine.is_syncing() && !self.cfg.mine_while_syncing;
        if syncing != self.paused && !self.failed {
            self.paused = syncing;
            self.event(if syncing {
                MinerEvent::Paused
            } else {
                MinerEvent::Resumed
            });
        }
        if !self.enabled.load(Ordering::SeqCst) || self.failed || syncing {
            self.miner.cancel();
            self.current = None;
            return events;
        }
        if !events.is_empty() {
            return events; // the block just found moves the tip: the next poll starts on the new one
        }
        if self
            .last_found
            .is_some_and(|t| t.elapsed() < self.cfg.min_block_interval)
        {
            return events; // pacing: not yet
        }
        let Ok((_, tip_id)) = engine.node().tip() else {
            return events;
        };
        let stale = match &self.current {
            None => true,
            Some(c) => c.tip != tip_id || c.started.elapsed() >= self.cfg.refresh_every,
        };
        if stale {
            self.miner.cancel();
            let Ok(next) = engine.node().next_block() else {
                return events;
            };
            let payout = self.payout.payout(next.height);
            let Ok(block) =
                engine
                    .node()
                    .block_template(now_ms / 1000, self.cfg.max_body_bytes, payout)
            else {
                return events;
            };
            let id = self.next_id;
            self.next_id += 1;
            self.stats.templates += 1;
            self.miner.submit(Job {
                id,
                header: block.header.clone(),
                height: next.height,
                target: next.target,
                stale: Arc::new(AtomicBool::new(false)),
            });
            self.current = Some(Current {
                job_id: id,
                block,
                tip: tip_id,
                target: next.target,
                started: Instant::now(),
            });
        }
        events
    }
}
