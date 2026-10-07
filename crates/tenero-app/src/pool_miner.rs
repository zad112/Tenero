//! A miner that works for a pool (`docs/POOL_PROTOCOL.md`): connects to it over the encrypted channel, says who to pay, searches the headers it is
//! given in its own slice of the nonces and hands in every share it finds. **Experimental and unaudited; nothing on any network it mines has value.**
//!
//! **The reward of a block found this way goes to the POOL**, which pays this miner by its own rules and on its own schedule. Nothing in the protocol
//! or the chain makes it. This program says so every time it connects, and a person who wants the reward for themselves mines on their own node
//! (`tenero-miner --data` or `--node`), where the miner checks that the block pays its own address. A miner never moves from one mode to the other by
//! itself.
//!
//! What it checks of the pool (it cannot check much: it never sees a block's body): that the job's header has this network's version and a clock within
//! an hour of this computer's, that the heights do not go back, that the share target and the nonce prefix make sense; and, before it hands a share in,
//! that the share really meets the share target (a backend that returns a nonce that does not is a bug, not sent).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tenero_core::u256::U256;
use tenero_core::v2::ids::{self, PowKind};
use tenero_core::v2::{BlockHeader, VERSION};
use tenero_miner::{meets_target, Counters, EventSink, Job as MineJob, Miner, MinerEvent, Msg};

use crate::pool::{Hello, HelloOk, Job, MinerMessage, PoolMessage};
use crate::pool_core::{first_nonce_of, work_of};
use crate::pool_net::{self, read_message, write_message, ReadHalf, WriteHalf};

/// How far a job's timestamp may be from this computer's clock before the miner refuses it.
pub const JOB_CLOCK_SLACK_SECS: u64 = 3600;

pub type Logger = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone)]
pub struct PoolMinerConfig {
    /// The network's name the pool must serve (`beta`).
    pub network: String,
    /// The address the pool is to pay.
    pub address: String,
    /// A name for this machine, shown to the pool (at most 32 characters).
    pub worker: String,
    pub log: Logger,
    pub events: EventSink,
    pub now_secs: Arc<dyn Fn() -> u64 + Send + Sync>,
    pub ping_every: Duration,
}

fn system_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl PoolMinerConfig {
    pub fn new(network: &str, address: &str, worker: &str) -> PoolMinerConfig {
        PoolMinerConfig {
            network: network.to_string(),
            address: address.to_string(),
            worker: worker.to_string(),
            log: Arc::new(|_| {}),
            events: Arc::new(|_| {}),
            now_secs: Arc::new(system_now),
            ping_every: Duration::from_secs(30),
        }
    }
}

/// What the miner has done, for the screen and for tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolMinerStats {
    pub jobs: u64,
    /// A job refused (a clock far off, a height going back, a header from the wrong rules).
    pub jobs_refused: u64,
    pub shares_sent: u64,
    pub shares_accepted: u64,
    /// Shares the pool refused: (stale, duplicate, above the target, wrong mix, unknown job, not allowed) are the reasons 1 to 6.
    pub shares_stale: u64,
    pub shares_rejected: u64,
    /// A solution a backend returned that does not meet the share target: not sent.
    pub bad_solutions: u64,
    pub connections: u64,
    pub connections_lost: u64,
}

/// What the pool last said, readable from another thread.
#[derive(Default)]
pub struct PoolProgress {
    pub connected: AtomicBool,
    pub height: std::sync::atomic::AtomicU64,
}

struct Current {
    pool_job: u64,
    local_id: u64,
    header: BlockHeader,
    height: u64,
    next_nonce: u64,
}

/// What a connection to a pool has agreed.
struct Session {
    hello: HelloOk,
    share_target: U256,
}

pub struct PoolMiner {
    miner: Miner,
    pow: PowKind,
    cfg: PoolMinerConfig,
    next_local: u64,
    last_height: u64,
    pub stats: PoolMinerStats,
    progress: Arc<PoolProgress>,
    failed: Option<String>,
    told_reward_goes_to_pool: bool,
}

impl PoolMiner {
    pub fn new(miner: Miner, pow: PowKind, cfg: PoolMinerConfig) -> PoolMiner {
        PoolMiner {
            miner,
            pow,
            cfg,
            next_local: 1,
            last_height: 0,
            stats: PoolMinerStats::default(),
            progress: Arc::new(PoolProgress::default()),
            failed: None,
            told_reward_goes_to_pool: false,
        }
    }

    pub fn counters(&self) -> Arc<Counters> {
        Arc::clone(&self.miner.counters)
    }

