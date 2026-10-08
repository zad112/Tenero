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

    /// The base rewards of blocks `1..=height` added up: the coins the schedule has created by that height (main emission
    /// and tail). It is the schedule, not what coinbases paid: an oversize penalty, which creates fewer coins, is not
    /// subtracted. For a block explorer; no rule uses it. `None` if the total does not fit in a `u64`.
    ///
    /// The main rewards add up to `issued_before(height + 1)` (each block takes what the cap leaves), and the main reward
    /// never grows, so the tail pays from one height on and the sum splits in two there.
    pub fn paid_through(&self, height: u64) -> Option<u64> {
        let main = self.issued_before(height.checked_add(1)?);
        if height == 0 || !self.in_tail(height) {
            return Some(main);
        }
        // the first height in the tail: `in_tail` is false below it and true from it on
        let (mut lo, mut hi) = (1u64, height);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.in_tail(mid) {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        let tail_blocks = height - lo + 1;
        self.issued_before(lo)
            .checked_add(self.tail_reward.checked_mul(tail_blocks)?)
    }
}

#[cfg(test)]
mod tests {
    use super::Emission;

    fn by_hand(e: &Emission, height: u64) -> u64 {
        (1..=height).map(|h| e.reward_at(h)).sum()
    }

    #[test]
    fn paid_through_is_the_rewards_added_up() {
        // small numbers, so the cap, the halvings and the tail all happen within a few hundred blocks
        let shapes = [
            Emission {
                initial_reward: 100,
                halving_interval: 10,
                max_supply: 1_000,
                tail_reward: 3,
            },
            // the cap bites in the middle of an era
            Emission {
                initial_reward: 100,
                halving_interval: 10,
                max_supply: 1_234,
                tail_reward: 7,
            },
            // no tail
            Emission {
                initial_reward: 64,
                halving_interval: 5,
                max_supply: 10_000,
                tail_reward: 0,
            },
            // the tail is above the first reward: every block is in the tail
            Emission {
                initial_reward: 5,
                halving_interval: 3,
                max_supply: 1_000,
                tail_reward: 9,
            },
        ];
        for e in shapes {
            for h in 0..400 {
                assert_eq!(e.paid_through(h), Some(by_hand(&e, h)), "{e:?} at {h}");
            }
        }
    }

    #[test]
    fn paid_through_on_the_real_schedule() {
        let e = Emission {
            initial_reward: 2_000_000_000,
            halving_interval: 525_600,
            max_supply: 2_000_000_000_000_000,
            tail_reward: 50_000_000,
        };
        assert_eq!(e.paid_through(0), Some(0));
        assert_eq!(e.paid_through(1), Some(2_000_000_000));
        assert_eq!(e.paid_through(10_000), Some(by_hand(&e, 10_000)));
        // across the first halving
        let h = 525_610;
        assert_eq!(e.paid_through(h), Some(by_hand(&e, h)));
        // long after the main emission ends, the tail goes on: the cap plus the tail of every block since
        let end = e.main_emission_end(1).unwrap();
        let far = end + 1_000_000;
        let paid = e.paid_through(far).unwrap();
        assert!(paid > e.max_supply);
        assert_eq!(
            paid - e.paid_through(far - 1).unwrap(),
            e.tail_reward,
            "one tail block more"
        );
        // and no overflow at the top of the range: it says None rather than a wrong number
        assert_eq!(e.paid_through(u64::MAX), None);
    }
}
