//! A node of a private TEST network: the real engine, real sockets, the Noise channel, a real store on disk, on the
//! SHA-256 test chain (which a CPU mines in an instant). For the M8.4 soak test, and for measuring a node's memory
//! and threads with many peers. **Not a real node: no wallet, no real proof of work, nothing to lose, unaudited.**
//!
//! ```text
//! cargo run --release -p tenero-net --example p2p_testnode -- \
//!     --data C:\scratch\node1 --listen 127.0.0.1:18331 --mine-every 60
//! cargo run --release -p tenero-net --example p2p_testnode -- \
//!     --data C:\scratch\node2 --listen 127.0.0.2:18331 --seed 127.0.0.1:18331 --mine-every 60
//! ```
//!
//! Options: `--data DIR` (required: the chain, the node key and the saved peers live there), `--listen IP:PORT`,
//! `--seed IP:PORT` (repeat), `--mine-every SECS` (a block about this often, with some jitter; 0 or absent: never), `--mine-for SECS` (stop
//! mining after this long, so a run can end with every node just listening),
//! `--status-every SECS` (default 30), `--duration SECS` (stop cleanly after this long), `--max-inbound N`
//! (default 64) and `--peer-target N` (default 8). Typing `nomine` and Enter stops it mining (it keeps running,
//! syncing and relaying); typing `quit` and Enter stops it cleanly (and saves its peers).
//! Nodes on one machine need different loopback addresses (127.0.0.1, 127.0.0.2, ...): the engine does not connect
//! to two peers on one host.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tenero_chain::{ProofsNotChecked, Sha256Pow};
use tenero_core::v2::ids::PowKind;
use tenero_net::addrbook::AddrBookConfig;
use tenero_net::noise::NodeKey;
use tenero_net::sim::{mine_test_block, test_chain_params, LABEL};
use tenero_net::transport::{Counters, Hooks, Net, NetConfig};
use tenero_net::{Engine, EngineConfig, Event};
use tenero_node::{Node, NodeConfig, Payout};
use tenero_store::Store;

struct Args {
    data: PathBuf,
    listen: Option<SocketAddr>,
    seeds: Vec<SocketAddr>,
    mine_every: u64,
    mine_for: u64,
    status_every: u64,
    duration: u64,
    max_inbound: usize,
    peer_target: usize,
}

fn parse() -> Result<Args, String> {
    let mut a = Args {
        data: PathBuf::new(),
        listen: None,
        seeds: vec![],
        mine_every: 0,
        mine_for: 0,
        status_every: 30,
        duration: 0,
        max_inbound: 64,
        peer_target: 8,
    };
    let mut it = std::env::args().skip(1);
    let mut have_data = false;
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--data" => {
                a.data = PathBuf::from(value()?);
                have_data = true;
            }
            "--listen" => a.listen = Some(value()?.parse().map_err(|_| "bad --listen")?),
            "--seed" => a.seeds.push(value()?.parse().map_err(|_| "bad --seed")?),
            "--mine-every" => a.mine_every = value()?.parse().map_err(|_| "bad --mine-every")?,
            "--mine-for" => a.mine_for = value()?.parse().map_err(|_| "bad --mine-for")?,
            "--status-every" => {
                a.status_every = value()?.parse().map_err(|_| "bad --status-every")?
            }
            "--duration" => a.duration = value()?.parse().map_err(|_| "bad --duration")?,
            "--max-inbound" => a.max_inbound = value()?.parse().map_err(|_| "bad --max-inbound")?,
            "--peer-target" => a.peer_target = value()?.parse().map_err(|_| "bad --peer-target")?,
            other => return Err(format!("unknown option {other}")),
        }
    }
    if !have_data {
        return Err("--data DIR is required".into());
    }
    Ok(a)
}

/// Mines a block every so often, prints a status line, and stops the node after `--duration`.
struct Runner {
    mine_every: Duration,
    next_mine: Instant,
    status_every: Duration,
    next_status: Instant,
    stop_at: Option<Instant>,
    shutdown: Arc<AtomicBool>,
    counters: Arc<Counters>,
    jitter: u64,
    payout_seed: [u8; 32],
    mined: u64,
    mining: Arc<AtomicBool>,
    mine_until: Option<Instant>,
}

