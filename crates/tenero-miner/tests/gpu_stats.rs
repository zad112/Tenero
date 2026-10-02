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
