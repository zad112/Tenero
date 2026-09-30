//! What it costs a node to verify a chain with the REAL matmulhash proof of work, and what assume-valid saves.
//! `#[ignore]`d: it needs about 4.3 GiB of RAM for one epoch's dataset (about 8.6 GiB while a second is built) and
//! minutes of CPU. Run it on the owner's machine:
//!
//! ```text
//! cargo test --release -p tenero-chain --test real_pow_sync -- --ignored --nocapture
//! ```
//!
//! Settings (environment variables; the defaults are the real parameters):
//! * `TENERO_POW_BLOCKS`  how many blocks to mine and verify (default 30)
//! * `TENERO_POW_EPOCH`   blocks per dataset (default 100, the real chain; a smaller number makes the chain cross
//!   epoch boundaries, so the once-per-epoch dataset build is paid more than once)
//! * `TENERO_POW_THREADS` threads that build a dataset (default 6, and never more: CLAUDE.md rule 8)
//! * `TENERO_POW_TAIL`    blocks at the end that assume-valid still checks in full (default 5)
//! * `TENERO_POW_SMALL=1` tiny parameters, a dataset of a few KiB: a check that this harness works, NOT a measurement
//!
//! What it measures: CPU time in `Chain::submit_block` for a chain of coinbase-only blocks, on an easy target
//! (so the mining is about verification-sized attempts, not luck). What it does NOT include: the network, disk
//! beyond the store, or the transaction proofs (none are present; `ProofsNotChecked`). It is the proof-of-work cost
//! of syncing, and only that.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tenero_chain::{Chain, ChainParams, MatmulPow, ProofsNotChecked, Submitted, Validator};
use tenero_core::matmulhash::{self, Params};
use tenero_core::u256::U256;
use tenero_core::v2::ids::{self, PowKind};
use tenero_core::v2::*;
use tenero_store::Store;

const LABEL: &str = "tenero real pow sync measurement";
/// The node's clock: far ahead of every block, so none is "not yet".
const NOW: u64 = 4_000_000_000;
const T0: u64 = 1_700_000_000;

struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> TempDb {
        let p =
            std::env::temp_dir().join(format!("tenero-realpow-{}-{name}.redb", std::process::id()));
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

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

struct Setup {
    params: Params,
    blocks: u64,
    epoch: u64,
    threads: usize,
    tail: u64,
    small: bool,
}

fn setup() -> Setup {
    let small = std::env::var("TENERO_POW_SMALL").is_ok_and(|v| v == "1");
    Setup {
        params: if small {
            Params {
                m: 8,
                k: 64,
                nb: 64,
                num_blocks: 8,
            }
        } else {
            Params::DEFAULT
        },
        blocks: env_u64("TENERO_POW_BLOCKS", 30),
        epoch: env_u64("TENERO_POW_EPOCH", if small { 4 } else { 100 }),
        threads: env_u64("TENERO_POW_THREADS", 6).clamp(1, 6) as usize,
        tail: env_u64("TENERO_POW_TAIL", 5),
        small,
    }
}

fn chain_params() -> ChainParams {
    // a target about one attempt in eight meets, so each block costs a few attempts to mine
    ChainParams::version_2(LABEL, PowKind::Matmul, U256::pow2(253).unwrap())
}

fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

fn median(v: &mut [Duration]) -> Duration {
    v.sort();
    v[v.len() / 2]
}

/// Mines `n` blocks with the real proof of work on one thread and returns them, how many attempts it took, and how
/// long. Each block is accepted by a store of its own as it is made, so the chain is valid by construction.
fn mine_chain(s: &Setup) -> (Vec<Block>, u64, Duration, [u8; 32]) {
    let pow = MatmulPow::new(s.params, s.epoch, s.threads).unwrap();
    let params = chain_params();
    let db = TempDb::new("mine");
    let store = Store::open(&db.0, LABEL, PowKind::Matmul).unwrap();
    let proofs = ProofsNotChecked;
    let started = Instant::now();
    let (mut blocks, mut attempts) = (Vec::new(), 0u64);
    let mut ts = T0;
    for h in 1..=s.blocks {
        let v = Validator::new(&store, &params, &pow, &proofs);
        let next = v.next_block().unwrap();
        let coinbase = Coinbase {
            version: VERSION,
            height: h,
            outputs: vec![CoinbaseOutput {
                onetime_address: ids::header_hash(&BlockHeader {
                    version: VERSION,
                    prev_id: [0; 32],
                    timestamp: h,
                    tx_root: [0; 32],
                    nonce: 0,
                    mix: [0; 64],
                }),
                amount: next.reward,
                view_tag: [0; 3],
                ephemeral_pubkey: [7; 32],
                anchor_enc: [0; 16],
            }],
            extra: vec![],
        };
        let mut b = Block {
            header: BlockHeader {
                version: VERSION,
                prev_id: next.prev_id,
                timestamp: ts,
                tx_root: ids::block_tx_root(&coinbase, &[]).unwrap(),
                nonce: 0,
                mix: [0; 64],
            },
            coinbase,
            transactions: vec![],
        };
        ts += 60;
        let data = pow.dataset_for(h).unwrap();
        let header_hash = ids::header_hash(&b.header);
        for nonce in 0.. {
            attempts += 1;
            let a = matmulhash::compute_attempt(&data, &header_hash, nonce).unwrap();
            if U256::from_be_bytes(&a.digest) < next.target {
                b.header.nonce = nonce;
                b.header.mix = a.mix;
                break;
            }
        }
        drop(data);
        assert!(
            v.accept_block(&b, NOW).is_ok(),
            "the block just mined is not accepted"
        );
        blocks.push(b);
        if h % 5 == 0 {
            eprintln!("  mined {h} of {} ({attempts} attempts so far)", s.blocks);
        }
    }
    let tip = store.tip().unwrap().1.block_id;
    (blocks, attempts, started.elapsed(), tip)
}

/// Feeds `blocks` to a fresh node, in order, and times each `submit_block`. `assumed` are the ids that skip the
/// full proof of work. A FRESH `MatmulPow` (so the datasets are built again, as a newly started node must).
fn sync(
    s: &Setup,
    blocks: &[Block],
    assumed: Option<HashSet<[u8; 32]>>,
    name: &str,
) -> (Vec<Duration>, [u8; 32]) {
    let pow = MatmulPow::new(s.params, s.epoch, s.threads).unwrap();
    let params = chain_params();
    let db = TempDb::new(name);
    let store = Store::open(&db.0, LABEL, PowKind::Matmul).unwrap();
    let proofs = ProofsNotChecked;
    let mut chain = Chain::new(&store, &params, &pow, &proofs);
    if let Some(ids) = assumed {
        chain.set_assumed(ids);
    }
    let mut times = Vec::with_capacity(blocks.len());
    for b in blocks {
        let started = Instant::now();
        let r = chain
            .submit_block(b, NOW)
            .expect("a valid block is accepted");
        times.push(started.elapsed());
        assert!(matches!(r, Submitted::Extended(_)), "{r:?}");
    }
    let tip = store.tip().unwrap().1.block_id;
    (times, tip)
}

fn report(label: &str, times: &[Duration]) {
    let total: Duration = times.iter().sum();
    let mut sorted = times.to_vec();
    let slowest = *sorted.iter().max().unwrap();
    let med = median(&mut sorted);
    eprintln!(
        "  {label:<34} total {:>8.2} s   per block: median {:>7.1} ms, slowest {:>8.1} ms",
        secs(total),
        med.as_secs_f64() * 1000.0,
        slowest.as_secs_f64() * 1000.0
    );
}

#[test]
#[ignore = "needs about 4.3 GiB of RAM and minutes of CPU: run it by hand (see the file's header)"]
fn what_it_costs_to_verify_a_chain_with_the_real_proof_of_work_and_what_assume_valid_saves() {
    let s = setup();
    eprintln!();
    eprintln!("=== real proof of work sync cost ===");
    eprintln!(
        "  parameters: {} (m {}, k {}, nb {}, {} slices); {} blocks; {} blocks per dataset; {} threads for the dataset build; assume-valid leaves the last {} in full",
        if s.small {
            "SMALL (a harness check, NOT a measurement)"
        } else {
            "the real ones"
        },
        s.params.m,
        s.params.k,
        s.params.nb,
        s.params.num_blocks,
        s.blocks,
        s.epoch,
        s.threads,
        s.tail
    );
    assert!(s.tail < s.blocks, "the tail must be shorter than the chain");

    eprintln!("mining (one thread, real attempts)...");
    let (blocks, attempts, mining, tip) = mine_chain(&s);
    eprintln!(
        "  mined {} blocks in {:.1} s: {attempts} attempts ({:.1} per block), about {:.1} attempts/s including dataset builds",
        blocks.len(),
        secs(mining),
        attempts as f64 / blocks.len() as f64,
        attempts as f64 / secs(mining)
    );

    eprintln!("a fresh node checks everything...");
    let (full, tip_full) = sync(&s, &blocks, None, "full");
    assert_eq!(
        tip_full, tip,
        "the node that checked everything ended on another tip"
    );

    eprintln!(
        "a fresh node with assume-valid (all but the last {} blocks)...",
        s.tail
    );
    let cut = blocks.len() - s.tail as usize;
    let assumed: HashSet<[u8; 32]> = blocks[..cut]
        .iter()
        .map(|b| ids::block_id(&b.header, PowKind::Matmul))
        .collect();
    let (fast, tip_fast) = sync(&s, &blocks, Some(assumed), "assumed");
    assert_eq!(tip_fast, tip, "assume-valid ended on another tip");

    eprintln!();
    eprintln!(
        "=== results (CPU time in Chain::submit_block; no network, no transaction proofs) ==="
    );
    report("everything checked", &full);
    report("assume-valid (whole run)", &fast);
    report("  of which the assumed blocks", &fast[..cut]);
    report("  of which the last blocks in full", &fast[cut..]);
    let first_full = full[0];
    let mut rest = full[1..].to_vec();
    let per_block = median(&mut rest);
    eprintln!(
        "  the first block of a full check took {:.2} s; a later one typically {:.1} ms, so the dataset build is about {:.2} s",
        secs(first_full),
        per_block.as_secs_f64() * 1000.0,
        secs(first_full.saturating_sub(per_block))
    );
    let saved = secs(full.iter().sum()) - secs(fast.iter().sum());
    eprintln!(
        "  assume-valid saved {saved:.2} s of {:.2} s on this chain ({:.0} percent)",
        secs(full.iter().sum()),
        100.0 * saved / secs(full.iter().sum())
    );
    eprintln!();
    eprintln!("The two nodes agree with the miner on the tip. Paste everything from '=== real proof of work' down.");
    // the dataset is the only thing that makes the real run slow: a sanity check that the harness measured it
    if !s.small {
        assert!(
            secs(first_full) > 0.5,
            "the first block should have included building a 4 GiB dataset"
        );
    }
}
