//! Times the engine target on its seeds (a development aid for the fuzz targets, not a test).
use std::time::Instant;

fn main() {
    let seeds: Vec<_> = tenero_fuzzcases::seeds()
        .into_iter()
        .filter(|(t, _, _)| *t == "engine_messages")
        .collect();
    println!("{} engine seeds", seeds.len());
    let rounds = 40;
    let t0 = Instant::now();
    let mut runs = 0u64;
    for _ in 0..rounds {
        for (_, _, bytes) in &seeds {
            let _ = tenero_fuzzcases::engine_messages(bytes);
            runs += 1;
        }
    }
    let dt = t0.elapsed().as_secs_f64();
    println!(
        "{runs} runs in {dt:.2} s: {:.0} runs/s, {:.2} ms each",
        runs as f64 / dt,
        1000.0 * dt / runs as f64
    );
}
