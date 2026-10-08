//! The miner program: mines for a node in another process, over its control interface. `tenero-miner help`.
//! **Experimental, unaudited; nothing on any network it mines has value.**

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tenero_app::client::RemoteNode;
use tenero_app::config::Network;
use tenero_app::daemon::MiningShared;
use tenero_app::log::{Level, Logger};
use tenero_app::pool_miner::{PoolMiner, PoolMinerConfig};
use tenero_app::remote_miner::{connect_to, RemoteMiner, RemoteMinerConfig};
use tenero_app::ui::{
    miner_event_to_ui, Banner, ColorChoice, Event as UiEvent, MinerStatus, NodeLink, Screen,
    Verbosity,
};
use tenero_chain::MatmulPow;
use tenero_core::matmulhash::Params;
use tenero_core::v2::ids::PowKind;
use tenero_miner::gpu::GpuBackend;
use tenero_miner::{CpuMatmulBackend, Miner, Sha256Backend, WalletPayout, MAX_CORES};

const USAGE: &str = "\
tenero-miner: mines for a Tenero node in another process (EXPERIMENTAL, UNAUDITED; test networks only, nothing on them has value)

  tenero-miner --data DIR --address TENg... --backend sha256|cpu|gpu [options]

  --data DIR         the node's data directory (the miner reads the node's cookie from it)
  --control IP:PORT  the node's control interface (default: the address's network's port on this machine, 127.0.0.1:38352
                     for gamma)
  --node HOST:PORT   instead of --data and --control: the miner service of a node on ANOTHER computer (default port 38334),
  --key HEX          and the key its operator gave you (64 hexadecimal digits). The node builds the blocks and checks yours; this
                     miner refuses a block that does not pay --address, but cannot tell a stale or wrong chain from the real one
  --address ADDR     the wallet's MAIN address block rewards are paid to (with --pool: where the pool is to pay you). Its
                     first letters say its network (TENg gamma, TENd dev, TENt test): a node or pool of another network is refused
  --pool HOST:PORT   instead of a node: work for a MINING POOL (or `default`: the pool built into this program, if any). The pool keeps the
  --pool-key HEX     block rewards and pays you by its own rules; nothing makes it pay. The pool's public key (64 hexadecimal digits, from its
                     operator) is PINNED: the miner refuses a pool that proves another. `--pool-unpinned` goes without (a person between
                     you and the pool would not be noticed)
  --network NET      with --pool: the network the pool serves (gamma, dev or test; default: the address's), so that a pool of
                     another network is refused
  --worker NAME      with --pool: a name for this machine, shown to the pool (default: the computer's name, at most 32 characters)
  --backend B        sha256 (the test network), cpu or gpu (the dev network's matmulhash)
  --cores N          CPU threads for the cpu backend, 1 to 6 (default 6)
  --gpu-device N     which GPU (default 0)       --gpu-batch N|auto   attempts per batch (default 128; auto measures at start-up)
  --pace SECS        wait this long after a block is found before the next job (default 0)
  --log-level L      error, warn, info, debug (default info)    --log-file FILE   also log to this file
  --status-file FILE rewrite this file every second with the miner's state, for a program that started it
  --status-every S   seconds between status lines when the output is not a terminal (default 60)
  --quiet            show only warnings and errors     --verbose   also show every line of the log
  --color C          auto, always or never (colour is off with NO_COLOR and when the output is not a terminal)

The miner asks the node for a block, searches for it on its own thread, and hands a found block back; the node checks it
completely. It pauses while the node is syncing and carries on if the node restarts.";

struct Args {
    data: PathBuf,
    control: std::net::SocketAddr,
    /// Whether `--control` was given (otherwise the address's network's default is used).
    control_given: bool,
    node: Option<std::net::SocketAddr>,
    key: Option<[u8; 32]>,
    pool: Option<String>,
    pool_key: Option<[u8; 32]>,
    pool_unpinned: bool,
    network: Option<String>,
    worker: String,
    address: String,
    backend: String,
    cores: usize,
    gpu_device: usize,
    gpu_batch: usize,
    gpu_batch_auto: bool,
    pace: u64,
    level: Level,
    log_file: Option<PathBuf>,
    status_every: u64,
    status_file: Option<PathBuf>,
    verbosity: Verbosity,
    color: ColorChoice,
}

fn parse() -> Result<Args, String> {
    let mut a = Args {
        data: PathBuf::new(),
        control: "127.0.0.1:38352".parse().expect("valid"),
        control_given: false,
        node: None,
        key: None,
        pool: None,
        pool_key: None,
        pool_unpinned: false,
        network: None,
        worker: String::new(),
        address: String::new(),
        backend: String::new(),
        cores: 6,
        gpu_device: 0,
        gpu_batch: 128,
        gpu_batch_auto: false,
        pace: 0,
        level: Level::Info,
        log_file: None,
        status_every: 60,
        status_file: None,
        verbosity: Verbosity::Normal,
        color: ColorChoice::Auto,
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut it = std::env::args().skip(1).peekable();
    while let Some(flag) = it.next() {
        let Some(key) = flag.strip_prefix("--") else {
            return Err(format!("unexpected argument `{flag}`"));
        };
        // `--pool-unpinned` stands alone
        if key == "pool-unpinned" {
            if !seen.insert(key.to_string()) {
                return Err(format!("--{key} given twice"));
            }
            a.pool_unpinned = true;
            continue;
        }
        // `--quiet` and `--verbose` stand alone
        if matches!(key, "quiet" | "verbose") {
            if !seen.insert(key.to_string()) {
                return Err(format!("--{key} given twice"));
            }
            a.verbosity = if key == "quiet" {
                Verbosity::Quiet
            } else {
                Verbosity::Verbose
            };
            if seen.contains("quiet") && seen.contains("verbose") {
                return Err("--quiet and --verbose cannot be combined".into());
            }
            continue;
        }
        let v = it.next().ok_or_else(|| format!("--{key} needs a value"))?;
        if !seen.insert(key.to_string()) {
            return Err(format!("--{key} given twice"));
        }
        let num = |what: &str| {
            v.parse::<u64>()
                .map_err(|_| format!("--{what}: `{v}` is not a number"))
        };
        match key {
            "data" => a.data = PathBuf::from(&v),
            "control" => {
                a.control_given = true;
                a.control = v
                    .parse()
                    .map_err(|_| format!("--control: `{v}` is not ip:port"))?
            }
            "node" => {
                a.node = Some(
                    v.parse()
                        .map_err(|_| format!("--node: `{v}` is not ip:port"))?,
                )
            }
            "key" => {
                if v.len() != 64 || !v.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err("--key must be 64 hexadecimal digits".into());
                }
                let mut k = [0u8; 32];
                for (i, b) in k.iter_mut().enumerate() {
                    *b = u8::from_str_radix(&v[2 * i..2 * i + 2], 16).expect("checked");
                }
                a.key = Some(k);
            }
            "pool" => a.pool = Some(v.clone()),
            "pool-key" => {
                if v.len() != 64 || !v.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err("--pool-key must be 64 hexadecimal digits".into());
                }
                let mut k = [0u8; 32];
                for (i, b) in k.iter_mut().enumerate() {
                    *b = u8::from_str_radix(&v[2 * i..2 * i + 2], 16).expect("checked");
                }
                a.pool_key = Some(k);
            }
            "network" => a.network = Some(v.clone()),
            "worker" => {
                if v.is_empty() || v.chars().count() > 32 || v.chars().any(char::is_control) {
                    return Err("--worker: 1 to 32 characters, no control characters".into());
                }
                a.worker = v.clone()
            }
            "address" => a.address = v.clone(),
            "backend" => a.backend = v.clone(),
            "cores" => a.cores = num("cores")? as usize,
            "gpu-device" => a.gpu_device = num("gpu-device")? as usize,
            "gpu-batch" if v == "auto" => a.gpu_batch_auto = true,
            "gpu-batch" => a.gpu_batch = num("gpu-batch")? as usize,
            "pace" => a.pace = num("pace")?,
            "log-level" => {
                a.level = Level::parse(&v).ok_or_else(|| {
                    format!("--log-level: `{v}` is not error, warn, info or debug")
                })?
            }
            "log-file" => a.log_file = Some(PathBuf::from(&v)),
            "status-file" => a.status_file = Some(PathBuf::from(&v)),
            "status-every" => a.status_every = num("status-every")?.max(1),
            "color" => {
                a.color = ColorChoice::parse(&v)
                    .ok_or_else(|| format!("--color: `{v}` is not auto, always or never"))?
            }
            other => return Err(format!("unknown option `--{other}`")),
        }
    }
    if a.pool.is_some() {
        if a.node.is_some() || a.key.is_some() || seen.contains("data") || seen.contains("control")
        {
            return Err("--pool replaces --data, --control, --node and --key: a miner works for a pool OR mines on a node, never both".into());
        }
        if a.pool_unpinned && a.pool_key.is_some() {
            return Err("--pool-key and --pool-unpinned cannot be combined".into());
        }
        if a.pool.as_deref() != Some("default") && a.pool_key.is_none() && !a.pool_unpinned {
            return Err("--pool needs --pool-key (the pool's operator gives it to you), or --pool-unpinned to go without".into());
        }
        if a.worker.is_empty() {
            a.worker = std::env::var("COMPUTERNAME")
                .or_else(|_| std::env::var("HOSTNAME"))
                .unwrap_or_else(|_| "miner".to_string())
                .chars()
                .filter(|c| !c.is_control())
                .take(32)
                .collect();
        }
    } else if a.pool_key.is_some() || a.pool_unpinned || a.network.is_some() || !a.worker.is_empty()
    {
        return Err("--pool-key, --pool-unpinned, --network and --worker are for --pool".into());
    } else if a.node.is_some() {
        if seen.contains("data") || seen.contains("control") {
            return Err("--node replaces --data and --control: give one or the other".into());
        }
        if a.key.is_none() {
            return Err("--node needs --key (the node's operator gives it to you)".into());
        }
    } else if a.key.is_some() {
        return Err("--key is for --node".into());
    } else if a.data.as_os_str().is_empty() {
        return Err(
            "--data is required (or --node HOST:PORT for a node on another computer, or --pool)"
                .into(),
        );
    }
    if a.address.is_empty() {
        return Err("--address is required".into());
    }
    if !matches!(a.backend.as_str(), "sha256" | "cpu" | "gpu") {
        return Err("--backend must be sha256, cpu or gpu".into());
    }
    if a.cores == 0 || a.cores > MAX_CORES {
        return Err(format!("--cores must be 1 to {MAX_CORES}"));
    }
    if a.gpu_batch == 0 {
        return Err("--gpu-batch must be at least 1".into());
    }
    Ok(a)
}

/// Where a pool is and what to pin.
struct PoolTarget {
    addr: std::net::SocketAddr,
    pin: Option<[u8; 32]>,
    network: Network,
}

/// The pool the arguments name: an address (a name is looked up) and the key to pin, or the pool built into the program.
fn resolve_pool(spec: &str, args: &Args, address_network: Network) -> Result<PoolTarget, String> {
    use std::net::ToSocketAddrs;
    let network = match &args.network {
        Some(n) => Network::parse(n)
            .ok_or_else(|| format!("--network: `{n}` is not gamma, dev or test"))?,
        None => address_network,
    };
    if spec == "default" {
        let Some((addr, key)) = tenero_app::pool_miner::default_pool(network) else {
            return Err(format!(
                "no pool is built into this program for the {} network yet: give --pool HOST:PORT and --pool-key",
                network.name()
            ));
        };
        let addr: std::net::SocketAddr = addr
            .parse()
            .map_err(|_| "the built-in pool address is not valid".to_string())?;
        return Ok(PoolTarget {
            addr,
            pin: Some(key),
            network,
        });
    }
    let addr = spec
        .to_socket_addrs()
        .map_err(|e| format!("--pool: cannot look up `{spec}`: {e} (it must be HOST:PORT)"))?
        .next()
        .ok_or_else(|| format!("--pool: `{spec}` has no address"))?;
    Ok(PoolTarget {
        addr,
        pin: args.pool_key,
        network,
    })
}

fn main() {
    if tenero_app::daemon::wants_version(std::env::args().nth(1).as_deref()) {
        println!("{}", tenero_app::daemon::version_line("tenero-miner"));
        return;
    }
    if matches!(
        std::env::args().nth(1).as_deref(),
        Some("help" | "--help" | "-h")
    ) {
        println!("{USAGE}");
        return;
    }
    let args = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    let screen = Arc::new(Screen::for_stderr(
        args.color,
        args.verbosity,
        args.status_every,
    ));
    let log = match Logger::new(args.level, args.log_file.as_deref(), false) {
        Ok(l) => Arc::new(l.with_screen(Arc::clone(&screen))),
        Err(e) => {
            eprintln!("error: cannot open the log file: {e}");
            std::process::exit(2);
        }
    };
    let address = match tenero_wallet::Address::parse_any(&args.address) {
        Ok(a) => a,
        Err(e) => {
            log.error(&format!("--address: {e}"));
            std::process::exit(2);
        }
    };
    let address_network = match Network::ALL
        .into_iter()
        .find(|n| n.wallet_network() == address.network)
    {
        Some(n) => n,
        None => {
            log.error("--address: a network this program does not run");
            std::process::exit(2);
        }
    };
    let mut args = args;
    if !args.control_given {
        args.control =
            std::net::SocketAddr::from(([127, 0, 0, 1], address_network.default_control_port()));
    }
    let Some(payout) = WalletPayout::new(address) else {
        log.error(
            "--address: a block reward is paid to a MAIN address only (not a subaddress or an integrated address)",
        );
        std::process::exit(2);
    };
    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let (s, l) = (Arc::clone(&shutdown), Arc::clone(&log));
        let _ = ctrlc::set_handler(move || {
            if s.swap(true, Ordering::SeqCst) {
                std::process::exit(130);
            }
            l.log_event(Level::Info, "shutdown requested", UiEvent::ShuttingDown);
        });
    }
    log.info(
        "tenero-miner: EXPERIMENTAL and UNAUDITED. Nothing on the test or dev networks has value.",
    );
    // the banner names the network the backend belongs to (the node is asked below, and a mismatch is an error)
    // (cpu and gpu mine the real proof of work on dev and on gamma; the node's answer below says which)
    let implied = if args.backend == "sha256" {
        Network::Test
    } else {
        Network::Dev
    };
    screen.banner(&Banner {
        role: "miner".to_string(),
        version: format!("v{}", tenero_app::daemon::VERSION),
        // before it has asked the node, the miner only knows the backend: cpu and gpu mine the real proof of work on dev AND on gamma, so it
        // must not say "dev" (it did, while mining alpha with an earlier version)
        network: if implied == Network::Test {
            implied.name().to_string()
        } else {
            "real proof of work (gamma or dev: the node says which)".to_string()
        },
        network_note: if implied == Network::Test {
            "SHA-256 test chain, no real proof of work".to_string()
        } else {
            "matmulhash".to_string()
        },
        details: {
            let mut d = vec![
                match (&args.pool, args.node) {
                    (Some(p), _) => format!(
                        "  pool     {p} (the rewards go to the POOL, which pays you by its own rules)"
                    ),
                    (None, Some(n)) => {
                        format!("  node     {n} (the miner service of a node on another computer)")
                    }
                    (None, None) => format!(
                        "  node     data {}, control {}",
                        args.data.display(),
                        args.control
                    ),
                },
                format!("  pays to  {}", args.address),
            ];
            if let Some(f) = &args.log_file {
                d.push(format!("  log      {} (full detail)", f.display()));
            }
            d
        },
    });

    // a pool miner never talks to a node: it needs only the network's name (to refuse a pool of another network)
    let pool_target: Option<PoolTarget> = match &args.pool {
        None => None,
        Some(spec) => match resolve_pool(spec, &args, address_network) {
            Ok(t) => Some(t),
            Err(e) => {
                log.error(&e);
                std::process::exit(2);
            }
        },
    };
    // the first connection tells us which network the node is on, so that the backend can be checked against it
    let connect = || match args.node {
        Some(n) => RemoteNode::connect_miner_service(n, args.key.as_ref()),
        None => connect_to(&args.data, args.control),
    };
    let network = if let Some(t) = &pool_target {
        t.network
    } else {
        let info = loop {
            if shutdown.load(Ordering::SeqCst) {
                return;
            }
            match connect().and_then(|n| n.info()) {
                Ok(i) => break i,
                Err(e) => {
                    log.warn(&format!("cannot reach the node: {e} (trying again)"));
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        };
        let Some(network) = Network::parse(&info.network) else {
            log.error(&format!(
                "the node is on a network this miner does not know: {}",
                info.network
            ));
            std::process::exit(2);
        };
        log.info(&format!(
            "node: network {}, height {}, version {}; backend {}",
            info.network, info.height, info.version, args.backend
        ));
        network
    };
    if network != address_network {
        log.error(&format!(
            "--address is a {} address, and the {} is on the {} network: rewards there could never reach it",
            address_network.name(),
            if pool_target.is_some() { "pool" } else { "node" },
            network.name()
        ));
        std::process::exit(2);
    }
    let pow = match (network, args.backend.as_str()) {
        (Network::Test, "sha256") => PowKind::Sha256,
        (n, "cpu" | "gpu") if n.real_pow() => PowKind::Matmul,
        _ => {
            log.error(&format!(
                "the {} network needs {}, not --backend {}",
                network.name(),
                if network == Network::Test {
                    "sha256"
                } else {
                    "cpu or gpu"
                },
                args.backend
            ));
            std::process::exit(2);
        }
    };
    if let Some(t) = &pool_target {
        log.info(&format!(
            "pool: {} on the {} network; backend {}",
            t.addr,
            network.name(),
            args.backend
        ));
        log.warn("the block rewards of a pool go to the POOL, which pays you by its own rules: nothing makes it pay. To keep the rewards, mine on your own node.");
    }
    let epoch = network.epoch_blocks();
    let miner = match args.backend.as_str() {
        "sha256" => Miner::spawn(|| Ok(Sha256Backend)),
        "cpu" => {
            let pow = match MatmulPow::new(Params::DEFAULT, epoch, 6) {
                Ok(p) => Arc::new(p.gathered_from(network.gather_from())),
                Err(e) => {
                    log.error(&e);
                    std::process::exit(1);
                }
            };
            let cores = args.cores;
            Miner::spawn(move || Ok(CpuMatmulBackend::new(pow, epoch, cores, 10)))
        }
        _ => {
            let device = args.gpu_device;
            let batch = if args.gpu_batch_auto {
                let l = Arc::clone(&log);
                tenero_miner::gpu::auto_batch(
                    device,
                    Params::DEFAULT,
                    epoch,
                    args.gpu_batch,
                    &move |m| l.info(&format!("miner: {m}")),
                )
            } else {
                args.gpu_batch
            };
            let gather_from = network.gather_from();
            Miner::spawn(move || {
                GpuBackend::new(device, Params::DEFAULT, epoch, batch)
                    .map(|b| b.gathered_from(gather_from))
            })
        }
    };
    let l = Arc::clone(&log);
    let tally = Arc::new(MiningShared::default());
    let events = {
        let (log, tally) = (Arc::clone(&log), Arc::clone(&tally));
        Arc::new(move |e: tenero_miner::MinerEvent| {
            tally.record(&e);
            if let (Some(ev), Some(sc)) = (miner_event_to_ui(&e), log.screen()) {
                sc.event(&ev);
            }
        })
    };
    let cfg_events = events;
    // what the status thread reads, whichever way this miner works: the backend's counters, and (connected, syncing, height)
    type View = Arc<dyn Fn() -> (bool, bool, u64) + Send + Sync>;
    type Runner = Box<dyn FnOnce(&AtomicBool) -> (Result<(), String>, String)>;
    let (counters, view, runner): (Arc<tenero_miner::Counters>, View, Runner) =
        if let Some(target) = pool_target {
            let mut pcfg = PoolMinerConfig::new(network.name(), &args.address, &args.worker);
            let l2 = Arc::clone(&l);
            pcfg.log = Arc::new(move |line| l2.info(&format!("miner: {line}")));
            pcfg.events = cfg_events;
            let mut pm = PoolMiner::new(miner, pow, pcfg);
            let progress = pm.progress();
            let view: View = Arc::new(move || {
                (
                    progress.connected.load(Ordering::Relaxed),
                    false,
                    progress.height.load(Ordering::Relaxed),
                )
            });
            let counters = pm.counters();
            let (addr, pin) = (target.addr, target.pin);
            let runner: Runner = Box::new(move |shutdown| {
                let result = pm.run(|| tenero_app::pool_miner::connect(addr, pin), shutdown);
                let s = pm.stats;
                (
                    result,
                    format!(
                        "stopped: {} shares sent, {} accepted, {} stale, {} refused",
                        s.shares_sent, s.shares_accepted, s.shares_stale, s.shares_rejected
                    ),
                )
            });
            (counters, view, runner)
        } else {
            let cfg = RemoteMinerConfig {
                min_block_interval: Duration::from_secs(args.pace),
                log: Arc::new(move |line| l.info(&format!("miner: {line}"))),
                events: cfg_events,
                ..RemoteMinerConfig::default()
            };
            let mut rm = RemoteMiner::new(miner, payout, pow, cfg);
            let progress = rm.progress();
            let view: View = Arc::new(move || {
                (
                    progress.connected.load(Ordering::Relaxed),
                    progress.syncing.load(Ordering::Relaxed),
                    progress.height.load(Ordering::Relaxed),
                )
            });
            let counters = rm.counters();
            // a node on another computer limits how often it is asked (120 requests a minute by default): once a second
            let poll = if args.node.is_some() {
                Duration::from_secs(1)
            } else {
                Duration::from_millis(200)
            };
            let (node, key, data, control) = (args.node, args.key, args.data.clone(), args.control);
            let runner: Runner = Box::new(move |shutdown| {
                let connect = move || match node {
                    Some(n) => RemoteNode::connect_miner_service(n, key.as_ref()),
                    None => connect_to(&data, control),
                };
                let result = rm.run(connect, shutdown, poll);
                let s = rm.stats;
                (
                    result,
                    format!(
                        "stopped: found {} blocks (in chain {}, lost a race {}, refused {})",
                        s.blocks_found, s.blocks_accepted, s.blocks_lost_race, s.blocks_refused
                    ),
                )
            });
            (counters, view, runner)
        };
    // the status, from a thread of its own (the miner is busy in `run`): the screen once a second, and a line of the log now and then
    {
        let tally = Arc::clone(&tally);
        let (l, screen, s) = (Arc::clone(&log), Arc::clone(&screen), Arc::clone(&shutdown));
        let (backend, every) = (args.backend.clone(), Duration::from_secs(args.status_every));
        let pool_mode = args.pool.is_some();
        let status_file = args.status_file.clone();
        // the card's health for the screen (GPU only); if NVML cannot be read the miner is not affected
        let probe = if args.backend == "gpu" {
            match tenero_miner::gpu_stats::GpuProbe::open(args.gpu_device) {
                Ok(p) => Some(p),
                Err(e) => {
                    log.info(&format!("GPU readings are not available: {e}"));
                    None
                }
            }
        } else {
            None
        };
        std::thread::spawn(move || {
            let started = Instant::now();
            // the rates: 10 s, 60 s, 15 min and the run, over the time spent searching (see `tenero_miner::rate`)
            let mut meter = tenero_miner::rate::RateMeter::new();
            let mut gpu = None;
            let mut last_log = Instant::now();
            while !s.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_secs(1));
                let now = Instant::now();
                let attempts = counters.attempts.load(Ordering::Relaxed);
                if let Some(p) = &probe {
                    gpu = Some(p.read());
                }
                meter.record(
                    now.duration_since(started).as_millis() as u64,
                    attempts,
                    counters.searching(),
                );
                let (connected, syncing, height) = view();
                let link = match (pool_mode, connected, syncing) {
                    (true, true, _) => NodeLink::PoolConnected,
                    (true, false, _) => NodeLink::PoolDown,
                    (false, false, _) => NodeLink::Down,
                    (false, true, true) => NodeLink::Syncing,
                    (false, true, false) => NodeLink::Connected,
                };
                let name = tally
                    .backend
                    .lock()
                    .map(|b| b.clone())
                    .ok()
                    .filter(|b| !b.is_empty())
                    .unwrap_or_else(|| backend.clone());
                let status = MinerStatus {
                    backend: name,
                    link,
                    node_height: height,
                    rates: meter.rates(),
                    gpu: gpu.clone(),
                    luck: tally
                        .luck_from(&counters, now.duration_since(started).as_millis() as u64),
                    found: tally.found.load(Ordering::Relaxed),
                    accepted: tally.accepted.load(Ordering::Relaxed),
                    lost_race: tally.lost_race.load(Ordering::Relaxed),
                    refused: tally.refused.load(Ordering::Relaxed),
                    uptime_secs: now.duration_since(started).as_secs(),
                };
                screen.miner_status(&status);
                if let Some(path) = &status_file {
                    let unix = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs());
                    let text =
                        tenero_app::miner_report::MinerReport::from_status(&status, unix).to_text();
                    // best effort: a screen file that cannot be written must never stop the miner
                    let _ = tenero_net::transport::write_atomic(path, text.as_bytes());
                }
                if now.duration_since(last_log) >= every {
                    l.info(&format!(
                        "status: attempts/s {} | jobs {} | solutions {}",
                        tenero_app::ui::rates_text(&meter.rates()),
                        counters.jobs.load(Ordering::Relaxed),
                        counters.found.load(Ordering::Relaxed)
                    ));
                    last_log = now;
                }
            }
        });
    }
    let (result, summary) = runner(&shutdown);
    log.log_event(Level::Info, &summary, UiEvent::Info(summary.clone()));
    if let Err(e) = result {
        log.error(&e);
        std::process::exit(1);
    }
}
