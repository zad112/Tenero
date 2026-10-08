//! Where the node is, what the explorer asks it, and the numbers worked out from the answers. No window here, so all of it
//! is tested on its own.
//!
//! **What is measured and what is estimated.** Height, the next block's target, the work, the pool and each block's size,
//! time and reward are what the node says. The difficulty is worked out from the target exactly. The NETWORK HASH RATE
//! cannot be measured by anyone: it is ESTIMATED from the work the last blocks proved and the time their timestamps say
//! they took. Miners write those timestamps, and with few blocks luck dominates, so it is a rough figure.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tenero_app::client::{read_cookie, RemoteNode, COOKIE_FILE};
use tenero_app::config::Network;
use tenero_app::control::{BlockSummary, ChainStats, NodeInfo, MAX_BLOCKS_PER_REQUEST};
use tenero_core::u256::U256;
use tenero_node::PoolEntry;

/// How many of the latest blocks the window lists.
pub const LATEST_BLOCKS: u64 = 30;
/// How many blocks back the hash rate is estimated over: the difficulty's own window (`ChainParams::version_2`).
pub const HASHRATE_WINDOW: u64 = 30;
/// How often the node is asked again.
pub const REFRESH: Duration = Duration::from_secs(5);

/// Where the node is: the folder holding its cookie file, and its control address (loopback only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub data_dir: PathBuf,
    pub control: SocketAddr,
    /// How it was found, for the screen.
    pub found_by: String,
}

impl Source {
    /// Reads the cookie (fresh each time: a restarted node has a new one) and connects.
    pub fn connect(&self) -> Result<RemoteNode, String> {
        let cookie = read_cookie(&self.data_dir.join(COOKIE_FILE))
            .map_err(|e| format!("{e} (is the node running, and is this its data folder?)"))?;
        RemoteNode::connect(self.control, &cookie)
    }
}

/// What the command line asked for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Args {
    pub app_dir: Option<PathBuf>,
    pub data: Option<PathBuf>,
    pub control: Option<SocketAddr>,
    pub network: Option<Network>,
}

pub const USAGE: &str =
    "tenero-explorer [--app-dir FOLDER] | [--data FOLDER [--network NAME] [--control IP:PORT]]";

/// `--app-dir FOLDER` (the wallet app's folder: the explorer shows the node it runs; the default), or `--data FOLDER` (a
/// node's data folder) with `--network test|dev|beta|alpha` (its default control port) or `--control IP:PORT`. A mistake
/// is an error, never a guess.
pub fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut out = Args::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .filter(|v| !v.is_empty())
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match a.as_str() {
            "--app-dir" => out.app_dir = Some(PathBuf::from(value("--app-dir")?)),
            "--data" => out.data = Some(PathBuf::from(value("--data")?)),
            "--control" => {
                let v = value("--control")?;
                let addr: SocketAddr = v
                    .parse()
                    .map_err(|_| format!("--control: `{v}` is not ip:port"))?;
                if !addr.ip().is_loopback() {
                    return Err("--control must be on this computer (127.0.0.1)".into());
                }
                out.control = Some(addr);
            }
            "--network" => {
                let v = value("--network")?;
                out.network =
                    Some(Network::parse(&v).ok_or_else(|| {
                        format!("--network: `{v}` is not test, dev, beta or alpha")
                    })?);
            }
            other => return Err(format!("unknown argument `{other}` (usage: {USAGE})")),
        }
    }
    if out.app_dir.is_some() && out.data.is_some() {
        return Err("give --app-dir or --data, not both".into());
    }
    if out.data.is_none() && out.network.is_some() {
        return Err(
            "--network goes with --data (the wallet app's settings already say the network)".into(),
        );
    }
    Ok(out)
}

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Finds the node: the one named on the command line, or else the one the wallet app runs (its settings file names the node's
/// data folder and control port; with no settings file, the app's defaults for the test network).
pub fn resolve(args: &Args, default_app_dir: &Path) -> Result<Source, String> {
    if let Some(data) = &args.data {
        let network = args.network.unwrap_or(Network::Test);
        return Ok(Source {
            data_dir: data.clone(),
            control: args
                .control
                .unwrap_or_else(|| loopback(network.default_control_port())),
            found_by: format!("--data {}", data.display()),
        });
    }
    let app_dir = args
        .app_dir
        .clone()
        .unwrap_or_else(|| default_app_dir.to_path_buf());
    let s = tenero_gui::settings::Settings::load(&app_dir).map_err(|e| {
        format!("{e}. Start the explorer with --data FOLDER (the node's data folder) and --network NAME")
    })?;
    Ok(Source {
        data_dir: s.data_dir,
        control: args.control.unwrap_or(s.control),
        found_by: format!(
            "the wallet app's settings ({}, {} network)",
            app_dir.display(),
            s.network.name()
        ),
    })
}

/// One look at the node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub info: NodeInfo,
    pub stats: ChainStats,
    /// The latest blocks, the newest first (enough for the list and the hash-rate window).
    pub blocks: Vec<BlockSummary>,
    /// How many transactions the pool holds, and the best of them by fee rate (at most 4096).
    pub pool_total: u32,
    pub pool: Vec<PoolEntry>,
    /// When it was taken (Unix seconds, this computer's clock).
    pub taken_at: u64,
}