    pub fn progress(&self) -> Arc<PoolProgress> {
        Arc::clone(&self.progress)
    }

    fn log(&self, line: &str) {
        (self.cfg.log)(line);
    }

    fn event(&self, e: MinerEvent) {
        (self.cfg.events)(e);
    }

    /// Mines for the pool that `connect` reaches until `shutdown` is set or the backend fails, reconnecting when the connection is lost.
    pub fn run(
        &mut self,
        mut connect: impl FnMut() -> Result<(ReadHalf, WriteHalf), String>,
        shutdown: &AtomicBool,
    ) -> Result<(), String> {
        let mut backoff = Duration::from_millis(500);
        let mut reported_down = false;
        while !shutdown.load(Ordering::SeqCst) {
            if let Some(why) = self.failed.clone() {
                self.miner.cancel();
                return Err(format!("the mining backend failed: {why}"));
            }
            match connect() {
                Ok((r, w)) => {
                    self.stats.connections += 1;
                    backoff = Duration::from_millis(500);
                    reported_down = false;
                    match self.session(r, w, shutdown) {
                        Ok(()) => {}
                        Err(e) => {
                            self.stats.connections_lost += 1;
                            self.log(&format!("lost the pool: {e}"));
                            self.event(MinerEvent::PoolLost { why: e });
                        }
                    }
                    self.progress.connected.store(false, Ordering::Relaxed);
                    self.miner.cancel();
                }
                Err(e) => {
                    self.log(&format!("cannot reach the pool: {e} (trying again)"));
                    self.progress.connected.store(false, Ordering::Relaxed);
                    if !reported_down {
                        reported_down = true;
                        self.event(MinerEvent::PoolLost { why: e });
                    }
                }
            }
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            sleep_until(shutdown, backoff);
            backoff = (backoff * 2).min(Duration::from_secs(15));
        }
        self.miner.cancel();
        Ok(())
    }

    fn hello(&self) -> MinerMessage {
        MinerMessage::Hello(Hello {
            min_version: 1,
            max_version: 1,
            capabilities: 0,
            network: self.cfg.network.clone(),
            address: self.cfg.address.clone(),
            worker: self.cfg.worker.chars().take(32).collect(),
            agent: format!("tenero-miner {}", crate::daemon::VERSION),
        })
    }

