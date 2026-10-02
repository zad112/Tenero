//! The memory gate and the byte-rate throttle (`budget.rs`), without sockets: what they promise, what they refuse, and that
//! a waiting reader is always let go. (The same bounds against real sockets are in `transport.rs`.)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use proptest::prelude::*;
use tenero_net::budget::{Gate, GateConfig, Lane, Throttle, LARGEST_FRAME, SMALL_FRAME};

const MB: usize = 1024 * 1024;

fn small_cfg() -> GateConfig {
    GateConfig {
        small_limit: 4 * MB,
        small_per_lane: MB,
        large_limit: 2 * LARGEST_FRAME,
        large_per_lane: LARGEST_FRAME,
    }
}

fn open() -> AtomicBool {
    AtomicBool::new(false)
}

// ---- the gate --------------------------------------------------------------------------------------------------------

#[test]
fn a_frame_is_reserved_and_given_back() {
    let g = Gate::new(small_cfg());
    let lane = Lane::new();
    let t = g.acquire(&lane, 1000, &open()).unwrap();
    assert_eq!(t.size(), 1000);
    assert_eq!(g.held(), (1000, 0));
    assert_eq!(lane.held(), (1000, 0));
    let big = g.acquire(&lane, SMALL_FRAME + 1, &open()).unwrap();
    assert_eq!(g.held(), (1000, SMALL_FRAME + 1));
    drop(t);
    assert_eq!(g.held(), (0, SMALL_FRAME + 1));
    drop(big);
    assert_eq!(g.held(), (0, 0));
    assert_eq!(lane.held(), (0, 0));
}

#[test]
fn the_size_that_decides_small_or_large_is_exact() {
    let g = Gate::new(small_cfg());
    let lane = Lane::new();
    let a = g.acquire(&lane, SMALL_FRAME, &open()).unwrap();
    assert_eq!(g.held(), (SMALL_FRAME, 0), "the largest small frame");
    let b = g.acquire(&lane, SMALL_FRAME + 1, &open()).unwrap();
    assert_eq!(
        g.held(),
        (SMALL_FRAME, SMALL_FRAME + 1),
        "one byte more is large"
    );
    drop((a, b));
}

#[test]
fn a_frame_over_the_largest_there_can_be_is_refused_at_once() {
    let g = Gate::new(small_cfg());
    let lane = Lane::new();
    assert!(g.acquire(&lane, LARGEST_FRAME + 1, &open()).is_none());
    assert!(g.try_acquire(&lane, LARGEST_FRAME + 1).is_none());
    assert!(g.try_acquire(&lane, LARGEST_FRAME).is_some());
}

#[test]
fn a_reader_waits_for_room_and_gets_it_when_a_ticket_is_dropped() {
    let g = Gate::new(small_cfg());
    let (a, b) = (Lane::new(), Lane::new());
    // two large frames fill the large pool (one lane each)
    let t1 = g.acquire(&a, LARGEST_FRAME, &open()).unwrap();
    let t2 = g.acquire(&b, LARGEST_FRAME, &open()).unwrap();
    let c = Lane::new();
    let (tx, rx) = channel();
    let g2 = Arc::clone(&g);
    let waiter = thread::spawn(move || {
        let t = g2.acquire(&c, LARGEST_FRAME, &open());
        tx.send(t.is_some()).unwrap();
        t
    });
    assert!(
        rx.recv_timeout(Duration::from_millis(250)).is_err(),
        "it must wait while the pool is full"
    );
    assert!(g.stats.waits.load(Ordering::Relaxed) >= 1);
    drop(t1);
    assert!(rx.recv_timeout(Duration::from_secs(2)).unwrap());
    drop(t2);
    drop(waiter.join().unwrap());
    assert_eq!(g.held(), (0, 0));
}

#[test]
fn one_connection_cannot_hold_two_large_frames_but_another_can_hold_one() {
    let g = Gate::new(small_cfg());
    let (a, b) = (Lane::new(), Lane::new());
    let t = g.try_acquire(&a, LARGEST_FRAME).unwrap();
    assert!(
        g.try_acquire(&a, LARGEST_FRAME).is_none(),
        "the pool has room, the lane has none"
    );
    assert!(g.try_acquire(&b, LARGEST_FRAME).is_some());
    drop(t);
    assert!(g.try_acquire(&a, LARGEST_FRAME).is_some());
}

#[test]
fn one_connection_cannot_take_the_whole_small_pool() {
    let g = Gate::new(small_cfg());
    let (a, b) = (Lane::new(), Lane::new());
    let mut held = vec![];
    while let Some(t) = g.try_acquire(&a, 64 * 1024) {
        held.push(t);
    }
    assert_eq!(held.len(), 16, "1 MiB per lane in 64 KiB frames");
    assert!(
        g.try_acquire(&b, 64 * 1024).is_some(),
        "the pool still has room"
    );
}

