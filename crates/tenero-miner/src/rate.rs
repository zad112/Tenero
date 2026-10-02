//! The rate meter (M10.2): attempts a second over 10 seconds, 60 seconds, 15 minutes and the whole run.
//!
//! What it measures is the speed of the search itself, so time when nothing was being searched (the miner paused while the node
//! syncs, a dataset being built, no job yet) is left out of the time, and so are the attempts counted in it. It is pure (it is
//! given the time), so it is tested without a clock.
//!
//! A window is shown only when the samples cover all of it: a "15 minute" figure in the first two minutes would be a two-minute
//! figure under a wrong name, so it is `None` until the window is full. (The average is the whole run, so it has no such rule.)

use std::collections::VecDeque;

/// The windows shown, in milliseconds.
pub const WINDOW_10S: u64 = 10_000;
pub const WINDOW_60S: u64 = 60_000;
pub const WINDOW_15M: u64 = 15 * 60_000;

/// Least searching time (ms) a figure must rest on, so that a rate is never a single batch divided by a tiny time.
const MIN_ACTIVE_MS: u64 = 1_000;

#[derive(Clone, Copy, Debug)]
struct Sample {
    /// Wall-clock time of the sample.
    at_ms: u64,
    /// Searching time so far (ms).
    active_ms: u64,
    /// Attempts made while searching, so far.
    attempts: u64,
}

/// The rates, as shown.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rates {
    pub s10: Option<f64>,
    pub s60: Option<f64>,
    pub m15: Option<f64>,
    /// The whole run.
    pub average: Option<f64>,
    /// Is the miner searching right now?
    pub searching: bool,
}

#[derive(Default)]
pub struct RateMeter {
    samples: VecDeque<Sample>,
    /// The attempt counter at the last sample (the counter is the backend's and counts everything).
    last_counter: u64,
    last_searching: bool,
    searching: bool,
    active_ms: u64,
    attempts: u64,
    started: bool,
}

impl RateMeter {
    pub fn new() -> RateMeter {
        RateMeter::default()
    }

    /// One look: the time, the backend's attempt counter, and whether it is searching now. An interval counts as searching time
    /// only if the miner was searching at both of its ends (a batch that straddles a pause is not guessed at).
    pub fn record(&mut self, now_ms: u64, counter: u64, searching: bool) {
        if !self.started {
            self.started = true;
            self.last_counter = counter;
            self.last_searching = searching;
            self.searching = searching;
            self.samples.push_back(Sample {
                at_ms: now_ms,
                active_ms: 0,
                attempts: 0,
            });
            return;
        }
        let prev = *self.samples.back().expect("a started meter has a sample");
        // A clock set back: that interval is neither time nor attempts, and the windows start again from here (the average, which is
        // in searching time, goes on). Counting the attempts without the time would inflate every rate.
        if now_ms < prev.at_ms {
            self.last_counter = counter;
            self.last_searching = searching;
            self.searching = searching;
            self.samples.clear();
            self.samples.push_back(Sample {
                at_ms: now_ms,
                active_ms: self.active_ms,
                attempts: self.attempts,
            });
            return;
        }
        let dt = now_ms - prev.at_ms;
        if searching && self.last_searching {
            self.active_ms += dt;
            self.attempts += counter.saturating_sub(self.last_counter);
        }
        self.last_counter = counter;
        self.last_searching = searching;
        self.samples.push_back(Sample {
            at_ms: now_ms,
            active_ms: self.active_ms,
            attempts: self.attempts,
        });
        // older than the longest window (plus one sample before it, which the window starts from)
        let horizon = now_ms.saturating_sub(WINDOW_15M);
        while self.samples.len() > 2 && self.samples[1].at_ms <= horizon {
            self.samples.pop_front();
        }
        self.searching = searching;
    }
}

impl RateMeter {
    /// The rate over the last `window_ms`, if the samples cover all of it and enough of it was spent searching.
    pub fn rate(&self, window_ms: u64) -> Option<f64> {
        let last = self.samples.back()?;
        let want = last.at_ms.checked_sub(window_ms)?;
        // the newest sample that is at or before the start of the window
        let first = self.samples.iter().rev().find(|s| s.at_ms <= want)?;
        let active = last.active_ms - first.active_ms;
        if active < MIN_ACTIVE_MS.min(window_ms) {
            return None;
        }
        Some((last.attempts - first.attempts) as f64 * 1000.0 / active as f64)
    }

    /// How many samples are kept (they are dropped once older than the longest window).
    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }

    /// The rate over the whole run (searching time only).
    pub fn average(&self) -> Option<f64> {
        (self.active_ms >= MIN_ACTIVE_MS)
            .then(|| self.attempts as f64 * 1000.0 / self.active_ms as f64)
    }

    pub fn rates(&self) -> Rates {
        Rates {
            s10: self.rate(WINDOW_10S),
            s60: self.rate(WINDOW_60S),
            m15: self.rate(WINDOW_15M),
            average: self.average(),
            searching: self.searching,
        }
    }
}
