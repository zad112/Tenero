//! The GPU miner, end to end, at the REAL parameters. `#[ignore]`d: it needs an NVIDIA GPU and its driver (not the CUDA
//! Toolkit), about 4.3 GiB of video memory (one dataset: the GPU backend builds the next epoch's when its
//! first job arrives) and about 4.3 GiB of RAM for the node's own CPU check (8.6 with this test's two-dataset pow). Run it on the owner's machine:
//!
//! ```text
//! cargo test --release -p tenero-miner --test gpu_mining -- --ignored --nocapture --test-threads=1
//! ```
//!
//! **Nothing here has been run by the author**: there is no GPU where the code was written. Every figure it prints is
//! for the owner to report (CLAUDE.md rule 5), and a failing assertion is news, not a surprise.
//!
//! Settings (environment variables): `TENERO_POW_BLOCKS` blocks to mine (default 20), `TENERO_POW_EPOCH` blocks per
//! dataset (default 100; 5 makes the run cross epoch boundaries, which is the point of the second run),
//! `TENERO_MINER_BATCH` attempts per batch (default 128), `TENERO_MINER_PREFETCH` blocks ahead at which the NODE's CPU
//! check builds the next dataset (default 5; the GPU backend does not build ahead), `TENERO_MINER_SECONDS` seconds per batch size in the speed test (default 15),
//! `TENERO_GATHER_FROM` the gather fork height for the node and the miner (default: never; 10 with 20 blocks mines across it).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tenero_chain::{ChainParams, MatmulPow, ProofsNotChecked};
use tenero_core::matmulhash::Params;
use tenero_core::u256::U256;
use tenero_core::v2::ids::{self, PowKind};
use tenero_miner::gpu::GpuBackend;
use tenero_miner::{Backend, Counters, Job, Miner, MinerConfig, MinerHook, PlaceholderPayout};
use tenero_net::sim::LABEL;
use tenero_net::transport::Hooks;
use tenero_net::{Engine, EngineConfig, Event};
use tenero_node::{Node, NodeConfig};
use tenero_store::Store;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> TempDb {
        let p =
            std::env::temp_dir().join(format!("tenero-gpumine-{}-{name}.redb", std::process::id()));
        let db = TempDb(p);
        db.remove();
        db
    }
    fn remove(&self) {
        let _ = std::fs::remove_file(&self.0);
        let mut s = self.0.clone().into_os_string();
        s.push(".segments");
        let _ = std::fs::remove_dir_all(PathBuf::from(s));
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        self.remove();
    }
}

