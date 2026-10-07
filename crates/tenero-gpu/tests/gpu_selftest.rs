//! Does the GPU agree with the CPU reference, bit for bit? These need an NVIDIA GPU and the CUDA
//! toolkit's DLLs on PATH, so they are `#[ignore]`d (CI has no GPU). Run them on a GPU machine:
//!
//!     cargo test --release -p tenero-gpu -- --ignored --test-threads=1
//!
//! (`--test-threads=1` because the last tests each hold a large dataset in video memory.)

use serde_json::Value;
use tenero_core::hash::{hex_lower, sha256};
use tenero_core::matmulhash::{self as mh, Dataset, Params};
use tenero_core::u256::U256;
use tenero_core::vectors::{hex, load};
use tenero_gpu::group::SliceGrouper;
use tenero_gpu::{DeviceDataset, Gpu};

const NEEDS_GPU: &str = "needs an NVIDIA GPU and the CUDA toolkit";

fn gpu() -> Gpu {
    Gpu::new(0).unwrap_or_else(|e| panic!("cannot start the GPU: {e}"))
}

/// A small deterministic pseudo-random generator (no dependency).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn i8s(&mut self, n: usize) -> Vec<i8> {
        (0..n).map(|_| self.next() as i8).collect()
    }
    fn i32s(&mut self, n: usize) -> Vec<i32> {
        (0..n).map(|_| self.next() as i32).collect()
    }
    fn key(&mut self) -> [u8; 32] {
        let mut k = [0u8; 32];
        for c in k.chunks_mut(8) {
            c.copy_from_slice(&self.next().to_le_bytes());
        }
        k
    }
}

fn sha_hex(bytes: &[u8]) -> String {
    hex_lower(&sha256(&[bytes]))
}

#[test]
#[ignore = "needs an NVIDIA GPU and the CUDA toolkit"]
fn the_kernels_compile_and_the_device_is_reported() {
    let g = gpu();
    println!(
        "device: {} (compute capability {}.{})",
        g.name, g.compute_capability.0, g.compute_capability.1
    );
    assert!(
        g.compute_capability.0 >= 7,
        "int8 tensor cores need Turing (7.5) or newer"
    );
    let _ = NEEDS_GPU;
}

#[test]
#[ignore = "needs an NVIDIA GPU and the CUDA toolkit"]
fn the_keystream_kernel_matches_the_cpu() {
    let g = gpu();
    let mut rng = Rng(3);
    let keys = [rng.key(), rng.key(), rng.key()];
    for start in [0u64, 12345, (1 << 32) - 40] {
        let got = g.keystream(&keys, 100, start).unwrap();
        for (i, key) in keys.iter().enumerate() {
            // (start + local) is truncated to 32 bits in the kernel, like the CPU's wrapping counter
            let want = tenero_core::chacha20::keystream(key, 100, start as u32, [0; 3]);
            assert_eq!(
                got[i * 6400..(i + 1) * 6400],
                want[..],
                "key {i}, counter start {start}"
            );
        }
    }
}

#[test]
#[ignore = "needs an NVIDIA GPU and the CUDA toolkit"]
fn the_fold_kernel_matches_the_cpu_on_random_values_and_the_int32_extremes() {
    let g = gpu();
    let mut rng = Rng(5);
    let chunks = 64;
    let mut c = rng.i32s(3 * chunks * 16);
    c[..4].copy_from_slice(&[i32::MIN, i32::MAX, 0, -1]);
    let got = g.fold_sums(&c, chunks).unwrap();
    assert_eq!(got.len(), 3);
    for (i, sums) in got.iter().enumerate() {
        assert_eq!(
            *sums,
            mh::fold_sums(&c[i * chunks * 16..(i + 1) * chunks * 16]),
            "product {i}"
        );
    }
}

#[test]
#[ignore = "needs an NVIDIA GPU and the CUDA toolkit"]
fn the_int8_matmul_is_exact_at_the_extremes() {
    let g = gpu();
    let (m, k, nb) = (32, 4096, 64);
    for (xv, wv) in [(-128i8, -128i8), (127, -128), (127, 127)] {
        let c = g
            .int8_matmul(&vec![xv; m * k], &vec![wv as u8; nb * k], m, k, nb)
            .unwrap();
        let want = k as i32 * i32::from(xv) * i32::from(wv);
        assert!(
            c.iter().all(|&v| v == want),
            "{xv} x {wv}: got {} want {want}",
            c[0]
        );
    }
}

