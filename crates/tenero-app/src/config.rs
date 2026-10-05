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
use crate::ui::ColorChoice;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Network {
    /// The SHA-256 test chain, which a CPU mines in an instant: for trying the programs. No real proof of work.
    Test,
    /// The real matmulhash proof of work, with a placeholder starting difficulty and genesis: the **development**
    /// network. It is not a launched network and nothing on it has value.
    Dev,
    /// The **release network** of the first test release (M11.2): the real matmulhash proof of work, a fresh genesis with no premine
    /// ("tenero alpha network 1"), a real starting difficulty (2^237: about 524,000 attempts a block) and 100-block epochs. It is still a
    /// test network: unaudited, and nothing on it has value.
    Alpha,
}

/// The proof-of-work epoch of the networks that use the real proof of work, in blocks (the owner's choice for M11.2; the same value the
/// development network always had).
pub const REAL_POW_EPOCH_BLOCKS: u64 = 100;

impl Network {
    pub fn parse(s: &str) -> Option<Network> {
        match s {
            "test" => Some(Network::Test),
            "dev" => Some(Network::Dev),
            "alpha" => Some(Network::Alpha),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Network::Test => "test",
            Network::Dev => "dev",
            Network::Alpha => "alpha",
        }
    }

    /// Every network, in the order the screens list them.
    pub const ALL: [Network; 3] = [Network::Test, Network::Dev, Network::Alpha];

    /// Whether the network uses the real matmulhash proof of work (a CPU or a GPU mines it) and not SHA-256.
    pub fn real_pow(self) -> bool {
        self != Network::Test
    }

    /// The proof-of-work epoch in blocks (the real proof of work's; the test chain has none, and the number is not used there).
    pub fn epoch_blocks(self) -> u64 {
        REAL_POW_EPOCH_BLOCKS
    }

    /// The default port of the loopback control interface: a different one for each network, so that nodes of two networks on one
    /// machine do not meet.
    pub fn default_control_port(self) -> u16 {
        match self {
            Network::Test => 18332,
            Network::Dev => 28332,
            Network::Alpha => 38332,
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

/// The seed addresses built into the program for the `alpha` network (`ip:port`). A brand-new node starts from these and from any `seed` setting.
/// **Empty until a seed exists**: nothing may be put here that nobody runs, and the list is only as trustworthy as the number of *independent
/// operators in different network groups* behind it (`docs/SEED_POLICY.md`; `check_seed_list` refuses a list that breaks the rules that can be
/// checked). A release carries whatever is here, so a change of address needs a new release (a node's own `seed` settings and `no_builtin_seeds`
/// are the way round that).
pub const ALPHA_SEEDS: &[&str] = &[];

impl Network {
    /// The seeds built into the program for this network (none for the private `test` and `dev` networks).
    pub fn builtin_seeds(self) -> &'static [&'static str] {
        match self {
            Network::Alpha => ALPHA_SEEDS,
            Network::Test | Network::Dev => &[],
        }
    }
}

/// Checks a list of built-in seeds for the mistakes a program can see: every entry is `ip:port` with a port, a public address (not loopback or private),
/// listed once, and **no two in one network group** (a new node counts a group once, so a second seed there adds nothing). It cannot tell whether a seed is
/// honest or whether two seeds are run by the same person: that is the author's to know.
pub fn check_seed_list(list: &[&str]) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut groups = std::collections::BTreeSet::new();
    for s in list {
        let sa: SocketAddr = s.parse().map_err(|_| format!("`{s}` is not ip:port"))?;
        if sa.port() == 0 {
            return Err(format!("`{s}` has no port"));
        }
        if !tenero_net::addrbook::is_routable(&sa) {
            return Err(format!("`{s}` is not a public address"));
        }
        if !seen.insert(sa) {
            return Err(format!("`{s}` is listed twice"));
        }
        let group = tenero_net::addrbook::group_of(s);
        if !groups.insert(group.clone()) {
            return Err(format!(
                "`{s}`: another seed is already in the network group {group}"
            ));
        }
    }
    Ok(())
}

/// The seeds a node starts from: the built-in ones (unless `use_builtin` is false), then the configured ones, each once.
pub fn effective_seeds(builtin: &[&str], configured: &[String], use_builtin: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let from_builtin = builtin
        .iter()
        .filter(|_| use_builtin)
        .map(|s| s.to_string());
    for s in from_builtin.chain(configured.iter().cloned()) {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub data: PathBuf,
    pub network: Network,
    pub listen: Option<SocketAddr>,
    /// The `ip:port` other nodes can reach this one at, told to every peer once (a peer takes it only if the IP is the one it connected from).
    /// This is how a seed learns where its peers are, and so what to tell the next node that asks: without it a node never tells anyone where it
    /// is, and a seed's address book stays empty. **`0.0.0.0:PORT` means "the address you see me at, on this port"**: the node need not know its
    /// own IP, which changes with a home connection (the wallet app uses this form). Leave it unset behind a router that does not forward the
    /// port (nobody could dial it). Needs `listen` (a node that is not listening cannot be reached).
    pub advertise: Option<String>,
    pub seeds: Vec<String>,
    /// Peers pinned by the operator (`ip:port`, got out of band): always dialled, whenever not connected.
    pub trusted_peers: Vec<String>,
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
    /// `gpu_batch = auto`: measure a few batch sizes at start-up and use the fastest (`gpu_batch` is then the fallback, 128).
    pub gpu_batch_auto: bool,
    pub log_level: Level,
    pub log_file: Option<PathBuf>,
    pub status_every: u64,
    /// Start even if other accounts on this computer can read the data directory (see `private_dir.rs`).
    pub allow_open_data_dir: bool,
    /// What the screen shows (`ui.rs`): only warnings and errors, or also every line of the log; and whether it uses colour.
    pub quiet: bool,
    pub verbose: bool,
    pub color: ColorChoice,
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
    "advertise",
    "seed",
    "no_builtin_seeds",
    "trusted_peer",
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
    "quiet",
    "verbose",
    "color",
];

/// Keys that may be given more than once.
fn is_repeatable(key: &str) -> bool {
    key == "seed" || key == "trusted_peer"
}

/// The most peers an operator may pin.
const MAX_TRUSTED_PEERS: usize = 16;

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
        if !is_repeatable(key) && !e.is_empty() {
            return Err(bad(key, "given twice"));
        }
        e.push(value.to_string());
        Ok(())
    }

