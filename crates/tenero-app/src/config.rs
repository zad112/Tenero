//! The node's settings: a `key = value` file and `--key value` command-line options, the same keys in both.
//!
//! The file has one setting per line; `#` starts a comment; blank lines are ignored; `seed` may repeat, every other
//! key may appear once. Command-line options override the file; `--seed` options add to the file's seeds only if the
//! file has none (a command line that names seeds means exactly those). An unknown key, a repeated key, a missing
//! value or a value that does not parse is an error that names the key, never a silent default: a typo in a
//! setting must not turn into a node that behaves differently from what its operator thinks.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use crate::log::Level;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Network {
    /// The SHA-256 test chain, which a CPU mines in an instant: for trying the programs. No real proof of work.
    Test,
    /// The real matmulhash proof of work, with a placeholder starting difficulty and genesis: the **development**
    /// network. It is not a launched network and nothing on it has value.
    Dev,
}

impl Network {
    pub fn parse(s: &str) -> Option<Network> {
        match s {
            "test" => Some(Network::Test),
            "dev" => Some(Network::Dev),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Network::Test => "test",
            Network::Dev => "dev",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MineMode {
    Off,
    Sha256,
    Cpu,
    Gpu,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub data: PathBuf,
    pub network: Network,
    pub listen: Option<SocketAddr>,
    pub seeds: Vec<String>,
    pub peer_target: usize,
    pub max_inbound: usize,
    pub allow_private_peers: bool,
    pub control: SocketAddr,
    /// 0 keeps every block in full (an archive node); N keeps the most recent N blocks' proofs.
    pub prune_keep: u64,
    pub assume_valid: Option<(u64, [u8; 32])>,
    pub mine: MineMode,
    pub mine_to: Option<String>,
    pub mine_cores: usize,
    /// Seconds between one block found and the next job (0: as fast as possible).
    pub mine_pace: u64,
    pub gpu_device: usize,
    pub gpu_batch: usize,
    pub log_level: Level,
    pub log_file: Option<PathBuf>,
    pub status_every: u64,
    /// Start even if other accounts on this computer can read the data directory (see `private_dir.rs`).
    pub allow_open_data_dir: bool,
}

/// What a node's operator is told when a setting is wrong.
#[derive(Debug, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ConfigError {}

fn bad(key: &str, why: impl std::fmt::Display) -> ConfigError {
    ConfigError(format!("setting `{key}`: {why}"))
}

const KEYS: &[&str] = &[
    "data",
    "network",
    "listen",
    "seed",
    "peers",
    "max_inbound",
    "allow_private_peers",
    "control",
    "prune_keep",
    "assume_valid",
    "mine",
    "mine_to",
    "mine_cores",
    "mine_pace",
    "gpu_device",
    "gpu_batch",
    "log_level",
    "log_file",
    "status_every",
    "allow_open_data_dir",
];

/// The raw settings: each key's values in the order given.
#[derive(Default, Debug)]
pub struct Raw(BTreeMap<String, Vec<String>>);

impl Raw {
    /// Reads the text of a config file.
    pub fn from_file_text(text: &str) -> Result<Raw, ConfigError> {
        let mut raw = Raw::default();
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                return Err(ConfigError(format!(
                    "line {}: expected `key = value`",
                    n + 1
                )));
            };
            let (k, v) = (k.trim(), v.trim());
            if v.is_empty() {
                return Err(bad(k, format!("line {}: no value", n + 1)));
            }
            raw.add(k, v)?;
        }
        Ok(raw)
    }

    fn add(&mut self, key: &str, value: &str) -> Result<(), ConfigError> {
        if !KEYS.contains(&key) {
            return Err(ConfigError(format!("unknown setting `{key}`")));
        }
        let e = self.0.entry(key.to_string()).or_default();
        if key != "seed" && !e.is_empty() {
            return Err(bad(key, "given twice"));
        }
        e.push(value.to_string());
        Ok(())
    }

    /// Applies command-line options (`--key value`) over the file's settings.
    pub fn with_args(mut self, args: &[String]) -> Result<Raw, ConfigError> {
        let mut seeds_from_args = false;
        let mut seen = std::collections::BTreeSet::new();
        let mut it = args.iter();
        while let Some(flag) = it.next() {
            let Some(key) = flag.strip_prefix("--") else {
                return Err(ConfigError(format!("unexpected argument `{flag}`")));
            };
            let Some(value) = it.next() else {
                return Err(bad(key, "needs a value"));
            };
            if !KEYS.contains(&key) {
                return Err(ConfigError(format!("unknown option `--{key}`")));
            }
            if key == "seed" {
                if !seeds_from_args {
                    self.0.remove("seed");
                    seeds_from_args = true;
                }
            } else {
                if !seen.insert(key.to_string()) {
                    return Err(bad(key, "given twice"));
                }
                self.0.remove(key);
            }
            self.add(key, value)?;
        }
        Ok(self)
    }

    fn one(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.first()).map(String::as_str)
    }

    fn parse<T: std::str::FromStr>(&self, key: &str, default: T) -> Result<T, ConfigError> {
        match self.one(key) {
            None => Ok(default),
            Some(v) => v
                .parse()
                .map_err(|_| bad(key, format!("`{v}` is not valid"))),
        }
    }

    fn flag(&self, key: &str, default: bool) -> Result<bool, ConfigError> {
        match self.one(key) {
            None => Ok(default),
            Some("true" | "yes" | "on") => Ok(true),
            Some("false" | "no" | "off") => Ok(false),
            Some(v) => Err(bad(key, format!("`{v}` is not yes or no"))),
        }
    }