/// Asks the node for everything the window shows: four requests, none of which changes anything.
pub fn fetch(node: &RemoteNode, now: u64) -> Result<Snapshot, String> {
    let info = node.info()?;
    let stats = node.chain_stats()?;
    let want = LATEST_BLOCKS
        .max(HASHRATE_WINDOW + 1)
        .min(u64::from(MAX_BLOCKS_PER_REQUEST));
    let from = stats.height.saturating_sub(want - 1);
    let mut blocks = node.headers(from, want as u16)?;
    blocks.reverse();
    let (pool_total, pool) = node.mempool()?;
    Ok(Snapshot {
        info,
        stats,
        blocks,
        pool_total,
        pool,
        taken_at: now,
    })
}

// ---- the numbers ------------------------------------------------------------------------------------------------

/// A 256-bit big-endian number as a float: for showing, never for any rule.
pub fn be_to_f64(b: &[u8; 32]) -> f64 {
    b.iter().fold(0.0, |acc, &x| acc * 256.0 + f64::from(x))
}

/// The difficulty of a target: the expected number of proof-of-work attempts to find a block, `floor(2^256 / target)`
/// (the work the chain counts for it). `None` for a target of 0 or 1.
pub fn difficulty(target: &[u8; 32]) -> Option<U256> {
    U256::work_of_target(&U256::from_be_bytes(target))
}

/// The hash rate the latest blocks imply.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rate {
    /// Proof-of-work attempts per second, all miners together (an estimate).
    pub per_second: f64,
    /// The blocks and the seconds it is measured over.
    pub blocks: u64,
    pub seconds: u64,
}

impl Rate {
    pub fn mean_block_time(&self) -> f64 {
        self.seconds as f64 / self.blocks as f64
    }
}

/// The work the blocks of the last `window` proved, over the time their timestamps say they took. `blocks` is newest first
/// and need not be complete. The genesis block is left out (its time is the network's launch, not a block found). `None`
/// with fewer than two blocks, or when the timestamps do not go forward (miners write them, and they may).
pub fn estimate_hashrate(blocks: &[BlockSummary], window: u64) -> Option<Rate> {
    let mut usable = blocks.iter().filter(|b| b.height >= 1);
    let newest = usable.next()?;
    let oldest = usable
        .filter(|b| b.height < newest.height && newest.height - b.height <= window)
        .min_by_key(|b| b.height)?;
    let seconds = newest.timestamp.checked_sub(oldest.timestamp)?;
    if seconds == 0 {
        return None;
    }
    let work = U256::from_be_bytes(&newest.cumulative_work)
        .checked_sub(&U256::from_be_bytes(&oldest.cumulative_work))?;
    Some(Rate {
        per_second: be_to_f64(&work.to_be_bytes()) / seconds as f64,
        blocks: newest.height - oldest.height,
        seconds,
    })
}

// ---- words for the screen ---------------------------------------------------------------------------------------

/// `1234567.0` is `"1.23 M"`: three significant figures and a metric prefix.
pub fn si(x: f64) -> String {
    const PREFIXES: [&str; 9] = ["", "k", "M", "G", "T", "P", "E", "Z", "Y"];
    if !x.is_finite() || x < 0.0 {
        return "?".into();
    }
    let mut v = x;
    let mut i = 0;
    while v >= 999.5 && i + 1 < PREFIXES.len() {
        v /= 1000.0;
        i += 1;
    }
    // a whole number of units is written whole
    let digits = if (i == 0 && v.fract() == 0.0) || v >= 99.95 {
        0
    } else if v >= 9.995 {
        1
    } else {
        2
    };
    let n = format!("{v:.digits$}");
    if PREFIXES[i].is_empty() {
        n
    } else {
        format!("{n} {}", PREFIXES[i])
    }
}

/// A hash rate: `"1.23 MH/s"`.
pub fn hashrate_text(per_second: f64) -> String {
    let s = si(per_second);
    match s.split_once(' ') {
        Some((n, p)) => format!("{n} {p}H/s"),
        None => format!("{s} H/s"),
    }
}

/// `1234567` is `"1,234,567"`.
pub fn grouped(n: u64) -> String {
    tenero_app::ui::group_digits(n)
}

