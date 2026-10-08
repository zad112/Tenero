//! The pool: serves jobs to miners over the pool protocol (`docs/POOL_PROTOCOL.md`), checks every share, keeps the PPLNS accounts
//! (`pool_core.rs`) and pays the miners from the pool's own wallet. **Experimental and unaudited; nothing on any network it serves has
//! value.**
//!
//! **What it is made of.**
//!
//! * A *node* of its own (`PoolNode`; in the program, a `tenerod` on this machine reached through the control interface), which builds
//!   the blocks the pool's miners search for, and takes a block once a share is one. The pool never trusts a miner for anything but a
//!   header's nonce and mix: the pool recomputes the proof of work of **every** share.
//! * A *session* for each connected miner: its address, its slice of the nonces, its difficulty. The rules of a share are in
//!   [`Pool::on_share`] and nowhere else, and it works on a `Session` with no socket, so every rule has a test that does not need one.
//! * The *job manager* ([`Pool::step_jobs`]): asks the node for its tip, makes a job when the tip moves (a *clean* job: every earlier one
//!   is dead) or when the last is old (a fresh one with the newest transactions), and hands it to every session.
//! * The *payout* ([`Pool::payout_round`]): once an interval, the miners owed at least the minimum are paid in as many transactions as it
//!   takes (`Wallet::build_batch`), and what was paid is taken off what is owed. **The pool pays the network's fees.**
//!
//! **What it does not do:** job declaration (the miner's own block building: the pool says so in its `hello_ok`), and it cannot tell who a
//! miner is (the address in `hello` is only where the pool pays; anyone may use anyone's address).
//!
//! **What a hostile miner can cost it** (limits below, `docs/THREAT_MODEL.md`): connections (a cap on miners, on connections from one
//! address, a handshake and an idle timeout), shares (a rate limit for each connection, and a ban of an address that sends many bad ones
//! in a row), and frames (nothing larger than the protocol's 4 MiB is read; a miner may send only `hello`, `submit_share` and `ping`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rand_core::OsRng;
use tenero_chain::PowCheck;
use tenero_core::u256::U256;
use tenero_core::v3::ids;
use tenero_core::v3::BlockHeader;
use tenero_net::noise::NodeKey;
use tenero_wallet::{Address, ChainView, FeeLevel, Submitter, Wallet};

use crate::client::{BlockVerdict, RemoteNode};
use crate::control::Template;
use crate::pool::{
    Hello, HelloOk, Job, MinerMessage, PoolMessage, HEADER_LEN, MAX_ADDRESS, MAX_AGENT, MAX_WORKER,
};
use crate::pool_core::{
    first_nonce_of, nonce_in_prefix, share_target, work_of, AccountError, Accounts, AddrId,
    Prefixes, VarDiff,
};
use crate::pool_net::{self, read_message, write_message};
use crate::remote_miner::check_template;

pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

/// The reasons of a `share_result` (`docs/POOL_PROTOCOL.md`).
pub const R_ACCEPTED: u8 = 0;
pub const R_STALE: u8 = 1;
pub const R_DUPLICATE: u8 = 2;
pub const R_LOW: u8 = 3;
pub const R_BAD_MIX: u8 = 4;
pub const R_UNKNOWN_JOB: u8 = 5;
pub const R_NOT_ALLOWED: u8 = 6;

// ---- the node the pool is built on -------------------------------------------------------------------------------------------

/// What the pool needs from its node.
pub trait PoolNode: Send + Sync {
    /// The height and id of the tip, and whether the node is still catching up (a node that is must not be mined on).
    fn tip(&self) -> Result<(u64, [u8; 32], bool), String>;
    /// A block to search for, its reward paying the main address `to`.
    fn template(&self, to: &Address, max_weight: u64) -> Result<Template, String>;
    /// A block found on one of the node's templates, handed back as its header (the node keeps the body).
    fn submit_header(&self, header: BlockHeader) -> Result<BlockVerdict, String>;
    /// The id of the block the node's chain has at `height`.
    fn block_id_at(&self, height: u64) -> Result<Option<[u8; 32]>, String>;
    /// The full proof-of-work check of a header at a height: is the mix right? The node does it with the dataset it already holds, so the pool needs none.
    fn check_pow(&self, header: &BlockHeader, height: u64) -> Result<bool, String>;
}

impl PoolNode for RemoteNode {
    fn tip(&self) -> Result<(u64, [u8; 32], bool), String> {
        let i = self.info()?;
        Ok((i.height, i.tip_id, i.syncing))
    }
    fn template(&self, to: &Address, max_weight: u64) -> Result<Template, String> {
        self.block_template(to, max_weight)
    }
    fn submit_header(&self, header: BlockHeader) -> Result<BlockVerdict, String> {
        RemoteNode::submit_header(self, header)
    }
    fn block_id_at(&self, height: u64) -> Result<Option<[u8; 32]>, String> {
        Ok(ChainView::block(self, height)?.map(|b| b.id))
    }
    fn check_pow(&self, header: &BlockHeader, height: u64) -> Result<bool, String> {
        RemoteNode::check_pow(self, height, header)
    }
}

/// A node reached through the control interface that is connected again when the connection is lost (the node restarted, or was busy too long).
pub struct ReconnectingNode {
    connect: Box<dyn Fn() -> Result<RemoteNode, String> + Send + Sync>,
    cur: Mutex<Option<RemoteNode>>,
}

impl ReconnectingNode {
    pub fn new(
        connect: Box<dyn Fn() -> Result<RemoteNode, String> + Send + Sync>,
    ) -> ReconnectingNode {
        ReconnectingNode {
            connect,
            cur: Mutex::new(None),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&RemoteNode) -> Result<R, String>) -> Result<R, String> {
        let mut cur = self.cur.lock().map_err(|_| "poisoned".to_string())?;
        if cur.is_none() {
            *cur = Some((self.connect)()?);
        }
        let r = f(cur.as_ref().expect("just made"));
        // a connection that failed is not used again: the next call makes a new one (an answer of "no" from the node is not a failure of the connection)
        if r.as_ref()
            .err()
            .is_some_and(|e| e.starts_with("lost the node"))
        {
            *cur = None;
        }
        r
    }
}

