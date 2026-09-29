//! Timings for the portable CPU code, at the real parameters. Run in release mode:
//!
//!     cargo run --release --example bench [-- --full]
//!
//! Prints what was MEASURED on this machine and how it was derived. Nothing here is an estimate for
//! any other machine, and none of it concerns the GPU. `--full` also builds the whole 4 GiB dataset
//! (needs about 4.3 GiB of free RAM).

use std::hint::black_box;
use std::time::Instant;
use tenero_core::chacha20;
use tenero_core::matmulhash::{self as mh, Dataset, Params};

const HEADER: [u8; 32] = [7u8; 32];

fn secs<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed().as_secs_f64())
}

fn best_of<T>(reps: usize, mut f: impl FnMut() -> T) -> f64 {
    (0..reps)
        .map(|_| secs(&mut f).1)
        .fold(f64::INFINITY, f64::min)
}

fn main() {
    let full = std::env::args().any(|a| a == "--full");
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let p = Params::DEFAULT;
    println!("Tenero CPU benchmark (portable Rust, release build)");
    println!(
        "logical cores reported: {cores}; parameters: m={} k={} nb={} slices={}",
        p.m, p.k, p.nb, p.num_blocks
    );
    println!(
        "target-cpu: {}\n",
        option_env!("RUSTFLAGS").unwrap_or("(default: generic x86-64)")
    );

    // ---- the ChaCha20 core on its own
    let n = 5_000_000u32;
    let (_, t) = secs(|| {
        let mut s = [0u32; 16];
        for i in 0..n {
            s[0] ^= i;
            s = black_box(chacha20::core(&s));
        }
        s
    });
    println!(
        "chacha20 core, one thread:        {:>8.1} ns/core   ({:.1} M cores/s)",
        t / f64::from(n) * 1e9,
        f64::from(n) / t / 1e6
    );

    // ---- the dataset build
    let seed = mh::epoch_seed(0);
    let probe = 32usize; // 512 MiB: a prefix is the same work per slice as the whole
    println!(
        "\ndataset build, first {probe} slices ({} MiB), best of 2:",
        (probe * p.slice_bytes()) >> 20
    );
    for threads in [1, 2, 4, 6] {
        if threads > cores {
            continue;
        }
        let t = best_of(2, || Dataset::build(&p, &seed, probe, threads).unwrap());
        println!(
            "  {threads} thread(s): {t:>6.2} s  = {:>6.1} MiB/s",
            ((probe * p.slice_bytes()) >> 20) as f64 / t
        );
    }
    // slices later in the dataset read from a larger earlier region, so the per-slice cost can grow
    if full {
        for threads in [4, 6] {
            if threads > cores {
                continue;
            }
            let (d, t) = secs(|| Dataset::build(&p, &seed, p.num_blocks, threads).unwrap());
            println!("  FULL 4 GiB, {threads} thread(s): {t:.2} s (measured; the dataset is rebuilt once per epoch of 100 blocks)");
            black_box(d.slices());
        }
    } else {
        println!("  (run with --full to time the whole 4 GiB build instead of extrapolating)");
    }

    // ---- one attempt, stage by stage (slice 0 of an 8-slice prefix is enough: the work is the same)
    let data = Dataset::build(&p, &seed, 8, cores.min(4)).unwrap();
    let slice = data.slice(0).unwrap();
    let attempt_seed = mh::attempt_seed(&HEADER, 1);
    let x = mh::make_x(&attempt_seed, &p);
    let c = mh::product(&x, slice, &p);
    let t_x = best_of(5, || mh::make_x(black_box(&attempt_seed), &p));
    let t_c = best_of(3, || mh::product(black_box(&x), slice, &p));
    let t_f = best_of(3, || mh::fold_sums(black_box(&c)));
    let t_pre = best_of(1000, || mh::precheck(&HEADER, 1, &[0u8; 64], "00", None));
    let one = t_x + t_c + t_f;
    println!("\none attempt, one thread, best of several:");
    println!(
        "  make X (ChaCha keystream, {} KiB):   {:>9.3} ms",
        (p.m * p.k) >> 10,
        t_x * 1e3
    );
    println!(
        "  matmul C = X @ W ({:.2} G multiply-adds): {:>9.3} ms   ({:.2} G multiply-adds/s)",
        (p.m * p.k * p.nb) as f64 / 1e9,
        t_c * 1e3,
        (p.m * p.k * p.nb) as f64 / t_c / 1e9
    );
    println!(
        "  fold ({} ChaCha cores):      {:>9.3} ms",
        p.m * p.nb / 16,
        t_f * 1e3
    );
    println!(
        "  total:                                {:>9.3} ms  = {:.2} attempts/s on one thread",
        one * 1e3,
        1.0 / one
    );
    println!(
        "  cheap precheck (no dataset):          {:>9.3} us",
        t_pre * 1e6
    );

    // ---- attempts in parallel: threads share the dataset (read-only)
    println!("\nattempts per second with several threads (each thread does whole attempts):");
    let per_thread = 3;
    for threads in [1, 2, 4, 6] {
        if threads > cores {
            continue;
        }
        let (_, t) = secs(|| {
            std::thread::scope(|s| {
                for w in 0..threads {
                    let (data, p) = (&data, &p);
                    s.spawn(move || {
                        for i in 0..per_thread {
                            let sd = mh::attempt_seed(&HEADER, (w * per_thread + i) as u64);
                            let x = mh::make_x(&sd, p);
                            let c = mh::product(&x, data.slice(0).unwrap(), p);
                            black_box(mh::fold_sums(&c));
                        }
                    });
                }
            });
        });
        println!(
            "  {threads} thread(s): {:>6.2} attempts/s",
            (threads * per_thread) as f64 / t
        );
    }
    println!("\nFor scale (Rule 5): the GPU baseline in docs is about 22,000 attempts/s, measured on the owner's");
    println!("RTX 5070 Ti, not here. A CPU check of a block needs ONE attempt plus the dataset.");
}
