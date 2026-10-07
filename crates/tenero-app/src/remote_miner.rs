//! A miner in a process of its own. It asks a node (over the control interface, `docs/CONTROL_PROTOCOL.md`) for a block
//! to mine, searches with one of the backends of `tenero-miner` on a thread of its own, and hands a found block back.
//!
//! * **The node validates everything.** A block found here is submitted as a local block, so the node's own checks (the
//!   full proof of work included) decide whether it joins the chain; the miner only reports what the node said.
//! * **It never mines on a stale tip.** Each step asks the node for its tip and whether it is syncing: a moved tip, or a
//!   template a minute old, makes a new job; a node that is catching up pauses it.
//! * **It survives the node.** If the control connection is lost the job is dropped and the miner reconnects; mining
//!   resumes when the node is back.
//!
//! **Experimental and unaudited.**

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tenero_core::u256::U256;
use tenero_core::v2::ids::{self, PowKind};
use tenero_core::v2::Block;
use tenero_miner::{
    block_reward, meets_target, Counters, EventSink, Job, Miner, MinerEvent, Msg, PayoutSource,
    Solution, WalletPayout,
};

use crate::client::{BlockVerdict, RemoteNode};
use crate::control::Template;
use tenero_node::Payout;

/// How far a template's timestamp may be from this computer's clock before the miner refuses it.
pub const TEMPLATE_CLOCK_SLACK_SECS: u64 = 3600;

/// Checks a block template the way a miner that does NOT trust its node must: a node on another computer can put its own
/// address in the coinbase, and the miner would then hash a block whose reward goes to the node. A template passes only if
///
/// * it is for the height and the previous block the miner asked about (a template for another is stale, not hostile:
///   the caller decides that first);
/// * its coinbase is for that height, has no extra data and pays exactly **one output, to `payout`** (the amount is the
///   chain's rule and cannot be checked without the chain: a wrong amount makes the block invalid and costs only work);
/// * the header's `tx_root` matches the coinbase and the transactions in the body, so the body cannot be swapped after
///   the work is done;
/// * the nonce and mix are empty and the timestamp is within [`TEMPLATE_CLOCK_SLACK_SECS`] of `now_secs`.
///
/// It cannot check that the target is the right difficulty or that the transactions are valid: both need the chain.
pub fn check_template(
    t: &Template,
    height: u64,
    prev_id: &[u8; 32],
    payout: &Payout,
    now_secs: u64,
) -> Result<(), String> {
    let b = &t.block;
    if t.height != height || b.coinbase.height != height {
        return Err(format!(
            "the template is for height {}, not {height}",
            t.height
        ));
    }
    if &b.header.prev_id != prev_id {
        return Err("the template builds on another block than the tip".into());
    }
    if !b.coinbase.extra.is_empty() {
        return Err("the coinbase carries extra data".into());
    }
    match b.coinbase.outputs.as_slice() {
        [o] if o.onetime_address == payout.onetime_address
            && o.view_tag == payout.view_tag
            && o.ephemeral_pubkey == payout.ephemeral_pubkey
            && o.anchor_enc == payout.anchor_enc => {}
        _ => return Err("the coinbase does not pay the address this miner asked for".into()),
    }
    match ids::block_tx_root(&b.coinbase, &b.transactions) {
        Ok(root) if root == b.header.tx_root => {}
        _ => return Err("the header's transaction root does not match the block's body".into()),
    }
    if b.header.nonce != 0 || b.header.mix != [0u8; 64] {
        return Err("the template already carries a nonce or a mix".into());
    }
    if b.header.timestamp.abs_diff(now_secs) > TEMPLATE_CLOCK_SLACK_SECS {
        return Err("the template's timestamp is far from this computer's clock".into());
    }
    Ok(())
}

pub type Logger = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone)]
pub struct RemoteMinerConfig {
    /// The most transaction bytes to ask for in a block.
    pub max_body_bytes: u32,
    /// A template this old is replaced even if the tip has not moved (new transactions, a later timestamp).
    pub refresh_every: Duration,
    /// The least time between a block found and the next job (0: as fast as possible).
    pub min_block_interval: Duration,
    pub log: Logger,
    /// Structured events for the screen; nothing by default.
    pub events: EventSink,
    /// This computer's clock in seconds since 1970 (the system clock by default; a test may give another), for the check
    /// of a template's timestamp.
    pub now_secs: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// How long to wait when the node says to slow down (its rate limit, or "busy"): the connection and the job are kept.
    pub slow_down_pause: Duration,
}

/// Whether a node's error answer means "too many requests" or "busy" (not a lost connection, and not a refused block).
pub fn is_slow_down(why: &str) -> bool {
    why.starts_with("too many requests") || why.starts_with("the node is busy")
}

