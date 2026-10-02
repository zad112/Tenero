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