#[test]
#[ignore = "needs an NVIDIA GPU and the CUDA toolkit"]
fn the_int8_matmul_matches_the_cpu_in_the_real_layout() {
    let g = gpu();
    let mut rng = Rng(11);
    let (m, k, nb) = (32, 1024, 1024);
    let x = rng.i8s(m * k);
    let w: Vec<u8> = rng.i8s(nb * k).iter().map(|&v| v as u8).collect();
    let p = Params {
        m,
        k,
        nb,
        num_blocks: 1,
    };
    let want = mh::product(&x, &w, &p);
    assert_eq!(g.int8_matmul(&x, &w, m, k, nb).unwrap(), want);
    // the shape of the real chain (m=64, nb=2048, k=8192) too
    let (m, k, nb) = (64, 8192, 2048);
    let x = rng.i8s(m * k);
    let w: Vec<u8> = rng.i8s(nb * k).iter().map(|&v| v as u8).collect();
    let p = Params {
        m,
        k,
        nb,
        num_blocks: 1,
    };
    assert_eq!(
        g.int8_matmul(&x, &w, m, k, nb).unwrap(),
        mh::product(&x, &w, &p),
        "real shape"
    );
}

#[test]
#[ignore = "needs an NVIDIA GPU and the CUDA toolkit"]
fn a_small_dataset_and_full_attempts_match_the_cpu() {
    let g = gpu();
    let p = Params {
        m: 32,
        k: 1024,
        nb: 1024,
        num_blocks: 8,
    };
    let seed = mh::epoch_seed(0);
    let cpu = Dataset::build(&p, &seed, p.num_blocks, 1).unwrap();
    let dev = g.build_dataset(&p, &seed, p.num_blocks).unwrap();
    for j in 0..p.num_blocks {
        assert_eq!(
            g.read_slice(&dev, j).unwrap(),
            cpu.slice(j).unwrap(),
            "slice {j}"
        );
    }
    let header = [9u8; 32];
    let nonces: Vec<u64> = (0..8).collect();
    let got = g
        .attempt_engine(&dev, 8)
        .unwrap()
        .attempts(&header, &nonces)
        .unwrap();
    for (nonce, a) in nonces.iter().zip(&got) {
        assert_eq!(
            *a,
            mh::compute_attempt(&cpu, &header, *nonce).unwrap(),
            "nonce {nonce}"
        );
    }
    // a batch smaller than the engine's, and one nonce at a time, give the same answers
    let mut engine = g.attempt_engine(&dev, 8).unwrap();
    assert_eq!(engine.attempts(&header, &nonces[..3]).unwrap(), got[..3]);
    assert_eq!(engine.attempts(&header, &nonces[5..6]).unwrap(), got[5..6]);
}

/// The benchmark runs batches of up to 256; the equivalence tests above use small ones. Check that
/// a big batch is bit-identical too (every attempt against the CPU), including nonces near 2^64.
#[test]
#[ignore = "needs an NVIDIA GPU and the CUDA toolkit"]
fn a_large_batch_matches_the_cpu_attempt_for_attempt() {
    let g = gpu();
    let p = Params {
        m: 32,
        k: 1024,
        nb: 1024,
        num_blocks: 8,
    };
    let seed = mh::epoch_seed(1);
    let cpu = Dataset::build(&p, &seed, p.num_blocks, 1).unwrap();
    let dev = g.build_dataset(&p, &seed, p.num_blocks).unwrap();
    let mut engine = g.attempt_engine(&dev, 256).unwrap();
    let header = [0xabu8; 32];
    for start in [0u64, 1 << 40, u64::MAX - 255] {
        let nonces: Vec<u64> = (0..256).map(|i| start + i).collect();
        let got = engine.attempts(&header, &nonces).unwrap();
        for (nonce, a) in nonces.iter().zip(&got) {
            assert_eq!(
                *a,
                mh::compute_attempt(&cpu, &header, *nonce).unwrap(),
                "nonce {nonce}"
            );
        }
    }
}

