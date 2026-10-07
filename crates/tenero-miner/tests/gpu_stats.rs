//! GPU readings (M10.2). The pure parts run anywhere; the reading of a real card is `#[ignore]`d (the owner's machine).

use nvml_wrapper::bitmasks::device::ThrottleReasons;
use tenero_miner::gpu_stats::{implied_read_bytes_per_sec, limited_by, GpuProbe, GpuReading};

#[test]
fn what_limits_the_clock_is_told_only_when_it_matters() {
    assert_eq!(limited_by(ThrottleReasons::empty()), None);
    assert_eq!(limited_by(ThrottleReasons::GPU_IDLE), None);
    assert_eq!(limited_by(ThrottleReasons::SW_POWER_CAP), Some("power cap"));
    assert_eq!(
        limited_by(ThrottleReasons::HW_POWER_BRAKE_SLOWDOWN),
        Some("power cap")
    );
    assert_eq!(
        limited_by(ThrottleReasons::SW_THERMAL_SLOWDOWN),
        Some("temperature")
    );
    assert_eq!(
        limited_by(ThrottleReasons::HW_THERMAL_SLOWDOWN),
        Some("temperature")
    );
    assert_eq!(
        limited_by(ThrottleReasons::HW_SLOWDOWN),
        Some("temperature")
    );
    // both: the temperature is the one to fix first
    assert_eq!(
        limited_by(ThrottleReasons::SW_POWER_CAP | ThrottleReasons::SW_THERMAL_SLOWDOWN),
        Some("temperature")
    );
}

#[test]
fn the_implied_reads_are_the_rate_times_one_slice() {
    let slice = 8192u64 * 2048; // k * nb: 16 MiB
    assert_eq!(slice, 16 * 1024 * 1024);
    assert_eq!(implied_read_bytes_per_sec(0.0, slice), 0.0);
    assert_eq!(implied_read_bytes_per_sec(1000.0, slice), 16_777_216_000.0);
}

#[test]
fn a_reading_with_nothing_in_it_is_the_default() {
    assert_eq!(GpuReading::default().temp_c, None);
}

#[test]
fn a_card_that_is_not_there_is_an_error_and_not_a_panic() {
    // (on a machine with no NVML the error is "not available"; with it, "cannot open GPU 99": either way an `Err`)
    assert!(GpuProbe::open(99).is_err());
}

/// The owner's machine: a real reading. Run with `--ignored --nocapture` and look at the numbers.
#[test]
#[ignore = "needs an NVIDIA GPU and its driver"]
fn a_real_card_gives_a_real_reading() {
    let p = GpuProbe::open(0).expect("NVML and GPU 0");
    let r = p.read();
    println!("{r:#?}");
    assert!(r.name.is_some() && r.temp_c.is_some());
    assert!(r.temp_c.unwrap() > 0 && r.temp_c.unwrap() < 120);
    // units: watts (not milliwatts) and MiB (not bytes or KiB)
    let w = r.power_w.unwrap();
    assert!(w > 1.0 && w < 800.0, "{w} W");
    assert!(r.mem_total_mib.unwrap() > 1024 && r.mem_total_mib.unwrap() < 1_000_000);
    assert!(r.mem_used_mib.unwrap() <= r.mem_total_mib.unwrap());
}

// ---- choosing a batch size -------------------------------------------------------------------------------------------------------

use tenero_miner::gpu::{auto_batch, choose_batch, measure_batches, AUTO_BATCHES};

#[test]
fn the_fastest_batch_is_chosen_and_a_tie_goes_to_the_smaller() {
    assert_eq!(
        choose_batch(&[(128, 33_000.0), (256, 35_000.0), (512, 34_000.0)]),
        Some(256)
    );
    assert_eq!(choose_batch(&[(128, 35_000.0), (256, 33_000.0)]), Some(128));
    assert_eq!(
        choose_batch(&[(256, 34_000.0), (128, 34_000.0)]),
        Some(128),
        "a tie: less video memory"
    );
    assert_eq!(choose_batch(&[(128, 34_000.0), (256, 34_000.0)]), Some(128));
    assert_eq!(choose_batch(&[(512, 1.0)]), Some(512));
}

