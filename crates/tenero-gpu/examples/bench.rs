//! Attempts per second on the GPU at the real chain parameters (a 4 GiB dataset).
//!
//!     cargo run --release -p tenero-gpu --example bench [-- --seconds 5]
//!
//! Needs an NVIDIA GPU, the CUDA toolkit's DLLs on PATH and about 4.5 GiB of video memory. The
//! search uses a target that is never met, so every batch is fully computed; a real search stops
//! at the first solution. What it prints is MEASURED on this machine, at these settings.

use std::time::Instant;
use tenero_core::matmulhash::{self as mh, Params};
use tenero_core::u256::U256;
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
    println!("\nFor scale: the Python miner's documented figure is about 22,000 attempts/s (README), measured");
    println!("earlier on this card; that is not a measurement made by this program.");
}