impl Hooks for Runner {
    fn poll(&mut self, engine: &mut Engine<'_>, _now_ms: u64) -> Vec<Event> {
        let now = Instant::now();
        if self.stop_at.is_some_and(|t| now >= t) {
            self.shutdown.store(true, Ordering::SeqCst);
        }
        if now >= self.next_status {
            self.next_status = now + self.status_every;
            let (h, tip) = engine.node().store().tip().unwrap();
            let c = &self.counters;
            eprintln!(
                "status: tip {h} ({}) | peers {} (in {}, out {}) | book {} | blocks applied {}, mined {} | bans {} | bytes in {}, out {} | threads {}",
                tip.block_id[..4].iter().map(|b| format!("{b:02x}")).collect::<String>(),
                engine.peer_count(),
                engine.inbound_count(),
                engine.outbound_count(),
                engine.addr_book().len(),
                engine.stats.blocks_applied,
                self.mined,
                engine.stats.bans,
                c.bytes_in.load(Ordering::Relaxed),
                c.bytes_out.load(Ordering::Relaxed),
                c.threads.load(Ordering::Relaxed),
            );
        }
        if self.mine_until.is_some_and(|t| now >= t) {
            self.mining.store(false, Ordering::SeqCst);
        }
        if self.mine_every.is_zero() || now < self.next_mine || !self.mining.load(Ordering::SeqCst)
        {
            return Vec::new();
        }
        // the next block in 0.5 to 1.5 times the interval, so three nodes do not always mine together
        self.jitter = self
            .jitter
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let f = 500 + (self.jitter >> 33) % 1000;
        self.next_mine = now + self.mine_every * f as u32 / 1000;
        let tip = engine.node().store().tip().unwrap().1;
        let clock = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let ts = clock.max(tip.header.timestamp + 1);
        self.mined += 1;
        let mut seed = self.payout_seed;
        seed[..8].copy_from_slice(&self.mined.to_le_bytes());
        let payout = Payout {
            onetime_address: seed,
            view_tag: [1; 3],
            ephemeral_pubkey: seed,
            anchor_enc: [3; 16],
        };
        vec![Event::LocalBlock(mine_test_block(
            engine.node(),
            ts,
            payout,
        ))]
    }
}

fn main() {
    let args = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n(see the top of crates/tenero-net/examples/p2p_testnode.rs)");
            std::process::exit(2);
        }
    };
    std::fs::create_dir_all(&args.data).expect("create the data directory");
    let store =
        Store::open(args.data.join("chain.redb"), LABEL, PowKind::Sha256).expect("open the chain");
    let params = test_chain_params();
    let key = NodeKey::load_or_create(&args.data.join("node.key")).expect("the node key");
    let node = Node::with_proof_check(
        &store,
        &params,
        &Sha256Pow,
        &ProofsNotChecked,
        NodeConfig {
            allow_unchecked_proofs_for_tests: true,
            ..NodeConfig::default()
        },
    )
    .expect("a node");
    let pk = key.public();
    let cfg = EngineConfig {
        addrbook: AddrBookConfig {
            accept_private: true,
            ..AddrBookConfig::default()
        },
        seeds: args.seeds.iter().map(|s| s.to_string()).collect(),
        peer_target: args.peer_target,
        outbound_target: args.peer_target.min(8),
        max_inbound: args.max_inbound,
        max_peers: args.max_inbound + 64,
        nonce: u64::from_le_bytes(pk[..8].try_into().unwrap()) | 1,
        ..EngineConfig::default()
    };
    let mut engine = Engine::new(node, cfg);
    let mut net_cfg = NetConfig::new(key, store.chain_id());
    net_cfg.listen = args.listen;
    net_cfg.state_path = Some(args.data.join("peers.dat"));
    let net = Net::bind(net_cfg).expect("bind");
    let shutdown = Arc::new(AtomicBool::new(false));
    eprintln!(
        "test node: data {}, listening on {:?}, node key {}, chain tip {}",
        args.data.display(),
        net.local_addr(),
        pk[..6]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
        store.tip().unwrap().0
    );
    // "quit" on standard input stops the node cleanly, "nomine" stops its mining
    let mining = Arc::new(AtomicBool::new(true));
    {
        let (shutdown, mining) = (Arc::clone(&shutdown), Arc::clone(&mining));
        std::thread::spawn(move || {
            let mut line = String::new();
            while std::io::stdin()
                .read_line(&mut line)
                .map(|n| n > 0)
                .unwrap_or(false)
            {
                match line.trim() {
                    "quit" => {
                        shutdown.store(true, Ordering::SeqCst);
                        return;
                    }
                    "nomine" => {
                        mining.store(false, Ordering::SeqCst);
                        eprintln!("mining stopped");
                    }
                    _ => {}
                }
                line.clear();
            }
        });
    }
    let mut runner = Runner {
        mine_every: Duration::from_secs(args.mine_every),
        next_mine: Instant::now() + Duration::from_secs(5),
        status_every: Duration::from_secs(args.status_every.max(1)),
        next_status: Instant::now() + Duration::from_secs(2),
        stop_at: (args.duration > 0).then(|| Instant::now() + Duration::from_secs(args.duration)),
        shutdown: Arc::clone(&shutdown),
        counters: net.counters(),
        jitter: u64::from_le_bytes(pk[8..16].try_into().unwrap()),
        payout_seed: pk,
        mined: 0,
        mining,
        mine_until: (args.mine_for > 0)
            .then(|| Instant::now() + Duration::from_secs(args.mine_for)),
    };
    net.run(&mut engine, shutdown, &mut runner).expect("run");
    let (h, tip) = store.tip().unwrap();
    eprintln!(
        "stopped at height {h}, tip {}",
        tip.block_id
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
}
