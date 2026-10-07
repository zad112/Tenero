//! The pool program: serves jobs to miners over the pool protocol, counts their shares, and pays them from its own wallet.
//! `tenero-pool help`. **Experimental and unaudited; nothing on any network it serves has value.** See `docs/RUNNING_A_POOL.md`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rand_core::OsRng;
use tenero_app::client::RemoteNode;
use tenero_app::config::Network;
use tenero_app::log::{Level, Logger};
use tenero_app::pool_core::{fee_percent_text, parse_fee_percent, Accounts};
use tenero_app::pool_net::DEFAULT_PORT;
use tenero_app::pool_server::{NodePow, Pool, PoolConfig, ReconnectingNode};
use tenero_chain::PowCheck;
use tenero_net::noise::NodeKey;
use tenero_wallet::{KdfParams, Wallet};

const USAGE: &str = "\
tenero-pool: a mining pool (EXPERIMENTAL, UNAUDITED; nothing on any network it serves has value)

  tenero-pool --data DIR --network NET --node-data DIR --control IP:PORT --wallet FILE --passphrase-file FILE [options]
  tenero-pool key --data DIR            print the pool's public key (miners pin it with --pool-key), making the key file if there is none

  --data DIR            where the pool keeps its key (pool.key), its books (pool-state.dat) and its payment record (payments.log)
  --network NET         test, dev, beta or alpha: the network of the node
  --node-data DIR       the data directory of the pool's own node (the pool reads its cookie)
  --control IP:PORT     that node's control interface (default: the network's usual port)
  --wallet FILE         the pool's wallet (made with tenero-wallet create). Every block pays it; the miners are paid from it.
                        It holds the pool's coins and nothing else should be kept in it.
  --passphrase-file F   the wallet's passphrase, in a file only the pool's user can read
  --listen ADDR         where miners connect (default 0.0.0.0:38335)
  --name TEXT           what the pool calls itself (default \"Tenero test pool\")
  --fee PERCENT         what the pool keeps of every block, 0 to 100, up to four decimals: 0.5 is a half of one percent (default 0). Publish it.
  --min-payout COINS    the least a miner is paid (default 0.1)
  --payout-every SECS   seconds between payouts (default 3600)
  --max-miners N        miners connected at once (default 256)   --per-address N   connections from one address (default 4)
  --window N            the PPLNS window, as a multiple of a block's work (default 2)
  --config FILE         settings as `key = value` lines (the names above, without the dashes); the command line wins
  --log-level L         error, warn, info, debug (default info)    --log-file FILE   also log to this file

The pool needs a node of its own on this machine (a tenerod on the same network, mining nothing). It holds no proof-of-work dataset of its own: it asks
its node to check every share's mix, so it needs little memory itself. The reward of every block goes to the pool's wallet; the pool pays its miners from it.";

struct Args {
    data: PathBuf,
    network: Option<Network>,
    node_data: Option<PathBuf>,
    control: Option<std::net::SocketAddr>,
    wallet: Option<PathBuf>,
    passphrase_file: Option<PathBuf>,
    listen: std::net::SocketAddr,
    name: String,
    /// parts per million of the reward (`--fee 0.5` is 5,000)
    fee: u64,
    min_payout: u64,
    payout_every: u64,
    max_miners: usize,
    per_address: usize,
    window: u64,
    level: Level,
    log_file: Option<PathBuf>,
}

/// Settings from the command line and a `--config` file, as `(name, value)` pairs.
fn pairs() -> Result<Vec<(String, String)>, String> {
    let mut cli: Vec<(String, String)> = Vec::new();
    let mut it = std::env::args().skip(1).peekable();
    while let Some(flag) = it.next() {
        let Some(key) = flag.strip_prefix("--") else {
            return Err(format!("unexpected argument `{flag}`"));
        };
        let v = it.next().ok_or_else(|| format!("--{key} needs a value"))?;
        if cli.iter().any(|(k, _)| k == key) {
            return Err(format!("--{key} given twice"));
        }
        cli.push((key.to_string(), v));
    }
    let mut out = cli.clone();
    if let Some((_, path)) = cli.iter().find(|(k, _)| k == "config") {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
        let mut seen: Vec<String> = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (k, v) = line
                .split_once('=')
                .ok_or_else(|| format!("{path}, line {}: expected `key = value`", n + 1))?;
            let (k, v) = (k.trim().to_string(), v.trim().to_string());
            if seen.contains(&k) {
                return Err(format!("{path}, line {}: `{k}` given twice", n + 1));
            }
            seen.push(k.clone());
            // the command line wins
            if !cli.iter().any(|(c, _)| *c == k) {
                out.push((k, v));
            }
        }
    }
    Ok(out)
}

