//! Difficulty and timestamps on a small network (E3 and E4 of `docs/THREAT_MODEL.md`): the REAL `retarget_recent` and the REAL timestamp
//! rule (since M11.2: a block's timestamp must be later than its parent's, `difficulty::earliest_time_after`, the wall clock plus 120 seconds
//! from above), driven by simulated miners that find blocks at random, join, leave, and (in the attack runs) choose their timestamps. The
//! rule BEFORE M11.2 (not below the median of the last 11 blocks) is kept here as a stand-in, [`Rule::OldMedian`], only so that the finding
//! that led to the change stays reproducible: it is no longer in the crate. `#[ignore]`d tests print tables (`--ignored --nocapture`), and the
//! plain ones keep the findings as regression tests.
//!
//! **What this is not:** a model of a real network. Block finding is an exponential wait at the current work and hash rate (no network
//! delay, no orphans, no miners that react to the difficulty by leaving or joining on their own), the miners are fixed shares of a
//! hash rate, and the clock is perfect for everyone. It shows what the ALGORITHM does with these inputs, which is a floor on the trouble
//! a real network will have, not an estimate of it.

use tenero_core::difficulty::{earliest_time_after, retarget_recent, DifficultyParams};
use tenero_core::u256::U256;

/// The chain's parameters (`CONSENSUS.md` section 3): one block a minute, a window of 30.
const T: u64 = 60;
const W: u64 = 30;
/// A block's timestamp may be at most this far ahead of the clock of the node that checks it.
const FUTURE_LIMIT: i64 = 120;
/// The window of the rule before M11.2 (used only by [`Rule::OldMedian`] and the candidates built on it).
const OLD_MEDIAN_WINDOW: usize = 11;

// ---- a small deterministic random source (no dependency) -----------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in (0, 1).
    fn unit(&mut self) -> f64 {
        ((self.next() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }
    /// An exponential wait with this mean (what a block search is, however long it has already gone on).
    fn exp(&mut self, mean: f64) -> f64 {
        -mean * self.unit().ln()
    }
}

/// `2^256 / target`: the attempts a block is expected to take.
fn work(target: &U256) -> f64 {
    match U256::work_of_target(target) {
        Some(w) => w
            .to_be_bytes()
            .iter()
            .fold(0.0, |v, b| v * 256.0 + f64::from(*b)),
        None => f64::INFINITY,
    }
}

fn target_for_work(w: f64) -> U256 {
    U256::MAX.div_u64(w.max(2.0) as u64)
}

// ---- the chain being built -------------------------------------------------------------------------------------------------------------

/// Which rule picks the next target: the chain's own (`retarget_recent`), or a CANDIDATE that is only here, to measure what changing it would do.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Rule {
    /// The rules of the chain as they are since M11.2 (`CONSENSUS.md` section 7), using the crate's own functions: a block must be later than
    /// its parent, a solve time is at least 1 second, nobody can backdate a block, and what a timestamp can do is bounded by the future limit.
    Current,
    /// The timestamp rule BEFORE M11.2, with the same difficulty rule: a block's timestamp may not be below the median of the last 11 (a
    /// stand-in kept here to reproduce the finding that a 30 % miner could pull the difficulty to 0.40x; the code itself is gone).
    OldMedian,
    /// The same, except that a solve time may be negative, down to minus the future limit (a backdated block then takes away from the
    /// next block's gap what it added to its own, so the sum of the window follows the clock), and the weighted sum is kept above a twentieth
    /// of what the aimed-for rate would give.
    NegativeSolveTimes,
    /// As `NegativeSolveTimes`, but the solve time is clamped to plus or minus six block times: wide enough that a block backdated to the
    /// median (about five blocks back) is not cut short, so that what a backdated block takes off its own gap is put back on the next one.
    SymmetricClamp,
}

