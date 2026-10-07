//! Attempts per second on the GPU at the real chain parameters (a 4 GiB dataset).
//!
//!     cargo run --release -p tenero-gpu --example gpu_bench [-- --seconds 5]
//!
//! Needs an NVIDIA GPU, the CUDA toolkit's DLLs on PATH and about 4.5 GiB of video memory. The
//! search uses a target that is never met, so every batch is fully computed; a real search stops
//! at the first solution. What it prints is MEASURED on this machine, at these settings.

use std::time::Instant;
use tenero_core::matmulhash::{self as mh, Params};
use tenero_core::u256::U256;
use tenero_gpu::group::SliceGrouper;
use tenero_gpu::Gpu;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let seconds: f64 = args
        .iter()
        .position(|a| a == "--seconds")
        .and_then(|i| args.get(i + 1))
        .map_or(5.0, |s| s.parse().expect("seconds"));
    let p = Params::DEFAULT;
    let g = Gpu::new(0).expect("start the GPU");
    println!(
        "device: {} (compute capability {}.{})",
        g.name, g.compute_capability.0, g.compute_capability.1
    );

    let seed = mh::epoch_seed(0);
    let t = Instant::now();
    let dev = g
        .build_dataset(&p, &seed, p.num_blocks)
        .expect("build the dataset");
    g.synchronize().unwrap();
    println!(
        "dataset: {:.2} GiB built in {:.3} s",
        (p.num_blocks * p.slice_bytes()) as f64 / (1u64 << 30) as f64,
        t.elapsed().as_secs_f64()
    );

    let header = [1u8; 32];
    let never = U256::ZERO; // nothing is below zero, so nothing is ever found
    println!(
        "\nsteady-state search, {seconds} s per batch size (one CUDA stream, whole attempts):"
    );
    println!("{:>7} {:>14} {:>12}", "batch", "attempts/s", "ms/batch");
    for batch in [8usize, 16, 32, 64, 128, 256] {
        let mut engine = match g.attempt_engine(&dev, batch) {
            Ok(e) => e,
            Err(e) => {
                println!("{batch:>7}  cannot allocate: {e}");
                continue;
            }
        };
        // warm up (kernel JIT caches, cuBLASLt heuristics, clocks)
        engine
            .search(&header, &never, 0, (batch * 2) as u64)
            .unwrap();
        let start = Instant::now();
        let mut nonce = 1_000_000u64;
        let mut total = 0u64;
        while start.elapsed().as_secs_f64() < seconds {
            let (_, tried) = engine.search(&header, &never, nonce, batch as u64).unwrap();
            total += tried;
            nonce += tried;
        }
        let secs = start.elapsed().as_secs_f64();
        println!(
            "{batch:>7} {:>14.0} {:>12.2}",
            total as f64 / secs,
            secs * 1e3 / (total as f64 / batch as f64)
        );
    }
    let pipeline = !args.iter().any(|a| a == "--no-pipeline");
    let tune = args.iter().any(|a| a == "--tune");
    let groups_arg: Option<Vec<usize>> = args
        .iter()
        .position(|a| a == "--groups")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.split(',').map(|v| v.parse().expect("group")).collect());
    let batches_arg: Option<Vec<usize>> = args
        .iter()
        .position(|a| a == "--batches")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.split(',').map(|v| v.parse().expect("batch")).collect());
    println!("\nnonces chosen so that each slice is read once for a group of attempts (group::SliceGrouper), {seconds} s each, {}:", if pipeline { "pipelined" } else { "one batch at a time" });
    println!(
        "{:>7} {:>7} {:>14} {:>12} {:>16}",
        "group", "batch", "attempts/s", "ms/batch", "GB/s of slices"
    );
    for group in groups_arg.unwrap_or(vec![1, 2, 4, 8, 16, 32]) {
        for &batch in batches_arg.as_deref().unwrap_or(&[256, 512, 1024]) {
            if batch < group || batch % group != 0 {
                continue;
            }
            let mut engine = match g.attempt_engine(&dev, batch) {
                Ok(e) => e,
                Err(e) => {
                    println!("{group:>7} {batch:>7}  cannot allocate: {e}");
                    continue;
                }
            };
            if tune {
                let times = engine.tune(group, 64).unwrap();
                let ms: Vec<String> = times.iter().map(|t| format!("{:.3}", t * 1e3)).collect();
                println!(
                    "        cuBLASLt candidates for group {group}, ms per multiply: {}",
                    ms.join(" ")
                );
            }
            let mut grouper = SliceGrouper::new(header, p.num_blocks, group, 0);
            // pipelined: the next batch is queued before the last one is collected, so the GPU never waits for the CPU
            let mut run = |grouper: &mut SliceGrouper| {
                let b = grouper.next_batch(batch / group);
                let n = b.len() as u64;
                engine.submit(b.seeds, b.slices).unwrap();
                if pipeline && engine.in_flight() < 2 {
                    return 0;
                }
                let got = engine.collect().unwrap().unwrap();
                assert!(!got.iter().any(|a| mh::meets_target(&a.digest, &never)));
                n
            };
            for _ in 0..3 {
                run(&mut grouper); // warm up, and fill the groups
            }
            let start = Instant::now();
            let (mut total, mut batches) = (0u64, 0u64);
            while start.elapsed().as_secs_f64() < seconds {
                total += run(&mut grouper);
                batches += 1;
            }
            let secs = start.elapsed().as_secs_f64();
            let rate = total as f64 / secs;
            println!(
                "{group:>7} {batch:>7} {rate:>14.0} {:>12.2} {:>16.0}",
                secs * 1e3 / batches as f64,
                rate / group as f64 * p.slice_bytes() as f64 / 1e9
            );
        }
    }
    println!("\nFor scale: the Python miner's documented figure is about 22,000 attempts/s (README), measured");
    println!("earlier on this card; that is not a measurement made by this program.");
}