fn parse() -> Result<Args, String> {
    let mut a = Args {
        data: PathBuf::new(),
        network: None,
        node_data: None,
        control: None,
        wallet: None,
        passphrase_file: None,
        listen: format!("0.0.0.0:{DEFAULT_PORT}").parse().expect("valid"),
        name: "Tenero test pool".into(),
        fee: 0,
        min_payout: 10_000_000,
        payout_every: 3600,
        max_miners: 256,
        per_address: 4,
        window: 2,
        level: Level::Info,
        log_file: None,
    };
    for (k, v) in pairs()? {
        let num = |what: &str| {
            v.parse::<u64>()
                .map_err(|_| format!("{what}: `{v}` is not a number"))
        };
        match k.as_str() {
            "config" => {}
            "data" => a.data = PathBuf::from(&v),
            "network" => {
                a.network = Some(Network::parse(&v).ok_or_else(|| format!("network: `{v}` is not test, dev, beta or alpha"))?)
            }
            "node-data" | "node_data" => a.node_data = Some(PathBuf::from(&v)),
            "control" => a.control = Some(v.parse().map_err(|_| format!("control: `{v}` is not ip:port"))?),
            "wallet" => a.wallet = Some(PathBuf::from(&v)),
            "passphrase-file" | "passphrase_file" => a.passphrase_file = Some(PathBuf::from(&v)),
            "listen" => a.listen = v.parse().map_err(|_| format!("listen: `{v}` is not ip:port"))?,
            "name" => {
                if v.is_empty() || v.len() > 64 || v.chars().any(char::is_control) {
                    return Err("name: 1 to 64 characters, no control characters".into());
                }
                a.name = v.clone()
            }
            "fee" => {
                a.fee = parse_fee_percent(&v).ok_or_else(|| {
                    format!("fee: `{v}` is not a percentage from 0 to 100 with at most four decimals (0.5 is a half of one percent)")
                })?
            }
            "min-payout" | "min_payout" => {
                a.min_payout = tenero_wallet::amount::parse_coins(&v)
                    .filter(|u| *u > 0)
                    .ok_or_else(|| format!("min-payout: `{v}` is not an amount of coins (up to 8 decimals, more than nothing)"))?
            }
            "payout-every" | "payout_every" => a.payout_every = num("payout-every")?.max(10),
            "max-miners" | "max_miners" => a.max_miners = num("max-miners")?.clamp(1, 100_000) as usize,
            "per-address" | "per_address" => a.per_address = num("per-address")?.clamp(1, 1000) as usize,
            "window" => a.window = num("window")?.clamp(1, 100),
            "log-level" | "log_level" => {
                a.level = Level::parse(&v).ok_or_else(|| format!("log-level: `{v}` is not error, warn, info or debug"))?
            }
            "log-file" | "log_file" => a.log_file = Some(PathBuf::from(&v)),
            other => return Err(format!("unknown setting `{other}`")),
        }
    }
    if a.data.as_os_str().is_empty() {
        return Err("--data is required".into());
    }
    Ok(a)
}

fn read_passphrase(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Ok(text.trim_end_matches(['\r', '\n']).to_string())
}