    fn session(
        &mut self,
        mut r: ReadHalf,
        mut w: WriteHalf,
        shutdown: &AtomicBool,
    ) -> Result<(), String> {
        let send = |w: &mut WriteHalf, m: &MinerMessage| -> Result<(), String> {
            let body = m.to_body().map_err(|e| e.to_string())?;
            write_message(w, &body).map_err(|e| e.to_string())
        };
        send(&mut w, &self.hello())?;
        // the answer to the hello: a hello_ok, or an error that says why not
        r.set_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        let body = read_message(&mut r).map_err(|e| format!("no answer to the hello: {e}"))?;
        let ok = match PoolMessage::from_body(&body).map_err(|e| e.to_string())? {
            PoolMessage::HelloOk(ok) => ok,
            PoolMessage::Error(why) => return Err(format!("the pool refused this miner: {why}")),
            other => return Err(format!("the pool answered the hello with {other:?}")),
        };
        if ok.version != 1 {
            return Err(format!(
                "the pool chose version {}, which this miner does not speak",
                ok.version
            ));
        }
        if ok.prefix_bits > 32 || (ok.prefix_bits < 64 && ok.prefix >> ok.prefix_bits != 0) {
            return Err("the pool gave a nonce prefix that does not fit its bits".into());
        }
        let share_target = U256::from_be_bytes(&ok.share_target);
        self.progress.connected.store(true, Ordering::Relaxed);
        self.event(MinerEvent::PoolConnected {
            name: ok.pool_name.clone(),
        });
        self.log(&format!(
            "connected to the pool \"{}\" (your slice of the nonces: prefix {} of {} bits)",
            ok.pool_name, ok.prefix, ok.prefix_bits
        ));
        if !self.told_reward_goes_to_pool {
            self.told_reward_goes_to_pool = true;
            self.log("NOTE: the block rewards go to the POOL, which pays you by its own rules; nothing makes it pay. Mine on your own node to keep the rewards.");
        }
        let mut sess = Session {
            hello: ok,
            share_target,
        };

        // a thread that turns what the pool says into messages for the loop below
        r.set_timeout(Some(Duration::from_secs(300)))
            .map_err(|e| e.to_string())?;
        let (tx, rx): (_, Receiver<Result<PoolMessage, String>>) = channel();
        let reader = thread::spawn(move || loop {
            let m = match read_message(&mut r) {
                Ok(b) => PoolMessage::from_body(&b).map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            let stop = m.is_err();
            if tx.send(m).is_err() || stop {
                return;
            }
        });
        let result = self.pool_loop(&mut sess, &mut w, &rx, shutdown, &send);
        w.shutdown();
        let _ = reader.join();
        result
    }

    fn pool_loop(
        &mut self,
        sess: &mut Session,
        w: &mut WriteHalf,
        rx: &Receiver<Result<PoolMessage, String>>,
        shutdown: &AtomicBool,
        send: &dyn Fn(&mut WriteHalf, &MinerMessage) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut current: Option<Current> = None;
        let mut last_ping = Instant::now();
        let mut token = 0u64;
        let mut rejected_logged = 0u32;
        loop {
            if shutdown.load(Ordering::SeqCst) {
                return Ok(());
            }
            // what the backend thread has found
            while let Some(msg) = self.miner.try_msg() {
                match msg {
                    Msg::Ready(name) => {
                        self.log(&format!("mining with {name}"));
                        self.event(MinerEvent::Started { backend: name });
                    }
                    Msg::Solved(sol) => {
                        if let Some(cur) = current.as_mut() {
                            if cur.local_id == sol.job_id {
                                self.on_solution(sess, cur, sol, w, send)?;
                            }
                        }
                    }
                    // the thread is idle: the job it had is over (a share was found, or it was replaced): nothing to do, the next job is already given
                    Msg::Finished(_) => {}
                    Msg::Failed(e) => {
                        self.log(&format!(
                            "the mining backend failed and mining has stopped: {e}"
                        ));
                        self.event(MinerEvent::BackendFailed { why: e.clone() });
                        self.failed = Some(e.clone());
                        return Err(format!("the mining backend failed: {e}"));
                    }
                }
            }
            if last_ping.elapsed() >= self.cfg.ping_every {
                last_ping = Instant::now();
                token += 1;
                send(w, &MinerMessage::Ping { token })?;
            }
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Ok(m)) => match m {
                    PoolMessage::Job(j) => {
                        if let Some(c) = self.on_job(sess, j) {
                            current = Some(c);
                        }
                    }
                    PoolMessage::SetShareTarget { share_target } => {
                        sess.share_target = U256::from_be_bytes(&share_target);
                        self.log("the pool changed this miner's share target");
                        // carry on with the same header and the new target, from the next nonce
                        if let Some(cur) = current.as_mut() {
                            self.start(sess, cur);
                        }
                    }
                    PoolMessage::ShareResult {
                        accepted,
                        reason,
                        text,
                        ..
                    } => {
                        if accepted {
                            self.stats.shares_accepted += 1;
                            self.event(MinerEvent::ShareAccepted {
                                work: work_of(&sess.share_target) as f64,
                            });
                        } else {
                            match reason {
                                1 => self.stats.shares_stale += 1,
                                _ => self.stats.shares_rejected += 1,
                            }
                            self.event(MinerEvent::ShareRejected { reason });
                            if reason != 1 && rejected_logged < 10 {
                                rejected_logged += 1;
                                self.log(&format!("a share was refused (reason {reason}): {text}"));
                            }
                        }
                    }
                    PoolMessage::Pong { .. } => {}
                    PoolMessage::Error(why) => return Err(format!("the pool said: {why}")),
                    // a pool that sends what only job declaration uses is not speaking version 1 as agreed
                    other => return Err(format!("the pool sent something unexpected: {other:?}")),
                },
                Ok(Err(e)) => return Err(e),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("the connection to the pool closed".into())
                }
            }
        }
    }

    /// Checks a job and, if it is good, starts mining it.
    fn on_job(&mut self, sess: &mut Session, j: Job) -> Option<Current> {
        let now = (self.cfg.now_secs)();
        let why = if j.header.version != VERSION {
            Some("the header's version is not this network's")
        } else if j.header.timestamp.abs_diff(now) > JOB_CLOCK_SLACK_SECS {
            Some("the header's timestamp is far from this computer's clock")
        } else if j.height < self.last_height && !j.clean {
            Some("the height went back")
        } else if j.block_target == [0u8; 32] {
            Some("the block target is zero")
        } else {
            None
        };
        if let Some(why) = why {
            self.stats.jobs_refused += 1;
            self.log(&format!("a job from the pool was REFUSED: {why}. This miner will not work on it (is this the right pool and network?)"));
            return None;
        }
        self.stats.jobs += 1;
        self.last_height = j.height;
        self.progress.height.store(j.height, Ordering::Relaxed);
        if j.clean {
            self.miner.cancel();
        }
        let id = self.next_local;
        self.next_local += 1;
        let mut cur = Current {
            pool_job: j.job_id,
            local_id: id,
            header: j.header,
            height: j.height,
            next_nonce: first_nonce_of(sess.hello.prefix, sess.hello.prefix_bits),
        };
        self.start(sess, &mut cur);
        Some(cur)
    }

    /// Gives the backend the job in `cur` at the current share target, from `cur.next_nonce`.
    fn start(&mut self, sess: &Session, cur: &mut Current) {
        let id = self.next_local;
        self.next_local += 1;
        cur.local_id = id;
        self.miner.submit(MineJob {
            id,
            header: cur.header.clone(),
            height: cur.height,
            target: sess.share_target,
            stale: Arc::new(AtomicBool::new(false)),
            nonce_start: Some(cur.next_nonce),
        });
    }

    fn on_solution(
        &mut self,
        sess: &Session,
        cur: &mut Current,
        sol: tenero_miner::Solution,
        w: &mut WriteHalf,
        send: &dyn Fn(&mut WriteHalf, &MinerMessage) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut header = cur.header.clone();
        header.nonce = sol.nonce;
        header.mix = sol.mix;
        let id = ids::block_id(&header, self.pow);
        // the next nonce first, so that the same one is never found twice
        cur.next_nonce = sol.nonce.wrapping_add(1);
        if !meets_target(&id, &sess.share_target) {
            self.stats.bad_solutions += 1;
            self.log(&format!(
                "the backend returned nonce {} but the id does not meet the share target: not sent",
                sol.nonce
            ));
        } else {
            send(
                w,
                &MinerMessage::SubmitShare {
                    job_id: cur.pool_job,
                    nonce: sol.nonce,
                    mix: sol.mix,
                },
            )?;
            self.stats.shares_sent += 1;
        }
        // carry on with the same header from the next nonce
        self.start(sess, cur);
        Ok(())
    }

    /// One line for the log.
    pub fn status_line(&self) -> String {
        let s = &self.stats;
        format!(
            "status: pool | jobs {} | shares sent {} (accepted {}, stale {}, refused {}) | connections {} (lost {})",
            s.jobs, s.shares_sent, s.shares_accepted, s.shares_stale, s.shares_rejected, s.connections, s.connections_lost
        )
    }
}