struct Chain {
    rule: Rule,
    ts: Vec<i64>,
    targets: Vec<U256>,
    /// The real time each block was found at, which the timestamp may differ from.
    real: Vec<f64>,
    params: DifficultyParams,
}

impl Chain {
    fn new(start_work: f64) -> Chain {
        Chain::with_rule(start_work, Rule::Current)
    }

    fn with_rule(start_work: f64, rule: Rule) -> Chain {
        let start = target_for_work(start_work);
        Chain {
            rule,
            ts: vec![0],
            targets: vec![start],
            real: vec![REAL_START],
            params: DifficultyParams {
                block_time: T,
                window: W,
                start_target: start,
            },
        }
    }

    fn height(&self) -> usize {
        self.ts.len() - 1
    }

    /// The target the next block must meet.
    fn next_target(&self) -> U256 {
        let pos = self.ts.len();
        let from = pos.saturating_sub(W as usize + 1);
        match self.rule {
            Rule::Current | Rule::OldMedian => {
                retarget_recent(&self.params, &self.ts[from..], &self.targets[from..], pos)
                    .expect("the history is long enough")
            }
            Rule::NegativeSolveTimes => retarget_negative(
                &self.params,
                &self.ts[from..],
                &self.targets[from..],
                pos,
                -i128::from(FUTURE_LIMIT),
            ),
            Rule::SymmetricClamp => retarget_negative(
                &self.params,
                &self.ts[from..],
                &self.targets[from..],
                pos,
                -6 * i128::from(T),
            ),
        }
    }

    /// The least timestamp the next block may carry. For the chain's own rule that is the crate's `earliest_time_after` (one second after the
    /// parent's); the candidates built on the old rule use the upper median of the last 11.
    fn earliest_time(&self) -> i64 {
        if self.rule == Rule::Current {
            return earliest_time_after(*self.ts.last().expect("the genesis block"));
        }
        let pos = self.ts.len();
        let lo = pos.saturating_sub(OLD_MEDIAN_WINDOW).max(1);
        let mut w: Vec<i64> = self.ts[lo..pos].to_vec();
        if w.is_empty() {
            return 0;
        }
        w.sort_unstable();
        w[w.len() / 2]
    }

    fn push(&mut self, ts: i64, real: f64) {
        let target = self.next_target();
        self.ts.push(ts);
        self.targets.push(target);
        self.real.push(real);
    }
}

/// The real time of the genesis block (its timestamp is 0, which is not a real time).
const REAL_START: f64 = 1_700_000_000.0;

/// `retarget_recent` with the solve time allowed to be negative (see [`Rule::NegativeSolveTimes`]). A copy of that function's arithmetic with
/// the one change, so that the measurements compare like with like.
fn retarget_negative(
    p: &DifficultyParams,
    ts: &[i64],
    targets: &[U256],
    pos: usize,
    lowest: i128,
) -> U256 {
    use tenero_core::difficulty::MAX_TARGET_STEP;
    use tenero_core::u256::U320;
    let offset = pos - ts.len();
    let n = match u64::try_from(pos as i64 - 2) {
        Ok(avail) => avail.min(p.window),
        Err(_) => return p.start_target,
    };
    if n < 1 {
        return p.start_target;
    }
    let n = n as usize;
    let t = i128::from(p.block_time);
    let mut weighted: i128 = 0;
    for k in 1..=n {
        let i = pos - n - 1 + k - offset;
        let solve = (i128::from(ts[i]) - i128::from(ts[i - 1])).clamp(lowest, 6 * t);
        weighted += solve * k as i128;
    }
    let divisor = (t as u128) * (n as u128) * (n as u128 + 1) / 2;
    // never less than a twentieth of what the aimed-for rate would give: the target cannot be driven to nothing by timestamps alone
    let weighted = (weighted.max((divisor / 20) as i128)) as u64;
    let divisor = divisor as u64;
    let mut sum = U320::ZERO;
    for target in &targets[pos - n - offset..pos - offset] {
        sum = sum.checked_add(&U320::from_u256(target)).unwrap();
    }
    let avg = sum.div_u64(n as u64);
    let new = avg.checked_mul_u64(weighted).unwrap().div_u64(divisor);
    let prev = U320::from_u256(&targets[pos - 1 - offset]);
    let upper = prev.checked_mul_u64(MAX_TARGET_STEP).unwrap();
    let lower = prev.div_u64(MAX_TARGET_STEP);
    new.min(upper)
        .max(lower)
        .to_u256()
        .unwrap_or(U256::MAX)
        .max(U256::ONE)
}