#[test]
fn nothing_usable_is_nothing_chosen() {
    assert_eq!(choose_batch(&[]), None);
    assert_eq!(
        choose_batch(&[
            (128, f64::NAN),
            (256, f64::INFINITY),
            (512, 0.0),
            (64, -3.0)
        ]),
        None
    );
    assert_eq!(
        choose_batch(&[(0, 5_000.0)]),
        None,
        "a batch of 0 is not a batch"
    );
    // the usable one wins over the rubbish
    assert_eq!(choose_batch(&[(128, f64::NAN), (256, 10.0)]), Some(256));
}

#[test]
fn auto_tries_the_sizes_that_were_measured_and_one_more() {
    assert_eq!(AUTO_BATCHES, [128, 256, 512]);
}

/// The owner's machine: measures for real (about 25 s) and picks.
#[test]
#[ignore = "needs an NVIDIA GPU, the CUDA DLLs and about 4.5 GiB of video memory"]
fn a_real_card_is_measured_and_a_batch_is_chosen() {
    let lines = std::sync::Mutex::new(vec![]);
    let log = |m: &str| {
        eprintln!("  {m}");
        lines.lock().unwrap().push(m.to_string());
    };
    let rates = measure_batches(
        0,
        tenero_core::matmulhash::Params::DEFAULT,
        100,
        &AUTO_BATCHES,
        3.0,
        &log,
    );
    assert_eq!(rates.len(), 3, "{:?}", lines.lock().unwrap());
    for (b, r) in &rates {
        // the speed of this card is in the tens of thousands (BENCHMARKS.md); anything near 0 or absurd is a broken measurement
        assert!(*r > 5_000.0 && *r < 200_000.0, "batch {b}: {r}");
    }
    let chosen = auto_batch(0, tenero_core::matmulhash::Params::DEFAULT, 100, 128, &log);
    assert!(AUTO_BATCHES.contains(&chosen));
}

/// The owner's machine. The GPU backend holds ONE 4 GiB dataset in video memory, whatever epoch it mines: it frees the old one before
/// it builds the next (it used to keep the epoch before and build the next ahead, which was about 9.2 GiB of committed memory for the
/// miner process on Windows). Mines at heights in four different epochs and reads the card's memory use each time: after the first
/// dataset it must not grow by anything near another 4 GiB. Needs the card to itself (other programs' video memory changing during the
/// test would show up in the numbers), so stop any other miner first.
#[test]
#[ignore = "needs an NVIDIA GPU, the CUDA DLLs and about 4.5 GiB of video memory, and no other miner running"]
fn on_a_real_card_one_dataset_is_held_in_video_memory_across_epoch_boundaries() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tenero_core::u256::U256;
    use tenero_core::v2::BlockHeader;
    use tenero_miner::gpu::GpuBackend;
    use tenero_miner::{Backend, Counters, Job};

    let probe = GpuProbe::open(0).expect("NVML and GPU 0");
    let before = probe.read().mem_used_mib.unwrap();
    let mut backend =
        GpuBackend::new(0, tenero_core::matmulhash::Params::DEFAULT, 100, 128).unwrap();
    let counters = Counters::default();
    let mut used = vec![];
    for (i, height) in [1u64, 101, 201, 301].into_iter().enumerate() {
        let job = Job {
            id: i as u64 + 1,
            header: BlockHeader {
                version: tenero_core::v2::VERSION,
                prev_id: [0; 32],
                timestamp: 1,
                tx_root: [0; 32],
                nonce: 0,
                mix: [0; 64],
            },
            height,
            target: U256::ZERO, // never met: it mines until it is told to stop
            stale: Arc::new(AtomicBool::new(false)),
            nonce_start: None,
        };
        std::thread::scope(|s| {
            let t = s.spawn(|| backend.mine(&job, &counters));
            std::thread::sleep(std::time::Duration::from_millis(2500));
            used.push(probe.read().mem_used_mib.unwrap());
            job.stale.store(true, Ordering::SeqCst);
            t.join().unwrap().expect("the GPU backend failed");
        });
    }
    eprintln!(
        "  video memory in use: {before} MiB before; while mining in epochs 0 to 3: {used:?} MiB"
    );
    assert_eq!(counters.dataset_builds.load(Ordering::Relaxed), 4);
    let first = used[0] - before;
    assert!(
        (3_800..6_000).contains(&first),
        "the first dataset took {first} MiB (4,096 expected, plus buffers)"
    );
    for (i, u) in used.iter().enumerate().skip(1) {
        let grown = *u as i64 - used[0] as i64;
        assert!(
            grown.abs() < 1_000,
            "epoch {i}: video memory is {grown} MiB away from the first epoch's (a kept second dataset would be about +4,096)"
        );
    }
}