/// What the miner does: nonces chosen in groups that read the same slice, a multiply algorithm picked by timing, and two
/// batches on the GPU at once. Every attempt must still be the CPU's, bit for bit, and a third batch must be refused.
#[test]
#[ignore = "needs an NVIDIA GPU and the CUDA toolkit"]
fn grouped_and_pipelined_batches_match_the_cpu_whatever_multiply_algorithm_is_chosen() {
    let g = gpu();
    let p = Params {
        m: 32,
        k: 1024,
        nb: 1024,
        num_blocks: 8,
    };
    let seed = mh::epoch_seed(2);
    let cpu = Dataset::build(&p, &seed, p.num_blocks, 1).unwrap();
    let dev = g.build_dataset(&p, &seed, p.num_blocks).unwrap();
    let header = [0x5au8; 32];
    for group in [1usize, 4, 8] {
        let mut engine = g.attempt_engine(&dev, 32).unwrap();
        let times = engine.tune(group, 4).unwrap();
        assert!(
            times.iter().any(|t| t.is_finite()),
            "group {group}: no algorithm ran"
        );
        let mut grouper = SliceGrouper::new(header, p.num_blocks, group, u64::MAX - 40);
        let mut sent = vec![];
        for _ in 0..2 {
            let b = grouper.next_batch(32 / group);
            sent.push(b.nonces);
            engine.submit(b.seeds, b.slices).unwrap();
        }
        assert_eq!(engine.in_flight(), 2);
        let b = grouper.next_batch(1);
        assert!(
            engine.submit(b.seeds, b.slices).is_err(),
            "a third batch in flight"
        );
        for nonces in sent {
            let got = engine.collect().unwrap().unwrap();
            assert_eq!(got.len(), nonces.len());
            for (nonce, a) in nonces.iter().zip(&got) {
                assert_eq!(
                    *a,
                    mh::compute_attempt(&cpu, &header, *nonce).unwrap(),
                    "group {group}, nonce {nonce}"
                );
            }
        }
        assert!(engine.collect().unwrap().is_none());
    }
}

#[test]
#[ignore = "needs an NVIDIA GPU and the CUDA toolkit"]
fn the_search_finds_only_valid_nonces_and_the_lowest_in_its_batch() {
    let g = gpu();
    let p = Params {
        m: 32,
        k: 1024,
        nb: 1024,
        num_blocks: 8,
    };
    let seed = mh::epoch_seed(0);
    let cpu = Dataset::build(&p, &seed, p.num_blocks, 1).unwrap();
    let dev = g.build_dataset(&p, &seed, p.num_blocks).unwrap();
    let header = [4u8; 32];
    let target = mh::bits_to_target(4).unwrap(); // about 1 in 16 attempts
    let mut engine = g.attempt_engine(&dev, 16).unwrap();
    let (found, tried) = engine.search(&header, &target, 100, 4096).unwrap();
    let found = found.expect("a solution within 4096 nonces");
    assert!((1..=4096).contains(&tried));
    assert!(
        mh::verify(&cpu, &header, found.nonce, &target).unwrap(),
        "the CPU agrees it is valid"
    );
    assert_eq!(
        found.attempt,
        mh::compute_attempt(&cpu, &header, found.nonce).unwrap()
    );
    // no earlier nonce in its batch is valid
    let batch_start = 100 + (found.nonce - 100) / 16 * 16;
    for n in batch_start..found.nonce {
        assert!(
            !mh::verify(&cpu, &header, n, &target).unwrap(),
            "nonce {n} is earlier and valid"
        );
    }
    // an impossible target: nothing found, everything tried
    let (none, tried) = engine.search(&header, &U256::ONE, 0, 64).unwrap();
    assert!(none.is_none());
    assert_eq!(tried, 64);
}

// ------------------------------------------------------------------ the golden vectors, on the GPU

fn bytes32(v: &Value) -> [u8; 32] {
    hex(v.as_str().unwrap()).unwrap().try_into().unwrap()
}

fn params_of(v: &Value) -> Params {
    let f = |k: &str| usize::try_from(v[k].as_u64().unwrap()).unwrap();
    Params {
        m: f("m"),
        k: f("k"),
        nb: f("nb"),
        num_blocks: f("num_blocks"),
    }
}

fn check_slices(g: &Gpu, dev: &DeviceDataset, want: &serde_json::Map<String, Value>, what: &str) {
    for (j, w) in want {
        let j: usize = j.parse().unwrap();
        assert_eq!(
            sha_hex(&g.read_slice(dev, j).unwrap()),
            w.as_str().unwrap(),
            "{what}: slice {j}"
        );
    }
}