fn system_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Default for RemoteMinerConfig {
    fn default() -> RemoteMinerConfig {
        RemoteMinerConfig {
            max_body_bytes: 1_000_000,
            refresh_every: Duration::from_secs(60),
            min_block_interval: Duration::ZERO,
            log: Arc::new(|_| {}),
            events: Arc::new(|_| {}),
            now_secs: Arc::new(system_now),
            slow_down_pause: Duration::from_secs(5),
        }
    }
}

/// What the node last said about itself, readable from another thread (the status display).
#[derive(Default)]
pub struct NodeProgress {
    pub height: std::sync::atomic::AtomicU64,
    pub syncing: AtomicBool,
    pub connected: AtomicBool,
}

/// What the miner has done, for an operator and for tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RemoteStats {
    pub templates: u64,
    /// A template for a height other than the one asked for (the tip moved in between): dropped.
    pub stale_templates: u64,
    /// A template that failed [`check_template`] (the node is hostile or broken): refused, not mined.
    pub refused_templates: u64,
    /// Times the node asked the miner to slow down (a rate limit or "busy"); the miner waited and carried on.
    pub slowed_down: u64,
    pub blocks_found: u64,
    pub blocks_accepted: u64,
    /// Found and valid, but another block took its place first.
    pub blocks_lost_race: u64,
    /// Found and refused by the node as invalid.
    pub blocks_refused: u64,
    /// A solution a backend returned that does not meet the target.
    pub bad_solutions: u64,
    /// Looks at the node that found it catching up (mining paused).
    pub paused_syncing: u64,
    /// Times the connection to the node was lost.
    pub connections_lost: u64,
}

struct Current {
    job_id: u64,
    block: Block,
    /// The block it builds on.
    tip: [u8; 32],
    target: U256,
    started: Instant,
}

pub struct RemoteMiner {
    miner: Miner,
    payout: WalletPayout,
    pow: PowKind,
    cfg: RemoteMinerConfig,
    current: Option<Current>,
    next_id: u64,
    last_found: Option<Instant>,
    last_refusal_log: Option<Instant>,
    last_slow_log: Option<Instant>,
    failed: Option<String>,
    /// Whether the node was last seen syncing (mining paused), and whether the loss of the node has been reported.
    paused: bool,
    reported_down: bool,
    progress: Arc<NodeProgress>,
    pub stats: RemoteStats,
}

impl RemoteMiner {
    /// `pow` is the proof of work of the node's network (to check a found block's id before submitting it).
    pub fn new(
        miner: Miner,
        payout: WalletPayout,
        pow: PowKind,
        cfg: RemoteMinerConfig,
    ) -> RemoteMiner {
        RemoteMiner {
            miner,
            payout,
            pow,
            cfg,
            current: None,
            next_id: 1,
            last_found: None,
            last_refusal_log: None,
            last_slow_log: None,
            failed: None,
            paused: false,
            reported_down: false,
            progress: Arc::new(NodeProgress::default()),
            stats: RemoteStats::default(),
        }
    }

    pub fn counters(&self) -> Arc<Counters> {
        Arc::clone(&self.miner.counters)
    }

    /// What the node last said, for a display on another thread.
    pub fn progress(&self) -> Arc<NodeProgress> {
        Arc::clone(&self.progress)
    }