/// A size in bytes: `"812 B"`, `"12.3 kB"` (1 kB = 1000 bytes).
pub fn bytes_text(n: u64) -> String {
    if n < 1000 {
        format!("{n} B")
    } else {
        format!("{}B", si(n as f64))
    }
}

/// An amount in units as coins, with the ticker, as the wallet app writes it: `"20 TNR"`, `"12,345.5 TNR"`.
pub fn coins_text(units: u64) -> String {
    tenero_gui::text::coins(units)
}

/// A fee per 1000 bytes, in coins.
pub fn fee_rate_text(fee: u64, size: u64) -> String {
    if size == 0 {
        return "?".into();
    }
    let per_kb = (u128::from(fee) * 1000 / u128::from(size)).min(u128::from(u64::MAX)) as u64;
    format!("{} /kB", coins_text(per_kb))
}

/// `"2026-10-07 09:05:03 UTC"`.
pub fn utc_text(secs: u64) -> String {
    let t = tenero_app::log::utc_timestamp(secs);
    format!("{} UTC", t.trim_end_matches('Z').replacen('T', " ", 1))
}

/// How long ago `then` was at `now`, in rough words: `"42 s ago"`, `"5 min ago"`, `"3 h ago"`, `"2 d ago"`. A time after
/// `now` (a miner's clock ahead of this one) says so: `"12 s ahead"`.
pub fn ago_text(now: u64, then: u64) -> String {
    fn span(s: u64) -> String {
        match s {
            0..=59 => format!("{s} s"),
            60..=3_599 => format!("{} min", s / 60),
            3_600..=86_399 => format!("{} h", s / 3_600),
            _ => format!("{} d", s / 86_400),
        }
    }
    if then > now {
        format!("{} ahead", span(then - now))
    } else {
        format!("{} ago", span(now - then))
    }
}

pub fn hex(b: &[u8]) -> String {
    tenero_core::hash::hex_lower(b)
}

/// The start and end of a hash: `"3fa9c01e…77b2d0"`.
pub fn short_hex(b: &[u8; 32]) -> String {
    let h = hex(b);
    format!("{}…{}", &h[..10], &h[h.len() - 6..])
}

