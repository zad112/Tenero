//! The rate meter (M10.2): what each window shows, and what it leaves out.

use tenero_miner::rate::{RateMeter, Rates};
use tenero_miner::Counters;

/// Feeds one look a second for `secs` seconds at `per_sec` attempts a second, starting at `*t` ms with the counter at `*c`.
fn run(m: &mut RateMeter, t: &mut u64, c: &mut u64, secs: u64, per_sec: u64, searching: bool) {
    for _ in 0..secs {
        *t += 1000;
        *c += per_sec;
        m.record(*t, *c, searching);
    }
}

fn near(a: Option<f64>, b: f64) -> bool {
    a.is_some_and(|a| (a - b).abs() < b * 0.001 + 0.001)
}

#[test]
fn a_new_meter_has_no_figures() {
    let m = RateMeter::new();
    assert_eq!(m.rates(), Rates::default());
    assert!(m.average().is_none() && m.rate(10_000).is_none());
}

#[test]
fn a_window_shows_only_once_the_samples_cover_it() {
    let (mut m, mut t, mut c) = (RateMeter::new(), 5_000u64, 0u64);
    m.record(t, c, true);
    run(&mut m, &mut t, &mut c, 9, 1000, true);
    assert!(
        m.rates().s10.is_none(),
        "9 seconds is not a 10 second window"
    );
    assert!(near(m.average(), 1000.0), "the average has no such rule");
    run(&mut m, &mut t, &mut c, 1, 1000, true);
    assert!(near(m.rates().s10, 1000.0));
    assert!(m.rates().s60.is_none() && m.rates().m15.is_none());
    run(&mut m, &mut t, &mut c, 50, 1000, true);
    assert!(near(m.rates().s60, 1000.0));
    assert!(m.rates().m15.is_none());
    run(&mut m, &mut t, &mut c, 840, 1000, true);
    let r = m.rates();
    assert!(
        near(r.m15, 1000.0)
            && near(r.s60, 1000.0)
            && near(r.s10, 1000.0)
            && near(r.average, 1000.0)
    );
    assert!(r.searching);
}

#[test]
fn less_than_a_second_of_searching_gives_no_figure() {
    let mut m = RateMeter::new();
    m.record(0, 0, true);
    m.record(500, 100_000, true);
    assert!(
        m.average().is_none(),
        "100,000 in half a second would be a guess"
    );
    m.record(1_000, 200_000, true);
    assert!(near(m.average(), 200_000.0));
}

#[test]
fn time_not_spent_searching_is_left_out_of_every_figure() {
    let (mut m, mut t, mut c) = (RateMeter::new(), 0u64, 0u64);
    m.record(t, c, true);
    run(&mut m, &mut t, &mut c, 30, 1000, true);
    // 20 s of waiting for the node (the counter does not move, and even if it did it would not count)
    run(&mut m, &mut t, &mut c, 20, 0, false);
    run(&mut m, &mut t, &mut c, 40, 1000, true);
    let r = m.rates();
    assert!(
        near(r.average, 1000.0),
        "{r:?}: the pause is not in the average"
    );
    assert!(
        near(r.s60, 1000.0),
        "{r:?}: nor in the 60 s window, which has 20 s of pause in it"
    );
    assert!(near(r.s10, 1000.0));
}

#[test]
fn attempts_counted_while_not_searching_are_not_counted() {
    let (mut m, mut t, mut c) = (RateMeter::new(), 0u64, 0u64);
    m.record(t, c, true);
    run(&mut m, &mut t, &mut c, 10, 1000, true);
    // not searching, yet the counter moves (a late batch finishing): ignored
    run(&mut m, &mut t, &mut c, 10, 5000, false);
    run(&mut m, &mut t, &mut c, 10, 1000, true);
    assert!(near(m.average(), 1000.0), "{:?}", m.average());
}