fn main() {
    if tenero_app::daemon::wants_version(std::env::args().nth(1).as_deref()) {
        println!("{}", tenero_app::daemon::version_line("tenero-pool"));
        return;
    }
    if matches!(
        std::env::args().nth(1).as_deref(),
        Some("help" | "--help" | "-h")
    ) {
        println!("{USAGE}");
        return;
    }
    // `tenero-pool key --data DIR`
    if std::env::args().nth(1).as_deref() == Some("key") {
        let rest: Vec<String> = std::env::args().skip(2).collect();
        let data = match rest.as_slice() {
            [flag, dir] if flag == "--data" => PathBuf::from(dir),
            _ => {
                eprintln!("error: usage: tenero-pool key --data DIR");
                std::process::exit(2);
            }
        };
        let _ = std::fs::create_dir_all(&data);
        match NodeKey::load_or_create(&data.join("pool.key")) {
            Ok(k) => println!("{}", tenero_core::hash::hex_lower(&k.public())),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    let args = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    let (Some(network), Some(node_data), Some(wallet_path), Some(pass_path)) = (
        args.network,
        args.node_data.clone(),
        args.wallet.clone(),
        args.passphrase_file.clone(),
    ) else {
        eprintln!(
            "error: --network, --node-data, --wallet and --passphrase-file are required\n\n{USAGE}"
        );
        std::process::exit(2);
    };
    let log = match Logger::new(args.level, args.log_file.as_deref(), true) {
        Ok(l) => Arc::new(l),
        Err(e) => {
            eprintln!("error: cannot open the log file: {e}");
            std::process::exit(2);
        }
    };
    let fail = |why: String| -> ! {
        log.error(&why);
        std::process::exit(1);
    };
    log.info(
        "tenero-pool: EXPERIMENTAL and UNAUDITED. Nothing on any network it serves has value.",
    );
    log.info(&tenero_app::daemon::version_line("tenero-pool"));
    if let Err(e) = std::fs::create_dir_all(&args.data) {
        fail(format!("cannot make {}: {e}", args.data.display()));
    }
    let pass = read_passphrase(&pass_path).unwrap_or_else(|e| fail(e));
    let wallet = Wallet::load(&wallet_path, pass.as_bytes()).unwrap_or_else(|e| {
        fail(format!(
            "cannot open the wallet {}: {e} (make it with `tenero-wallet create`)",
            wallet_path.display()
        ))
    });
    let key = NodeKey::load_or_create(&args.data.join("pool.key")).unwrap_or_else(|e| fail(e));
    let params = tenero_app::daemon::params_of(network).unwrap_or_else(|e| fail(e));
    // the books: what was kept, or empty; a file that cannot be read is never overwritten
    let state_path = args.data.join("pool-state.dat");
    let accounts = match std::fs::read(&state_path) {
        Ok(bytes) => Accounts::from_bytes(&bytes).unwrap_or_else(|e| {
            fail(format!(
                "{}: {e}. The pool will not start over it: move the file away to start with empty books",
                state_path.display()
            ))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Accounts::new(),
        Err(e) => fail(format!("cannot read {}: {e}", state_path.display())),
    };
    log.info(&format!(
        "books: {} miners, {} shares in the window, {} blocks waiting to mature",
        accounts.addresses(),
        accounts.window_len(),
        accounts.pending().len()
    ));
    let control = args.control.unwrap_or_else(|| {
        std::net::SocketAddr::from(([127, 0, 0, 1], network.default_control_port()))
    });
    let connect = {
        let (data, control) = (node_data.clone(), control);
        move || tenero_app::remote_miner::connect_to(&data, control)
    };
    let node = Arc::new(ReconnectingNode::new(Box::new(connect.clone())));
    // the pool holds NO proof-of-work dataset of its own (4 GiB): a share's mix is checked by the pool's node, which has one
    let kind = if network.real_pow() {
        tenero_core::v2::ids::PowKind::Matmul
    } else {
        tenero_core::v2::ids::PowKind::Sha256
    };
    let pow: Arc<dyn PowCheck> = Arc::new(NodePow::new(node.clone(), kind));
    let pool_address = wallet.address();
    let mut cfg = PoolConfig::new(network.name(), pool_address, params.coinbase_maturity);
    cfg.name = args.name.clone();
    cfg.fee_ppm = args.fee;
    cfg.min_payout = args.min_payout;
    cfg.payout_interval = args.payout_every;
    cfg.max_miners = args.max_miners;
    cfg.per_address = args.per_address;
    cfg.window_factor = args.window;
    let l = Arc::clone(&log);
    let pool = Pool::new(
        cfg,
        node,
        pow,
        key,
        accounts,
        Some(state_path),
        Arc::new(move |line| l.info(line)),
    );
    log.info(&format!(
        "pool key (miners pin this): {}",
        tenero_core::hash::hex_lower(&pool.public_key())
    ));
    log.info(&format!(
        "pays blocks to {} ; fee {} % ; minimum payout {} units every {} s ; PPLNS window {} blocks of work",
        pool_address.to_text(),
        fee_percent_text(args.fee),
        args.min_payout,
        args.payout_every,
        args.window
    ));
    let handle = pool
        .start(args.listen)
        .unwrap_or_else(|e| fail(format!("cannot listen on {}: {e}", args.listen)));
    log.info(&format!("listening for miners on {}", handle.addr));

    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let (s, p) = (Arc::clone(&shutdown), Arc::clone(&pool));
        let _ = ctrlc::set_handler(move || {
            if s.swap(true, Ordering::SeqCst) {
                std::process::exit(130);
            }
            p.stop();
        });
    }
    // the payouts, on a thread of their own with a connection of their own to the node
    {
        let (pool, log, s) = (Arc::clone(&pool), Arc::clone(&log), Arc::clone(&shutdown));
        let wallet_path = wallet_path.clone();
        let paylog_path = args.data.join("payments.log");
        let wallet = Arc::new(Mutex::new(wallet));
        std::thread::spawn(move || {
            let mut conn: Option<RemoteNode> = None;
            while !s.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_secs(5));
                if conn.is_none() {
                    match connect() {
                        Ok(c) => conn = Some(c),
                        Err(_) => continue,
                    }
                }
                let Some(node) = conn.as_mut() else { continue };
                let mut lines: Vec<String> = Vec::new();
                let result = {
                    let mut w = wallet.lock().expect("wallet lock");
                    let r = pool.payout_round(&mut w, node, &mut |l| lines.push(l.to_string()));
                    // the reservations are in the wallet file: it must be saved before anything else happens
                    if matches!(&r, Ok(Some(rep)) if rep.sent_any) || r.is_err() {
                        if let Err(e) = w.save(
                            &wallet_path,
                            pass.as_bytes(),
                            KdfParams::DEFAULT,
                            &mut OsRng,
                        ) {
                            log.error(&format!("cannot save the pool's wallet: {e}"));
                        }
                    }
                    r
                };
                if !lines.is_empty() {
                    use std::io::Write as _;
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&paylog_path)
                    {
                        for l in &lines {
                            let _ = writeln!(f, "{l}");
                        }
                    }
                }
                if let Err(e) = result {
                    log.warn(&format!("payout: {e}"));
                    if e.contains("lost the node") {
                        conn = None;
                    }
                }
            }
        });
    }
    // the status line, until the program is told to stop
    let mut last = Instant::now();
    while !shutdown.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(200));
        if last.elapsed() >= Duration::from_secs(60) {
            last = Instant::now();
            let st = &pool.stats;
            let a = pool.accounts();
            log.info(&format!(
                "status: miners {} | shares {} (stale {}, refused {}) | blocks found {} (in chain {}, lost {}, refused {}) | owed {} units, {} blocks maturing | paid {} units",
                pool.sessions_now(),
                st.shares_accepted.load(Ordering::Relaxed),
                st.shares_stale.load(Ordering::Relaxed),
                st.shares_low.load(Ordering::Relaxed) + st.shares_bad_mix.load(Ordering::Relaxed) + st.shares_unknown_job.load(Ordering::Relaxed),
                st.blocks_found.load(Ordering::Relaxed),
                st.blocks_in_chain.load(Ordering::Relaxed),
                st.blocks_lost_race.load(Ordering::Relaxed),
                st.blocks_refused.load(Ordering::Relaxed),
                a.total_balances(),
                a.pending().len(),
                a.total_paid
            ));
        }
    }
    pool.save_state();
    log.info("stopped");
}
