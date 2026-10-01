//! The matmulhash dataset cache: a background prefetch, one build however many callers, at most two datasets at once,
//! and a failed build that does not leave the next caller waiting for ever.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

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
fn pow() -> MatmulPow {
    MatmulPow::new(small(), 3, 1).unwrap()
}

fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        if cond() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("waited for {what}");
}

#[test]
fn a_prefetch_builds_the_next_epochs_dataset_in_the_background_and_only_once() {
    let p = pow();
    assert!(!p.has_dataset(4));
    p.prefetch(4); // returns at once; the build runs on a thread of its own
    wait_for("the prefetched dataset", || p.has_dataset(4));
    assert_eq!(p.builds(), 1);
    // asking for it is instant and builds nothing more
    let d = p.dataset_for(5).unwrap();
    assert_eq!(p.builds(), 1);
    // nor does prefetching what is already there
    for _ in 0..20 {
        p.prefetch(4);
        p.prefetch(6);
    }
    thread::sleep(Duration::from_millis(100));
    assert_eq!(p.builds(), 1);
    assert!(Arc::ptr_eq(&d, &p.dataset_for(4).unwrap()));
    // the genesis block has no proof of work, so there is nothing to prefetch for it
    p.prefetch(0);
    thread::sleep(Duration::from_millis(50));
    assert_eq!(p.builds(), 1);
    assert!(!p.has_dataset(0));
}

#[test]
fn callers_that_need_the_same_epoch_at_once_share_one_build() {
    let p = Arc::new(pow());
    let handles: Vec<_> = (0..12)
        .map(|_| {
            let p = Arc::clone(&p);
            thread::spawn(move || p.dataset_for(10).unwrap())
        })
        .collect();
    let datasets: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(p.builds(), 1, "twelve callers, one epoch, one build");
    assert!(datasets.iter().all(|d| Arc::ptr_eq(d, &datasets[0])));
}

#[test]
fn a_prefetch_and_a_caller_for_the_same_epoch_share_one_build_too() {
    let p = Arc::new(pow());
    p.prefetch(7);
    let p2 = Arc::clone(&p);
    let d = thread::spawn(move || p2.dataset_for(8).unwrap())
        .join()
        .unwrap();
    wait_for("the dataset", || p.has_dataset(7));
    assert_eq!(p.builds(), 1);
    assert!(Arc::ptr_eq(&d, &p.dataset_for(9).unwrap()));
}

#[test]
fn at_most_two_datasets_are_kept_and_the_one_furthest_away_goes_first() {
    let p = pow();
    p.dataset_for(1).unwrap(); // epoch 0
    p.dataset_for(4).unwrap(); // epoch 1
    assert!(p.has_dataset(1) && p.has_dataset(4));
    p.dataset_for(7).unwrap(); // epoch 2: epoch 0 is the furthest and goes
    assert!(!p.has_dataset(1), "the oldest was kept");
    assert!(p.has_dataset(4) && p.has_dataset(7));
    // going back (a reorganisation over an epoch boundary): the one furthest from epoch 1 is epoch 2... of {1, 2}
    // the nearest to epoch 0 stays
    p.dataset_for(1).unwrap(); // epoch 0 again: epochs 1 and 2 are kept, 2 is furthest
    assert!(p.has_dataset(1) && p.has_dataset(4));
    assert!(!p.has_dataset(7), "the furthest (the newest) was kept");
    assert_eq!(p.builds(), 4);
}

#[test]
fn checking_a_block_of_the_current_epoch_is_not_held_up_by_another_epochs_build() {
    // epoch 0 builds at once; every later epoch takes 600 ms
    let p = Arc::new(
        MatmulPow::with_builder(
            small(),
            3,
            1,
            Box::new(|params, epoch, threads| {
                if epoch > 0 {
                    thread::sleep(Duration::from_millis(600));
                }
                Dataset::build(
                    params,
                    &matmulhash::epoch_seed(epoch),
                    params.num_blocks,
                    threads,
                )
            }),
        )
        .unwrap(),
    );
    let d0 = p.dataset_for(1).unwrap();
    p.prefetch(4); // the slow one, in the background
    thread::sleep(Duration::from_millis(50)); // let it start
    let started = Instant::now();
    for _ in 0..50 {
        assert!(Arc::ptr_eq(&d0, &p.dataset_for(2).unwrap()));
    }
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "checking epoch 0 waited {:?} for the build of epoch 1",
        started.elapsed()
    );
    assert!(!p.has_dataset(4), "the slow build should still be running");
    wait_for("the prefetch", || p.has_dataset(4));
}

/// A builder that takes `delay`, fails the first `fail` times it is called and otherwise makes the real (small) dataset.
fn flaky(fail: u64, delay: Duration, calls: Arc<AtomicU64>) -> MatmulPow {
    MatmulPow::with_builder(
        small(),
        3,
        1,
        Box::new(move |params, epoch, threads| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            thread::sleep(delay);
            if n < fail {
                return Err("a build that fails".into());
            }
            Dataset::build(
                params,
                &matmulhash::epoch_seed(epoch),
                params.num_blocks,
                threads,
            )
        }),
    )
    .unwrap()
}

#[test]
fn a_build_that_fails_is_not_cached_and_does_not_leave_the_next_caller_waiting() {
    let calls = Arc::new(AtomicU64::new(0));
    let p = Arc::new(flaky(1, Duration::ZERO, Arc::clone(&calls)));
    let (tx, rx) = channel();
    let p2 = Arc::clone(&p);
    thread::spawn(move || {
        let first = p2.dataset_for(1).is_err();
        // would wait for ever if the failed build had left its epoch marked "building"
        let second = p2.dataset_for(1).is_ok();
        let _ = tx.send((first, second));
    });
    let (first, second) = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the second call never returned");
    assert!(first, "the first build fails");
    assert!(second, "and the next call tries again and succeeds");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(p.builds(), 1, "a failed build is not a build");
    assert!(p.has_dataset(1));
}

#[test]
fn callers_waiting_on_a_build_that_fails_are_woken_and_one_of_them_builds_it_again() {
    let calls = Arc::new(AtomicU64::new(0));
    let p = Arc::new(flaky(1, Duration::from_millis(200), Arc::clone(&calls)));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let p = Arc::clone(&p);
            thread::spawn(move || p.dataset_for(1))
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(
        results.iter().filter(|r| r.is_err()).count(),
        1,
        "only the caller whose build failed sees the error"
    );
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 3);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "one failed build and one that worked, not one per caller"
    );
    assert_eq!(p.builds(), 1);
}

#[test]
fn a_proof_of_work_without_datasets_ignores_a_prefetch() {
    let p = tenero_chain::Sha256Pow;
    p.prefetch(1000); // the default: nothing happens, nothing blocks
}
