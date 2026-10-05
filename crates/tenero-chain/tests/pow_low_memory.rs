//! The node's low-memory proof of work: ONE dataset at a time (about 4 GiB at the real parameters, so a node stays inside 8 GB).
//! Measured before this mode existed (2026-10-04): a process checking a real-proof-of-work chain went from 4.0 to 8.0 GiB of dataset at the first
//! epoch boundary and stayed there, because the last two epochs were kept. Each test has a contrast with the default mode, so it can tell them apart.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::Duration;

use tenero_chain::{MatmulPow, PowCheck};
use tenero_core::matmulhash::{self, Dataset, Params};

fn small() -> Params {
    Params {
        m: 8,
        k: 64,
        nb: 64,
        num_blocks: 8,
    }
}

/// Epochs of 3 blocks: heights 1 to 3 are epoch 0, 4 to 6 epoch 1, 7 to 9 epoch 2, and so on.
fn low() -> MatmulPow {
    MatmulPow::low_memory(small(), 3, 1).unwrap()
}

fn real_dataset(params: &Params, epoch: u64, threads: usize) -> Result<Dataset, String> {
    Dataset::build(
        params,
        &matmulhash::epoch_seed(epoch),
        params.num_blocks,
        threads,
    )
}

#[test]
fn one_dataset_is_kept_at_a_time_and_going_back_across_an_epoch_rebuilds_it() {
    let p = low();
    assert_eq!(p.max_datasets(), 1);
    p.dataset_for(1).unwrap();
    assert!(p.has_dataset(1));
    p.dataset_for(4).unwrap();
    assert!(!p.has_dataset(1), "the old epoch's dataset was kept");
    assert!(p.has_dataset(4));
    assert_eq!(p.builds(), 2);
    // a block of the old epoch again (a fork across the boundary): rebuilt, and the new one goes
    p.dataset_for(2).unwrap();
    assert!(p.has_dataset(2) && !p.has_dataset(4));
    assert_eq!(p.builds(), 3);
    // the same epoch costs nothing
    p.dataset_for(3).unwrap();
    assert_eq!(p.builds(), 3);

    // the default (the miner's) keeps two
    let d = MatmulPow::new(small(), 3, 1).unwrap();
    assert_eq!(d.max_datasets(), 2);
    d.dataset_for(1).unwrap();
    d.dataset_for(4).unwrap();
    assert!(d.has_dataset(1) && d.has_dataset(4));
}

/// Builds `chain` of heights with a builder that counts how often the PREVIOUS dataset was still in memory when a build began.
fn still_alive_at_build_start(max_kept: usize, heights: &[u64]) -> usize {
    let prev: Arc<Mutex<Option<Weak<Dataset>>>> = Arc::new(Mutex::new(None));
    let alive = Arc::new(AtomicUsize::new(0));
    let (prev2, alive2) = (Arc::clone(&prev), Arc::clone(&alive));
    let p = MatmulPow::with_builder_limit(
        small(),
        3,
        1,
        max_kept,
        Box::new(move |params, epoch, threads| {
            if prev2
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|w| w.upgrade().is_some())
            {
                alive2.fetch_add(1, Ordering::SeqCst);
            }
            real_dataset(params, epoch, threads)
        }),
    )
    .unwrap();
    for &h in heights {
        let d = p.dataset_for(h).unwrap();
        *prev.lock().unwrap() = Some(Arc::downgrade(&d));
        drop(d);
    }
    alive.load(Ordering::SeqCst)
}

#[test]
fn the_old_dataset_is_freed_before_the_next_one_is_built() {
    assert_eq!(still_alive_at_build_start(1, &[1, 4, 7, 1, 10]), 0);
    // with two allowed, the previous one is still there when the next is built (that is what made 8 GiB)
    assert!(still_alive_at_build_start(2, &[1, 4]) >= 1);
}

/// The most builds that ever ran at the same moment, for callers asking for different epochs at once.
fn most_builds_at_once(max_kept: usize, heights: &[u64]) -> usize {
    let (now, most) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (now2, most2) = (Arc::clone(&now), Arc::clone(&most));
    let p = Arc::new(
        MatmulPow::with_builder_limit(
            small(),
            3,
            1,
            max_kept,
            Box::new(move |params, epoch, threads| {
                let n = now2.fetch_add(1, Ordering::SeqCst) + 1;
                most2.fetch_max(n, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(150));
                let d = real_dataset(params, epoch, threads);
                now2.fetch_sub(1, Ordering::SeqCst);
                d
            }),
        )
        .unwrap(),
    );
    let handles: Vec<_> = heights
        .iter()
        .map(|&h| {
            let p = Arc::clone(&p);
            thread::spawn(move || p.dataset_for(h).map(|_| ()))
        })
        .collect();
    for h in handles {
        h.join().unwrap().unwrap();
    }
    most.load(Ordering::SeqCst)
}

#[test]
fn builds_never_overlap_with_one_dataset() {
    assert_eq!(most_builds_at_once(1, &[1, 4, 7, 10]), 1);
    // with two allowed, builds of different epochs do overlap (two datasets under construction at once)
    assert_eq!(most_builds_at_once(2, &[1, 4]), 2);
}

#[test]
fn a_check_still_using_the_old_dataset_holds_the_next_build_back() {
    let prev: Arc<Mutex<Option<Weak<Dataset>>>> = Arc::new(Mutex::new(None));
    let prev2 = Arc::clone(&prev);
    let p = Arc::new(
        MatmulPow::with_builder_limit(
            small(),
            3,
            1,
            1,
            Box::new(move |params, epoch, threads| {
                // a build must not begin while the old dataset is still in memory
                assert!(
                    prev2
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_none_or(|w| w.upgrade().is_none()),
                    "a build began while the old dataset was in use"
                );
                real_dataset(params, epoch, threads)
            }),
        )
        .unwrap(),
    );
    let old = p.dataset_for(1).unwrap(); // a check in progress holds this
    *prev.lock().unwrap() = Some(Arc::downgrade(&old));
    let p2 = Arc::clone(&p);
    let next = thread::spawn(move || p2.dataset_for(4).map(|_| ()));
    thread::sleep(Duration::from_millis(250));
    assert!(
        !next.is_finished(),
        "the next build did not wait for the check"
    );
    drop(old); // the check ends
    next.join().unwrap().unwrap();
    assert!(p.has_dataset(4) && !p.has_dataset(1));
}

#[test]
fn a_prefetch_does_nothing_with_one_dataset() {
    let p = low();
    p.prefetch(4);
    thread::sleep(Duration::from_millis(150));
    assert!(!p.has_dataset(4));
    assert_eq!(p.builds(), 0);
}

#[test]
fn the_number_of_datasets_must_be_one_or_two() {
    for bad in [0usize, 3, 9] {
        assert!(
            MatmulPow::with_builder_limit(small(), 3, 1, bad, Box::new(real_dataset)).is_err(),
            "{bad}"
        );
    }
}
