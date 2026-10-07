//! The GPU's gathered attempt (`tenero_gpu::gather`, required from the gather fork height) against the consensus code
//! (`matmulhash::compute_gathered_attempt`, itself checked against the reference's vectors), bit for bit. Needs an NVIDIA
//! GPU, so `#[ignore]`d:
//!
//!     cargo test --release -p tenero-gpu --test gather_selftest -- --ignored --test-threads=1

use tenero_core::matmulhash::{self as mh, Dataset, Params};
use tenero_gpu::gather::GatherEngine;
use tenero_gpu::Gpu;

fn check(p: Params, attempts: usize, batch: usize) {
    let g = Gpu::new(0).expect("a GPU");
    let seed = mh::epoch_seed(3);
    let cpu = Dataset::build(&p, &seed, p.num_blocks, 6).unwrap();
    let dev = g.build_dataset(&p, &seed, p.num_blocks).unwrap();
    let mut engine = GatherEngine::new(&g, &dev, batch).unwrap();
    let header = [0x77u8; 32];
    let nonces: Vec<u64> = (0..attempts as u64)
        .map(|n| n.wrapping_add(u64::MAX - 3))
        .collect();
    for chunk in nonces.chunks(batch) {
        let seeds: Vec<[u8; 32]> = chunk
            .iter()
            .map(|&n| mh::attempt_seed(&header, n))
            .collect();
        let got = engine.attempts(&seeds, false).unwrap();
        for (&nonce, a) in chunk.iter().zip(&got) {
            let want = mh::compute_gathered_attempt(&cpu, &header, nonce).unwrap();
            assert_eq!(*a, want, "{p:?} nonce {nonce}");
        }
    }
}

#[test]
#[ignore = "needs an NVIDIA GPU and the CUDA toolkit"]
fn the_gathered_multiply_matches_the_cpu_on_small_shapes() {
    // a power-of-two number of columns, and one that is not (5 slices of 384)
    check(
        Params {
            m: 64,
            k: 1024,
            nb: 256,
            num_blocks: 8,
        },
        70,
        32,
    );
    check(
        Params {
            m: 64,
            k: 640,
            nb: 384,
            num_blocks: 5,
        },
        40,
        16,
    );
}

#[test]
#[ignore = "needs an NVIDIA GPU, the CUDA toolkit and about 4.5 GiB of RAM and of video memory"]
fn the_gathered_multiply_matches_the_cpu_at_the_real_parameters() {
    check(Params::DEFAULT, 3, 3);
}