    /// Checks everything and fills in defaults.
    pub fn into_config(self) -> Result<Config, ConfigError> {
        let data = self.one("data").ok_or_else(|| {
            bad(
                "data",
                "is required (the directory for the chain and the node's files)",
            )
        })?;
        let network = self.one("network").ok_or_else(|| {
            bad(
                "network",
                "is required: `test` (a CPU-mined test chain) or `dev` (the development network)",
            )
        })?;
        let network = Network::parse(network)
            .ok_or_else(|| bad("network", format!("`{network}` is not `test` or `dev`")))?;
        let listen = match self.one("listen") {
            Some(v) => Some(
                v.parse::<SocketAddr>()
                    .map_err(|_| bad("listen", format!("`{v}` is not ip:port")))?,
            ),
            None => None,
        };
        let control: SocketAddr = match self.one("control") {
            Some(v) => v
                .parse()
                .map_err(|_| bad("control", format!("`{v}` is not ip:port")))?,
            None => match network {
                Network::Test => "127.0.0.1:18332".parse().expect("valid"),
                Network::Dev => "127.0.0.1:28332".parse().expect("valid"),
            },
        };
        if !control.ip().is_loopback() {
            return Err(bad("control", "must be a loopback address (127.0.0.1): the control interface is for this machine only"));
        }
        let seeds = self.0.get("seed").cloned().unwrap_or_default();
        for s in &seeds {
            if s.parse::<SocketAddr>().is_err() {
                return Err(bad("seed", format!("`{s}` is not ip:port")));
            }
        }
        let peer_target: usize = self.parse("peers", 50)?;
        let max_inbound: usize = self.parse("max_inbound", 64)?;
        if peer_target == 0 {
            return Err(bad(
                "peers",
                "must be at least 1 (use a node with no seeds to run alone)",
            ));
        }
        let prune_keep: u64 = self.parse("prune_keep", 0)?;
        if prune_keep != 0 && prune_keep < 1_000 {
            return Err(bad(
                "prune_keep",
                "must be 0 (keep everything) or at least 1000 blocks (a shorter history cannot serve a reorganisation)",
            ));
        }
        let assume_valid = match self.one("assume_valid") {
            None => None,
            Some(v) => {
                let (h, id) = v
                    .split_once(':')
                    .ok_or_else(|| bad("assume_valid", "must be `height:block_id_in_hex`"))?;
                let height: u64 = h
                    .parse()
                    .map_err(|_| bad("assume_valid", "the height is not a number"))?;
                if id.len() != 64 || !id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                    return Err(bad(
                        "assume_valid",
                        "the block id must be 64 lower-case hexadecimal digits",
                    ));
                }
                let mut out = [0u8; 32];
                for (i, b) in out.iter_mut().enumerate() {
                    *b = u8::from_str_radix(&id[2 * i..2 * i + 2], 16).expect("checked");
                }
                Some((height, out))
            }
        };
        let mine = match self.one("mine") {
            None | Some("off") => MineMode::Off,
            Some("sha256") => MineMode::Sha256,
            Some("cpu") => MineMode::Cpu,
            Some("gpu") => MineMode::Gpu,
            Some(v) => return Err(bad("mine", format!("`{v}` is not off, sha256, cpu or gpu"))),
        };
        match (mine, network) {
            (MineMode::Sha256, Network::Dev) => {
                return Err(bad(
                    "mine",
                    "sha256 is the test network's proof of work; the dev network needs cpu or gpu",
                ))
            }
            (MineMode::Cpu | MineMode::Gpu, Network::Test) => {
                return Err(bad("mine", "the test network's proof of work is sha256"))
            }
            _ => {}
        }
        let mine_to = self.one("mine_to").map(str::to_string);
        if mine != MineMode::Off {
            let Some(to) = &mine_to else {
                return Err(bad(
                    "mine_to",
                    "is required when mining: the wallet address to pay rewards to",
                ));
            };
            tenero_wallet::Address::from_text(to).map_err(|e| bad("mine_to", e))?;
        }
        let mine_cores: usize = self.parse("mine_cores", 6)?;
        if mine_cores == 0 || mine_cores > tenero_miner::MAX_CORES {
            return Err(bad(
                "mine_cores",
                format!("must be 1 to {}", tenero_miner::MAX_CORES),
            ));
        }
        let mine_pace: u64 =
            self.parse("mine_pace", if network == Network::Test { 5 } else { 0 })?;
        let gpu_batch: usize = self.parse("gpu_batch", 128)?;
        if gpu_batch == 0 {
            return Err(bad("gpu_batch", "must be at least 1"));
        }
        let log_level = match self.one("log_level") {
            None => Level::Info,
            Some(v) => Level::parse(v).ok_or_else(|| {
                bad(
                    "log_level",
                    format!("`{v}` is not error, warn, info or debug"),
                )
            })?,
        };
        let status_every: u64 = self.parse("status_every", 60)?;
        if status_every == 0 {
            return Err(bad("status_every", "must be at least 1 second"));
        }
        Ok(Config {
            data: PathBuf::from(data),
            network,
            listen,
            seeds,
            peer_target,
            max_inbound,
            allow_private_peers: self.flag("allow_private_peers", network == Network::Test)?,
            control,
            prune_keep,
            assume_valid,
            mine,
            mine_to,
            mine_cores,
            mine_pace,
            gpu_device: self.parse("gpu_device", 0)?,
            gpu_batch,
            log_level,
            log_file: self.one("log_file").map(PathBuf::from),
            status_every,
            allow_open_data_dir: self.flag("allow_open_data_dir", false)?,
        })
    }
}