    /// Applies command-line options (`--key value`) over the file's settings.
    pub fn with_args(mut self, args: &[String]) -> Result<Raw, ConfigError> {
        // a repeatable key given on the command line replaces the file's values (it means exactly those)
        let mut repeated_from_args = std::collections::BTreeSet::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut it = args.iter().peekable();
        while let Some(flag) = it.next() {
            let Some(key) = flag.strip_prefix("--") else {
                return Err(ConfigError(format!("unexpected argument `{flag}`")));
            };
            // `--quiet` and `--verbose` alone mean yes
            let bare = matches!(key, "quiet" | "verbose")
                && it.peek().is_none_or(|next| next.starts_with("--"));
            let value = if bare {
                &"yes".to_string()
            } else {
                match it.next() {
                    Some(v) => v,
                    None => return Err(bad(key, "needs a value")),
                }
            };
            if !KEYS.contains(&key) {
                return Err(ConfigError(format!("unknown option `--{key}`")));
            }
            if is_repeatable(key) {
                if repeated_from_args.insert(key.to_string()) {
                    self.0.remove(key);
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
                "is required: `test` (a CPU-mined test chain), `dev` (the development network) or `alpha` (the test release's network)",
            )
        })?;
        let network = Network::parse(network).ok_or_else(|| {
            bad(
                "network",
                format!("`{network}` is not `test`, `dev` or `alpha`"),
            )
        })?;
        let listen = match self.one("listen") {
            Some(v) => Some(
                v.parse::<SocketAddr>()
                    .map_err(|_| bad("listen", format!("`{v}` is not ip:port")))?,
            ),
            None => None,
        };
        let advertise = match self.one("advertise") {
            Some(v) => {
                v.parse::<SocketAddr>()
                    .map_err(|_| bad("advertise", format!("`{v}` is not ip:port")))?;
                Some(v.to_string())
            }
            None => None,
        };
        if advertise.is_some() && listen.is_none() {
            return Err(bad(
                "advertise",
                "tells other nodes where to reach this one, but this node is not listening (set `listen` too)",
            ));
        }
        let control: SocketAddr = match self.one("control") {
            Some(v) => v
                .parse()
                .map_err(|_| bad("control", format!("`{v}` is not ip:port")))?,
            None => SocketAddr::from(([127, 0, 0, 1], network.default_control_port())),
        };
        if !control.ip().is_loopback() {
            return Err(bad("control", "must be a loopback address (127.0.0.1): the control interface is for this machine only"));
        }
        let configured_seeds = self.0.get("seed").cloned().unwrap_or_default();
        for s in &configured_seeds {
            if s.parse::<SocketAddr>().is_err() {
                return Err(bad("seed", format!("`{s}` is not ip:port")));
            }
        }
        // the program's own seeds for this network, then the operator's (`no_builtin_seeds yes` for a private network of one's own)
        let seeds = effective_seeds(
            network.builtin_seeds(),
            &configured_seeds,
            !self.flag("no_builtin_seeds", false)?,
        );
        let trusted_peers = self.0.get("trusted_peer").cloned().unwrap_or_default();
        for s in &trusted_peers {
            match s.parse::<SocketAddr>() {
                Ok(sa) if sa.port() != 0 => {}
                _ => return Err(bad("trusted_peer", format!("`{s}` is not ip:port"))),
            }
        }
        if trusted_peers.len() > MAX_TRUSTED_PEERS {
            return Err(bad(
                "trusted_peer",
                format!("at most {MAX_TRUSTED_PEERS} peers may be pinned"),
            ));
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
            (MineMode::Sha256, n) if n.real_pow() => {
                return Err(bad(
                    "mine",
                    format!(
                    "sha256 is the test network's proof of work; the {} network needs cpu or gpu",
                    n.name()
                ),
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
        let gpu_batch_auto = self.one("gpu_batch") == Some("auto");
        let gpu_batch: usize = if gpu_batch_auto {
            128
        } else {
            self.parse("gpu_batch", 128)?
        };
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
        let (quiet, verbose) = (self.flag("quiet", false)?, self.flag("verbose", false)?);
        if quiet && verbose {
            return Err(bad("quiet", "cannot be combined with `verbose`"));
        }
        let color = match self.one("color") {
            None => ColorChoice::Auto,
            Some(v) => ColorChoice::parse(v)
                .ok_or_else(|| bad("color", format!("`{v}` is not auto, always or never")))?,
        };
        Ok(Config {
            data: PathBuf::from(data),
            network,
            listen,
            advertise,
            seeds,
            trusted_peers,
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
            gpu_batch_auto,
            log_level,
            log_file: self.one("log_file").map(PathBuf::from),
            status_every,
            allow_open_data_dir: self.flag("allow_open_data_dir", false)?,
            quiet,
            verbose,
            color,
        })
    }
}
