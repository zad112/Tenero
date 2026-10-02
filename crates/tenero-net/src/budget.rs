//! Bounds on what a peer can make a node hold in memory and how fast it can feed it (M9, threat model B1 and B2).
//!
//! **The problem.** The wire's per-kind caps keep every frame under 73 KB except `blocks` and `txs`, which may be 16 MiB. The
//! transport used to hand decoded messages to the node's loop through a channel bounded in *messages* (1,024), so a flood of
//! large frames could in principle queue gigabytes while the loop was busy (a block's CPU check takes about 0.2 s; the first
//! block of an epoch about 3 s).
//!
//! **What this module does.**
//! * A [`Gate`] keeps two pools of bytes, one for *small* frames (up to [`SMALL_FRAME`]) and one for *large* ones. A reader
//!   reserves the whole declared size of a frame **before** it accepts the rest of it, and the reservation is carried by a
//!   [`Ticket`] attached to the decoded message and given back when the loop has dealt with it. If there is no room the reader
//!   waits, so the socket is not read, TCP's window closes, and the sender is slowed by the network, not by our memory.
//! * Each connection has a [`Lane`] with its own ceilings (one large frame at a time; a few MiB of small ones), so one peer
//!   cannot take the whole pool.
//! * The pools are separate so that peers holding large frames open (a declared 16 MiB frame that never arrives) cannot stop
//!   the small frames that keep a node alive: pings, announcements, headers, addresses.
//! * A [`Throttle`] (a token bucket in bytes) limits how fast one connection is read, again by waiting rather than by punishing:
//!   honest peers serving a sync legitimately send a lot.
//!
//! **Limits, stated plainly.** The reservation counts frame bytes; the decoded message is about the same size, and for the
//! moment of decoding both exist, so real memory can be up to about twice the pools (plus the 64 KiB chunk being read). A peer
//! that declares a large frame and then stalls holds a large reservation until the engine drops it for not answering a ping
//! (about 90 s with the default settings); a few such connections can delay other peers' large frames (blocks, transactions),
//! but not small frames. Nothing here limits how many connections one host opens (the engine's per-host rules do).

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::wire::MAX_FRAME;

/// Frames up to this size (four length bytes included) use the small pool.
pub const SMALL_FRAME: usize = 128 * 1024;
/// The largest frame there can be, length bytes included.
pub const LARGEST_FRAME: usize = MAX_FRAME + 4;

#[derive(Clone, Debug)]
pub struct GateConfig {
    /// Bytes of small frames held at once, in all, and by one connection.
    pub small_limit: usize,
    pub small_per_lane: usize,
    /// Bytes of large frames held at once, in all, and by one connection.
    pub large_limit: usize,
    pub large_per_lane: usize,
}

impl Default for GateConfig {
    fn default() -> GateConfig {
        GateConfig {
            small_limit: 16 * 1024 * 1024,
            small_per_lane: 2 * 1024 * 1024,
            // four frames of the largest size
            large_limit: 4 * LARGEST_FRAME,
            // one at a time
            large_per_lane: LARGEST_FRAME,
        }
    }
}

impl GateConfig {
    /// The same settings with the ceilings raised to what every frame needs in order to fit (a frame that could never be
    /// reserved would stop its reader for ever).
    pub fn sane(mut self) -> GateConfig {
        self.small_per_lane = self.small_per_lane.max(SMALL_FRAME);
        self.small_limit = self.small_limit.max(self.small_per_lane);
        self.large_per_lane = self.large_per_lane.max(LARGEST_FRAME);
        self.large_limit = self.large_limit.max(self.large_per_lane);
        self
    }
}

#[derive(Default)]
struct State {
    small: usize,
    large: usize,
}

/// One connection's share, counted under the gate's lock.
#[derive(Default)]
pub struct Lane {
    small: AtomicUsize,
    large: AtomicUsize,
}

impl Lane {
    pub fn new() -> Arc<Lane> {
        Arc::new(Lane::default())
    }

    /// Bytes this connection holds now (small, large).
    pub fn held(&self) -> (usize, usize) {
        (
            self.small.load(Ordering::SeqCst),
            self.large.load(Ordering::SeqCst),
        )
    }
}

/// What the gate has seen, readable from any thread.
#[derive(Default)]
pub struct GateStats {
    pub peak_small: AtomicUsize,
    pub peak_large: AtomicUsize,
    /// Times a reader had to wait for room.
    pub waits: AtomicU64,
}

pub struct Gate {
    cfg: GateConfig,
    state: Mutex<State>,
    room: Condvar,
    pub stats: GateStats,
}

/// Memory reserved for one frame; giving it up (dropping it) makes room for the next.
pub struct Ticket {
    gate: Arc<Gate>,
    lane: Arc<Lane>,
    size: usize,
    large: bool,
}