/// What a miner puts in the timestamp of a block it finds at real time `now`.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Stamp {
    /// The clock.
    Honest,
    /// As late as the rules allow: the clock plus 120 seconds.
    Forward,
    /// As early as the rules allow: the median time.
    Backward,
    /// Forward on one block and backward on the next that it finds.
    Alternate,
}

/// A group of miners: a share of the hash rate and what they do with timestamps.
#[derive(Clone, Copy)]
struct Group {
    share: f64,
    stamp: Stamp,
}

/// The hash rate (attempts a second) as a function of the real time, and the groups it is divided between.
struct Run<'a> {
    hash_rate: &'a dyn Fn(f64) -> f64,
    groups: &'a [Group],
}

/// Runs the chain to `blocks` blocks (or `max_secs` of real time). Returns the chain.
fn simulate(start_work: f64, run: &Run, blocks: usize, max_secs: f64, seed: u64) -> Chain {
    simulate_with(Rule::Current, start_work, run, blocks, max_secs, seed)
}

fn simulate_with(
    rule: Rule,
    start_work: f64,
    run: &Run,
    blocks: usize,
    max_secs: f64,
    seed: u64,
) -> Chain {
    let mut rng = Rng(seed);
    let mut chain = Chain::with_rule(start_work, rule);
    let t0 = REAL_START; // real time is kept apart from the genesis block's timestamp, which is 0
    let mut now = t0;
    let mut flip = vec![false; run.groups.len()];
    while chain.height() < blocks && now - t0 < max_secs {
        let target = chain.next_target();
        let w = work(&target);
        // each group finds blocks at its own rate; the first one to find wins (the others' work is lost, as in a race)
        let rate = (run.hash_rate)(now - t0);
        let mut best: Option<(f64, usize)> = None;
        for (g, grp) in run.groups.iter().enumerate() {
            let mean = w / (rate * grp.share).max(1e-9);
            let wait = rng.exp(mean);
            if best.is_none_or(|(b, _)| wait < b) {
                best = Some((wait, g));
            }
        }
        let (wait, g) = best.unwrap();
        now += wait;
        let want = match run.groups[g].stamp {
            Stamp::Honest => now as i64,
            Stamp::Forward => now as i64 + FUTURE_LIMIT,
            Stamp::Backward => chain.earliest_time(),
            Stamp::Alternate => {
                flip[g] = !flip[g];
                if flip[g] {
                    now as i64 + FUTURE_LIMIT
                } else {
                    chain.earliest_time()
                }
            }
        };
        // the rules: not below the floor (later than the parent), not more than 120 s past the clock of the node that checks it (a floor that is
        // itself past that limit means the block would be held a moment: it is taken at the floor, which is at most a second beyond)
        let ts = want
            .min(now as i64 + FUTURE_LIMIT)
            .max(chain.earliest_time());
        chain.push(ts, now);
    }
    chain
}

/// Mean, median and the 95th percentile and maximum of the real gaps between blocks, over `from..` (a block number).
fn gaps(chain: &Chain, from: usize) -> (f64, f64, f64, f64) {
    let mut g: Vec<f64> = (from.max(1)..=chain.height())
        .map(|i| chain.real[i] - chain.real[i - 1])
        .collect();
    if g.is_empty() {
        return (f64::NAN, f64::NAN, f64::NAN, f64::NAN);
    }
    g.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = g.iter().sum::<f64>() / g.len() as f64;
    (
        mean,
        g[g.len() / 2],
        g[g.len() * 95 / 100],
        *g.last().unwrap(),
    )
}