#[test]
fn a_full_pool_of_large_frames_does_not_stop_small_ones() {
    let g = Gate::new(small_cfg());
    let (a, b, c) = (Lane::new(), Lane::new(), Lane::new());
    let _t1 = g.try_acquire(&a, LARGEST_FRAME).unwrap();
    let _t2 = g.try_acquire(&b, LARGEST_FRAME).unwrap();
    assert!(
        g.try_acquire(&c, LARGEST_FRAME).is_none(),
        "the large pool is full"
    );
    assert!(
        g.try_acquire(&c, 100).is_some(),
        "a ping still gets through while two peers hold large frames open"
    );
}

#[test]
fn a_waiting_reader_is_let_go_when_its_connection_closes() {
    let g = Gate::new(small_cfg());
    let a = Lane::new();
    let _t = g.try_acquire(&a, LARGEST_FRAME).unwrap();
    let closed = Arc::new(AtomicBool::new(false));
    let (g2, c2, a2) = (Arc::clone(&g), Arc::clone(&closed), Arc::clone(&a));
    let waiter = thread::spawn(move || g2.acquire(&a2, LARGEST_FRAME, &c2).is_none());
    thread::sleep(Duration::from_millis(150));
    let at = Instant::now();
    closed.store(true, Ordering::SeqCst);
    assert!(waiter.join().unwrap(), "it must give up, not get a ticket");
    assert!(
        at.elapsed() < Duration::from_millis(500),
        "took {:?} to notice",
        at.elapsed()
    );
}

#[test]
fn a_closed_connection_gets_nothing_even_when_there_is_room() {
    let g = Gate::new(small_cfg());
    let closed = AtomicBool::new(true);
    assert!(g.acquire(&Lane::new(), 10, &closed).is_none());
}

#[test]
fn settings_that_could_never_fit_a_frame_are_raised_so_no_reader_waits_for_ever() {
    let g = Gate::new(GateConfig {
        small_limit: 1,
        small_per_lane: 1,
        large_limit: 1,
        large_per_lane: 1,
    });
    let lane = Lane::new();
    assert!(g.config().small_per_lane >= SMALL_FRAME);
    assert!(g.config().large_per_lane >= LARGEST_FRAME);
    assert!(g.config().large_limit >= g.config().large_per_lane);
    assert!(g.config().small_limit >= g.config().small_per_lane);
    assert!(g.acquire(&lane, LARGEST_FRAME, &open()).is_some());
    assert!(g.acquire(&lane, SMALL_FRAME, &open()).is_some());
}

#[test]
fn the_peaks_are_recorded() {
    let g = Gate::new(small_cfg());
    let lane = Lane::new();
    {
        let _a = g.try_acquire(&lane, 1000).unwrap();
        let _b = g.try_acquire(&lane, 2000).unwrap();
        let _c = g.try_acquire(&lane, SMALL_FRAME + 5).unwrap();
    }
    assert_eq!(g.stats.peak_small.load(Ordering::SeqCst), 3000);
    assert_eq!(g.stats.peak_large.load(Ordering::SeqCst), SMALL_FRAME + 5);
    assert_eq!(g.held(), (0, 0));
}

