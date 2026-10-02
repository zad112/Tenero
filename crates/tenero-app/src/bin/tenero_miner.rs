//! The miner program: mines for a node in another process, over its control interface. `tenero-miner help`.
//! **Experimental, unaudited; nothing on any network it mines has value.**

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tenero_app::config::Network;
use tenero_app::daemon::{MiningShared, DEV_EPOCH_BLOCKS};
use tenero_app::log::{Level, Logger};
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
tenero-miner: mines for a Tenero node in another process (EXPERIMENTAL, UNAUDITED; no launched network exists)

  tenero-miner --data DIR --address tni1... --backend sha256|cpu|gpu [options]

  --data DIR         the node's data directory (the miner reads the node's cookie from it)
  --control IP:PORT  the node's control interface (default 127.0.0.1:18332, the test network's)
  --address ADDR     the wallet address block rewards are paid to
  --backend B        sha256 (the test network), cpu or gpu (the dev network's matmulhash)
  --cores N          CPU threads for the cpu backend, 1 to 6 (default 6)
  --gpu-device N     which GPU (default 0)       --gpu-batch N   attempts per batch (default 128)
  --pace SECS        wait this long after a block is found before the next job (default 0)
  --log-level L      error, warn, info, debug (default info)    --log-file FILE   also log to this file
  --status-every S   seconds between status lines when the output is not a terminal (default 60)
  --quiet            show only warnings and errors     --verbose   also show every line of the log
  --color C          auto, always or never (colour is off with NO_COLOR and when the output is not a terminal)

The miner asks the node for a block, searches for it on its own thread, and hands a found block back; the node checks it
completely. It pauses while the node is syncing and carries on if the node restarts.";

struct Args {
    data: PathBuf,
    control: std::net::SocketAddr,
    address: String,
    backend: String,
    cores: usize,
    gpu_device: usize,
    gpu_batch: usize,
    pace: u64,
    level: Level,
    log_file: Option<PathBuf>,
    status_every: u64,
    verbosity: Verbosity,
    color: ColorChoice,
}

fn parse() -> Result<Args, String> {
    let mut a = Args {
        data: PathBuf::new(),
        control: "127.0.0.1:18332".parse().expect("valid"),
        address: String::new(),
        backend: String::new(),
        cores: 6,
        gpu_device: 0,
        gpu_batch: 128,
        pace: 0,
        level: Level::Info,
        log_file: None,
        status_every: 60,
        verbosity: Verbosity::Normal,
        color: ColorChoice::Auto,
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut it = std::env::args().skip(1).peekable();
    while let Some(flag) = it.next() {
        let Some(key) = flag.strip_prefix("--") else {
            return Err(format!("unexpected argument `{flag}`"));
        };
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
                a.control = v
                    .parse()
                    .map_err(|_| format!("--control: `{v}` is not ip:port"))?
            }
            "address" => a.address = v.clone(),
            "backend" => a.backend = v.clone(),
            "cores" => a.cores = num("cores")? as usize,
            "gpu-device" => a.gpu_device = num("gpu-device")? as usize,
            "gpu-batch" => a.gpu_batch = num("gpu-batch")? as usize,
            "pace" => a.pace = num("pace")?,
            "log-level" => {
                a.level = Level::parse(&v).ok_or_else(|| {
                    format!("--log-level: `{v}` is not error, warn, info or debug")
                })?
            }
            "log-file" => a.log_file = Some(PathBuf::from(&v)),
            "status-every" => a.status_every = num("status-every")?.max(1),
            "color" => {
                a.color = ColorChoice::parse(&v)
                    .ok_or_else(|| format!("--color: `{v}` is not auto, always or never"))?
            }
            other => return Err(format!("unknown option `--{other}`")),
        }
    }
    if a.data.as_os_str().is_empty() {
        return Err("--data is required".into());
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

fn main() {
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
    let address = match tenero_wallet::Address::from_text(&args.address) {
        Ok(a) => a,
        Err(e) => {
            log.error(&format!("--address: {e}"));
            std::process::exit(2);
        }
    };
    let Some(payout) = WalletPayout::new(address) else {
        log.error("--address holds an invalid key");
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
    let implied = if args.backend == "sha256" {
        Network::Test
    } else {
        Network::Dev
    };
    screen.banner(&Banner {
        role: "miner".to_string(),
        version: format!("v{}", tenero_app::daemon::VERSION),
        network: implied.name().to_string(),
        network_note: if implied == Network::Test {
            "SHA-256 test chain, no real proof of work".to_string()
        } else {
            "development chain, real matmulhash proof of work".to_string()
        },
        details: {
            let mut d = vec![
                format!(
                    "  node     data {}, control {}",
                    args.data.display(),
                    args.control
                ),
                format!("  pays to  {}", args.address),
            ];
            if let Some(f) = &args.log_file {
                d.push(format!("  log      {} (full detail)", f.display()));
            }
            d
        },
    });

    // the first connection tells us which network the node is on, so that the backend can be checked against it
    let connect = || connect_to(&args.data, args.control);
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
    let pow = match (network, args.backend.as_str()) {
        (Network::Test, "sha256") => PowKind::Sha256,
        (Network::Dev, "cpu" | "gpu") => PowKind::Matmul,
        _ => {
            log.error(&format!(
                "the node is on the {} network, which needs {}, not --backend {}",
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
    log.info(&format!(
        "node: network {}, height {}, version {}; backend {}",
        info.network, info.height, info.version, args.backend
    ));
    let miner = match args.backend.as_str() {
        "sha256" => Miner::spawn(|| Ok(Sha256Backend)),
        "cpu" => {
            let pow = match MatmulPow::new(Params::DEFAULT, DEV_EPOCH_BLOCKS, 6) {
                Ok(p) => Arc::new(p),
                Err(e) => {
                    log.error(&e);
                    std::process::exit(1);
                }
            };
            let cores = args.cores;
            Miner::spawn(move || Ok(CpuMatmulBackend::new(pow, DEV_EPOCH_BLOCKS, cores, 10)))
        }
        _ => {
            let (device, batch) = (args.gpu_device, args.gpu_batch);
            Miner::spawn(move || {
                GpuBackend::new(device, Params::DEFAULT, DEV_EPOCH_BLOCKS, batch, 10)
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
    let cfg = RemoteMinerConfig {
        min_block_interval: Duration::from_secs(args.pace),
        log: Arc::new(move |line| l.info(&format!("miner: {line}"))),
        events,
        ..RemoteMinerConfig::default()
    };
    let mut rm = RemoteMiner::new(miner, payout, pow, cfg);
    // the status, from a thread of its own (the miner is busy in `run`): the screen once a second, and a line of the log now and then
    {
        let (counters, progress, tally) = (rm.counters(), rm.progress(), Arc::clone(&tally));
        let (l, screen, s) = (Arc::clone(&log), Arc::clone(&screen), Arc::clone(&shutdown));
        let (backend, every) = (args.backend.clone(), Duration::from_secs(args.status_every));
        std::thread::spawn(move || {
            let started = Instant::now();
            // the rate over the last few seconds, so that it neither jumps about nor lags
            let mut window: std::collections::VecDeque<(Instant, u64)> = Default::default();
            let (mut last_log, mut last_logged) = (Instant::now(), 0u64);
            while !s.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_secs(1));
                let now = Instant::now();
                let attempts = counters.attempts.load(Ordering::Relaxed);
                window.push_back((now, attempts));
                while window
                    .front()
                    .is_some_and(|(t, _)| now.duration_since(*t) > Duration::from_secs(5))
                {
                    window.pop_front();
                }
                let rate = match (window.front(), window.back()) {
                    (Some((t0, a0)), Some((t1, a1)))
                        if t1.duration_since(*t0).as_secs_f64() >= 2.0 =>
                    {
                        Some((a1 - a0) as f64 / t1.duration_since(*t0).as_secs_f64())
                    }
                    _ => None,
                };
                let link = if !progress.connected.load(Ordering::Relaxed) {
                    NodeLink::Down
                } else if progress.syncing.load(Ordering::Relaxed) {
                    NodeLink::Syncing
                } else {
                    NodeLink::Connected
                };
                let name = tally
                    .backend
                    .lock()
                    .map(|b| b.clone())
                    .ok()
                    .filter(|b| !b.is_empty())
                    .unwrap_or_else(|| backend.clone());
                screen.miner_status(&MinerStatus {
                    backend: name,
                    link,
                    node_height: progress.height.load(Ordering::Relaxed),
                    rate,
                    found: tally.found.load(Ordering::Relaxed),
                    accepted: tally.accepted.load(Ordering::Relaxed),
                    lost_race: tally.lost_race.load(Ordering::Relaxed),
                    refused: tally.refused.load(Ordering::Relaxed),
                    uptime_secs: now.duration_since(started).as_secs(),
                });
                if now.duration_since(last_log) >= every {
                    let rate = (attempts - last_logged) as f64
                        / now.duration_since(last_log).as_secs_f64().max(0.001);
                    l.info(&format!(
                        "status: {rate:.0} attempts/s | jobs {} | solutions {}",
                        counters.jobs.load(Ordering::Relaxed),
                        counters.found.load(Ordering::Relaxed)
                    ));
                    (last_log, last_logged) = (now, attempts);
                }
            }
        });
    }
    let result = rm.run(connect, &shutdown, Duration::from_millis(200));
    let s = rm.stats;
    log.log_event(
        Level::Info,
        &format!(
            "stopped: found {} blocks (in chain {}, lost a race {}, refused {})",
            s.blocks_found, s.blocks_accepted, s.blocks_lost_race, s.blocks_refused
        ),
        UiEvent::Info(format!(
            "stopped: {} blocks found, {} in the chain, {} lost a race, {} refused",
            s.blocks_found, s.blocks_accepted, s.blocks_lost_race, s.blocks_refused
        )),
    );
    if let Err(e) = result {
        log.error(&e);
        std::process::exit(1);
    }
}