/// The pool built into the program for a network, if there is one: its address and the key a miner pins. A pool is added here only when someone runs
/// it (and a release carries it), as for the seeds. Used by `tenero-miner --pool default` and by the app when its pool field is empty.
pub fn default_pool(network: crate::config::Network) -> Option<(&'static str, [u8; 32])> {
    DEFAULT_POOLS
        .iter()
        .find(|(n, _, _)| *n == network.name())
        .map(|(_, addr, key)| (*addr, *key))
}

/// `(network, address, public key)`: **one**, the author's test pool on the beta network (a rented server, 2026-10-07; `docs/RUNNING_A_POOL.md`). It is one computer run by one person, the
/// pool keeps the block rewards and pays by its own rules, and nothing on the network has any value. The key is the one the pool printed at its first start (`tenero-pool key`); a miner that
/// uses this entry pins it, so a person who sits between the miner and the pool is refused. **A change of this list needs a new release**, as the seeds' does.
pub const DEFAULT_POOLS: &[(&str, &str, [u8; 32])] = &[(
    "beta",
    "195.26.244.245:38335",
    [
        0x02, 0x7d, 0x64, 0x2d, 0xea, 0x40, 0x30, 0x70, 0x45, 0x0c, 0x70, 0xb0, 0xbd, 0x4b, 0x59,
        0xf0, 0x1b, 0x66, 0xb6, 0x78, 0x2b, 0x99, 0x36, 0x6e, 0x27, 0x69, 0xa1, 0x9c, 0xff, 0x35,
        0xb2, 0x59,
    ],
)];

/// Dials a pool: its address and, if the miner was given one, its public key (pinned).
pub fn connect(
    addr: std::net::SocketAddr,
    pinned: Option<[u8; 32]>,
) -> Result<(ReadHalf, WriteHalf), String> {
    pool_net::connect(addr, pinned.as_ref())
}

fn sleep_until(shutdown: &AtomicBool, d: Duration) {
    let end = Instant::now() + d;
    while Instant::now() < end && !shutdown.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(10).min(d));
    }
}