impl Ticket {
    pub fn size(&self) -> usize {
        self.size
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut st = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        if self.large {
            st.large -= self.size;
            self.lane.large.fetch_sub(self.size, Ordering::SeqCst);
        } else {
            st.small -= self.size;
            self.lane.small.fetch_sub(self.size, Ordering::SeqCst);
        }
        drop(st);
        self.gate.room.notify_all();
    }
}

impl Gate {
    pub fn new(cfg: GateConfig) -> Arc<Gate> {
        Arc::new(Gate {
            cfg: cfg.sane(),
            state: Mutex::new(State::default()),
            room: Condvar::new(),
            stats: GateStats::default(),
        })
    }

    pub fn config(&self) -> &GateConfig {
        &self.cfg
    }

    /// Bytes held now (small, large), in all.
    pub fn held(&self) -> (usize, usize) {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (st.small, st.large)
    }

    /// Reserves `size` bytes if there is room now, in the pool and in the lane; never waits. `None` if there is no room (or
    /// `size` is over the largest frame there can be).
    pub fn try_acquire(self: &Arc<Gate>, lane: &Arc<Lane>, size: usize) -> Option<Ticket> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.reserve(&mut st, lane, size)
    }

    fn reserve(self: &Arc<Gate>, st: &mut State, lane: &Arc<Lane>, size: usize) -> Option<Ticket> {
        if size > LARGEST_FRAME {
            return None;
        }
        let large = size > SMALL_FRAME;
        let (limit, per_lane) = if large {
            (self.cfg.large_limit, self.cfg.large_per_lane)
        } else {
            (self.cfg.small_limit, self.cfg.small_per_lane)
        };
        let (used, mine) = if large {
            (st.large, lane.large.load(Ordering::SeqCst))
        } else {
            (st.small, lane.small.load(Ordering::SeqCst))
        };
        if used + size > limit || mine + size > per_lane {
            return None;
        }
        if large {
            st.large += size;
            lane.large.fetch_add(size, Ordering::SeqCst);
            self.stats.peak_large.fetch_max(st.large, Ordering::SeqCst);
        } else {
            st.small += size;
            lane.small.fetch_add(size, Ordering::SeqCst);
            self.stats.peak_small.fetch_max(st.small, Ordering::SeqCst);
        }
        Some(Ticket {
            gate: Arc::clone(self),
            lane: Arc::clone(lane),
            size,
            large,
        })
    }

    /// Reserves `size` bytes (a whole frame, length bytes included) for the connection of `lane`, waiting while there is no
    /// room in the pool or in the lane. `None` if `closed` becomes true while waiting (the connection is going away), or if
    /// `size` is over the largest frame there can be.
    pub fn acquire(
        self: &Arc<Gate>,
        lane: &Arc<Lane>,
        size: usize,
        closed: &AtomicBool,
    ) -> Option<Ticket> {
        if size > LARGEST_FRAME {
            return None;
        }
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut waited = false;
        loop {
            if closed.load(Ordering::SeqCst) {
                return None;
            }
            if let Some(t) = self.reserve(&mut st, lane, size) {
                return Some(t);
            }
            if !waited {
                waited = true;
                self.stats.waits.fetch_add(1, Ordering::Relaxed);
            }
            // wake now and then to look at `closed`: nothing tells a waiting reader that its connection was closed
            let (guard, _) = self
                .room
                .wait_timeout(st, Duration::from_millis(50))
                .unwrap_or_else(|e| e.into_inner());
            st = guard;
        }
    }
}

/// A token bucket in bytes: `rate` bytes a second, with up to `burst` saved up. [`Throttle::take`] says how long to wait
/// before reading more. A rate of zero turns it off. The clock is passed in, so it can be tested without waiting.
#[derive(Clone, Debug)]
pub struct Throttle {
    rate: u64,
    burst: u64,
    /// May go below zero (a debt that the wait pays off).
    tokens: i64,
    last: Option<Instant>,
}

impl Throttle {
    pub fn new(rate: u64, burst: u64) -> Throttle {
        Throttle {
            rate,
            burst: burst.max(1),
            tokens: i64::try_from(burst.max(1)).unwrap_or(i64::MAX),
            last: None,
        }
    }

    /// Records that `bytes` were just read at `now`; returns how long to wait before reading again.
    pub fn take(&mut self, bytes: usize, now: Instant) -> Duration {
        if self.rate == 0 {
            return Duration::ZERO;
        }
        if let Some(last) = self.last {
            let secs = now.saturating_duration_since(last).as_secs_f64();
            let gained = (secs * self.rate as f64) as i64;
            self.tokens = self
                .tokens
                .saturating_add(gained)
                .min(i64::try_from(self.burst).unwrap_or(i64::MAX));
        }
        self.last = Some(now);
        self.tokens = self
            .tokens
            .saturating_sub(i64::try_from(bytes).unwrap_or(i64::MAX));
        if self.tokens >= 0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64((-self.tokens) as f64 / self.rate as f64)
        }
    }
}