#[test]
fn an_interval_that_straddles_a_change_of_state_is_not_guessed_at() {
    let mut m = RateMeter::new();
    m.record(0, 0, true);
    m.record(1_000, 1_000, true);
    m.record(2_000, 2_000, true);
    // searching at one end only: the interval is left out whichever end it is
    m.record(3_000, 1_000_000, false);
    m.record(4_000, 2_000_000, true);
    m.record(5_000, 2_001_000, true);
    m.record(6_000, 2_002_000, true);
    assert!(near(m.average(), 1000.0), "{:?}", m.average());
}

#[test]
fn a_change_of_speed_shows_first_in_the_short_window() {
    let (mut m, mut t, mut c) = (RateMeter::new(), 0u64, 0u64);
    m.record(t, c, true);
    run(&mut m, &mut t, &mut c, 1000, 1000, true);
    run(&mut m, &mut t, &mut c, 12, 2000, true);
    let r = m.rates();
    assert!(near(r.s10, 2000.0), "{r:?}");
    let s60 = r.s60.unwrap();
    assert!(
        s60 > 1100.0 && s60 < 1300.0,
        "{s60}: the longer window lags"
    );
    let avg = r.average.unwrap();
    assert!(avg > 1000.0 && avg < s60, "{avg}");
    assert!(r.m15.unwrap() < s60);
}

#[test]
fn old_samples_are_dropped_so_a_long_run_does_not_grow() {
    let (mut m, mut t, mut c) = (RateMeter::new(), 0u64, 0u64);
    m.record(t, c, true);
    run(&mut m, &mut t, &mut c, 3 * 3600, 500, true);
    assert!(m.sample_count() <= 905, "{} samples kept", m.sample_count());
    assert!(near(m.rates().m15, 500.0) && near(m.average(), 500.0));
}

#[test]
fn a_clock_set_back_neither_inflates_nor_loses_the_rates() {
    let (mut m, mut t, mut c) = (RateMeter::new(), 100_000u64, 0u64);
    m.record(t, c, true);
    run(&mut m, &mut t, &mut c, 30, 1000, true);
    // the clock goes back 50 s while the miner goes on (1,000 attempts in that interval, not counted: the time is not known)
    t -= 50_000;
    c += 1000;
    m.record(t, c, true);
    assert!(
        near(m.average(), 1000.0),
        "{:?}: the average is not inflated",
        m.average()
    );
    assert!(m.rates().s10.is_none(), "the windows start again from here");
    run(&mut m, &mut t, &mut c, 9, 1000, true);
    assert!(m.rates().s10.is_none());
    run(&mut m, &mut t, &mut c, 1, 1000, true);
    assert!(near(m.rates().s10, 1000.0), "{:?}", m.rates());
    assert!(near(m.average(), 1000.0));
    assert!(m.rates().s60.is_none());
}

#[test]
fn a_window_rests_on_a_second_of_searching_and_not_on_less() {
    // the last 10 s have 0.5 s of searching in them: too little to say anything of
    let mut m = RateMeter::new();
    m.record(0, 0, false);
    let mut c = 0u64;
    for step in 1..=200u64 {
        // idle until 20 s, then 0.5 s searching, then idle to the end of 30 s
        let t = step * 150;
        let on = (20_000..=20_450).contains(&t);
        c += if on { 150 } else { 0 };
        m.record(t, c, on);
    }
    assert!(m.rate(10_000).is_none(), "{:?}", m.rate(10_000));
    // exactly 1,000 ms of searching in the window is enough
    let mut m = RateMeter::new();
    m.record(0, 0, false);
    let (mut c, mut t) = (0u64, 0u64);
    for k in 1..=400u64 {
        t = k * 100;
        let on = (20_000..=21_000).contains(&t);
        c += if on { 100 } else { 0 };
        m.record(t, c, on);
    }
    assert_eq!(t, 40_000);
    assert!(near(m.rate(30_000), 1000.0), "{:?}", m.rate(30_000));
}

