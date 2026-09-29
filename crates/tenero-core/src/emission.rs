//! Emission and rewards (`CONSENSUS.md` section 5). All amounts are in units. Heights start at 1.
//!
//! `scheduled(h) = initial >> ((h-1)/H)`; `main_reward(h) = min(scheduled, cap - issued_before(h))`;
//! `reward(h) = max(main_reward, tail)`. `issued_before` counts the main schedule, not what was paid.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Emission {
    pub initial_reward: u64,
    pub halving_interval: u64,
    pub max_supply: u64,
    pub tail_reward: u64,
}

impl Emission {
    pub fn validate(&self) -> Result<(), String> {
        if self.halving_interval < 1 {
            return Err("halving_interval must be at least 1".into());
        }
        Ok(())
    }

    /// The main-emission reward of block `height` before the cap is applied.
    pub fn scheduled_reward(&self, height: u64) -> u64 {
        let era = height.saturating_sub(1) / self.halving_interval;
        u32::try_from(era)
            .ok()
            .and_then(|e| self.initial_reward.checked_shr(e))
            .unwrap_or(0)
    }

    /// Main-emission coins scheduled for blocks `1..height`, at most `max_supply`.
    pub fn issued_before(&self, height: u64) -> u64 {
        let interval = u128::from(self.halving_interval);
        let end = u128::from(height);
        let mut total: u128 = 0;
        let mut h: u128 = 1;
        while h < end {
            let r = u128::from(self.scheduled_reward(h as u64));
            if r == 0 {
                break;
            }
            let era_last = ((h - 1) / interval + 1) * interval;
            let last = era_last.min(end - 1);
            total += r * (last - h + 1);
            h = last + 1;
        }
        // at most max_supply, which is a u64
        total.min(u128::from(self.max_supply)) as u64
    }

    /// The main emission only: the schedule, trimmed so `max_supply` is never passed.
    pub fn main_reward_at(&self, height: u64) -> u64 {
        let remaining = self.max_supply - self.issued_before(height);
        self.scheduled_reward(height).min(remaining)
    }

    /// The base reward of block `height`, before any oversize penalty.
    pub fn reward_at(&self, height: u64) -> u64 {
        self.main_reward_at(height).max(self.tail_reward)
    }

    /// True once the main emission pays less than the tail (even if the cap is not reached yet).
    pub fn in_tail(&self, height: u64) -> bool {
        self.tail_reward > 0 && self.main_reward_at(height) < self.tail_reward
    }

    /// The first height at or after `height` at which the main emission pays nothing. Walks the eras,
    /// so it is instant even when the end is millions of blocks away. `None` if it does not fit in a `u64`.
    pub fn main_emission_end(&self, height: u64) -> Option<u64> {
        let interval = u128::from(self.halving_interval);
        let mut h = u128::from(height);
        loop {
            let r = u128::from(self.main_reward_at(u64::try_from(h).ok()?));
            if r == 0 {
                return u64::try_from(h).ok();
            }
            let era_end = ((h - 1) / interval + 1) * interval;
            let remaining =
                u128::from(self.max_supply - self.issued_before(u64::try_from(h).ok()?));
            if remaining <= r * (era_end - h + 1) {
                // the blocks needed to use up the rest of the cap
                return u64::try_from(h + remaining.div_ceil(r)).ok();
            }
            h = era_end + 1;
        }
    }
}