#[test]
#[ignore = "needs an NVIDIA GPU and about 9 GiB of RAM and video memory: run it by hand (see the header)"]
fn the_gpu_mines_blocks_a_cpu_node_verifies_in_full() {
    let blocks = env_u64("TENERO_POW_BLOCKS", 20);
    let epoch = env_u64("TENERO_POW_EPOCH", 100);
    let batch = env_u64("TENERO_MINER_BATCH", 128) as usize;
    let prefetch = env_u64("TENERO_MINER_PREFETCH", 5);
    let gather_from = env_u64("TENERO_GATHER_FROM", u64::MAX);
    let params = Params::DEFAULT;
    eprintln!();
    eprintln!("=== GPU mining, verified by the node's CPU check ===");
    eprintln!("  real parameters; {blocks} blocks; {epoch} blocks per dataset; batch {batch}; the node's check looks {prefetch} blocks ahead");
    if gather_from != u64::MAX {
        eprintln!("  the gather fork at height {gather_from}: blocks from there on need the gathered attempt");
    }

    let db = TempDb::new("gpu");
    let store = Store::open(&db.0, LABEL, PowKind::Matmul).unwrap();
    // an easy target (about one attempt in eight), so a block takes a handful of attempts and the time is the
    // GPU's speed and the node's checking, not luck
    let chain = ChainParams::version_2(LABEL, PowKind::Matmul, U256::pow2(253).unwrap());
    // the node's own CPU proof of work: every block the GPU finds goes through this, bit for bit
    let pow = Arc::new(
        MatmulPow::new(params, epoch, 6)
            .unwrap()
            .gathered_from(gather_from),
    );
    let node = Node::with_proof_check(
        &store,
        &chain,
        &*pow,
        &ProofsNotChecked,
        NodeConfig {
            allow_unchecked_proofs_for_tests: true,
            ..NodeConfig::default()
        },
    )
    .unwrap();
    // the node's CPU check looks ahead (0 turns it off, for comparison): with epochs this short and blocks this quick,
    // the default of 10 blocks would prefetch the epoch AFTER the next
    let mut engine = Engine::new(
        node,
        EngineConfig {
            pow_prefetch_blocks: prefetch,
            ..EngineConfig::default()
        },
    );
    let cfg = MinerConfig {
        log: Arc::new(|l| eprintln!("  miner: {l}")),
        ..MinerConfig::default()
    };
    let mut hook = MinerHook::new(
        Miner::spawn(move || {
            GpuBackend::new(0, params, epoch, batch).map(|b| b.gathered_from(gather_from))
        }),
        PlaceholderPayout { seed: [3; 32] },
        cfg,
    );
    let counters = hook.counters();

    let started = Instant::now();
    let mut clock = 1_700_000_000_000u64;
    let (mut last_height, mut last_at) = (0u64, Instant::now());
    let mut gaps: Vec<(u64, Duration)> = Vec::new();
    let end = started + Duration::from_secs(600);
    while Instant::now() < end {
        for ev in hook.poll(&mut engine, clock) {
            let is_block = matches!(ev, Event::LocalBlock(_));
            engine.handle(clock, ev);
            if is_block {
                clock += 60_000;
            }
        }
        let h = engine.node().tip().unwrap().0;
        if h != last_height {
            gaps.push((h, last_at.elapsed()));
            last_height = h;
            last_at = Instant::now();
        }
        if h >= blocks || hook.failed() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    hook.enabled().store(false, Ordering::SeqCst);
    hook.poll(&mut engine, clock); // the verdict on the last block
    let elapsed = started.elapsed();
    assert!(
        !hook.failed(),
        "the GPU backend failed (no GPU, an old driver, or out of memory): see the log above"
    );

    let attempts = counters.attempts.load(Ordering::Relaxed);
    eprintln!();
    eprintln!("=== results ===");
    eprintln!("  {last_height} blocks in {:.1} s", elapsed.as_secs_f64());
    eprintln!(
        "  {attempts} attempts in all: {:.0} attempts per second over the whole run (includes each block's CPU check, the dataset builds and the start-up)",
        attempts as f64 / elapsed.as_secs_f64()
    );
    eprintln!(
        "  datasets built on the GPU: {} (none ahead of time: one dataset in video memory at a time)",
        counters.dataset_builds.load(Ordering::Relaxed)
    );
    let slowest = gaps.iter().max_by_key(|(_, d)| *d).unwrap();
    let first = gaps[0];
    let mut rest: Vec<Duration> = gaps[1..].iter().map(|(_, d)| *d).collect();
    rest.sort();
    eprintln!(
        "  the first block took {:.2} s (start-up and the first dataset); after it a block took a median {:.2} s and at most {:.2} s (block {})",
        first.1.as_secs_f64(),
        rest[rest.len() / 2].as_secs_f64(),
        slowest.1.as_secs_f64(),
        slowest.0
    );
    eprintln!(
        "  blocks accepted {}, lost a race {}, REFUSED {}, bad answers {}",
        hook.stats.blocks_accepted,
        hook.stats.blocks_lost_race,
        hook.stats.blocks_rejected,
        hook.stats.bad_solutions
    );
    eprintln!();
    eprintln!("Paste everything from '=== GPU mining' down.");

    // the end-to-end "bit for bit": not one block the GPU found was refused by the CPU check
    assert_eq!(
        hook.stats.blocks_rejected, 0,
        "the node's CPU check refused a block the GPU found"
    );
    assert_eq!(hook.stats.bad_solutions, 0);
    assert!(hook.stats.blocks_accepted >= blocks - 1, "{:?}", hook.stats);
    assert_eq!(store.tip().unwrap().0, last_height);
    // a full chain of blocks, each accepted by the CPU proof of work, is the proof
    let id = store.tip().unwrap().1.block_id;
    assert_eq!(
        id,
        ids::block_id(&store.tip().unwrap().1.header, PowKind::Matmul)
    );
}

#[test]
#[ignore = "needs an NVIDIA GPU and about 4.5 GiB of video memory: run it by hand (see the header)"]
fn how_many_attempts_per_second_the_gpu_backend_does_at_each_batch_size() {
    let seconds = env_u64("TENERO_MINER_SECONDS", 15);
    let params = Params::DEFAULT;
    eprintln!();
    eprintln!("=== GPU backend speed (real parameters, a target that is never met, {seconds} s per batch size) ===");
    let rig = tenero_net::sim::SimRig::rigs("gpuspeed", 1);
    let node = Node::with_proof_check(
        &rig[0].store,
        &rig[0].params,
        &tenero_chain::Sha256Pow,
        &ProofsNotChecked,
        NodeConfig {
            allow_unchecked_proofs_for_tests: true,
            ..NodeConfig::default()
        },
    )
    .unwrap();
    let block = node
        .block_template(
            1_700_000_060,
            1000,
            tenero_node::Payout {
                onetime_address: [1; 32],
                view_tag: [0; 3],
                ephemeral_pubkey: [2; 32],
                anchor_enc: [0; 16],
            },
        )
        .unwrap();
    eprintln!("{:>7} {:>14}", "batch", "attempts/s");
    for batch in [32usize, 64, 128, 256] {
        let mut backend = match GpuBackend::new(0, params, 100, batch) {
            Ok(b) => b,
            Err(e) => panic!("cannot start the GPU backend: {e}"),
        };
        eprintln!("  device: {}", backend.device_name());
        let stale = Arc::new(AtomicBool::new(false));
        let job = Job {
            id: batch as u64,
            header: block.header.clone(),
            height: 1,
            target: U256::ZERO, // never met
            stale: Arc::clone(&stale),
            nonce_start: None,
        };
        let counters = Arc::new(Counters::default());
        let c = Arc::clone(&counters);
        let t = std::thread::spawn(move || backend.mine(&job, &c));
        // the first seconds include building the dataset and warming up: measure from when it is steady
        std::thread::sleep(Duration::from_secs(5));
        let (a0, t0) = (counters.attempts.load(Ordering::Relaxed), Instant::now());
        std::thread::sleep(Duration::from_secs(seconds));
        let (a1, t1) = (counters.attempts.load(Ordering::Relaxed), Instant::now());
        stale.store(true, Ordering::SeqCst);
        assert_eq!(t.join().unwrap().unwrap(), None);
        eprintln!(
            "{:>7} {:>14.0}",
            batch,
            (a1 - a0) as f64 / t1.duration_since(t0).as_secs_f64()
        );
    }
    eprintln!();
    eprintln!("Paste everything from '=== GPU backend speed' down.");
}
