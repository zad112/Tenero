//! The node program's body: opens the chain, builds the engine, starts the network, the control interface and
//! (if asked) the miner, keeps house (status line, saving the pool, pruning), and shuts down cleanly when told.
//!
//! **There is no launched Tenero network.** `test` is a CPU-mined test chain; `dev` runs the real matmulhash proof
//! of work with a placeholder starting difficulty and a genesis made from a label. Neither has value, and the
//! launch gates in `docs/M8_PLAN.md` section 7 are not met. **Experimental and unaudited.**

use std::collections::{BTreeSet, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand_core::OsRng;
use tenero_chain::{ChainParams, MatmulPow, PowCheck, Sha256Pow};
use tenero_core::matmulhash::Params;
use tenero_core::u256::U256;
use tenero_core::v2::ids::PowKind;
use tenero_miner::gpu::GpuBackend;
use tenero_miner::{
    CpuMatmulBackend, Miner, MinerConfig, MinerEvent, MinerHook, Sha256Backend, WalletPayout,
};
use tenero_net::addrbook::AddrBookConfig;
use tenero_net::engine::Alarm;
use tenero_net::noise::NodeKey;
use tenero_net::transport::{Counters, Hooks, Net, NetConfig};
use tenero_net::{AssumeValid, Engine, EngineConfig, Event};
use tenero_node::{Node, NodeConfig};
use tenero_store::Store;

use crate::client::{create_cookie, COOKIE_FILE};
use crate::config::{Config, MineMode, Network};
use crate::control::NodeKind;
use crate::log::{Level, Logger};
use crate::miner_service::{self, ServiceConfig};
use crate::server::{self, ControlHook, Meta};
use crate::ui::{Banner, Event as UiEvent, MiningStatus, NodeStatus, SyncProgress};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The source this build was made from (a git commit, `-dirty` if it had local changes, or `unknown`): see `build.rs`.
pub const COMMIT: &str = env!("TENERO_COMMIT");

/// What `--version` prints, for every program of the project: which program, which version, which source commit, and what it is.
pub fn version_line(program: &str) -> String {
    format!("{program} v{VERSION} (commit {COMMIT}) EXPERIMENTAL, UNAUDITED; nothing on any network it runs has value")
}

/// Whether the first command-line argument asks for the version.
pub fn wants_version(first: Option<&str>) -> bool {
    matches!(first, Some("version" | "--version" | "-V"))
}
/// What every start-up prints, so nobody mistakes what this is.
pub const BANNER: &str = "tenero node: EXPERIMENTAL and UNAUDITED. No launched network exists; nothing on the test or dev networks has value.";

pub const POOL_FILE: &str = "pool.dat";
const POOL_SAVE_EVERY: Duration = Duration::from_secs(300);
const PRUNE_EVERY: Duration = Duration::from_secs(600);
/// The proof-of-work epoch of the development network, in blocks (the same as the release network's: `Network::epoch_blocks`).
pub const DEV_EPOCH_BLOCKS: u64 = crate::config::REAL_POW_EPOCH_BLOCKS;
/// The development network starts easy (one attempt in eight meets the target): a placeholder, not a decision.
const DEV_START_TARGET_POW2: u32 = 253;
/// The release network ("alpha", M11.2) starts at a real difficulty: a target of 2^237 is about 524,000 attempts a block. At the measured
/// 34,000 attempts a second of one RTX 5070 Ti that is a block every 15 s at first, never under a second; a GPU a quarter as fast gets a
/// block a minute; a 6-thread CPU (about 164 a second) alone about one an hour. The difficulty then adjusts by up to 4x a block, so the
/// start matters for roughly the first hour. The owner's choice (2026-10-04), from those measured speeds.
pub const ALPHA_START_TARGET_POW2: u32 = 237;
/// The release network's genesis label (hashed into the chain id). A restart of the network (for Carrot) gets "alpha network 2".
pub const ALPHA_LABEL: &str = "tenero alpha network 1";
/// The beta network's genesis label (Beta.1: the fresh network after the hard fork). It starts at the same difficulty as alpha did.
pub const BETA_LABEL: &str = "tenero beta network 1";

/// The rules and proof of work of a network.
struct Chain {
    label: String,
    kind: PowKind,
    params: ChainParams,
    /// The real proof of work's checker, shared with the miner so a dataset is built once.
    matmul: Option<Arc<MatmulPow>>,
}

fn chain_of(network: Network) -> Result<Chain, String> {
    match network {
        Network::Test => Ok(Chain {
            label: tenero_net::sim::LABEL.to_string(),
            kind: PowKind::Sha256,
            params: tenero_net::sim::test_chain_params(),
            matmul: None,
        }),
        Network::Dev => {
            let label = "tenero development network".to_string();
            Ok(Chain {
                params: ChainParams::version_2(
                    &label,
                    PowKind::Matmul,
                    U256::pow2(DEV_START_TARGET_POW2).ok_or("bad start target")?,
                ),
                label,
                kind: PowKind::Matmul,
                matmul: Some(Arc::new(MatmulPow::low_memory(
                    Params::DEFAULT,
                    DEV_EPOCH_BLOCKS,
                    6,
                )?)),
            })
        }
        Network::Alpha | Network::Beta => {
            let label = if network == Network::Alpha {
                ALPHA_LABEL
            } else {
                BETA_LABEL
            }
            .to_string();
            let mut params = ChainParams::version_2(
                &label,
                PowKind::Matmul,
                U256::pow2(ALPHA_START_TARGET_POW2).ok_or("bad start target")?,
            );
            // alpha keeps the limits of alpha.4 so that this version and the older nodes still on that network agree on what is valid
            params.legacy_tx_limits = network == Network::Alpha;
            Ok(Chain {
                params,
                label,
                kind: PowKind::Matmul,
                matmul: Some(Arc::new(MatmulPow::low_memory(
                    Params::DEFAULT,
                    network.epoch_blocks(),
                    6,
                )?)),
            })
        }
    }
}

/// How many proof-of-work datasets (4 GiB each at the real parameters) a node on this network holds at once: `None` for the SHA-256 test network,
/// which has none. **One**: the node must stay inside 8 GB (the owner's limit, 2026-10-04), which two datasets plus the rest of the node would not
/// (`MatmulPow::low_memory`; measured before the change: 8.0 GiB of dataset after the first epoch boundary). The cost is a wait of a few seconds
/// for the first block of each epoch.
pub fn proof_of_work_datasets(network: Network) -> Result<Option<usize>, String> {
    Ok(chain_of(network)?.matmul.map(|m| m.max_datasets()))
}

/// The engine's two limits for the `max_inbound` setting: `(inbound limit, limit on all peers)`. 0 means the operator set no limit, and then
/// neither does the program (the machine's own limits and the per-connection budgets still apply); otherwise the peers allowed are the inbound
/// limit plus the outbound ones (at least 64).
pub fn inbound_limits(max_inbound: usize, peer_target: usize) -> (usize, usize) {
    if max_inbound == 0 {
        (usize::MAX, usize::MAX)
    } else {
        (max_inbound, max_inbound.saturating_add(peer_target.max(64)))
    }
}

/// How long a seed repeats one answer to a network group's request for addresses (milliseconds). The default is 15 minutes (it was 24 hours; see
/// `docs/THREAT_MODEL.md` C4): it slows one group harvesting the address book by asking again, and lets a node that has just become reachable
/// be passed on to newcomers soon. On a PRIVATE network (`allow_private_peers`: every node on one machine or one LAN, so all
/// of them are one "group" and a first, empty answer would be repeated to every later node) there is no such group to protect, so it is 0.
pub fn address_answer_ttl_ms(allow_private_peers: bool) -> u64 {
    if allow_private_peers {
        0
    } else {
        EngineConfig::default().addr_answer_ttl_ms
    }
}

/// The consensus parameters of a network (its genesis label, proof of work, starting target, block time and so on), for tests and tools.
pub fn params_of(network: Network) -> Result<ChainParams, String> {
    Ok(chain_of(network)?.params)
}

/// The chain id (the genesis block id) of a network. Every node of the network works it out the same way, from the label of the network, and
/// speaks only to nodes with the same one; the seed check needs it to talk to the nodes of a network. Found by opening a throwaway database.
pub fn chain_id_of(network: Network) -> Result<[u8; 32], String> {
    let chain = chain_of(network)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("tenero-chainid-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let id = Store::open(dir.join("chain.redb"), &chain.label, chain.kind)
        .map(|s| s.chain_id())
        .map_err(|e| e.to_string());
    let _ = std::fs::remove_dir_all(&dir);
    id
}

/// What `rewind` found, or did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewindReport {
    /// The chain as it was: its height and the id of its tip.
    pub tip_height: u64,
    pub tip_id: [u8; 32],
    /// The chain as it is after the rewind (or would be, for a dry run).
    pub new_height: u64,
    pub new_tip_id: [u8; 32],
    /// The blocks taken off, newest first: height and id.
    pub removed: Vec<(u64, [u8; 32])>,
    /// Whether anything was changed (`false`: a dry run).
    pub applied: bool,
    /// The file that lists the removed blocks, when `applied`.
    pub record: Option<PathBuf>,
}

/// The emergency rewind (`docs/EMERGENCY_PLAN.md` section 5): takes the newest blocks off a node's chain, down to `to_height`, **with the
/// node stopped**. A dry run (`apply == false`) only says what it would remove. When it acts it first writes the ids of the blocks it removes to
/// `rewind-<time>.txt` in the data directory, takes the side-branch pool aside (`pool.dat` to `pool.dat.before-rewind`, so a removed block
/// is not put back from it), and then removes the blocks one at a time with the store's own rollback, each one a complete database
/// transaction: a stop in the middle leaves a shorter chain that is whole, never a broken one. It does NOT stop the node from taking the
/// same blocks again from a peer that still has them: the plan's step is a build that refuses the bad block, then every node rewinds.
pub fn rewind(
    data: &std::path::Path,
    network: Network,
    to_height: u64,
    apply: bool,
) -> Result<RewindReport, String> {
    let chain = chain_of(network)?;
    let db = data.join("chain.redb");
    if !db.exists() {
        return Err(format!("there is no chain in {}", data.display()));
    }
    let store = Store::open(&db, &chain.label, chain.kind).map_err(|e| {
        format!(
            "cannot open the chain in {}: {e} (is the node still running? stop it first)",
            data.display()
        )
    })?;
    let (tip_height, tip) = store.tip().map_err(|e| e.to_string())?;
    if to_height >= tip_height {
        return Err(format!(
            "nothing to do: the chain is at height {tip_height} and --to {to_height} is not below it"
        ));
    }
    let mut removed = Vec::new();
    for h in (to_height + 1..=tip_height).rev() {
        let index = store
            .get_block(h)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("block {h} is missing"))?
            .index;
        removed.push((h, index.block_id));
    }
    let new_tip_id = match store.get_block(to_height).map_err(|e| e.to_string())? {
        Some(b) => b.index.block_id,
        None if to_height == 0 => store.chain_id(),
        None => return Err(format!("block {to_height} is missing")),
    };
    let mut report = RewindReport {
        tip_height,
        tip_id: tip.block_id,
        new_height: to_height,
        new_tip_id,
        removed,
        applied: false,
        record: None,
    };
    if !apply {
        return Ok(report);
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let record = data.join(format!("rewind-{stamp}.txt"));
    let mut text = format!(
        "rewind from height {tip_height} (tip {}) to height {to_height} (tip {}); newest first, height and block id\n",
        hex_id(&tip.block_id),
        hex_id(&new_tip_id)
    );
    for (h, id) in &report.removed {
        text.push_str(&format!("{h} {}\n", hex_id(id)));
    }
    std::fs::write(&record, text).map_err(|e| format!("cannot write {}: {e}", record.display()))?;
    let pool = data.join(POOL_FILE);
    if pool.exists() {
        std::fs::rename(&pool, data.join(format!("{POOL_FILE}.before-rewind")))
            .map_err(|e| format!("cannot set the side-branch pool aside: {e}"))?;
    }
    for _ in to_height..tip_height {
        store.pop_block().map_err(|e| e.to_string())?;
    }
    report.applied = true;
    report.record = Some(record);
    Ok(report)
}

fn hex_id(id: &[u8; 32]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

/// Where a running node says it is listening (for tests, which ask for port 0).
#[derive(Clone, Copy, Debug)]
pub struct Ready {
    pub p2p: Option<SocketAddr>,
    pub control: SocketAddr,
}

/// How the node ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub height: u64,
    pub tip_id: [u8; 32],
}

pub fn short_id(id: &[u8; 32]) -> String {
    id[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// A time span for the status line: `42s`, `7m`, `2h05m`, `3d04h`.
pub fn format_age(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=119 => format!("{s}s"),
        120..=7199 => format!("{}m", s / 60),
        7200..=172_799 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        _ => format!("{}d{:02}h", s / 86_400, (s % 86_400) / 3600),
    }
}

/// What changed since the last look: `(true, text)` for an alarm that has just begun and `(false, kind)` for one that has just ended.
/// `known` is the set of alarm kinds already reported, and is updated. A kind that stays raised says nothing more (its numbers change,
/// the situation does not), so a log of a long episode is two lines, not one a minute.
pub fn alarm_changes(known: &mut BTreeSet<&'static str>, alarms: &[Alarm]) -> Vec<(bool, String)> {
    let mut out = Vec::new();
    let now: BTreeSet<&'static str> = alarms.iter().map(|a| a.kind()).collect();
    for a in alarms {
        if known.insert(a.kind()) {
            out.push((true, a.describe()));
        }
    }
    let ended: Vec<&'static str> = known.difference(&now).copied().collect();
    for k in ended {
        known.remove(k);
        out.push((false, k.to_string()));
    }
    out
}

/// The network part of the status line: outbound network groups, the age of the newest block, feeler samples, and the alarms.
pub fn health_summary(h: &tenero_net::engine::NetHealth) -> String {
    let alarms = if h.alarms.is_empty() {
        "none".to_string()
    } else {
        h.alarms
            .iter()
            .map(|a| a.kind())
            .collect::<Vec<_>>()
            .join(",")
    };
    format!(
        "out groups {} | last block {} ago | samples {} | alarms {}",
        h.outbound_groups,
        format_age(h.tip_age_ms),
        h.samples,
        alarms
    )
}

/// What the in-process miner has done, kept by its event callback and read for the status block.
#[derive(Default)]
pub struct MiningShared {
    pub backend: std::sync::Mutex<String>,
    pub found: std::sync::atomic::AtomicU64,
    pub accepted: std::sync::atomic::AtomicU64,
    pub lost_race: std::sync::atomic::AtomicU64,
    pub refused: std::sync::atomic::AtomicU64,
    pub paused: AtomicBool,
    /// The backend's counters (set when the miner is made) and the meter that turns them into rates.
    pub counters: std::sync::Mutex<Option<Arc<tenero_miner::Counters>>>,
    pub meter: std::sync::Mutex<tenero_miner::rate::RateMeter>,
    /// The card's health (GPU mining only, and only if NVML can be read): the connection, and what it said at the last look.
    pub probe: std::sync::Mutex<Option<tenero_miner::gpu_stats::GpuProbe>>,
    pub gpu: std::sync::Mutex<Option<tenero_miner::gpu_stats::GpuReading>>,
    /// The work of the blocks that are in the chain, added up (what the effective rate is made of), and when mining began and was last
    /// looked at (milliseconds after the node started).
    pub accepted_work: std::sync::Mutex<f64>,
    pub began_ms: std::sync::Mutex<Option<u64>>,
    pub last_ms: std::sync::atomic::AtomicU64,
}

impl MiningShared {
    /// One look at the backend's counters, `now_ms` after the node started (the status block takes one a second).
    pub fn sample(&self, now_ms: u64) {
        self.last_ms.store(now_ms, Ordering::Relaxed);
        if let Some(p) = self.probe.lock().ok().as_deref().and_then(|p| p.as_ref()) {
            let reading = p.read();
            if let Ok(mut g) = self.gpu.lock() {
                *g = Some(reading);
            }
        }
        let Some(c) = self.counters.lock().ok().and_then(|c| c.clone()) else {
            return;
        };
        if let Ok(mut b) = self.began_ms.lock() {
            b.get_or_insert(now_ms);
        }
        if let Ok(mut m) = self.meter.lock() {
            m.record(now_ms, c.attempts.load(Ordering::Relaxed), c.searching());
        }
    }

    /// Counts what the miner reports (the screen is told separately).
    pub fn record(&self, e: &MinerEvent) {
        let one = |c: &std::sync::atomic::AtomicU64| c.fetch_add(1, Ordering::Relaxed);
        match e {
            MinerEvent::Started { backend } => {
                if let Ok(mut b) = self.backend.lock() {
                    *b = backend.clone();
                }
            }
            MinerEvent::InChain { work, .. } => {
                if let Ok(mut w) = self.accepted_work.lock() {
                    *w += *work;
                }
                one(&self.found);
                one(&self.accepted);
            }
            MinerEvent::LostRace { .. } => {
                one(&self.found);
                one(&self.lost_race);
            }
            MinerEvent::Refused { .. } => {
                one(&self.found);
                one(&self.refused);
            }
            MinerEvent::Paused => self.paused.store(true, Ordering::Relaxed),
            MinerEvent::Resumed => self.paused.store(false, Ordering::Relaxed),
            // for a pool miner a share is what a block is for a solo miner: found, and then accepted or not
            MinerEvent::ShareAccepted { work } => {
                if let Ok(mut w) = self.accepted_work.lock() {
                    *w += *work;
                }
                one(&self.found);
                one(&self.accepted);
            }
            MinerEvent::ShareRejected { reason } => {
                one(&self.found);
                // a stale share was a share that came too late, as a block that lost a race
                if *reason == 1 {
                    one(&self.lost_race);
                } else {
                    one(&self.refused);
                }
            }
            MinerEvent::BackendFailed { .. }
            | MinerEvent::NodeConnected
            | MinerEvent::NodeLost { .. }
            | MinerEvent::PoolConnected { .. }
            | MinerEvent::PoolLost { .. } => {}
        }
    }

    /// The status with a look at the counters first (`now_ms` after the node started): the rates are made of these looks.
    pub fn status_at(&self, now_ms: u64) -> MiningStatus {
        self.sample(now_ms);
        self.status()
    }

    /// How the blocks have gone, from the counters of the backend (`elapsed_ms` of mining so far).
    pub fn luck_from(
        &self,
        c: &tenero_miner::Counters,
        elapsed_ms: u64,
    ) -> tenero_miner::rate::Luck {
        tenero_miner::rate::Luck {
            found: self.found.load(Ordering::Relaxed),
            expected_blocks: c.expected_blocks(),
            accepted_work: self.accepted_work.lock().map(|w| *w).unwrap_or(0.0),
            elapsed_ms,
        }
    }

    pub fn status(&self) -> MiningStatus {
        MiningStatus {
            // (the backend names itself a moment after the miner starts)
            backend: self
                .backend
                .lock()
                .map(|b| b.clone())
                .ok()
                .filter(|b| !b.is_empty())
                .unwrap_or_else(|| "starting".to_string()),
            blocks_found: self.found.load(Ordering::Relaxed),
            blocks_accepted: self.accepted.load(Ordering::Relaxed),
            paused: self.paused.load(Ordering::Relaxed),
            rates: self.meter.lock().map(|m| m.rates()).unwrap_or_default(),
            gpu: self.gpu.lock().ok().and_then(|g| g.clone()),
            luck: {
                let began = self.began_ms.lock().ok().and_then(|b| *b);
                let counters = self.counters.lock().ok().and_then(|c| c.clone());
                match (counters, began) {
                    (Some(c), Some(b)) => {
                        self.luck_from(&c, self.last_ms.load(Ordering::Relaxed).saturating_sub(b))
                    }
                    _ => Default::default(),
                }
            },
        }
    }
}

/// The size of everything under `path` in bytes, to a few levels down (the chain database and its segments).
pub fn dir_size(path: &std::path::Path) -> u64 {
    fn walk(p: &std::path::Path, depth: u32) -> u64 {
        let Ok(rd) = std::fs::read_dir(p) else {
            return 0;
        };
        rd.flatten()
            .map(|e| match e.metadata() {
                Ok(m) if m.is_dir() && depth < 4 => walk(&e.path(), depth + 1),
                Ok(m) if m.is_file() => m.len(),
                _ => 0,
            })
            .sum()
    }
    walk(path, 0)
}

/// Blocks a second over the samples (newest last), if they span long enough to say.
pub fn sync_rate(samples: &VecDeque<(Instant, u64)>) -> Option<f64> {
    let (t0, h0) = *samples.front()?;
    let (t1, h1) = *samples.back()?;
    let secs = t1.duration_since(t0).as_secs_f64();
    (secs >= 5.0 && h1 > h0).then(|| (h1 - h0) as f64 / secs)
}

/// Status line, saving the pool, pruning.
struct Maintenance {
    log: Arc<Logger>,
    counters: Arc<Counters>,
    status_every: Duration,
    next_status: Instant,
    pool_path: std::path::PathBuf,
    next_save: Instant,
    prune_keep: u64,
    next_prune: Instant,
    /// The alarm kinds already reported (to log a change, not every look).
    alarms: BTreeSet<&'static str>,
    /// What the screen shows: when the node started, when the status block is next redrawn, the heights seen over the last half
    /// minute (for the sync rate), when a sync began, the size of the data directory (looked at now and then), the mining state.
    started: Instant,
    next_ui: Instant,
    heights: VecDeque<(Instant, u64)>,
    sync_began: Option<(Instant, u64)>,
    data_dir: std::path::PathBuf,
    disk: Option<u64>,
    next_disk: Instant,
    mining: Option<Arc<MiningShared>>,
}

impl Maintenance {
    /// The facts for the status block, from the node as it is now.
    fn node_status(
        &mut self,
        engine: &Engine<'_>,
        health: &tenero_net::engine::NetHealth,
    ) -> Option<NodeStatus> {
        let (height, tip) = engine.node().store().tip().ok()?;
        let now = Instant::now();
        self.heights.push_back((now, height));
        while self
            .heights
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > Duration::from_secs(30))
        {
            self.heights.pop_front();
        }
        if now >= self.next_disk {
            self.next_disk = now + Duration::from_secs(30);
            self.disk = Some(dir_size(&self.data_dir));
        }
        let target = engine.best_peer_height();
        let sync = (engine.is_syncing() && target > height).then(|| SyncProgress {
            current: height,
            target,
            rate: sync_rate(&self.heights),
        });
        Some(NodeStatus {
            height,
            tip: short_id(&tip.block_id),
            last_block_age_secs: health.tip_age_ms / 1000,
            peers_in: engine.inbound_count(),
            peers_out: engine.outbound_count(),
            out_groups: health.outbound_groups,
            sync,
            mempool: engine.node().pool().len(),
            uptime_secs: now.duration_since(self.started).as_secs(),
            disk_bytes: self.disk,
            pruned_below: engine.node().store().pruned_below().unwrap_or(0),
            alarms: health.alarms.iter().map(|a| a.kind().to_string()).collect(),
            mining: self
                .mining
                .as_ref()
                .map(|m| m.status_at(now.duration_since(self.started).as_millis() as u64)),
        })
    }
}

impl Hooks for Maintenance {
    fn poll(&mut self, engine: &mut Engine<'_>, _now_ms: u64) -> Vec<Event> {
        let now = Instant::now();
        let health = engine.health();
        for (begun, text) in alarm_changes(&mut self.alarms, &health.alarms) {
            if begun {
                self.log
                    .log_event(Level::Warn, &text, UiEvent::AlarmBegan(text.clone()));
            } else {
                self.log.log_event(
                    Level::Info,
                    &format!("alarm ended: {text}"),
                    UiEvent::AlarmEnded(text),
                );
            }
        }
        if self.log.screen().is_some() && now >= self.next_ui {
            self.next_ui = now + Duration::from_secs(1);
            // the end of a real sync is worth a line (a few blocks fetched in passing are not)
            let syncing = engine.is_syncing();
            match (syncing, self.sync_began) {
                (true, None) => {
                    let h = engine.node().store().tip().map(|(h, _)| h).unwrap_or(0);
                    self.sync_began = Some((now, h));
                }
                (false, Some((t, h0))) => {
                    self.sync_began = None;
                    if let Ok((h, _)) = engine.node().store().tip() {
                        if now.duration_since(t) >= Duration::from_secs(5)
                            || h.saturating_sub(h0) >= 10
                        {
                            self.log.log_event(
                                Level::Info,
                                &format!("synced: the chain is up to date at height {h}"),
                                UiEvent::Synced { height: h },
                            );
                        }
                    }
                }
                _ => {}
            }
            if let Some(st) = self.node_status(engine, &health) {
                if let Some(sc) = self.log.screen() {
                    sc.status(&st);
                }
            }
        }
        if now >= self.next_status {
            self.next_status = now + self.status_every;
            if let Ok((h, tip)) = engine.node().store().tip() {
                self.log.info(&format!(
                    "status: tip {h} ({}) | peers {} (in {}, out {}) | {} | book {} | mempool {} | blocks applied {} | bans {} | bytes in {}, out {} | {}",
                    short_id(&tip.block_id),
                    engine.peer_count(),
                    engine.inbound_count(),
                    engine.outbound_count(),
                    health_summary(&health),
                    engine.addr_book().len(),
                    engine.node().pool().len(),
                    engine.stats.blocks_applied,
                    engine.stats.bans,
                    self.counters.bytes_in.load(Ordering::Relaxed),
                    self.counters.bytes_out.load(Ordering::Relaxed),
                    if engine.is_syncing() { "syncing" } else { "in sync" },
                ));
            }
        }
        if now >= self.next_save {
            self.next_save = now + POOL_SAVE_EVERY;
            if let Err(e) = engine.node().save_pool(&self.pool_path) {
                self.log
                    .warn(&format!("could not save the side-branch pool: {e}"));
            }
        }
        if self.prune_keep > 0 && now >= self.next_prune {
            self.next_prune = now + PRUNE_EVERY;
            match engine.node().store().prune_keeping(self.prune_keep) {
                Ok(s) if s.transactions_pruned > 0 => self.log.info(&format!(
                    "pruned {} transactions' proofs ({} bytes)",
                    s.transactions_pruned, s.prunable_bytes_freed
                )),
                Ok(_) => {}
                Err(e) => self.log.warn(&format!("pruning failed: {e}")),
            }
        }
        Vec::new()
    }
}

/// All the hooks the node runs, in order.
struct AllHooks {
    control: ControlHook,
    maintenance: Maintenance,
    miner: Option<Box<dyn Hooks>>,
}

impl Hooks for AllHooks {
    fn poll(&mut self, engine: &mut Engine<'_>, now_ms: u64) -> Vec<Event> {
        let mut events = self.control.poll(engine, now_ms);
        events.extend(self.maintenance.poll(engine, now_ms));
        if let Some(m) = self.miner.as_mut() {
            events.extend(m.poll(engine, now_ms));
        }
        events
    }
}

/// What the screen is told when the miner reports something, and the counters the status block shows.
fn miner_events(log: &Arc<Logger>, shared: &Arc<MiningShared>) -> tenero_miner::EventSink {
    let (log, shared) = (Arc::clone(log), Arc::clone(shared));
    Arc::new(move |e| {
        shared.record(&e);
        if let Some(ev) = crate::ui::miner_event_to_ui(&e) {
            if let Some(sc) = log.screen() {
                sc.event(&ev);
            }
        }
    })
}

fn miner_hook(
    cfg: &Config,
    chain: &Chain,
    log: &Arc<Logger>,
    shared: &Arc<MiningShared>,
) -> Result<Option<Box<dyn Hooks>>, String> {
    if cfg.mine == MineMode::Off {
        return Ok(None);
    }
    let to = cfg
        .mine_to
        .as_deref()
        .ok_or("mine_to is required when mining")?;
    let address = tenero_wallet::Address::from_text(to).map_err(|e| e.to_string())?;
    let payout = WalletPayout::new(address).ok_or("the mining address holds an invalid key")?;
    let l = Arc::clone(log);
    let mcfg = MinerConfig {
        log: Arc::new(move |line| l.info(&format!("miner: {line}"))),
        events: miner_events(log, shared),
        min_block_interval: Duration::from_secs(cfg.mine_pace),
        ..MinerConfig::default()
    };
    // the hook is made here and the meter is told where its counters are, then the hook goes into the node's loop
    let seen = |h: MinerHook<WalletPayout>| -> Box<dyn Hooks> {
        if let Ok(mut c) = shared.counters.lock() {
            *c = Some(h.counters());
        }
        Box::new(h)
    };
    let epoch = cfg.network.epoch_blocks();
    let hook: Box<dyn Hooks> = match cfg.mine {
        MineMode::Off => unreachable!("handled above"),
        MineMode::Sha256 => seen(MinerHook::new(
            Miner::spawn(|| Ok(Sha256Backend)),
            payout,
            mcfg,
        )),
        MineMode::Cpu => {
            let pow = Arc::clone(chain.matmul.as_ref().ok_or(
                "cpu mining needs a network with the real proof of work (dev, beta or alpha)",
            )?);
            let cores = cfg.mine_cores;
            seen(MinerHook::new(
                Miner::spawn(move || Ok(CpuMatmulBackend::new(pow, epoch, cores, 10))),
                payout,
                mcfg,
            ))
        }
        MineMode::Gpu => {
            let device = cfg.gpu_device;
            let batch = if cfg.gpu_batch_auto {
                let l = Arc::clone(log);
                tenero_miner::gpu::auto_batch(
                    device,
                    Params::DEFAULT,
                    epoch,
                    cfg.gpu_batch,
                    &move |m| l.info(&format!("miner: {m}")),
                )
            } else {
                cfg.gpu_batch
            };
            // the card's health for the screen; if NVML cannot be read the miner is not affected
            match tenero_miner::gpu_stats::GpuProbe::open(device) {
                Ok(p) => {
                    if let Ok(mut slot) = shared.probe.lock() {
                        *slot = Some(p);
                    }
                }
                Err(e) => log.info(&format!("GPU readings are not available: {e}")),
            }
            seen(MinerHook::new(
                Miner::spawn(move || GpuBackend::new(device, Params::DEFAULT, epoch, batch)),
                payout,
                mcfg,
            ))
        }
    };
    Ok(Some(hook))
}

/// Runs a node until `shutdown` is set (by Ctrl-C, by a `stop` over the control interface, or by the caller).
/// `ready` is told where the node listens once it does.
pub fn run(
    cfg: &Config,
    log: Arc<Logger>,
    shutdown: Arc<AtomicBool>,
    ready: Option<Sender<Ready>>,
) -> Result<Summary, String> {
    log.info(BANNER);
    log.info(&format!("build: v{VERSION}, commit {COMMIT}"));
    // a new data directory is made private to its owner; an existing one that other accounts can read is refused
    crate::private_dir::ensure_private(&cfg.data, cfg.allow_open_data_dir, &log)?;
    let chain = chain_of(cfg.network)?;
    if let Some(sc) = log.screen() {
        let mut details = vec![
            format!("  data     {}", cfg.data.display()),
            format!(
                "  kind     {}",
                if cfg.prune_keep == 0 {
                    "archive node (keeps every block in full)".to_string()
                } else {
                    format!(
                        "pruned node (keeps the last {} blocks' proofs)",
                        cfg.prune_keep
                    )
                }
            ),
        ];
        if let Some(f) = &cfg.log_file {
            details.push(format!("  log      {} (full detail)", f.display()));
        }
        sc.banner(&Banner {
            role: "node".to_string(),
            version: format!("v{VERSION}"),
            network: cfg.network.name().to_string(),
            network_note: match cfg.network {
                Network::Test => "SHA-256 test chain, no real proof of work".to_string(),
                Network::Dev => "development chain, real matmulhash proof of work".to_string(),
                Network::Alpha => {
                    "alpha network (first test release): real matmulhash proof of work, no premine; still no value"
                        .to_string()
                }
                Network::Beta => {
                    "beta network (second test release): real matmulhash proof of work, no premine; still no value"
                        .to_string()
                }
            },
            details,
        });
    }
    log.info(&format!(
        "network {} ({}), data {}, {}",
        cfg.network.name(),
        chain.label,
        cfg.data.display(),
        if cfg.prune_keep == 0 {
            "archive node".to_string()
        } else {
            format!(
                "pruned node (keeps the last {} blocks' proofs)",
                cfg.prune_keep
            )
        }
    ));
    let store =
        Store::open(cfg.data.join("chain.redb"), &chain.label, chain.kind).map_err(|e| {
            format!(
                "cannot open the chain in {}: {e} (is another node using this directory?)",
                cfg.data.display()
            )
        })?;
    let key = NodeKey::load_or_create(&cfg.data.join("node.key"))?;
    let pow: &dyn PowCheck = match &chain.matmul {
        Some(m) => &**m,
        None => &Sha256Pow,
    };
    let mut node = Node::new(&store, &chain.params, pow, NodeConfig::default())
        .map_err(|e| format!("cannot start the node: {e}"))?;
    let pool_path = cfg.data.join(POOL_FILE);
    let now_s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match node.load_pool(&pool_path, now_s) {
        Ok(l) => log.info(&format!(
            "loaded the side-branch pool: {} adopted, {} side, {} orphans, {} dropped",
            l.adopted, l.side, l.orphans, l.dropped
        )),
        Err(e) => log.warn(&format!("ignored a damaged side-branch pool file: {e}")),
    }
    let pk = key.public();
    let mut ecfg = EngineConfig {
        addrbook: AddrBookConfig {
            accept_private: cfg.allow_private_peers,
            ..AddrBookConfig::default()
        },
        seeds: cfg.seeds.clone(),
        advertise: cfg.advertise.clone(),
        addr_answer_ttl_ms: address_answer_ttl_ms(cfg.allow_private_peers),
        trusted: cfg.trusted_peers.clone(),
        peer_target: cfg.peer_target,
        outbound_target: cfg.peer_target.min(8),
        max_inbound: inbound_limits(cfg.max_inbound, cfg.peer_target).0,
        max_peers: inbound_limits(cfg.max_inbound, cfg.peer_target).1,
        nonce: u64::from_le_bytes(pk[..8].try_into().expect("8 bytes")) | 1,
        ..EngineConfig::default()
    };
    if !cfg.trusted_peers.is_empty() {
        log.info(&format!(
            "pinned peers (always dialled): {}",
            cfg.trusted_peers.join(", ")
        ));
    }
    if let Some((height, id)) = cfg.assume_valid {
        log.warn(&format!(
            "assume-valid is ON: blocks up to height {height} are trusted to have valid proofs (id {})",
            short_id(&id)
        ));
        ecfg.assume_valid = Some(AssumeValid { height, id });
    }
    let mut engine = Engine::new(node, ecfg);

    let mut ncfg = NetConfig::new(key, store.chain_id());
    ncfg.listen = cfg.listen;
    ncfg.state_path = Some(cfg.data.join("peers.dat"));
    ncfg.hook_tick = Duration::from_millis(10);
    let nl = Arc::clone(&log);
    ncfg.log = Arc::new(move |line| nl.info(line));
    let net = Net::bind(ncfg).map_err(|e| format!("cannot listen: {e}"))?;
    let counters = net.counters();

    let cookie = create_cookie(&cfg.data.join(COOKIE_FILE), &mut OsRng)
        .map_err(|e| format!("cannot write the control cookie: {e}"))?;
    let meta = Meta {
        kind: if cfg.prune_keep == 0 {
            NodeKind::Archive
        } else {
            NodeKind::Pruned
        },
        network: cfg.network.name().to_string(),
        version: VERSION.to_string(),
    };
    let (handle, control) = server::start(cfg.control, cookie, Arc::clone(&shutdown), meta)
        .map_err(|e| format!("cannot start the control interface on {}: {e}", cfg.control))?;
    log.log_event(
        Level::Info,
        &format!(
            "peer-to-peer on {:?}; control interface on {} (cookie in {})",
            net.local_addr(),
            handle.addr,
            cfg.data.join(COOKIE_FILE).display()
        ),
        UiEvent::Listening {
            p2p: net.local_addr().map(|a| a.to_string()),
            control: handle.addr.to_string(),
        },
    );
    // miners on other computers (off unless `miner_listen` is set; `config.rs` makes a key mandatory for now)
    let miner_service = match cfg.miner_listen {
        None => None,
        Some(addr) => {
            let scfg = ServiceConfig {
                max_miners: cfg.miner_max,
                requests_per_minute: cfg.miner_rate,
                key: cfg.miner_key,
                ..ServiceConfig::default()
            };
            let h = miner_service::start(addr, &handle, scfg)
                .map_err(|e| format!("cannot start the miner service on {addr}: {e}"))?;
            log.info(&format!(
                "miner service on {} ({}): miners on other computers may ask this node for blocks to mine and hand blocks back; it answers nothing else",
                h.addr,
                if cfg.miner_key.is_some() { "a key is required" } else { "NO key: anyone may connect" },
            ));
            Some(h)
        }
    };
    if let Some(tx) = ready {
        let _ = tx.send(Ready {
            p2p: net.local_addr(),
            control: handle.addr,
        });
    }

    let mining = Arc::new(MiningShared::default());
    let miner = miner_hook(cfg, &chain, &log, &mining)?;
    if miner.is_some() {
        log.info("mining is ON (in this process)");
    }
    let mut hooks = AllHooks {
        control,
        maintenance: Maintenance {
            log: Arc::clone(&log),
            counters,
            status_every: Duration::from_secs(cfg.status_every),
            next_status: Instant::now() + Duration::from_secs(2),
            pool_path: pool_path.clone(),
            next_save: Instant::now() + POOL_SAVE_EVERY,
            prune_keep: cfg.prune_keep,
            next_prune: Instant::now(),
            alarms: BTreeSet::new(),
            started: Instant::now(),
            next_ui: Instant::now(),
            heights: VecDeque::new(),
            sync_began: None,
            data_dir: cfg.data.clone(),
            disk: None,
            next_disk: Instant::now(),
            mining: miner.is_some().then(|| Arc::clone(&mining)),
        },
        miner,
    };
    let result = net.run(&mut engine, Arc::clone(&shutdown), &mut hooks);
    // shutting down: the side-branch pool (the peers were saved by the network loop)
    log.info("shutting down");
    if let Err(e) = engine.node().save_pool(&pool_path) {
        log.warn(&format!("could not save the side-branch pool: {e}"));
    }
    drop(miner_service);
    drop(handle);
    result.map_err(|e| format!("network error: {e}"))?;
    let (height, tip) = store.tip().map_err(|e| e.to_string())?;
    log.log_event(
        Level::Info,
        &format!(
            "stopped at height {height}, tip {}",
            short_id(&tip.block_id)
        ),
        UiEvent::Stopped {
            height,
            tip: short_id(&tip.block_id),
        },
    );
    Ok(Summary {
        height,
        tip_id: tip.block_id,
    })
}