#[test]
fn a_window_means_its_own_length_and_not_a_longer_or_shorter_one() {
    let (mut m, mut t, mut c) = (RateMeter::new(), 0u64, 0u64);
    m.record(t, c, true);
    run(&mut m, &mut t, &mut c, 2000, 1000, true);
    run(&mut m, &mut t, &mut c, 55, 2000, true);
    // 55 s at 2,000 and 5 s at 1,000: 1,916.7 (a window of 50 s would say 2,000)
    assert!(near(m.rates().s60, 1916.667), "{:?}", m.rates());
    run(&mut m, &mut t, &mut c, 815, 2000, true);
    // 870 s at 2,000 and 30 s at 1,000 in the last 900 s (a 14 minute window would say 2,000)
    assert!(near(m.rates().m15, 1966.667), "{:?}", m.rates());
}

#[test]
fn the_searching_mark_follows_the_latest_look() {
    let (mut m, mut t, mut c) = (RateMeter::new(), 0u64, 0u64);
    m.record(t, c, false);
    assert!(!m.rates().searching);
    run(&mut m, &mut t, &mut c, 3, 1000, true);
    assert!(m.rates().searching);
    run(&mut m, &mut t, &mut c, 1, 0, false);
    assert!(!m.rates().searching);
}

#[test]
fn the_window_is_counted_in_searching_time_when_there_was_a_pause_inside_it() {
    // 10 s window with a 5 s pause inside: 5 s of searching at 1,000/s is 1,000/s, not 500/s
    let (mut m, mut t, mut c) = (RateMeter::new(), 0u64, 0u64);
    m.record(t, c, true);
    run(&mut m, &mut t, &mut c, 20, 1000, true);
    run(&mut m, &mut t, &mut c, 5, 0, false);
    run(&mut m, &mut t, &mut c, 5, 1000, true);
    assert!(near(m.rates().s10, 1000.0), "{:?}", m.rates());
}

// ---- the counters the backends keep for it ----------------------------------------------------------------------------------------

#[test]
fn searching_is_a_job_in_progress_and_no_dataset_being_built() {
    use std::sync::atomic::Ordering;
    let c = Counters::default();
    assert!(!c.searching(), "no job yet");
    c.in_job.store(true, Ordering::SeqCst);
    assert!(c.searching());
    {
        let _a = c.building();
        assert!(!c.searching(), "building a dataset");
        {
            let _b = c.building();
            assert!(!c.searching());
        }
        assert!(!c.searching(), "one build is still going");
    }
    assert!(c.searching(), "the builds are done");
    c.in_job.store(false, Ordering::SeqCst);
    assert!(!c.searching());
}

#[test]
fn a_real_backend_is_searching_while_it_has_a_job_and_not_after() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tenero_core::u256::U256;
    use tenero_core::v2::BlockHeader;
    use tenero_miner::{Job, Miner, Sha256Backend};

    let mut miner = Miner::spawn(|| Ok(Sha256Backend));
    let c = Arc::clone(&miner.counters);
    assert!(!c.searching(), "no job yet");
    miner.submit(Job {
        id: 1,
        header: BlockHeader {
            version: tenero_core::v2::VERSION,
            prev_id: [0; 32],
            timestamp: 1,
            tx_root: [0; 32],
            nonce: 0,
            mix: [0; 64],
        },
        height: 1,
        target: U256::ZERO, // never met
        stale: Arc::new(AtomicBool::new(false)),
    });
    let until = Instant::now() + Duration::from_secs(10);
    while c.attempts.load(Ordering::Relaxed) == 0 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(c.attempts.load(Ordering::Relaxed) > 0);
    assert!(c.searching(), "attempts are being made");
    miner.cancel();
    while c.searching() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!c.searching(), "the job was dropped");
}

#[test]
fn after_a_clock_set_back_a_window_never_mixes_the_time_before_with_the_time_after() {
    let (mut m, mut t, mut c) = (RateMeter::new(), 100_000u64, 0u64);
    m.record(t, c, true);
    run(&mut m, &mut t, &mut c, 30, 5000, true);
    t -= 3_000; // a small step back: the old samples are still older than the windows
    m.record(t, c, true);
    run(&mut m, &mut t, &mut c, 9, 1000, true);
    assert!(m.rates().s10.is_none(), "{:?}", m.rates().s10);
    run(&mut m, &mut t, &mut c, 1, 1000, true);
    assert!(near(m.rates().s10, 1000.0), "{:?}", m.rates().s10);
}