const STEADY: f64 = 35_000.0; // attempts a second: about what the owner's one card does (BENCHMARKS.md)

// ---- the plain tests: findings kept ------------------------------------------------------------------------------------------------

#[test]
fn at_a_steady_hash_rate_blocks_come_about_once_a_minute() {
    let flat = |_t: f64| STEADY;
    let groups = [Group {
        share: 1.0,
        stamp: Stamp::Honest,
    }];
    let mut means = vec![];
    for seed in 1..=8 {
        let chain = simulate(
            STEADY * 60.0,
            &Run {
                hash_rate: &flat,
                groups: &groups,
            },
            3000,
            f64::MAX,
            seed,
        );
        let (mean, _, _, _) = gaps(&chain, 200);
        means.push(mean);
    }
    for m in &means {
        assert!(
            (45.0..=75.0).contains(m),
            "mean gap {m} s (of the runs: {means:?})"
        );
    }
}

#[test]
fn the_simulation_reproduces_the_rules_it_drives() {
    // the starting target, for the first two blocks (CONSENSUS.md section 7)
    let c = Chain::new(1000.0);
    assert_eq!(c.next_target(), target_for_work(1000.0));
    // every target step is at most a factor of 4 either way
    let flat = |_t: f64| STEADY;
    let chain = simulate(
        1000.0,
        &Run {
            hash_rate: &flat,
            groups: &[Group {
                share: 1.0,
                stamp: Stamp::Honest,
            }],
        },
        400,
        f64::MAX,
        7,
    );
    for i in 2..chain.targets.len() {
        let (a, b) = (work(&chain.targets[i - 1]), work(&chain.targets[i]));
        assert!(
            b / a <= 4.0 + 1e-6 && a / b <= 4.0 + 1e-6,
            "work {a} then {b} at block {i}"
        );
    }
    // every block is later than its parent, and none runs more than 120 s (plus the second of the floor) ahead of the clock
    let back = simulate(
        STEADY * 60.0,
        &Run {
            hash_rate: &flat,
            groups: &[
                Group {
                    share: 0.3,
                    stamp: Stamp::Backward,
                },
                Group {
                    share: 0.7,
                    stamp: Stamp::Forward,
                },
            ],
        },
        500,
        f64::MAX,
        3,
    );
    for i in 12..back.ts.len() {
        assert!(
            back.ts[i] > back.ts[i - 1],
            "block {i} is not later than its parent"
        );
        assert!(
            back.ts[i] as f64 <= back.real[i] + FUTURE_LIMIT as f64 + 1.0,
            "block {i} is too far ahead"
        );
    }
}

/// The geometric mean of the work per block over blocks 1000 to 3000, averaged over `seeds` runs, with this group of miners (the rest honest).
fn mean_work(rule: Rule, share: f64, stamp: Stamp, seeds: u64) -> f64 {
    let flat = |_t: f64| STEADY;
    let groups = [
        Group { share, stamp },
        Group {
            share: 1.0 - share,
            stamp: Stamp::Honest,
        },
    ];
    let mut sum = 0.0;
    for seed in 1..=seeds {
        let c = simulate_with(
            rule,
            STEADY * 60.0,
            &Run {
                hash_rate: &flat,
                groups: &groups,
            },
            3000,
            f64::MAX,
            7000 + seed,
        );
        sum += (1000..c.height())
            .map(|i| work(&c.targets[i]).ln())
            .sum::<f64>()
            / (c.height() - 1000) as f64;
    }
    (sum / seeds as f64).exp()
}