/// The owner's machine. What the separate miner showed on 2026-10-02 at the chain's easy starting difficulty (one attempt in eight): 40
/// blocks found against 184 expected, because a batch of 512 attempts holds dozens of solutions and only the first becomes a block.
/// With the attempts after the first solution left out, the blocks found and the blocks expected agree.
#[test]
#[ignore = "needs an NVIDIA GPU, the CUDA DLLs and about 4.5 GiB of video memory"]
fn on_a_real_card_at_an_easy_target_the_blocks_found_match_the_blocks_expected() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tenero_core::u256::U256;
    use tenero_core::v2::BlockHeader;
    use tenero_miner::gpu::GpuBackend;
    use tenero_miner::rate::work_of;
    use tenero_miner::{Job, Miner, Msg};

    let target = U256::pow2(253).unwrap(); // one attempt in eight
    assert_eq!(work_of(&target), 8.0);
    let mut miner =
        Miner::spawn(|| GpuBackend::new(0, tenero_core::matmulhash::Params::DEFAULT, 100, 512));
    let c = Arc::clone(&miner.counters);
    let jobs = 300u64;
    let mut found = 0u64;
    let mut last_counted = 0u64;
    for id in 1..=jobs {
        miner.submit(Job {
            id,
            header: BlockHeader {
                version: tenero_core::v2::VERSION,
                prev_id: [0; 32],
                timestamp: id,
                tx_root: [0; 32],
                nonce: 0,
                mix: [0; 64],
            },
            height: 1,
            target,
            stale: Arc::new(AtomicBool::new(false)),
            nonce_start: None,
        });
        let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            match miner.try_msg() {
                Some(Msg::Solved(_)) => {
                    found += 1;
                    // exactly: the solution is itself an attempt that counts, so every job counts at least one
                    let counted =
                        c.attempts.load(Ordering::Relaxed) - c.discarded.load(Ordering::Relaxed);
                    assert!(counted > last_counted, "job {id} counted no attempt");
                    last_counted = counted;
                    break;
                }
                Some(Msg::Failed(e)) => panic!("the GPU backend failed: {e}"),
                Some(_) => {}
                None if std::time::Instant::now() > until => panic!("no solution in 60 s"),
                None => std::thread::sleep(std::time::Duration::from_millis(1)),
            }
        }
    }
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while c.searching() && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let (expected, attempts, discarded) = (
        c.expected_blocks(),
        c.attempts.load(Ordering::Relaxed),
        c.discarded.load(Ordering::Relaxed),
    );
    eprintln!("{found} blocks found, {expected:.1} expected, {attempts} attempts made, {discarded} of them after a first solution");
    assert_eq!(found, jobs);
    // (without the fix: about 300 * 512 / 8 = 19,200 expected. The number of attempts to a first solution varies by about 5 % over 300
    // jobs, so the bounds are 15 %.)
    assert!(
        expected > 0.85 * jobs as f64 && expected < 1.15 * jobs as f64,
        "{expected} expected for {found} found"
    );
}
