//! The node program's body: opens the chain, builds the engine, starts the network, the control interface and
//! (if asked) the miner, keeps house (status line, saving the pool, pruning), and shuts down cleanly when told.
//!
//! **There is no launched Tenero network.** `test` is a CPU-mined test chain; `dev` runs the real matmulhash proof
//! of work with a placeholder starting difficulty and a genesis made from a label. Neither has value, and the
//! launch gates in `docs/M8_PLAN.md` section 7 are not met. **Experimental and unaudited.**

use std::collections::BTreeSet;
use std::net::SocketAddr;
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
use tenero_miner::{CpuMatmulBackend, Miner, MinerConfig, MinerHook, Sha256Backend, WalletPayout};
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
use crate::log::Logger;
use crate::server::{self, ControlHook, Meta};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// What every start-up prints, so nobody mistakes what this is.
pub const BANNER: &str = "tenero node: EXPERIMENTAL and UNAUDITED. No launched network exists; nothing on the test or dev networks has value.";

pub const POOL_FILE: &str = "pool.dat";
const POOL_SAVE_EVERY: Duration = Duration::from_secs(300);
const PRUNE_EVERY: Duration = Duration::from_secs(600);
/// The proof-of-work epoch of the development network, in blocks.
pub const DEV_EPOCH_BLOCKS: u64 = 100;
/// The development network starts easy (one attempt in eight meets the target): a placeholder, not a decision.
const DEV_START_TARGET_POW2: u32 = 253;

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
                matmul: Some(Arc::new(MatmulPow::new(
                    Params::DEFAULT,
                    DEV_EPOCH_BLOCKS,
                    6,
                )?)),
            })
        }
    }
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
}

impl Hooks for Maintenance {
    fn poll(&mut self, engine: &mut Engine<'_>, _now_ms: u64) -> Vec<Event> {
        let now = Instant::now();
        let health = engine.health();
        for (begun, text) in alarm_changes(&mut self.alarms, &health.alarms) {
            if begun {
                self.log.warn(&text);
            } else {
                self.log.info(&format!("alarm ended: {text}"));
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

fn miner_hook(
    cfg: &Config,
    chain: &Chain,
    log: &Arc<Logger>,
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
        min_block_interval: Duration::from_secs(cfg.mine_pace),
        ..MinerConfig::default()
    };
    let hook: Box<dyn Hooks> = match cfg.mine {
        MineMode::Off => unreachable!("handled above"),
        MineMode::Sha256 => Box::new(MinerHook::new(
            Miner::spawn(|| Ok(Sha256Backend)),
            payout,
            mcfg,
        )),
        MineMode::Cpu => {
            let pow = Arc::clone(
                chain
                    .matmul
                    .as_ref()
                    .ok_or("cpu mining needs the dev network")?,
            );
            let cores = cfg.mine_cores;
            Box::new(MinerHook::new(
                Miner::spawn(move || Ok(CpuMatmulBackend::new(pow, DEV_EPOCH_BLOCKS, cores, 10))),
                payout,
                mcfg,
            ))
        }
        MineMode::Gpu => {
            let (device, batch) = (cfg.gpu_device, cfg.gpu_batch);
            Box::new(MinerHook::new(
                Miner::spawn(move || {
                    GpuBackend::new(device, Params::DEFAULT, DEV_EPOCH_BLOCKS, batch, 10)
                }),
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
    // a new data directory is made private to its owner; an existing one that other accounts can read is refused
    crate::private_dir::ensure_private(&cfg.data, cfg.allow_open_data_dir, &log)?;
    let chain = chain_of(cfg.network)?;
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
        trusted: cfg.trusted_peers.clone(),
        peer_target: cfg.peer_target,
        outbound_target: cfg.peer_target.min(8),
        max_inbound: cfg.max_inbound,
        max_peers: cfg.max_inbound + cfg.peer_target.max(64),
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
    log.info(&format!(
        "peer-to-peer on {:?}; control interface on {} (cookie in {})",
        net.local_addr(),
        handle.addr,
        cfg.data.join(COOKIE_FILE).display()
    ));
    if let Some(tx) = ready {
        let _ = tx.send(Ready {
            p2p: net.local_addr(),
            control: handle.addr,
        });
    }

    let miner = miner_hook(cfg, &chain, &log)?;
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
        },
        miner,
    };
    let result = net.run(&mut engine, Arc::clone(&shutdown), &mut hooks);
    // shutting down: the side-branch pool (the peers were saved by the network loop)
    log.info("shutting down");
    if let Err(e) = engine.node().save_pool(&pool_path) {
        log.warn(&format!("could not save the side-branch pool: {e}"));
    }
    drop(handle);
    result.map_err(|e| format!("network error: {e}"))?;
    let (height, tip) = store.tip().map_err(|e| e.to_string())?;
    log.info(&format!(
        "stopped at height {height}, tip {}",
        short_id(&tip.block_id)
    ));
    Ok(Summary {
        height,
        tip_id: tip.block_id,
    })
}