impl PoolNode for ReconnectingNode {
    fn tip(&self) -> Result<(u64, [u8; 32], bool), String> {
        self.with(PoolNode::tip)
    }
    fn template(&self, to: &Address, max_weight: u64) -> Result<Template, String> {
        self.with(|n| PoolNode::template(n, to, max_weight))
    }
    fn submit_header(&self, header: BlockHeader) -> Result<BlockVerdict, String> {
        self.with(|n| PoolNode::submit_header(n, header.clone()))
    }
    fn block_id_at(&self, height: u64) -> Result<Option<[u8; 32]>, String> {
        self.with(|n| PoolNode::block_id_at(n, height))
    }
    fn check_pow(&self, header: &BlockHeader, height: u64) -> Result<bool, String> {
        self.with(|n| PoolNode::check_pow(n, header, height))
    }
}

/// The proof-of-work check of a pool that holds no dataset: the cheap check (the id against a target) is made here, and the full one (is the mix right) is
/// asked of the pool's node. **This is what lets a pool, its node and a seed share one machine**: a dataset is 4 GiB, and the pool would otherwise have one
/// of its own beside its node's.
pub struct NodePow {
    node: Arc<dyn PoolNode>,
    kind: tenero_core::v2::ids::PowKind,
}

impl NodePow {
    pub fn new(node: Arc<dyn PoolNode>, kind: tenero_core::v2::ids::PowKind) -> NodePow {
        NodePow { node, kind }
    }
}

impl PowCheck for NodePow {
    fn kind(&self) -> tenero_core::v2::ids::PowKind {
        self.kind
    }

    fn check_full(&self, header: &BlockHeader, height: u64) -> Result<bool, String> {
        self.node.check_pow(header, height)
    }
}

// ---- configuration ---------------------------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct PoolConfig {
    /// The network's name (`beta`): a miner that says another is refused.
    pub network: String,
    /// What the pool calls itself in `hello_ok`.
    pub name: String,
    /// The pool wallet's address: every block the pool builds pays it.
    pub pool_address: tenero_wallet::Address,
    pub max_miners: usize,
    /// Connections from one address at once.
    pub per_address: usize,
    pub handshake_timeout: Duration,
    /// A connection that sends nothing for this long is closed (a miner pings, and sends shares).
    pub idle_timeout: Duration,
    /// Where each miner's difficulty starts: its share target is this many times the block target.
    pub initial_ratio: u64,
    /// What the pool keeps of every block, in parts per million of the reward (10,000 is 1 %; `pool_core::parse_fee_percent`).
    /// **A number the pool publishes.**
    pub fee_ppm: u64,
    /// The PPLNS window, as a multiple of a block's work.
    pub window_factor: u64,
    /// How many blocks deep a block must be before its reward is credited (the coinbase maturity of the network).
    pub maturity: u64,
    /// The least a payout is, in units.
    pub min_payout: u64,
    /// Seconds between payouts.
    pub payout_interval: u64,
    /// The most miners paid in one round.
    pub max_payees: usize,
    /// Seconds a job is good for (in the job message).
    pub job_ttl: u32,
    /// A job for the same tip is replaced by a fresh one this often (new transactions, a later timestamp).
    pub refresh_every: Duration,
    /// The most transaction weight a job's block carries (the node gives at most `server::MAX_TEMPLATE_WEIGHT`).
    pub max_weight: u64,
    /// Shares one connection may send in a minute.
    pub shares_per_minute: usize,
    /// Bad shares in a row (above the target, a wrong mix) before the address is banned.
    pub bad_shares_ban: u32,
    pub ban_secs: u64,
}

impl PoolConfig {
    /// The defaults of the first test pool (the owner's decisions of 2026-10-07: PPLNS, a payout every hour of at least 0.1 coins).
    pub fn new(network: &str, pool_address: tenero_wallet::Address, maturity: u64) -> PoolConfig {
        PoolConfig {
            network: network.to_string(),
            name: "Tenero test pool".to_string(),
            pool_address,
            max_miners: 256,
            per_address: 4,
            handshake_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(300),
            initial_ratio: 16,
            fee_ppm: 0,
            window_factor: 2,
            maturity,
            min_payout: 10_000_000,
            payout_interval: 3600,
            max_payees: 500,
            job_ttl: 120,
            refresh_every: Duration::from_secs(30),
            max_weight: u64::MAX,
            shares_per_minute: 600,
            bad_shares_ban: 20,
            ban_secs: 600,
        }
    }
}

// ---- state -------------------------------------------------------------------------------------------------------------------

/// What the pool has done, readable from any thread.
#[derive(Default)]
pub struct PoolStats {
    pub connections: AtomicU64,
    pub turned_away: AtomicU64,
    pub handshake_failed: AtomicU64,
    pub banned_refused: AtomicU64,
    pub hello_refused: AtomicU64,
    pub shares_accepted: AtomicU64,
    pub shares_stale: AtomicU64,
    pub shares_duplicate: AtomicU64,
    pub shares_low: AtomicU64,
    pub shares_bad_mix: AtomicU64,
    pub shares_unknown_job: AtomicU64,
    pub blocks_found: AtomicU64,
    pub blocks_in_chain: AtomicU64,
    pub blocks_lost_race: AtomicU64,
    pub blocks_refused: AtomicU64,
    pub jobs: AtomicU64,
    pub bans: AtomicU64,
    pub rate_limited: AtomicU64,
    pub payout_rounds: AtomicU64,
    pub payout_transactions: AtomicU64,
}