/// KNOWN_ISSUES.md item 12 and THREAT_MODEL.md E3, kept as numbers. THE FINDING BEFORE M11.2: with the median rule (the stand-in
/// [`Rule::OldMedian`]) a minority that backdates or stamps ahead lowers the difficulty. The next test is the same miners on the real rule.
#[test]
fn with_the_old_median_rule_a_minority_that_backdates_or_stamps_ahead_lowers_the_difficulty() {
    let honest = mean_work(Rule::OldMedian, 0.0001, Stamp::Honest, 6);
    let backward30 = mean_work(Rule::OldMedian, 0.30, Stamp::Backward, 6) / honest;
    let backward10 = mean_work(Rule::OldMedian, 0.10, Stamp::Backward, 6) / honest;
    let forward30 = mean_work(Rule::OldMedian, 0.30, Stamp::Forward, 6) / honest;
    // measured over 20 runs: 0.40, 0.69 and 0.70
    assert!(
        backward30 < 0.55,
        "a 30 % miner stamping backward: {backward30:.2}x the honest difficulty"
    );
    assert!(
        backward10 < 0.85,
        "a 10 % miner stamping backward: {backward10:.2}x"
    );
    assert!(
        forward30 < 0.85,
        "a 30 % miner stamping forward: {forward30:.2}x"
    );
    assert!(backward30 < backward10, "more hash rate, more effect");
}

/// THE REAL RULE (M11.2): a block must be later than its parent's, checked with the crate's own function. The same miners cannot move the difficulty.
#[test]
fn with_the_real_rule_a_block_must_be_later_than_its_parent_and_the_same_miners_cannot_move_the_difficulty(
) {
    let honest = mean_work(Rule::Current, 0.0001, Stamp::Honest, 6);
    for (share, stamp, lo, hi) in [
        (0.10, Stamp::Backward, 0.95, 1.08),
        (0.30, Stamp::Backward, 0.95, 1.12),
        (0.30, Stamp::Forward, 0.95, 1.05),
        (0.45, Stamp::Alternate, 0.95, 1.10),
    ] {
        let r = mean_work(Rule::Current, share, stamp, 6) / honest;
        assert!(
            (lo..=hi).contains(&r),
            "{share} {stamp:?}: {r:.2}x the honest difficulty (measured 1.00 to 1.06)"
        );
    }
    // and a rule that looks like a fix and is not: the symmetric clamp lets the miner push the difficulty UP
    let up = mean_work(Rule::SymmetricClamp, 0.30, Stamp::Backward, 6)
        / mean_work(Rule::SymmetricClamp, 0.0001, Stamp::Honest, 6);
    assert!(
        up > 1.5,
        "{up:.2}x: backdating raises the difficulty under the symmetric clamp (measured 2.6x)"
    );
}

// ---- the measurements ---------------------------------------------------------------------------------------------------------------------