#[test]
fn many_threads_never_take_more_than_the_pools_hold() {
    let g = Gate::new(small_cfg());
    let cfg = g.config().clone();
    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = vec![];
    for i in 0..12usize {
        let (g, stop) = (Arc::clone(&g), Arc::clone(&stop));
        threads.push(thread::spawn(move || {
            let lane = Lane::new();
            let mut n = 0u64;
            while !stop.load(Ordering::SeqCst) {
                let size =
                    [200, 70_000, SMALL_FRAME + 1, 3 * MB, LARGEST_FRAME][(i + n as usize) % 5];
                if let Some(t) = g.acquire(&lane, size, &stop) {
                    let (s, l) = g.held();
                    assert!(s <= 4 * MB && l <= 2 * LARGEST_FRAME, "held {s} {l}");
                    thread::sleep(Duration::from_micros(200));
                    drop(t);
                }
                n += 1;
            }
            n
        }));
    }
    thread::sleep(Duration::from_millis(700));
    stop.store(true, Ordering::SeqCst);
    let done: u64 = threads.into_iter().map(|t| t.join().unwrap()).sum();
    assert!(done > 100, "only {done} reservations: threads starved");
    assert!(g.stats.peak_small.load(Ordering::SeqCst) <= cfg.small_limit);
    assert!(g.stats.peak_large.load(Ordering::SeqCst) <= cfg.large_limit);
    assert_eq!(g.held(), (0, 0), "everything was given back");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Any order of reservations and releases on three connections: the pool totals always equal the sum of the live tickets,
    /// never pass a limit, and a lane never passes its own.
    #[test]
    fn the_accounts_always_balance(ops in proptest::collection::vec((0usize..3, 0usize..6, any::<bool>()), 1..200)) {
        let cfg = small_cfg();
        let g = Gate::new(cfg.clone());
        let lanes = [Lane::new(), Lane::new(), Lane::new()];
        let sizes = [10usize, 5_000, SMALL_FRAME, SMALL_FRAME + 1, 3 * MB, LARGEST_FRAME];
        let mut live: Vec<(usize, tenero_net::budget::Ticket)> = vec![];
        for (lane, size, release) in ops {
            if release && !live.is_empty() {
                live.remove(size % live.len());
            } else if let Some(t) = g.try_acquire(&lanes[lane], sizes[size]) {
                live.push((lane, t));
            }
            let (mut s, mut l) = (0, 0);
            let mut per = [(0usize, 0usize); 3];
            for (lane, t) in &live {
                if t.size() > SMALL_FRAME { l += t.size(); per[*lane].1 += t.size(); } else { s += t.size(); per[*lane].0 += t.size(); }
            }
            prop_assert_eq!(g.held(), (s, l));
            prop_assert!(s <= cfg.small_limit && l <= cfg.large_limit);
            for (i, p) in per.iter().enumerate() {
                prop_assert_eq!(lanes[i].held(), *p);
                prop_assert!(p.0 <= cfg.small_per_lane && p.1 <= cfg.large_per_lane);
            }
        }
    }
}

// ---- the throttle ------------------------------------------------------------------------------------------------------

#[test]
fn a_burst_is_free_and_more_than_that_waits_in_proportion() {
    let t0 = Instant::now();
    let mut t = Throttle::new(1000, 5000);
    assert_eq!(
        t.take(5000, t0),
        Duration::ZERO,
        "the burst is allowed at once"
    );
    // 2,000 over the burst at 1,000 a second: two seconds
    let w = t.take(2000, t0);
    assert!(
        w >= Duration::from_millis(1990) && w <= Duration::from_millis(2010),
        "{w:?}"
    );
}

#[test]
fn what_was_waited_for_is_paid_and_the_bucket_refills_over_time() {
    let t0 = Instant::now();
    let mut t = Throttle::new(1000, 5000);
    t.take(5000, t0);
    let w = t.take(1000, t0);
    assert_eq!(w, Duration::from_secs(1));
    // after waiting that second, the debt is paid and the next small read is free again
    let t1 = t0 + Duration::from_secs(1);
    assert_eq!(t.take(0, t1), Duration::ZERO);
    // a quiet minute refills to the burst and no further
    let t2 = t1 + Duration::from_secs(60);
    assert_eq!(t.take(5000, t2), Duration::ZERO);
    let w = t.take(1000, t2);
    assert_eq!(
        w,
        Duration::from_secs(1),
        "the bucket held the burst, not a minute's worth"
    );
}

#[test]
fn a_rate_of_zero_is_no_limit() {
    let mut t = Throttle::new(0, 10);
    assert_eq!(t.take(usize::MAX, Instant::now()), Duration::ZERO);
}

#[test]
fn a_steady_reader_under_the_rate_never_waits() {
    let t0 = Instant::now();
    let mut t = Throttle::new(10_000, 10_000);
    for i in 0..100u64 {
        // 5,000 bytes every half second is 10,000 a second: right at the rate; 4,000 is under it
        let w = t.take(4000, t0 + Duration::from_millis(500 * i));
        assert_eq!(w, Duration::ZERO, "at step {i}");
    }
}

#[test]
fn a_sustained_flood_is_held_to_the_rate() {
    // read as fast as allowed for 100 simulated seconds at 1,000 bytes a second: the bytes allowed through are the
    // burst plus the rate times the time, give or take one read
    let t0 = Instant::now();
    let mut t = Throttle::new(1000, 2000);
    let mut now = t0;
    let mut total = 0usize;
    while now < t0 + Duration::from_secs(100) {
        total += 500;
        now += t.take(500, now);
    }
    let allowed = 2000 + 1000 * 100;
    assert!(
        total <= allowed + 1000,
        "{total} bytes through, {allowed} allowed"
    );
    assert!(total >= allowed - 1500, "{total} bytes through: too slow");
}