    /// Why mining has stopped for good (the backend failed), if it has.
    pub fn failure(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    fn log(&self, line: &str) {
        (self.cfg.log)(line);
    }

    fn event(&self, e: MinerEvent) {
        (self.cfg.events)(e);
    }

    /// Drops the job in hand (the connection is gone, or the node is not ready).
    pub fn drop_job(&mut self) {
        self.miner.cancel();
        self.current = None;
    }

    fn on_solution(&mut self, sol: Solution, node: &RemoteNode) -> Result<(), String> {
        let Some(cur) = self.current.take() else {
            self.stats.bad_solutions += 1;
            return Ok(());
        };
        if cur.job_id != sol.job_id {
            // for a job that has been replaced: late, not wrong
            self.current = Some(cur);
            return Ok(());
        }
        let mut block = cur.block;
        block.header.nonce = sol.nonce;
        block.header.mix = sol.mix;
        let id = ids::block_id(&block.header, self.pow);
        if !meets_target(&id, &cur.target) {
            self.stats.bad_solutions += 1;
            self.log(&format!(
                "the backend returned nonce {} but the id does not meet the target: discarded",
                sol.nonce
            ));
            return Ok(());
        }
        let height = block.coinbase.height;
        let reward = block_reward(&block);
        let secs = cur.started.elapsed().as_secs_f64();
        self.stats.blocks_found += 1;
        self.last_found = Some(Instant::now());
        self.log(&format!(
            "found a block at height {height} (nonce {}), {:.1} s after starting it",
            sol.nonce,
            cur.started.elapsed().as_secs_f64()
        ));
        let mut verdict = node.submit_block(block.clone())?;
        // a block the node would not take because it was asked too often is NOT an invalid block: it is handed in again
        for _ in 0..5 {
            match &verdict {
                BlockVerdict::Refused(why) if is_slow_down(why) => {
                    self.stats.slowed_down += 1;
                    std::thread::sleep(self.cfg.slow_down_pause);
                    verdict = node.submit_block(block.clone())?;
                }
                _ => break,
            }
        }
        match verdict {
            BlockVerdict::InChain(_) => {
                self.stats.blocks_accepted += 1;
                self.log(&format!("block {height} is in the chain"));
                self.event(MinerEvent::InChain {
                    height,
                    secs,
                    reward,
                    work: tenero_miner::rate::work_of(&cur.target),
                });
            }
            BlockVerdict::LostRace(_) => {
                self.stats.blocks_lost_race += 1;
                self.log(&format!(
                    "block {height} lost a race: another block took its place and ours is on a side branch"
                ));
                self.event(MinerEvent::LostRace { height });
            }
            BlockVerdict::Refused(why) => {
                self.stats.blocks_refused += 1;
                self.log(&format!(
                    "block {height} was REFUSED by the node: {why} (the block or its proof of work is wrong)"
                ));
                self.event(MinerEvent::Refused { height });
            }
        }
        Ok(())
    }

    /// One look: what the thread has to say, what the node is doing, and a new job if one is needed. `Err` means the
    /// connection to the node failed (the caller reconnects).
    pub fn step(&mut self, node: &RemoteNode) -> Result<(), String> {
        while let Some(msg) = self.miner.try_msg() {
            match msg {
                Msg::Ready(name) => {
                    self.log(&format!("mining with {name}"));
                    self.event(MinerEvent::Started { backend: name });
                }
                Msg::Solved(sol) => self.on_solution(sol, node)?,
                Msg::Finished(id) => {
                    // the thread is idle: if that was our job, it needs another
                    if self.current.as_ref().is_some_and(|c| c.job_id == id) {
                        self.current = None;
                    }
                }
                Msg::Failed(e) => {
                    self.log(&format!(
                        "the mining backend failed and mining has stopped: {e}"
                    ));
                    self.event(MinerEvent::BackendFailed { why: e.clone() });
                    self.failed = Some(e);
                }
            }
        }
        if self.failed.is_some() {
            return Ok(());
        }
        let info = node.info()?;
        self.progress.height.store(info.height, Ordering::Relaxed);
        self.progress.syncing.store(info.syncing, Ordering::Relaxed);
        self.progress.connected.store(true, Ordering::Relaxed);
        if info.syncing != self.paused {
            self.paused = info.syncing;
            self.event(if info.syncing {
                MinerEvent::Paused
            } else {
                MinerEvent::Resumed
            });
        }
        if info.syncing {
            // a node that is catching up has no tip worth building on
            if self.current.is_some() {
                self.stats.paused_syncing += 1;
                self.log("the node is syncing: mining paused");
            }
            self.drop_job();
            return Ok(());
        }
        if self
            .last_found
            .is_some_and(|t| t.elapsed() < self.cfg.min_block_interval)
        {
            return Ok(()); // pacing: not yet
        }
        let stale = match &self.current {
            None => true,
            Some(c) => c.tip != info.tip_id || c.started.elapsed() >= self.cfg.refresh_every,
        };
        if !stale {
            return Ok(());
        }
        self.miner.cancel();
        let height = info.height + 1;
        let payout_for_height = self.payout.payout(height);
        let t = node.block_template(payout_for_height.clone(), self.cfg.max_body_bytes)?;
        self.stats.templates += 1;
        if t.height != height
            || t.block.coinbase.height != height
            || t.block.header.prev_id != info.tip_id
        {
            // the tip moved between our two questions: the reward would not be readable by the wallet (its key
            // exchange binds the height), so this template is not used
            self.stats.stale_templates += 1;
            self.current = None;
            return Ok(());
        }
        // the node may be on another computer: trust nothing it built
        let now = (self.cfg.now_secs)();
        if let Err(why) = check_template(&t, height, &info.tip_id, &payout_for_height, now) {
            self.stats.refused_templates += 1;
            self.current = None;
            // said at most once in 30 seconds: a node that keeps this up must not fill the log
            if self
                .last_refusal_log
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(30))
            {
                self.last_refusal_log = Some(Instant::now());
                self.log(&format!(
                    "the node's template was REFUSED: {why}. This miner will not mine it (is the node honest?)"
                ));
            }
            return Ok(());
        }
        let id = self.next_id;
        self.next_id += 1;
        let tip = t.block.header.prev_id;
        self.miner.submit(Job {
            id,
            header: t.block.header.clone(),
            height,
            target: U256::from_be_bytes(&t.target),
            stale: Arc::new(AtomicBool::new(false)),
            nonce_start: None,
        });
        self.current = Some(Current {
            job_id: id,
            block: t.block,
            tip,
            target: U256::from_be_bytes(&t.target),
            started: Instant::now(),
        });
        Ok(())
    }