/// The emission so far as a share of the main emission's cap: `"0.81 %"`. Past the cap the tail goes on, so more than 100 %
/// is possible.
pub fn emitted_share_text(emitted: u64, max_supply: u64) -> String {
    if max_supply == 0 {
        return "?".into();
    }
    let pct = emitted as f64 * 100.0 / max_supply as f64;
    if emitted > 0 && pct < 0.001 {
        // the first blocks of a chain: "0.000 %" would say nothing was paid
        "< 0.001 %".into()
    } else if pct < 10.0 {
        format!("{pct:.3} %")
    } else {
        format!("{pct:.1} %")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(height: u64, timestamp: u64, work: u64) -> BlockSummary {
        BlockSummary {
            height,
            id: [height as u8; 32],
            timestamp,
            target: [0xff; 32],
            cumulative_work: U256::from_u64(work).to_be_bytes(),
            size: 300,
            tx_count: 0,
            coinbase_total: 0,
        }
    }

    #[test]
    fn the_hash_rate_is_the_work_over_the_time_of_the_window() {
        // 1,000 attempts a block, a block a minute: 1000/60 attempts a second
        let blocks: Vec<BlockSummary> = (0..=40)
            .rev()
            .map(|h| summary(h, 1_000 + 60 * h, 1_000 * h))
            .collect();
        let r = estimate_hashrate(&blocks, 30).unwrap();
        assert_eq!((r.blocks, r.seconds), (30, 1_800));
        assert!((r.per_second - 1_000.0 / 60.0).abs() < 1e-9);
        assert!((r.mean_block_time() - 60.0).abs() < 1e-9);
        // a window wider than what is there uses what is there, leaving out the genesis block
        let r = estimate_hashrate(&blocks, 1_000).unwrap();
        assert_eq!((r.blocks, r.seconds), (39, 39 * 60));
    }

    #[test]
    fn no_hash_rate_without_two_blocks_or_with_time_going_backwards() {
        assert_eq!(estimate_hashrate(&[], 30), None);
        assert_eq!(estimate_hashrate(&[summary(5, 100, 50)], 30), None);
        // the genesis block does not count
        assert_eq!(
            estimate_hashrate(&[summary(1, 100, 50), summary(0, 0, 0)], 30),
            None
        );
        // timestamps equal, or the newer one earlier
        assert_eq!(
            estimate_hashrate(&[summary(6, 100, 60), summary(5, 100, 50)], 30),
            None
        );
        assert_eq!(
            estimate_hashrate(&[summary(6, 90, 60), summary(5, 100, 50)], 30),
            None
        );
    }

    #[test]
    fn the_difficulty_is_two_to_the_256_over_the_target() {
        let mut half = [0u8; 32];
        half[0] = 0x80;
        assert_eq!(difficulty(&half), Some(U256::from_u64(2)));
        let mut t = [0u8; 32];
        t[2] = 1; // 2^232
        assert_eq!(difficulty(&t), Some(U256::from_u64(1 << 24)));
        assert_eq!(difficulty(&[0; 32]), None);
        assert!(
            (be_to_f64(&U256::from_u64(123_456_789).to_be_bytes()) - 123_456_789.0).abs() < 1e-6
        );
    }

    #[test]
    fn words_for_numbers() {
        assert_eq!(si(0.0), "0");
        assert_eq!(si(999.0), "999");
        assert_eq!(si(1_234.0), "1.23 k");
        assert_eq!(si(12_345_678.0), "12.3 M");
        assert_eq!(si(999_999.0), "1.00 M");
        assert_eq!(hashrate_text(2_500_000.0), "2.50 MH/s");
        assert_eq!(hashrate_text(16.6667), "16.7 H/s");
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(1_234_567), "1,234,567");
        assert_eq!(grouped(100), "100");
        assert_eq!(bytes_text(812), "812 B");
        assert_eq!(bytes_text(12_345), "12.3 kB");
        assert_eq!(coins_text(2_000_000_000), "20 TNR");
        assert_eq!(coins_text(150_000_000), "1.5 TNR");
        assert_eq!(fee_rate_text(100_000, 2_000), "0.0005 TNR /kB");
        assert_eq!(fee_rate_text(1, 0), "?");
        assert_eq!(utc_text(1_700_000_000), "2023-11-14 22:13:20 UTC");
        assert_eq!(ago_text(1_000, 958), "42 s ago");
        assert_eq!(ago_text(1_000, 1_012), "12 s ahead");
        assert_eq!(ago_text(100_000, 100_000 - 7_300), "2 h ago");
        assert_eq!(ago_text(1_000_000, 0), "11 d ago");
        assert_eq!(short_hex(&[0xab; 32]), "ababababab…ababab");
        assert_eq!(
            emitted_share_text(16_240_000_000_000, 2_000_000_000_000_000),
            "0.812 %"
        );
        assert_eq!(emitted_share_text(1, 0), "?");
        assert_eq!(
            emitted_share_text(80 * 100_000_000, 2_000_000_000_000_000),
            "< 0.001 %"
        );
        assert_eq!(emitted_share_text(0, 2_000_000_000_000_000), "0.000 %");
    }

    fn args(v: &[&str]) -> Result<Args, String> {
        parse_args(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn the_command_line_names_a_node_or_the_wallet_apps_folder() {
        assert_eq!(args(&[]).unwrap(), Args::default());
        let a = args(&["--data", "D:/node", "--network", "beta"]).unwrap();
        let s = resolve(&a, Path::new("unused")).unwrap();
        assert_eq!(s.data_dir, PathBuf::from("D:/node"));
        assert_eq!(s.control, loopback(Network::Beta.default_control_port()));
        let a = args(&["--data", "D:/node", "--control", "127.0.0.1:1234"]).unwrap();
        assert_eq!(resolve(&a, Path::new("x")).unwrap().control, loopback(1234));
        // with no network, the test network's port
        let a = args(&["--data", "D:/node"]).unwrap();
        assert_eq!(
            resolve(&a, Path::new("x")).unwrap().control,
            loopback(18332)
        );
        // mistakes are refused, not guessed at
        assert!(args(&["--control", "10.0.0.1:18332"])
            .unwrap_err()
            .contains("this computer"));
        assert!(args(&["--control", "nonsense"]).is_err());
        assert!(args(&["--network", "main"]).is_err());
        assert!(args(&["--network", "beta"]).unwrap_err().contains("--data"));
        assert!(args(&["--data"]).unwrap_err().contains("needs a value"));
        assert!(args(&["--app-dir", "a", "--data", "b"]).is_err());
        assert!(args(&["--frobnicate"]).unwrap_err().contains("unknown"));
    }

    #[test]
    fn with_no_arguments_it_finds_the_wallet_apps_node() {
        let dir = std::env::temp_dir().join(format!("tenero-explorer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // no settings file: the wallet app's defaults
        let s = resolve(&Args::default(), &dir).unwrap();
        let d = tenero_gui::settings::Settings::defaults(&dir, Network::Test);
        assert_eq!((s.data_dir, s.control), (d.data_dir.clone(), d.control));
        // a settings file for another network: its folder and its port
        let mut beta = tenero_gui::settings::Settings::defaults(&dir, Network::Beta);
        beta.control = loopback(40000);
        beta.save(&dir).unwrap();
        let s = resolve(&Args::default(), &dir).unwrap();
        assert_eq!((s.data_dir, s.control), (beta.data_dir, loopback(40000)));
        assert!(s.found_by.contains("beta"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