/// A job: a block the node built, paying the pool, for the miners to search.
pub struct JobRecord {
    pub id: u64,
    pub height: u64,
    pub tip: [u8; 32],
    /// What the block pays (its coinbase's outputs added up): the reward and the fees.
    pub reward: u64,
    pub header: BlockHeader,
    pub block_target: U256,
    created: Instant,
    /// The nonces already handed in for this job (a repeat is a duplicate).
    nonces: Mutex<HashSet<u64>>,
}

#[derive(Default)]
struct JobBook {
    tip: Option<(u64, [u8; 32])>,
    jobs: VecDeque<Arc<JobRecord>>,
    /// Ids of jobs that were dropped: a share for one is stale, not unknown.
    dead: VecDeque<u64>,
    next_id: u64,
}

const MAX_JOBS_PER_TIP: usize = 4;
const MAX_DEAD: usize = 256;

impl JobBook {
    fn get(&self, id: u64) -> Option<Arc<JobRecord>> {
        self.jobs.iter().find(|j| j.id == id).cloned()
    }
    fn is_dead(&self, id: u64) -> bool {
        self.dead.contains(&id)
    }
    fn kill(&mut self, id: u64) {
        self.dead.push_back(id);
        while self.dead.len() > MAX_DEAD {
            self.dead.pop_front();
        }
    }
    fn latest(&self) -> Option<Arc<JobRecord>> {
        self.jobs.back().cloned()
    }
}

/// One connected miner.
pub struct Session {
    pub id: u64,
    pub ip: IpAddr,
    pub address_id: AddrId,
    pub worker: String,
    pub prefix: u64,
    pub prefix_bits: u8,
    pub vardiff: VarDiff,
    /// The share target in force, and the one before it (shares that meet it are still taken for a minute after a change).
    pub target: U256,
    prev_target: Option<(U256, u64)>,
    block_target: U256,
    pub shares: u64,
    bad_in_row: u32,
    /// How many refused shares of this miner have been written in the log (only the first few are: a miner that sends junk must not fill it).
    refused_logged: u32,
    recent: VecDeque<u64>,
}