#[test]
#[ignore = "a measurement: run with --ignored --nocapture"]
fn measure_steady_and_a_hash_rate_that_changes() {
    for rule in [
        Rule::OldMedian,
        Rule::NegativeSolveTimes,
        Rule::SymmetricClamp,
        Rule::Current,
    ] {
        println!(
            "
##### rule: {rule:?}"
        );
        println!("\n=== difficulty: LWMA, window {W}, one block a minute aimed for; real rules; {STEADY} attempts/s as the unit ===");
        let groups = [Group {
            share: 1.0,
            stamp: Stamp::Honest,
        }];
        let seeds = 20u64;
        let run_case = |label: &str,
                        start_work_factor: f64,
                        rate: &dyn Fn(f64) -> f64,
                        blocks: usize,
                        from: usize| {
            let (mut means, mut p95s, mut maxes) = (vec![], vec![], vec![]);
            for seed in 1..=seeds {
                let c = simulate_with(
                    rule,
                    STEADY * 60.0 * start_work_factor,
                    &Run {
                        hash_rate: rate,
                        groups: &groups,
                    },
                    blocks,
                    40.0 * 86400.0,
                    100 + seed,
                );
                let (m, _, p95, max) = gaps(&c, from);
                means.push(m);
                p95s.push(p95);
                maxes.push(max);
            }
            let avg = |v: &Vec<f64>| v.iter().sum::<f64>() / v.len() as f64;
            let worst = |v: &Vec<f64>| v.iter().cloned().fold(0.0, f64::max);
            println!("  {label:<52} mean gap {:>7.1} s | p95 {:>7.1} | worst single gap {:>9.1} s (over {seeds} runs)", avg(&means), avg(&p95s), worst(&maxes));
        };
        run_case(
            "steady, started at the right difficulty",
            1.0,
            &|_| STEADY,
            3000,
            100,
        );
        run_case(
            "steady, started 100x too easy",
            0.01,
            &|_| STEADY,
            3000,
            100,
        );
        run_case(
            "steady, started 1000x too easy (first blocks free)",
            0.001,
            &|_| STEADY,
            3000,
            100,
        );
        run_case(
            "steady, started 100x too hard",
            100.0,
            &|_| STEADY,
            3000,
            100,
        );
        println!("\n  --- the whole run from the start, the first 100 blocks ---");
        run_case(
            "started 1000x too easy: blocks 1-100",
            0.001,
            &|_| STEADY,
            100,
            1,
        );
        run_case(
            "started 100x too hard: blocks 1-100",
            100.0,
            &|_| STEADY,
            100,
            1,
        );
        println!("\n  --- the hash rate changes at hour 3 (blocks 1-3000 of 60 s: the change comes at about block 180) ---");
        for (label, factor) in [
            ("hash rate x2", 2.0),
            ("hash rate x10", 10.0),
            ("hash rate x100", 100.0),
            ("hash rate /2", 0.5),
            ("hash rate /10", 0.1),
            ("hash rate /100", 0.01),
        ] {
            let rate = move |t: f64| {
                if t < 3.0 * 3600.0 {
                    STEADY
                } else {
                    STEADY * factor
                }
            };
            run_case(label, 1.0, &rate, 3000, 180);
        }
    }
}

#[test]
#[ignore = "a measurement: run with --ignored --nocapture"]
fn measure_how_long_it_takes_to_recover_from_a_loss_of_hash_rate() {
    for rule in [
        Rule::OldMedian,
        Rule::NegativeSolveTimes,
        Rule::SymmetricClamp,
        Rule::Current,
    ] {
        println!(
            "
##### rule: {rule:?}"
        );
        println!("\n=== recovery: the hash rate drops at block 300 and stays down; blocks until the mean gap is back within 25 % of 60 s ===");
        println!("  (the time is the real time from the drop until then; 20 runs each, the median and the worst)");
        for factor in [0.5, 0.2, 0.1, 0.05, 0.01] {
            let (mut blocks_needed, mut secs_needed) = (vec![], vec![]);
            for seed in 1..=20u64 {
                // the drop is by block number, so that every run drops at the same place in the chain
                let dropped_at = std::cell::Cell::new(None::<f64>);
                let c = {
                    // two phases: build 300 blocks at the normal rate, then continue the same chain at the lower rate
                    let flat = |_t: f64| STEADY;
                    let mut chain = simulate(
                        STEADY * 60.0,
                        &Run {
                            hash_rate: &flat,
                            groups: &[Group {
                                share: 1.0,
                                stamp: Stamp::Honest,
                            }],
                        },
                        300,
                        f64::MAX,
                        500 + seed,
                    );
                    dropped_at.set(Some(chain.real[chain.height()]));
                    let mut rng = Rng(900 + seed);
                    let mut now = chain.real[chain.height()];
                    while chain.height() < 300 + 1500 {
                        let w = work(&chain.next_target());
                        now += rng.exp(w / (STEADY * factor));
                        let ts = (now as i64).max(chain.earliest_time());
                        chain.push(ts, now);
                    }
                    chain
                };
                // the first block after which the next 30 gaps average within 25 % of 60 s
                let mut found = None;
                for start in 301..=(c.height() - 30) {
                    let mean = (c.real[start + 30] - c.real[start]) / 30.0;
                    if (45.0..=75.0).contains(&mean) {
                        found = Some(start);
                        break;
                    }
                }
                let start = found.unwrap_or(c.height());
                blocks_needed.push((start - 300) as f64);
                secs_needed.push(c.real[start] - dropped_at.get().unwrap());
            }
            blocks_needed.sort_by(|a, b| a.partial_cmp(b).unwrap());
            secs_needed.sort_by(|a, b| a.partial_cmp(b).unwrap());
            println!(
            "  hash rate x{factor:<5} blocks: median {:>5.0}, worst {:>5.0} | real time: median {:>7.2} h, worst {:>7.2} h",
            blocks_needed[10], blocks_needed[19], secs_needed[10] / 3600.0, secs_needed[19] / 3600.0
        );
        }
    }
}

