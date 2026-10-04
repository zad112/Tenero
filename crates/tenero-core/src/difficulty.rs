//! Difficulty (LWMA) and the timestamp rule: later than the parent's (`CONSENSUS.md` section 7).
//!
//! `ts[i]` is the timestamp and `targets[i]` the required target of the block at position `i`;
//! position 0 is the genesis block with timestamp 0 and the starting target. Timestamps are `i64`
//! seconds, and may be out of order (a backwards step counts as a 1-second solve time).

use crate::u256::{U256, U320};

/// The target can change by at most this factor, up or down, per block.
pub const MAX_TARGET_STEP: u64 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DifficultyParams {
    /// The aimed-for seconds per block.
    pub block_time: u64,
    /// Blocks to look back; 0 turns the adjustment off (every block must meet `start_target`).
    pub window: u64,
    pub start_target: U256,
}

/// The target the block at position `pos` must meet, from the timestamps and required targets of
/// positions `0..pos`.
///
/// Widths: the averaged target and the weighted solve times are multiplied, which needs more than
/// 256 bits (a 320-bit intermediate). An error is returned if the weighted times or their divisor
/// do not fit in 64 bits (an absurd `block_time` or `window`), never a wrong answer.
pub fn retarget(
    p: &DifficultyParams,
    ts: &[i64],
    targets: &[U256],
    pos: usize,
) -> Result<U256, String> {
    if ts.len() < pos || targets.len() < pos {
        return Err("not enough history".into());
    }
    retarget_recent(p, &ts[..pos], &targets[..pos], pos)
}

/// The same as [`retarget`], from only the most recent history: `ts` and `targets` hold the timestamps and
/// required targets of the last `ts.len()` positions before `pos` (positions `pos - len .. pos`, oldest
/// first). The adjustment looks back at most `window + 1` positions, so `window + 1` entries are always
/// enough (fewer are enough near the start of the chain, where `pos` itself is smaller). This is what lets
/// a node check a block without reading the whole chain.
pub fn retarget_recent(
    p: &DifficultyParams,
    ts: &[i64],
    targets: &[U256],
    pos: usize,
) -> Result<U256, String> {
    if p.window == 0 {
        return Ok(p.start_target);
    }
    if block_time_is_bad(p) {
        return Err("block_time must be at least 1".into());
    }
    if ts.len() != targets.len() || ts.len() > pos {
        return Err(
            "the history must be one timestamp and one target per position, ending before pos"
                .into(),
        );
    }
    let offset = pos - ts.len(); // the absolute position of ts[0]
                                 // blocks 2 .. pos-1 have a measurable solve time (block 1 follows the genesis block)
    let n = match u64::try_from(pos as i64 - 2) {
        Ok(avail) => avail.min(p.window),
        Err(_) => return Ok(p.start_target),
    };
    if n < 1 {
        return Ok(p.start_target);
    }
    let n = usize::try_from(n).map_err(|_| "window too large")?;
    if pos - n - 1 < offset {
        return Err("not enough history".into());
    }
    let t = i128::from(p.block_time);
    let mut weighted: u128 = 0;
    for k in 1..=n {
        let i = pos - n - 1 + k - offset; // k = 1 is the oldest block in the window (a local index)
        let solve = (i128::from(ts[i]) - i128::from(ts[i - 1]))
            .min(6 * t)
            .max(1);
        weighted += u128::try_from(solve).expect("solve is at least 1") * k as u128;
    }
    let weighted =
        u64::try_from(weighted).map_err(|_| "the weighted solve times do not fit in 64 bits")?;
    let divisor = u128::from(p.block_time) * (n as u128) * (n as u128 + 1) / 2;
    let divisor = u64::try_from(divisor).map_err(|_| "block_time and window are too large")?;

    let mut sum = U320::ZERO;
    for target in &targets[pos - n - offset..pos - offset] {
        sum = sum
            .checked_add(&U320::from_u256(target))
            .ok_or("the target sum overflows")?;
    }
    let avg = sum.div_u64(n as u64);
    let new = avg
        .checked_mul_u64(weighted)
        .ok_or("the new target overflows")?
        .div_u64(divisor);

    let prev = U320::from_u256(&targets[pos - 1 - offset]);
    let upper = prev
        .checked_mul_u64(MAX_TARGET_STEP)
        .expect("4 * a 256-bit value fits in 320 bits");
    let lower = prev.div_u64(MAX_TARGET_STEP);
    let clamped = new.min(upper).max(lower);
    // at most 2^256 - 1, at least 1
    Ok(clamped.to_u256().unwrap_or(U256::MAX).max(U256::ONE))
}

fn block_time_is_bad(p: &DifficultyParams) -> bool {
    p.block_time == 0
}

/// The required targets of blocks 1, 2, ... given every block's timestamp: `timestamps[i]` is the
/// timestamp of block `i + 1`. Returns one more entry than `timestamps`, the last being the target
/// of the next block.
pub fn required_targets(p: &DifficultyParams, timestamps: &[i64]) -> Result<Vec<U256>, String> {
    let mut ts = vec![0i64];
    let mut targets = vec![p.start_target];
    for pos in 1..=timestamps.len() + 1 {
        let next = retarget(p, &ts, &targets, pos)?;
        targets.push(next);
        if let Some(&t) = timestamps.get(pos - 1) {
            ts.push(t);
        }
    }
    Ok(targets.split_off(1))
}

/// The earliest timestamp a block may carry when its parent's is `parent`: one second later. **A block's timestamp must be later than its
/// parent's** (`CONSENSUS.md` section 7; M11.2). It replaced "not below the median of the last 11", under which a miner with 30 % of the hash rate
/// could backdate its blocks and pull the difficulty to 0.40x (`THREAT_MODEL.md` E3).
pub fn earliest_time_after(parent: i64) -> i64 {
    parent.saturating_add(1)
}

/// The earliest timestamp the block at position `pos` may carry, from `ts` (position 0, the genesis block, has timestamp 0): one second after
/// position `pos - 1`'s. 0 for position 0, which is no block's child.
pub fn earliest_time(ts: &[i64], pos: usize) -> i64 {
    match pos.checked_sub(1).and_then(|i| ts.get(i)) {
        Some(&parent) => earliest_time_after(parent),
        None => 0,
    }
}

/// `earliest_time` for blocks 1, 2, ... (one more entry than `timestamps`, as `required_targets`).
pub fn earliest_times(timestamps: &[i64]) -> Vec<i64> {
    let mut ts = vec![0i64];
    ts.extend_from_slice(timestamps);
    (1..=timestamps.len() + 1)
        .map(|pos| earliest_time(&ts, pos))
        .collect()
}