    /// Mines until `shutdown` is set or the backend fails, reconnecting to the node (with `connect`) whenever the
    /// connection is lost. `poll` is how often the node is asked for its tip.
    pub fn run(
        &mut self,
        mut connect: impl FnMut() -> Result<RemoteNode, String>,
        shutdown: &AtomicBool,
        poll: Duration,
    ) -> Result<(), String> {
        let mut node: Option<RemoteNode> = None;
        let mut backoff = Duration::from_millis(500);
        while !shutdown.load(Ordering::SeqCst) {
            if let Some(why) = self.failed.clone() {
                self.drop_job();
                return Err(format!("the mining backend failed: {why}"));
            }
            if node.is_none() {
                match connect() {
                    Ok(n) => {
                        self.log("connected to the node");
                        self.event(MinerEvent::NodeConnected);
                        self.reported_down = false;
                        node = Some(n);
                        backoff = Duration::from_millis(500);
                    }
                    Err(e) => {
                        self.log(&format!("cannot reach the node: {e} (trying again)"));
                        self.progress.connected.store(false, Ordering::Relaxed);
                        if !self.reported_down {
                            self.reported_down = true;
                            self.event(MinerEvent::NodeLost { why: e.clone() });
                        }
                        sleep_until(shutdown, backoff);
                        backoff = (backoff * 2).min(Duration::from_secs(10));
                        continue;
                    }
                }
            }
            if let Some(n) = &node {
                if let Err(e) = self.step(n) {
                    if is_slow_down(&e) {
                        // the node is not gone, it wants fewer requests: keep the connection and the job, wait, ask again
                        self.stats.slowed_down += 1;
                        if self
                            .last_slow_log
                            .is_none_or(|t| t.elapsed() >= Duration::from_secs(30))
                        {
                            self.last_slow_log = Some(Instant::now());
                            self.log(&format!(
                                "the node asked this miner to slow down ({e}): waiting {} s",
                                self.cfg.slow_down_pause.as_secs_f32()
                            ));
                        }
                        sleep_until(shutdown, self.cfg.slow_down_pause);
                        continue;
                    }
                    self.stats.connections_lost += 1;
                    self.log(&format!("lost the node: {e}"));
                    self.reported_down = true;
                    self.progress.connected.store(false, Ordering::Relaxed);
                    self.event(MinerEvent::NodeLost { why: e.clone() });
                    self.drop_job();
                    node = None;
                    continue;
                }
            }
            sleep_until(shutdown, poll);
        }
        self.drop_job();
        Ok(())
    }

    /// One line for the operator: speed since `since`, and what has become of the blocks.
    pub fn status_line(&self, attempts_before: u64, since: Duration) -> String {
        let attempts = self.miner.counters.attempts.load(Ordering::Relaxed);
        let rate = attempts.saturating_sub(attempts_before) as f64 / since.as_secs_f64().max(0.001);
        let s = &self.stats;
        format!(
            "status: {rate:.0} attempts/s | templates {} | found {} (in chain {}, lost a race {}, refused {}) | paused while syncing {} | connections lost {}",
            s.templates,
            s.blocks_found,
            s.blocks_accepted,
            s.blocks_lost_race,
            s.blocks_refused,
            s.paused_syncing,
            s.connections_lost
        )
    }
}

/// Sleeps for `d`, but wakes early if `shutdown` is set.
fn sleep_until(shutdown: &AtomicBool, d: Duration) {
    let end = Instant::now() + d;
    while Instant::now() < end && !shutdown.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(10).min(d));
    }
}

/// Connects to the node's control interface with the cookie from its data directory.
pub fn connect_to(data: &std::path::Path, control: SocketAddr) -> Result<RemoteNode, String> {
    let cookie = crate::client::read_cookie(&data.join(crate::client::COOKIE_FILE))?;
    RemoteNode::connect(control, &cookie)
}
