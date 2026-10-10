#![cfg(feature = "amd-hip")]

//! Bit-for-bit tests for the AMD HIP backend. Run on the target machine with:
//! `cargo test --release -p tenero-gpu --features amd-hip --test amd_selftest -- --ignored --test-threads=1`

use tenero_core::matmulhash::{self as mh, Dataset, Params};
use tenero_gpu::amd::AmdGpu;

fn compare(p: Params, epoch: u64, nonces: &[u64], batch: usize) {
    let epoch_seed = mh::epoch_seed(epoch);
    let cpu = Dataset::build(&p, &epoch_seed, p.num_blocks, 6).expect("CPU dataset");
    let mut gpu = AmdGpu::new(0, p, batch).expect("AMD GPU");
    let build_ms = gpu.build_dataset(&epoch_seed).expect("GPU dataset");
    println!(
        "device={} arch={} dataset={build_ms:.2} ms VRAM={:?}",
        gpu.name,
        gpu.arch,
        gpu.memory().ok()
    );
    for slice in 0..p.num_blocks {
        assert_eq!(
            gpu.read_slice(slice).expect("GPU slice"),
            cpu.slice(slice).expect("CPU slice"),
            "dataset slice {slice}"
        );
    }

    let header = [0x6du8; 32];
    for chunk in nonces.chunks(batch) {
        let seeds: Vec<[u8; 32]> = chunk
            .iter()
            .map(|&nonce| mh::attempt_seed(&header, nonce))
            .collect();
        let slices: Vec<usize> = seeds
            .iter()
            .map(|seed| mh::attempt_slice(seed, p.num_blocks))
            .collect();

        let original = gpu
            .attempts(&seeds, Some(&slices))
            .expect("original AMD attempts");
        for (&nonce, got) in chunk.iter().zip(&original) {
            assert_eq!(
                *got,
                mh::compute_attempt(&cpu, &header, nonce).expect("CPU original attempt"),
                "original nonce {nonce}"
            );
        }

        let gathered = gpu.attempts(&seeds, None).expect("gathered AMD attempts");
        for (&nonce, got) in chunk.iter().zip(&gathered) {
            assert_eq!(
                *got,
                mh::compute_gathered_attempt(&cpu, &header, nonce).expect("CPU gathered attempt"),
                "gathered nonce {nonce}"
            );
        }
        println!(
            "{} attempts: {:.3} ms ({:.1} attempts/s)",
            chunk.len(),
            gpu.last_batch_ms,
            chunk.len() as f32 * 1000.0 / gpu.last_batch_ms
        );
    }
}

#[test]
#[ignore = "needs the local AMD RDNA3 GPU"]
fn small_dataset_original_and_gathered_are_bit_exact() {
    compare(
        Params {
            m: 64,
            k: 128,
            nb: 128,
            num_blocks: 4,
        },
        3,
        &[0, 1, 2, 37, 1 << 32, 1 << 63, u64::MAX],
        7,
    );
}

#[test]
#[ignore = "needs about 4.5 GiB of RAM and VRAM on the local AMD RDNA3 GPU"]
fn real_parameters_original_and_gathered_are_bit_exact() {
    compare(Params::DEFAULT, 0, &[0, 37, u64::MAX], 3);
}
