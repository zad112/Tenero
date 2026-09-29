//! Minimum fee, block-size median and oversize penalty (`CONSENSUS.md` section 6). Amounts are units
//! and sizes are bytes, both `u64`; the penalty's intermediate `base * over^2` is `u128`.

/// Minimum fee rate: 0.01 coins per 1000 bytes = 100 units per 1000 bytes.
pub const MIN_FEE_RATE_UNITS_PER_1000_BYTES: u64 = 100;
/// The median never goes below this, in bytes (version 1).
pub const MIN_BLOCK_MEDIAN: u64 = 300_000;
/// The same floor in version 2: 150,000 bytes. It is the size up to which a block carries no penalty,
/// so it bounds the free growth of the chain (about 79 GB a year at 60-second blocks, at the very most).
pub const V2_MIN_BLOCK_MEDIAN: u64 = 150_000;
/// The median is taken over this many recent blocks.
pub const MEDIAN_WINDOW: usize = 10;

/// `max(1, ceil(rate * size / 1000))` units. `None` if it does not fit in a `u64`.
pub fn min_fee(size: u64, rate_units_per_1000_bytes: u64) -> Option<u64> {
    let f = (u128::from(rate_units_per_1000_bytes) * u128::from(size)).div_ceil(1000);
    u64::try_from(f.max(1)).ok()
}

/// Version 2: the reference transaction size of the dynamic minimum fee, in bytes.
pub const FEE_REFERENCE_WEIGHT: u64 = 3000;

/// Version 2's dynamic minimum fee, in units: `max(1, ceil(base_reward * FEE_REFERENCE_WEIGHT * size /
/// median^2))`. `base_reward` is the block's reward before any penalty and `median` the block-size
/// median the block is judged against; both are known before the block, so this is a rule every node
/// computes the same way. An error when `median` is 0 or the result does not fit in a `u64`.
pub fn dynamic_min_fee(size: u64, base_reward: u64, median: u64) -> Result<u64, String> {
    if median == 0 {
        return Err("the median cannot be 0".into());
    }
    let numerator = u128::from(base_reward)
        .checked_mul(u128::from(FEE_REFERENCE_WEIGHT))
        .and_then(|x| x.checked_mul(u128::from(size)))
        .ok_or("the minimum fee does not fit")?;
    let m = u128::from(median);
    let fee = numerator.div_ceil(m * m).max(1);
    u64::try_from(fee).map_err(|_| "the minimum fee does not fit in 64 bits".to_string())
}

/// The upper median of `sizes` (the element at index `len / 2` once sorted), at least `floor`.
/// With no sizes it is `floor`.
pub fn median(sizes: &[u64], floor: u64) -> u64 {
    let mut s = sizes.to_vec();
    s.sort_unstable();
    floor.max(s.get(s.len() / 2).copied().unwrap_or(0))
}

/// The median the block at position `pos` is judged against. `sizes[i]` is the size of the block
/// at position `i` (position 0 is the genesis block and is never counted). The window is the
/// `MEDIAN_WINDOW` positions before `pos`, from position 1 on.
pub fn median_at(sizes: &[u64], pos: usize, floor: u64) -> u64 {
    let lo = pos.saturating_sub(MEDIAN_WINDOW).max(1);
    let hi = pos.min(sizes.len());
    median(if lo < hi { &sizes[lo..hi] } else { &[] }, floor)
}

/// A block larger than twice the median is invalid.
pub fn over_hard_limit(size: u64, median: u64) -> bool {
    u128::from(size) > 2 * u128::from(median)
}

/// The reward lost by a block over the median: `ceil(base * over^2 / median^2)` where
/// `over = size - median`, and 0 when `size <= median`. It equals `base` at `size = 2 * median`.
/// An error when `median` is 0 or the result does not fit in a `u64`.
pub fn penalty(base: u64, size: u64, median: u64) -> Result<u64, String> {
    if median == 0 {
        return Err("the median cannot be 0".into());
    }
    if size <= median || base == 0 {
        return Ok(0);
    }
    let over = u128::from(size - median);
    let m = u128::from(median);
    let numerator = u128::from(base)
        .checked_mul(over)
        .and_then(|x| x.checked_mul(over))
        .ok_or("the penalty overflows")?;
    u64::try_from(numerator.div_ceil(m * m))
        .map_err(|_| "the penalty does not fit in 64 bits".to_string())
}