/// What became of a share.
#[derive(Debug, PartialEq, Eq)]
pub struct ShareOutcome {
    pub accepted: bool,
    pub reason: u8,
    pub text: String,
    /// The share was also a block, and the node said this about it.
    pub block: Option<BlockOutcome>,
    /// Close the connection (the miner is misbehaving) and say why.
    pub close: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum BlockOutcome {
    InChain,
    LostRace,
    Refused(String),
    /// The node could not be asked.
    NodeError(String),
}

enum SessionEvent {
    Msg(Result<MinerMessage, String>),
    Push(Arc<JobRecord>, bool),
    Closed(String),
}

pub struct Pool {
    pub cfg: PoolConfig,
    node: Arc<dyn PoolNode>,
    pow: Arc<dyn PowCheck>,
    key: NodeKey,
    accounts: Mutex<Accounts>,
    state_path: Option<PathBuf>,
    book: Mutex<JobBook>,
    prefixes: Mutex<Prefixes>,
    sessions: Mutex<HashMap<u64, SyncSender<SessionEvent>>>,
    per_ip: Mutex<HashMap<IpAddr, usize>>,
    bans: Mutex<HashMap<IpAddr, Instant>>,
    next_session: AtomicU64,
    stop: AtomicBool,
    dirty: AtomicBool,
    last_refresh: Mutex<Option<Instant>>,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
    log: Log,
    pub stats: PoolStats,
}

fn system_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Pool {
    /// `accounts` is what was kept in the state file (or empty). `now` is the clock in seconds (the system's by default).
    pub fn new(
        cfg: PoolConfig,
        node: Arc<dyn PoolNode>,
        pow: Arc<dyn PowCheck>,
        key: NodeKey,
        accounts: Accounts,
        state_path: Option<PathBuf>,
        log: Log,
    ) -> Arc<Pool> {
        let prefixes = Prefixes::for_miners(cfg.max_miners);
        let start_id = system_now() << 20;
        Arc::new(Pool {
            cfg,
            node,
            pow,
            key,
            accounts: Mutex::new(accounts),
            state_path,
            book: Mutex::new(JobBook {
                next_id: start_id,
                ..JobBook::default()
            }),
            prefixes: Mutex::new(prefixes),
            sessions: Mutex::new(HashMap::new()),
            per_ip: Mutex::new(HashMap::new()),
            bans: Mutex::new(HashMap::new()),
            next_session: AtomicU64::new(1),
            stop: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            last_refresh: Mutex::new(None),
            now: Arc::new(system_now),
            log,
            stats: PoolStats::default(),
        })
    }

    /// Replaces the clock (tests).
    pub fn with_clock(mut self: Arc<Self>, now: Arc<dyn Fn() -> u64 + Send + Sync>) -> Arc<Pool> {
        Arc::get_mut(&mut self).expect("not shared yet").now = now;
        self
    }

    fn log(&self, line: &str) {
        (self.log)(line);
    }

    fn now(&self) -> u64 {
        (self.now)()
    }

    /// The pool's public key: what a miner pins.
    pub fn public_key(&self) -> [u8; 32] {
        self.key.public()
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// A copy of the books (for a status line, for tests).
    pub fn accounts(&self) -> Accounts {
        self.accounts.lock().map(|a| a.clone()).unwrap_or_default()
    }

    pub fn with_accounts<R>(&self, f: impl FnOnce(&mut Accounts) -> R) -> R {
        let mut a = self.accounts.lock().expect("accounts lock");
        let r = f(&mut a);
        self.dirty.store(true, Ordering::SeqCst);
        r
    }

    pub fn sessions_now(&self) -> usize {
        self.sessions.lock().map(|s| s.len()).unwrap_or(0)
    }

    /// Writes the books to the state file if they changed (atomically: a crash leaves the old file or the new, never half).
    pub fn save_state(&self) {
        let Some(path) = &self.state_path else { return };
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        let bytes = match self.accounts.lock().map(|a| a.to_bytes()) {
            Ok(Ok(b)) => b,
            _ => {
                self.log("cannot encode the pool's state: not saved");
                self.dirty.store(true, Ordering::SeqCst);
                return;
            }
        };
        if let Err(e) = tenero_net::transport::write_atomic(path, &bytes) {
            self.log(&format!(
                "cannot save the pool's state to {}: {e}",
                path.display()
            ));
            self.dirty.store(true, Ordering::SeqCst);
        }
    }

    // ---- bans and limits ----

    fn is_banned(&self, ip: IpAddr) -> bool {
        let Ok(mut b) = self.bans.lock() else {
            return false;
        };
        match b.get(&ip) {
            Some(until) if Instant::now() < *until => true,
            Some(_) => {
                b.remove(&ip);
                false
            }
            None => false,
        }
    }

    fn ban(&self, ip: IpAddr) {
        self.stats.bans.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut b) = self.bans.lock() {
            if b.len() > 4096 {
                let now = Instant::now();
                b.retain(|_, until| now < *until);
            }
            b.insert(ip, Instant::now() + Duration::from_secs(self.cfg.ban_secs));
        }
        self.log(&format!(
            "banned {ip} for {} s: too many bad shares in a row",
            self.cfg.ban_secs
        ));
    }

    // ---- sessions ----

    /// Makes a session for a miner whose `hello` has been checked here: its address, a nonce prefix of its own, a share target. `Err` is what
    /// the pool says before it closes the connection.
    pub fn open_session(&self, hello: &Hello, ip: IpAddr) -> Result<(Session, HelloOk), String> {
        if hello.min_version > 1 || hello.max_version < 1 {
            return Err("this pool speaks version 1 of the pool protocol".into());
        }
        if hello.network != self.cfg.network {
            return Err(format!(
                "this pool serves the {} network, not {}",
                self.cfg.network, hello.network
            ));
        }
        if hello.address.len() > MAX_ADDRESS
            || hello.worker.len() > MAX_WORKER
            || hello.agent.len() > MAX_AGENT
        {
            return Err("a field of the hello is too long".into());
        }
        if hello.worker.chars().any(char::is_control) || hello.agent.chars().any(char::is_control) {
            return Err("the worker name has a control character".into());
        }
        let network = crate::config::Network::parse(&self.cfg.network)
            .ok_or("the pool's network is not one this program knows")?
            .wallet_network();
        let address = Address::parse(hello.address.trim(), network)
            .map_err(|e| format!("the payout address is not valid: {e}"))?;
        let address_id = {
            let mut a = self
                .accounts
                .lock()
                .map_err(|_| "the pool is broken".to_string())?;
            let id = a.id_of(&address).map_err(|e: AccountError| e.to_string())?;
            self.dirty.store(true, Ordering::SeqCst);
            id
        };
        let (prefix, bits) = {
            let mut p = self
                .prefixes
                .lock()
                .map_err(|_| "the pool is broken".to_string())?;
            match p.take() {
                Some(x) => (x, p.bits()),
                None => return Err("the pool is full".into()),
            }
        };
        let now = self.now();
        let block_target = self
            .book
            .lock()
            .ok()
            .and_then(|b| b.latest())
            .map(|j| j.block_target)
            .unwrap_or_else(|| U256::pow2(200).expect("fits"));
        let vardiff = VarDiff::new(self.cfg.initial_ratio, now);
        let target = share_target(&block_target, vardiff.ratio);
        let id = self.next_session.fetch_add(1, Ordering::SeqCst);
        let session = Session {
            id,
            ip,
            address_id,
            worker: hello.worker.clone(),
            prefix,
            prefix_bits: bits,
            vardiff,
            target,
            prev_target: None,
            block_target,
            shares: 0,
            bad_in_row: 0,
            refused_logged: 0,
            recent: VecDeque::new(),
        };
        let ok = HelloOk {
            version: 1,
            // no job declaration: capability bit 0 is not offered
            capabilities: 0,
            session: id,
            prefix_bits: bits,
            prefix,
            share_target: target.to_be_bytes(),
            pool_name: self.cfg.name.clone(),
            pays_pool: true,
        };
        self.log(&format!(
            "miner {} from {ip}: {} ({}), prefix {prefix}/{bits} bits, share target ratio {}",
            id,
            hello.address,
            if hello.worker.is_empty() {
                "no worker name"
            } else {
                &hello.worker
            },
            session.vardiff.ratio
        ));
        Ok((session, ok))
    }

    pub fn close_session(&self, s: &Session) {
        if let Ok(mut p) = self.prefixes.lock() {
            p.give_back(s.prefix);
        }
    }

    /// The share target a session should have against `block_target`, and whether that is a change (then the old one stays good for a minute).
    /// `Some(bytes)` is a `set_share_target` to send.
    pub fn retarget(&self, s: &mut Session, block_target: &U256) -> Option<[u8; 32]> {
        s.block_target = *block_target;
        let new = share_target(block_target, s.vardiff.ratio);
        if new == s.target {
            return None;
        }
        s.prev_target = Some((s.target, self.now() + 60));
        s.target = new;
        Some(new.to_be_bytes())
    }

    /// Looks at the miner's difficulty (call it every few seconds): `Some` is a new share target to send.
    pub fn vardiff_tick(&self, s: &mut Session) -> Option<[u8; 32]> {
        let ratio = s.vardiff.tick(self.now())?;
        let _ = ratio;
        let bt = s.block_target;
        self.retarget(s, &bt)
    }

    fn reject(&self, s: &mut Session, reason: u8, text: &str, bad: bool) -> ShareOutcome {
        let counter = match reason {
            R_STALE => &self.stats.shares_stale,
            R_DUPLICATE => &self.stats.shares_duplicate,
            R_LOW => &self.stats.shares_low,
            R_BAD_MIX => &self.stats.shares_bad_mix,
            _ => &self.stats.shares_unknown_job,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        // a stale share is routine (the tip moved); anything else is worth knowing about, for the first few
        if reason != R_STALE && s.refused_logged < 10 {
            s.refused_logged += 1;
            self.log(&format!(
                "miner {} ({}): a share was refused, reason {reason}: {text}",
                s.id, s.worker
            ));
        }
        let mut close = None;
        if bad {
            s.bad_in_row += 1;
            if s.bad_in_row >= self.cfg.bad_shares_ban {
                self.ban(s.ip);
                close = Some("too many bad shares in a row".to_string());
            }
        }
        ShareOutcome {
            accepted: false,
            reason: if close.is_some() {
                R_NOT_ALLOWED
            } else {
                reason
            },
            text: text.to_string(),
            block: None,
            close,
        }
    }

    /// A share: a nonce and a mix for a job. **The rules** (`docs/POOL_PROTOCOL.md`), in the order they are checked:
    /// the job must be alive (else stale, or unknown), the nonce must be in the miner's slice and not handed in before, the block id must be under
    /// the miner's share target (the one in force, or the one before it for a minute after a change), and the proof of work must really give the mix.
    /// An accepted share is worth the work of the target it met, and goes in the PPLNS window; if it is under the block's target too, it is a block, and
    /// goes to the node.
    pub fn on_share(
        &self,
        s: &mut Session,
        job_id: u64,
        nonce: u64,
        mix: [u8; 64],
    ) -> ShareOutcome {
        // a rate limit for the connection
        let now = self.now();
        s.recent.push_back(now);
        while s
            .recent
            .front()
            .is_some_and(|t| now.saturating_sub(*t) >= 60)
        {
            s.recent.pop_front();
        }
        if s.recent.len() > self.cfg.shares_per_minute {
            self.stats.rate_limited.fetch_add(1, Ordering::Relaxed);
            return ShareOutcome {
                accepted: false,
                reason: R_NOT_ALLOWED,
                text: "too many shares a minute".into(),
                block: None,
                close: Some("too many shares a minute".into()),
            };
        }
        let job = {
            let book = self.book.lock().expect("book lock");
            match book.get(job_id) {
                Some(j) => j,
                None if book.is_dead(job_id) => {
                    drop(book);
                    return self.reject(s, R_STALE, "the job is dead: the tip moved", false);
                }
                None => {
                    drop(book);
                    return self.reject(s, R_UNKNOWN_JOB, "no such job", true);
                }
            }
        };
        if !nonce_in_prefix(nonce, s.prefix, s.prefix_bits) {
            return self.reject(s, R_LOW, "the nonce is outside this miner's range", true);
        }
        let mut header = job.header.clone();
        header.nonce = nonce;
        header.mix = mix;
        let id = U256::from_be_bytes(&ids::block_id(&header, self.pow.kind()));
        let meets_now = id < s.target;
        let meets_before = s
            .prev_target
            .is_some_and(|(t, until)| now < until && id < t);
        if !meets_now && !meets_before {
            return self.reject(s, R_LOW, "above the share target", true);
        }
        // the nonce is only remembered once it has passed the cheap checks, so that junk cannot fill the set
        let first_time = job
            .nonces
            .lock()
            .map(|mut n| n.insert(nonce))
            .unwrap_or(false);
        if !first_time {
            return self.reject(s, R_DUPLICATE, "this nonce was handed in already", false);
        }
        match self.pow.check_full(&header, job.height) {
            Ok(true) => {}
            Ok(false) => {
                return self.reject(
                    s,
                    R_BAD_MIX,
                    "the mix is not what the proof of work gives",
                    true,
                )
            }
            Err(e) => {
                // the pool could not check (no memory for the dataset): not the miner's fault, and not counted
                self.log(&format!("could not check a share: {e}"));
                return self.reject(
                    s,
                    R_NOT_ALLOWED,
                    "the pool could not check this share, try again",
                    false,
                );
            }
        }
        // a good share
        s.bad_in_row = 0;
        s.shares += 1;
        s.vardiff.share();
        let met = if meets_now {
            s.target
        } else {
            s.prev_target.expect("meets_before").0
        };
        let weight = work_of(&met);
        let need = u128::from(self.cfg.window_factor) * u128::from(work_of(&job.block_target));
        self.stats.shares_accepted.fetch_add(1, Ordering::Relaxed);
        self.with_accounts(|a| a.add_share(s.address_id, weight, need));
        let mut outcome = ShareOutcome {
            accepted: true,
            reason: R_ACCEPTED,
            text: String::new(),
            block: None,
            close: None,
        };
        if id < job.block_target {
            outcome.block = Some(self.found_block(&job, header, need));
        }
        outcome
    }

    /// The share is a block: hands it to the node, and if it is in the chain, fixes who is owed what for it.
    fn found_block(&self, job: &JobRecord, header: BlockHeader, need: u128) -> BlockOutcome {
        self.stats.blocks_found.fetch_add(1, Ordering::Relaxed);
        let block_id = ids::block_id(&header, self.pow.kind());
        let reward = job.reward;
        self.log(&format!(
            "a share is a BLOCK: height {}, id {}, reward {reward}",
            job.height,
            crate::daemon::short_id(&block_id)
        ));
        match self.node.submit_header(header) {
            Ok(BlockVerdict::InChain(id)) => {
                self.stats.blocks_in_chain.fetch_add(1, Ordering::Relaxed);
                let credits = self.with_accounts(|a| {
                    a.block_found(job.height, id, reward, self.cfg.fee_ppm, need)
                });
                self.log(&format!(
                    "block {} is in the chain: {} miners will be credited when it is {} blocks deep",
                    job.height,
                    credits.len(),
                    self.cfg.maturity
                ));
                self.save_state();
                BlockOutcome::InChain
            }
            Ok(BlockVerdict::LostRace(_)) => {
                self.stats.blocks_lost_race.fetch_add(1, Ordering::Relaxed);
                self.log(&format!(
                    "block {} lost a race: another block took its place",
                    job.height
                ));
                BlockOutcome::LostRace
            }
            Ok(BlockVerdict::Refused(why)) => {
                self.stats.blocks_refused.fetch_add(1, Ordering::Relaxed);
                self.log(&format!(
                    "block {} was REFUSED by the node: {why}",
                    job.height
                ));
                BlockOutcome::Refused(why)
            }
            Err(e) => {
                self.log(&format!(
                    "could not hand block {} to the node: {e}",
                    job.height
                ));
                BlockOutcome::NodeError(e)
            }
        }
    }

    // ---- jobs ----

    /// One look at the node: a new job if the tip moved (clean) or the last is old; and the accounts settled. `Err` is the node not answering.
    pub fn step_jobs(&self) -> Result<(), String> {
        let (height, tip, syncing) = self.node.tip()?;
        if syncing {
            // a node catching up has no tip worth building on: the jobs are dropped, so no share is taken for them
            let mut book = self.book.lock().expect("book lock");
            let old: Vec<u64> = book.jobs.drain(..).map(|j| j.id).collect();
            for id in old {
                book.kill(id);
            }
            book.tip = None;
            return Ok(());
        }
        let (moved, old) = {
            let book = self.book.lock().expect("book lock");
            let moved = book.tip != Some((height, tip));
            let old = self
                .last_refresh
                .lock()
                .ok()
                .and_then(|l| *l)
                .is_none_or(|t| t.elapsed() >= self.cfg.refresh_every);
            (moved, old)
        };
        if moved || old {
            self.make_job(height, tip, moved)?;
        }
        // settle what has matured
        let node = Arc::clone(&self.node);
        let (credited, lost) = {
            let mut a = self.accounts.lock().expect("accounts lock");
            a.settle(height, self.cfg.maturity, &mut |h| {
                node.block_id_at(h).ok().flatten()
            })
        };
        if credited + lost > 0 {
            self.log(&format!(
                "blocks settled: {credited} credited, {lost} lost to another block"
            ));
            self.dirty.store(true, Ordering::SeqCst);
        }
        self.save_state();
        Ok(())
    }

    fn make_job(&self, height: u64, tip: [u8; 32], clean: bool) -> Result<(), String> {
        let next = height + 1;
        let t = self
            .node
            .template(&self.cfg.pool_address, self.cfg.max_weight)?;
        if t.height != next || t.header.prev_id != tip {
            // the tip moved between our two questions: the next look makes the job
            return Ok(());
        }
        // the node is ours, but a template that does not pay the pool must never be handed out
        check_template(&t, next, &tip, &self.cfg.pool_address, self.now())
            .map_err(|e| format!("the node's template was refused: {e}"))?;
        let mut header = t.header.clone();
        header.nonce = 0;
        header.mix = [0u8; 64];
        let record = {
            let mut book = self.book.lock().expect("book lock");
            let id = book.next_id;
            book.next_id += 1;
            if clean {
                let old: Vec<u64> = book.jobs.drain(..).map(|j| j.id).collect();
                for o in old {
                    book.kill(o);
                }
            }
            while book.jobs.len() >= MAX_JOBS_PER_TIP {
                if let Some(j) = book.jobs.pop_front() {
                    book.kill(j.id);
                }
            }
            let record = Arc::new(JobRecord {
                id,
                height: next,
                tip,
                reward: t
                    .coinbase
                    .outputs
                    .iter()
                    .fold(0u64, |a, o| a.saturating_add(o.amount)),
                header,
                block_target: U256::from_be_bytes(&t.target),
                created: Instant::now(),
                nonces: Mutex::new(HashSet::new()),
            });
            book.jobs.push_back(Arc::clone(&record));
            book.tip = Some((height, tip));
            record
        };
        if let Ok(mut l) = self.last_refresh.lock() {
            *l = Some(Instant::now());
        }
        self.stats.jobs.fetch_add(1, Ordering::Relaxed);
        // to every miner that is connected; one whose queue is full is too slow and is left to be closed by its own thread
        if let Ok(sessions) = self.sessions.lock() {
            for tx in sessions.values() {
                let _ = tx.try_send(SessionEvent::Push(Arc::clone(&record), clean));
            }
        }
        Ok(())
    }

    /// The job message for a miner: the header with no nonce and no mix, the block target, and how long it is good for.
    pub fn job_message(&self, j: &JobRecord, clean: bool) -> PoolMessage {
        debug_assert_eq!(HEADER_LEN, 146);
        PoolMessage::Job(Job {
            job_id: j.id,
            height: j.height,
            clean,
            header: j.header.clone(),
            block_target: j.block_target.to_be_bytes(),
            ttl: self
                .cfg
                .job_ttl
                .saturating_sub(j.created.elapsed().as_secs().min(u64::from(u32::MAX)) as u32)
                .max(1),
        })
    }

    pub fn current_job(&self) -> Option<Arc<JobRecord>> {
        self.book.lock().ok().and_then(|b| b.latest())
    }

    // ---- payouts ----

    /// Pays the miners who are owed at least the minimum, if the interval has passed: `wallet` is the pool's, `node` its node. Returns what was
    /// done in a line for the log, or `None` when it was not time. The wallet file must be saved by the caller if anything was sent (the
    /// reservations are in it): `sent_any` says so.
    pub fn payout_round<C: ChainView + Submitter>(
        &self,
        wallet: &mut Wallet,
        node: &mut C,
        paylog: &mut dyn FnMut(&str),
    ) -> Result<Option<PayoutReport>, String> {
        let now = self.now();
        let due_at = self.accounts.lock().map(|a| a.next_payout).unwrap_or(0);
        if now < due_at {
            return Ok(None);
        }
        let owed = self
            .accounts
            .lock()
            .map(|a| a.due(self.cfg.min_payout, self.cfg.max_payees))
            .unwrap_or_default();
        self.stats.payout_rounds.fetch_add(1, Ordering::Relaxed);
        if owed.is_empty() {
            self.with_accounts(|a| a.next_payout = now + self.cfg.payout_interval);
            self.save_state();
            return Ok(Some(PayoutReport::default()));
        }
        wallet.sync(&*node).map_err(|e| e.to_string())?;
        let dests: Vec<(tenero_wallet::Address, u64)> =
            owed.iter().map(|o| (o.address, o.amount)).collect();
        let plan = match wallet.build_batch(&*node, &mut OsRng, &dests, FeeLevel::Low) {
            Ok(p) => p,
            Err(e) => {
                // not enough coins matured yet, or the node is busy: try again in ten minutes, not in an hour
                self.with_accounts(|a| a.next_payout = now + 600);
                self.save_state();
                return Err(format!("cannot build the payout: {e}"));
            }
        };
        let sent = wallet.send_batch(node, &plan.txs);
        let mut report = PayoutReport {
            sent_any: sent.sent > 0,
            transactions: sent.sent,
            ..PayoutReport::default()
        };
        for built in &plan.txs[..sent.sent] {
            report.fees += built.fee;
            for part in &built.parts {
                let text = part.to.to_text();
                let id = self.accounts.lock().ok().and_then(|a| a.id_by_text(&text));
                if let Some(id) = id {
                    self.with_accounts(|a| a.debit(id, part.amount));
                    report.paid += part.amount;
                    report.payments += 1;
                    paylog(&format!(
                        "{now} {} {} {}",
                        tenero_core::hash::hex_lower(&built.id),
                        text,
                        part.amount
                    ));
                }
            }
        }
        report.unpaid = plan.unsent.len();
        self.stats
            .payout_transactions
            .fetch_add(sent.sent as u64, Ordering::Relaxed);
        // the next round: an hour from now if everything went, soon if not
        let next = if sent.failed.is_some() || !plan.unsent.is_empty() {
            now + 600
        } else {
            now + self.cfg.payout_interval
        };
        self.with_accounts(|a| a.next_payout = next);
        self.save_state();
        self.log(&format!(
            "payout: {} payments in {} transactions, {} units paid, fees {}, {} still to pay{}",
            report.payments,
            report.transactions,
            report.paid,
            report.fees,
            report.unpaid,
            sent.failed
                .as_ref()
                .map(|e| format!("; the node refused one: {e}"))
                .unwrap_or_default()
        ));
        if let Some(e) = sent.failed {
            return Err(format!("a payout transaction was refused: {e}"));
        }
        Ok(Some(report))
    }

    // ---- the network ----

    /// Listens on `listen`, and runs the job manager. Returns the address bound and a handle that stops it when dropped.
    pub fn start(self: &Arc<Self>, listen: SocketAddr) -> io::Result<ListenHandle> {
        let listener = TcpListener::bind(listen)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        {
            let pool = Arc::clone(self);
            thread::spawn(move || {
                while !pool.stopped() {
                    match listener.accept() {
                        Ok((stream, peer)) => pool.accepted(stream, peer),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(20))
                        }
                        Err(_) => thread::sleep(Duration::from_millis(100)),
                    }
                }
            });
        }
        {
            let pool = Arc::clone(self);
            thread::spawn(move || {
                let mut last_error = Instant::now() - Duration::from_secs(60);
                while !pool.stopped() {
                    if let Err(e) = pool.step_jobs() {
                        if last_error.elapsed() >= Duration::from_secs(30) {
                            last_error = Instant::now();
                            pool.log(&format!("the node did not answer: {e} (trying again)"));
                        }
                    }
                    let mut slept = Duration::ZERO;
                    while slept < Duration::from_millis(500) && !pool.stopped() {
                        thread::sleep(Duration::from_millis(50));
                        slept += Duration::from_millis(50);
                    }
                }
                pool.save_state();
            });
        }
        Ok(ListenHandle {
            addr,
            pool: Arc::clone(self),
        })
    }

