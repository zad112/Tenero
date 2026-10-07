//! The gather PROTOTYPE's speed (`tenero_gpu::gather`, not consensus), next to the card's own memory bandwidth.
//!
//!     cargo run --release -p tenero-gpu --example gather_bench [-- --seconds 5]
//!
//! Needs an NVIDIA GPU and about 4.5 GiB of video memory plus the batch buffers. What it prints is MEASURED on this
//! machine. One batch at a time (no pipeline), with the same seeds every batch (their columns are spread over the
//! whole 4 GiB, so no batch is helped by the cache).

use std::time::Instant;
use tenero_core::matmulhash::{self as mh, Params};
use tenero_gpu::gather::GatherEngine;
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
    println!("device: {}", g.name);

    let (gbs, _) = g.copy_bandwidth(1 << 30, seconds).expect("copy test");
    println!(
        "device-to-device copy of 1 GiB: {gbs:.0} GB/s of reads plus writes (the practical memory bandwidth)"
    );

    let dev = g
        .build_dataset(&p, &mh::epoch_seed(0), p.num_blocks)
        .expect("build the dataset");
    g.synchronize().unwrap();
    let header = [1u8; 32];
    let column_bytes = (p.nb * p.k) as f64; // read per attempt: nb columns of k bytes = 16 MiB

    {
        // the access pattern alone: no multiply, `piece` bytes of each of a block's 128 columns per step
        let batch = 256;
        let mut engine = GatherEngine::new(&g, &dev, batch).expect("engine");
        let seeds: Vec<[u8; 32]> = (0..batch as u64)
            .map(|n| mh::attempt_seed(&header, n))
            .collect();
        println!("\nreading the columns only (no multiply), batch {batch}:");
        println!("{:>16} {:>12}", "bytes per step", "GB/s read");
        for piece in [64usize, 128, 256, 512, 1024] {
            engine.read_only(&seeds, piece).unwrap();
            let start = Instant::now();
            let mut total = 0u64;
            while start.elapsed().as_secs_f64() < seconds / 2.0 {
                engine.read_only(&seeds, piece).unwrap();
                total += batch as u64;
            }
            let rate = total as f64 / start.elapsed().as_secs_f64();
            println!("{piece:>16} {:>12.0}", rate * column_bytes / 1e9);
        }
    }

    println!(
        "\n{:>7} {:>16} {:>14} {:>12}",
        "batch", "columns", "attempts/s", "GB/s read"
    );
    for batch in [32usize, 64, 128, 256, 512] {
        let mut engine = match GatherEngine::new(&g, &dev, batch) {
            Ok(e) => e,
            Err(e) => {
                println!("{batch:>7}  cannot start: {e}");
                continue;
            }
        };
        let seeds: Vec<[u8; 32]> = (0..batch as u64)
            .map(|n| mh::attempt_seed(&header, n))
            .collect();
        for (same, what) in [(false, "its own"), (true, "all the same")] {
            engine.attempts(&seeds, same).unwrap(); // warm up
            let start = Instant::now();
            let mut total = 0u64;
            while start.elapsed().as_secs_f64() < seconds {
                engine.attempts(&seeds, same).unwrap();
                total += batch as u64;
            }
            let rate = total as f64 / start.elapsed().as_secs_f64();
            let read = if same {
                column_bytes / batch as f64
            } else {
                column_bytes
            };
            println!(
                "{batch:>7} {what:>16} {rate:>14.0} {:>12.0}",
                rate * read / 1e9
            );
        }
    }
    println!("\n'all the same' gives WRONG results: every attempt reads attempt 0's columns. No miner can choose nonces that");
    println!(
        "do this; it shows what the gathered design would cost if reading the columns were free."
    );
}
