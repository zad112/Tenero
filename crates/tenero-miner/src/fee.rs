//! The miner's fee: **1% of its running time**, mined for the author of the backend in use (the owner's decision, 2026-10-10,
//! agreed with the AMD backend's contributor and announced to the community). For one minute in every hundred the miner works
//! for the fee's address instead of the user's: on its own node, the block template pays the fee's address; for a pool, it logs
//! in to the pool with the fee's address. Nothing else changes, and nothing is hidden: the miner says when the fee's minute
//! begins and ends, and a block found in it is shown as the fee's, not the user's.
//!
//! Who is paid, per network and backend, is a list in the program (`tenero_app::mining_fee`); a backend with no address there
//! pays no fee (on `gamma` the NVIDIA and AMD backends have one; at main-net its own addresses replace them). The CPU and SHA-256
//! test backends never carry a fee. It is a time share, as in other open miners, so it is fair on average and not block by block: a small miner may
//! find no block in its fee minutes, or find one there.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tenero_core::hash::sha256;

/// The share of the running time mined for the fee, in percent.
pub const FEE_PERCENT: u32 = 1;
/// The fee's period: one slice of [`FEE_SLICE`] in every cycle.
pub const FEE_CYCLE: Duration = Duration::from_secs(100 * 60);
/// The time mined for the fee in each cycle: [`FEE_PERCENT`] of [`FEE_CYCLE`].
pub const FEE_SLICE: Duration = Duration::from_secs(FEE_CYCLE.as_secs() * FEE_PERCENT as u64 / 100);

/// When the fee's slices fall, counted from the miner's start: one of `slice` in every `cycle`, the first `first` after the
/// start. A random `first` (as [`FeeSchedule::standard`] picks) makes a miner that is started and stopped often pay the same
/// share on average as one that runs for days.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeeSchedule {
    pub cycle: Duration,
    pub slice: Duration,
    pub first: Duration,
}

impl FeeSchedule {
    /// [`FEE_SLICE`] in every [`FEE_CYCLE`], the first slice starting at a random point of the first cycle.
    pub fn standard() -> FeeSchedule {
        let span = (FEE_CYCLE - FEE_SLICE).as_secs();
        // not a secret, only a spread: the clock's nanoseconds and the process, hashed
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let h = sha256(&[
            b"tenero miner fee start",
            &nanos.to_le_bytes(),
            &std::process::id().to_le_bytes(),
        ]);
        let r = u64::from_le_bytes(h[..8].try_into().expect("eight bytes"));
        FeeSchedule {
            cycle: FEE_CYCLE,
            slice: FEE_SLICE,
            first: Duration::from_secs(r % (span + 1)),
        }
    }

    /// Whether `elapsed` (time since the miner started) falls in one of the fee's slices.
    pub fn is_fee_time(&self, elapsed: Duration) -> bool {
        if self.cycle.is_zero() || elapsed < self.first {
            return false;
        }
        let into = (elapsed - self.first).as_millis() % self.cycle.as_millis();
        into < self.slice.as_millis()
    }
}

/// A fee the miner pays: where to, to whom, and when.
#[derive(Clone)]
pub struct MiningFee {
    /// A main address (a block reward cannot be paid to a subaddress).
    pub address: tenero_wallet::Address,
    /// Who it pays, for the screen: "the NVIDIA backend's author".
    pub to: String,
    pub schedule: FeeSchedule,
    /// The time since the miner started (a clock from the start by default; a test gives another).
    pub elapsed: Arc<dyn Fn() -> Duration + Send + Sync>,
}

impl MiningFee {
    /// `None` if `address` cannot be paid a block reward (not a main address).
    pub fn new(
        address: tenero_wallet::Address,
        to: &str,
        schedule: FeeSchedule,
    ) -> Option<MiningFee> {
        crate::WalletPayout::new(address)?;
        let start = Instant::now();
        Some(MiningFee {
            address,
            to: to.to_string(),
            schedule,
            elapsed: Arc::new(move || start.elapsed()),
        })
    }

    /// Whether the miner should be working for the fee now.
    pub fn due(&self) -> bool {
        self.schedule.is_fee_time((self.elapsed)())
    }

    /// The fee's address as text.
    pub fn address_text(&self) -> String {
        self.address.to_text()
    }

    /// One line that says what the fee is, for the screen and the log.
    pub fn describe(&self) -> String {
        format!(
            "{FEE_PERCENT}% of the mining time ({} s in every {} min) goes to {} ({})",
            self.schedule.slice.as_secs(),
            self.schedule.cycle.as_secs() / 60,
            self.to,
            self.address_text()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &FeeSchedule, secs: u64) -> bool {
        s.is_fee_time(Duration::from_secs(secs))
    }

    #[test]
    fn the_standard_fee_is_one_minute_in_a_hundred() {
        assert_eq!(FEE_PERCENT, 1);
        assert_eq!(FEE_SLICE, Duration::from_secs(60));
        assert_eq!(FEE_CYCLE, Duration::from_secs(6000));
        for _ in 0..50 {
            let s = FeeSchedule::standard();
            assert!(s.first <= FEE_CYCLE - FEE_SLICE, "{s:?}");
        }
    }

    #[test]
    fn the_slices_fall_where_the_schedule_says() {
        let s = FeeSchedule {
            cycle: Duration::from_secs(6000),
            slice: Duration::from_secs(60),
            first: Duration::from_secs(1000),
        };
        assert!(!at(&s, 0));
        assert!(!at(&s, 999));
        assert!(at(&s, 1000));
        assert!(at(&s, 1059));
        assert!(!at(&s, 1060));
        assert!(!at(&s, 6999));
        assert!(at(&s, 7000));
        assert!(at(&s, 7059));
        assert!(!at(&s, 7060));
    }

    #[test]
    fn over_a_long_run_the_share_is_one_percent() {
        let s = FeeSchedule::standard();
        let total = 6000 * 50;
        let fee = (0..total).filter(|&t| at(&s, t)).count() as u64;
        // 50 whole cycles after the first slice: 50 or 49 slices of 60 s, depending on where the first one fell
        assert!((49 * 60..=50 * 60).contains(&fee), "{fee} s of {total}");
    }

    #[test]
    fn a_zero_cycle_never_charges() {
        let s = FeeSchedule {
            cycle: Duration::ZERO,
            slice: Duration::from_secs(60),
            first: Duration::ZERO,
        };
        assert!(!at(&s, 0));
        assert!(!at(&s, 100));
    }
}