#[test]
#[ignore = "a measurement: run with --ignored --nocapture"]
fn measure_timestamp_manipulation() {
    for rule in [
        Rule::OldMedian,
        Rule::NegativeSolveTimes,
        Rule::SymmetricClamp,
        Rule::Current,
    ] {
        println!(
            "
##### rule: {rule:?}"
        );
        println!("\n=== timestamps: a group of miners with a share of the hash rate chooses its timestamps; the rest are honest ===");
        println!("  (real mean gap and the mean REAL gap relative to 60 s: above 1 means blocks are slower than aimed for. 20 runs of 3000 blocks)");
        println!(
            "  {:<44} {:>6} | {:>12} {:>14}",
            "attacker", "share", "real gap (s)", "difficulty vs honest"
        );
        // the honest baseline: the mean real gap is the aimed-for 60 s only if the difficulty is right; compare the work at the end
        let baseline_work = {
            let flat = |_t: f64| STEADY;
            let mut sum = 0.0;
            for seed in 1..=20u64 {
                let c = simulate_with(
                    rule,
                    STEADY * 60.0,
                    &Run {
                        hash_rate: &flat,
                        groups: &[Group {
                            share: 1.0,
                            stamp: Stamp::Honest,
                        }],
                    },
                    3000,
                    f64::MAX,
                    2000 + seed,
                );
                sum += (1000..c.height())
                    .map(|i| work(&c.targets[i]).ln())
                    .sum::<f64>()
                    / (c.height() - 1000) as f64;
            }
            (sum / 20.0).exp()
        };
        for stamp in [Stamp::Forward, Stamp::Backward, Stamp::Alternate] {
            for share in [0.10, 0.30, 0.45, 0.60] {
                let flat = |_t: f64| STEADY;
                let groups = [
                    Group { share, stamp },
                    Group {
                        share: 1.0 - share,
                        stamp: Stamp::Honest,
                    },
                ];
                let (mut gap, mut w) = (0.0, 0.0);
                for seed in 1..=20u64 {
                    let c = simulate_with(
                        rule,
                        STEADY * 60.0,
                        &Run {
                            hash_rate: &flat,
                            groups: &groups,
                        },
                        3000,
                        f64::MAX,
                        3000 + seed,
                    );
                    gap += gaps(&c, 1000).0;
                    w += (1000..c.height())
                        .map(|i| work(&c.targets[i]).ln())
                        .sum::<f64>()
                        / (c.height() - 1000) as f64;
                }
                let (gap, w) = (gap / 20.0, (w / 20.0).exp());
                println!(
                    "  {:<44} {:>5.0}% | {:>12.1} {:>13.2}x",
                    format!("{stamp:?}"),
                    share * 100.0,
                    gap,
                    w / baseline_work
                );
            }
        }
    }
}