    fn accepted(self: &Arc<Self>, stream: std::net::TcpStream, peer: SocketAddr) {
        let ip = peer.ip();
        self.stats.connections.fetch_add(1, Ordering::Relaxed);
        if self.is_banned(ip) {
            self.stats.banned_refused.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let ok = {
            let (Ok(mut per), Ok(sessions)) = (self.per_ip.lock(), self.sessions.lock()) else {
                return;
            };
            if sessions.len() >= self.cfg.max_miners
                || per.get(&ip).copied().unwrap_or(0) >= self.cfg.per_address
            {
                false
            } else {
                *per.entry(ip).or_default() += 1;
                true
            }
        };
        if !ok {
            self.stats.turned_away.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let pool = Arc::clone(self);
        thread::spawn(move || {
            pool.serve(stream, ip);
            if let Ok(mut per) = pool.per_ip.lock() {
                if let Some(c) = per.get_mut(&ip) {
                    *c = c.saturating_sub(1);
                    if *c == 0 {
                        per.remove(&ip);
                    }
                }
            }
        });
    }

    fn serve(self: &Arc<Self>, stream: std::net::TcpStream, ip: IpAddr) {
        let Ok((mut reader, mut writer, _remote)) = pool_net::accept(
            stream,
            &self.key,
            self.cfg.handshake_timeout,
            self.cfg.idle_timeout,
        ) else {
            self.stats.handshake_failed.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let send = |w: &mut pool_net::WriteHalf, m: &PoolMessage| -> io::Result<()> {
            let body = m
                .to_body()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            write_message(w, &body)
        };
        // the first message must be a hello, within the handshake time
        let _ = reader.set_timeout(Some(self.cfg.handshake_timeout));
        let hello = match read_message(&mut reader).map(|b| MinerMessage::from_body(&b)) {
            Ok(Ok(MinerMessage::Hello(h))) => h,
            Ok(_) => {
                self.stats.hello_refused.fetch_add(1, Ordering::Relaxed);
                let _ = send(
                    &mut writer,
                    &PoolMessage::Error("the first message must be a hello".into()),
                );
                return;
            }
            Err(_) => return,
        };
        let (mut session, ok) = match self.open_session(&hello, ip) {
            Ok(x) => x,
            Err(why) => {
                self.stats.hello_refused.fetch_add(1, Ordering::Relaxed);
                let why: String = why.chars().take(120).collect();
                let _ = send(&mut writer, &PoolMessage::Error(why));
                return;
            }
        };
        let (tx, rx): (SyncSender<SessionEvent>, Receiver<SessionEvent>) = sync_channel(64);
        if let Ok(mut s) = self.sessions.lock() {
            s.insert(session.id, tx.clone());
        }
        let _ = reader.set_timeout(Some(self.cfg.idle_timeout));
        // the reader thread turns what the miner says into events, for the loop below
        {
            let tx = tx.clone();
            thread::spawn(move || loop {
                let ev = match read_message(&mut reader) {
                    Ok(b) => {
                        SessionEvent::Msg(MinerMessage::from_body(&b).map_err(|e| e.to_string()))
                    }
                    Err(e) => {
                        let _ = tx.send(SessionEvent::Closed(e.to_string()));
                        return;
                    }
                };
                let bad = matches!(&ev, SessionEvent::Msg(Err(_)));
                if tx.send(ev).is_err() || bad {
                    return;
                }
            });
        }
        let result = self.session_loop(&mut session, ok, &mut writer, &rx, &send);
        if let Err(why) = result {
            self.log(&format!("miner {} left: {why}", session.id));
        }
        writer.shutdown();
        if let Ok(mut s) = self.sessions.lock() {
            s.remove(&session.id);
        }
        self.close_session(&session);
    }

    fn session_loop(
        &self,
        s: &mut Session,
        hello_ok: HelloOk,
        w: &mut pool_net::WriteHalf,
        rx: &Receiver<SessionEvent>,
        send: &dyn Fn(&mut pool_net::WriteHalf, &PoolMessage) -> io::Result<()>,
    ) -> Result<(), String> {
        let e = |x: io::Error| x.to_string();
        send(w, &PoolMessage::HelloOk(hello_ok)).map_err(e)?;
        if let Some(j) = self.current_job() {
            if let Some(t) = self.retarget(s, &j.block_target) {
                send(w, &PoolMessage::SetShareTarget { share_target: t }).map_err(e)?;
            }
            send(w, &self.job_message(&j, true)).map_err(e)?;
        }
        let mut last_tick = Instant::now();
        loop {
            if self.stopped() {
                return Ok(());
            }
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(SessionEvent::Push(job, clean)) => {
                    if let Some(t) = self.retarget(s, &job.block_target) {
                        send(w, &PoolMessage::SetShareTarget { share_target: t }).map_err(e)?;
                    }
                    send(w, &self.job_message(&job, clean)).map_err(e)?;
                }
                Ok(SessionEvent::Msg(Ok(m))) => match m {
                    MinerMessage::SubmitShare { job_id, nonce, mix } => {
                        let o = self.on_share(s, job_id, nonce, mix);
                        send(
                            w,
                            &PoolMessage::ShareResult {
                                job_id,
                                accepted: o.accepted,
                                reason: o.reason,
                                text: o.text.chars().take(120).collect(),
                            },
                        )
                        .map_err(e)?;
                        if let Some(why) = o.close {
                            return Err(why);
                        }
                    }
                    MinerMessage::Ping { token } => {
                        send(w, &PoolMessage::Pong { token }).map_err(e)?
                    }
                    _ => {
                        let _ = send(
                            w,
                            &PoolMessage::Error("this message is not allowed here".into()),
                        );
                        return Err("sent a message that is not allowed here".into());
                    }
                },
                Ok(SessionEvent::Msg(Err(why))) => {
                    let _ = send(w, &PoolMessage::Error("malformed message".into()));
                    return Err(format!("sent a malformed message ({why})"));
                }
                Ok(SessionEvent::Closed(why)) => return Err(why),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
            if last_tick.elapsed() >= Duration::from_secs(5) {
                last_tick = Instant::now();
                if let Some(t) = self.vardiff_tick(s) {
                    send(w, &PoolMessage::SetShareTarget { share_target: t }).map_err(e)?;
                }
            }
        }
    }
}

/// What a payout round did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PayoutReport {
    pub payments: usize,
    pub transactions: usize,
    pub paid: u64,
    pub fees: u64,
    /// Payments that could not be built now (the coins ran out): still owed.
    pub unpaid: usize,
    /// Transactions were handed to the node: the wallet file must be saved (it holds the reservations).
    pub sent_any: bool,
}

/// Keeps the pool listening; dropping it stops the pool.
pub struct ListenHandle {
    pub addr: SocketAddr,
    pool: Arc<Pool>,
}

impl ListenHandle {
    pub fn pool(&self) -> &Arc<Pool> {
        &self.pool
    }
}

impl Drop for ListenHandle {
    fn drop(&mut self) {
        self.pool.stop();
    }
}

/// The first nonce a miner of this session should try (the start of its slice).
pub fn session_first_nonce(prefix: u64, bits: u8) -> u64 {
    first_nonce_of(prefix, bits)
}