fn check_attempts(g: &Gpu, dev: &DeviceDataset, attempts: &[Value], what: &str) {
    let mut engine = g.attempt_engine(dev, 4).unwrap();
    for (i, a) in attempts.iter().enumerate() {
        let (header, nonce) = (bytes32(&a["header_hash"]), a["nonce"].as_u64().unwrap());
        let got = engine.attempts(&header, &[nonce]).unwrap().remove(0);
        let sums: Vec<String> = got.sums.iter().map(|s| format!("{s:016x}")).collect();
        let want: Vec<&str> = a["sums"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        assert_eq!(
            hex_lower(&got.seed),
            a["seed"].as_str().unwrap(),
            "{what} attempt {i}: seed"
        );
        assert_eq!(
            got.slice_index as u64,
            a["slice_index"].as_u64().unwrap(),
            "{what} attempt {i}: slice"
        );
        assert_eq!(sums, want, "{what} attempt {i}: fold sums");
        assert_eq!(
            hex_lower(&got.mix),
            a["mix"].as_str().unwrap(),
            "{what} attempt {i}: mix"
        );
        assert_eq!(
            hex_lower(&got.digest),
            a["digest"].as_str().unwrap(),
            "{what} attempt {i}: digest"
        );
    }
}

#[test]
#[ignore = "needs an NVIDIA GPU and the CUDA toolkit"]
fn the_small_vectors_on_the_gpu() {
    let g = gpu();
    let v = load("matmulhash_small").unwrap();
    for (n, case) in v["cases"].as_array().unwrap().iter().enumerate() {
        let p = params_of(&case["params"]);
        let dev = g
            .build_dataset(&p, &bytes32(&case["epoch_seed"]), p.num_blocks)
            .unwrap();
        for (j, w) in case["slice_sha256"].as_array().unwrap().iter().enumerate() {
            assert_eq!(
                sha_hex(&g.read_slice(&dev, j).unwrap()),
                w.as_str().unwrap(),
                "case {n} slice {j}"
            );
        }
        // some small vector shapes are not multiples of 4 in nb/k for the tensor cores; those
        // are skipped, and reported, rather than silently passed
        let attempts = case["attempts"].as_array().unwrap();
        if p.m.is_multiple_of(4) && p.k.is_multiple_of(4) && p.nb.is_multiple_of(4) {
            check_attempts(&g, &dev, attempts, &format!("small case {n}"));
        } else {
            println!("case {n}: shape {p:?} is not a multiple of 4; the GEMM part was skipped");
        }
    }
}

#[test]
#[ignore = "needs an NVIDIA GPU and the CUDA toolkit"]
fn the_real_and_deep_vectors_on_the_gpu() {
    let g = gpu();
    for name in ["matmulhash_real", "matmulhash_deep"] {
        let v = load(name).unwrap();
        let p = params_of(&v["params"]);
        let built = usize::try_from(v["slices_built"].as_u64().unwrap()).unwrap();
        let dev = g
            .build_dataset(&p, &bytes32(&v["epoch_seed"]), built)
            .unwrap();
        check_slices(&g, &dev, v["slice_sha256"].as_object().unwrap(), name);
        check_attempts(&g, &dev, v["attempts"].as_array().unwrap(), name);
    }
}

#[test]
#[ignore = "needs an NVIDIA GPU and the CUDA toolkit (and about 4.5 GiB of video memory)"]
fn the_full_vector_on_the_gpu_every_one_of_the_256_slices() {
    let g = gpu();
    let v = load("matmulhash_full").unwrap();
    let p = params_of(&v["params"]);
    let t = std::time::Instant::now();
    let dev = g
        .build_dataset(&p, &bytes32(&v["epoch_seed"]), p.num_blocks)
        .unwrap();
    g.synchronize().unwrap();
    println!(
        "built the whole 4 GiB dataset on the GPU in {:.3} s",
        t.elapsed().as_secs_f64()
    );
    let want = v["slice_sha256"].as_array().unwrap();
    assert_eq!(want.len(), 256);
    for (j, w) in want.iter().enumerate() {
        assert_eq!(
            sha_hex(&g.read_slice(&dev, j).unwrap()),
            w.as_str().unwrap(),
            "slice {j}"
        );
    }
    // attempts over the whole dataset agree with the reference implementation's real attempts
    let real = load("matmulhash_real").unwrap();
    check_attempts(
        &g,
        &dev,
        real["attempts"].as_array().unwrap(),
        "full dataset",
    );
}
