//! The wallet app's settings: a small `key = value` file (`settings.conf`) in the app folder. Everything has a
//! default, a bad value is an error that names the key, and an unknown key is an error too (a typo must not be
//! silently ignored). The file holds no secret: not the seed, not the passphrase.
//!
//! The app folder is `%LOCALAPPDATA%\Tenero` on Windows and `~/.tenero` elsewhere. A network's node data and its wallet
//! file live under it, one set per network: the test network (a SHA-256 chain a CPU can mine, for trying things) and the
//! development network (the real matmulhash proof of work). **Neither is a launched network and nothing on them has
//! value.**

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use tenero_app::config::Network;

/// The settings of the 0.3.0 app (the `gamma`, `dev` and `test` networks of version 3). The 0.2.0 app's `settings.conf`, beside it
/// in the same folder, is left alone: the two can be installed side by side, and neither reads the other's networks.
pub const SETTINGS_FILE: &str = "settings-v3.conf";

/// What the node keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    /// Every block's proofs.
    Archive,
    /// Throws away the proofs of old blocks, keeping this many recent blocks' (at least 1,000).
    Pruned { keep: u64 },
}

/// What the miner uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MinerBackend {
    /// The test network's proof of work.
    Sha256,
    /// The real proof of work on the CPU (slow; for trying).
    Cpu,
    /// The real proof of work on the GPU.
    Gpu,
}

impl MinerBackend {
    pub fn name(self) -> &'static str {
        match self {
            MinerBackend::Sha256 => "sha256",
            MinerBackend::Cpu => "cpu",
            MinerBackend::Gpu => "gpu",
        }
    }

    pub fn parse(s: &str) -> Option<MinerBackend> {
        match s {
            "sha256" => Some(MinerBackend::Sha256),
            "cpu" => Some(MinerBackend::Cpu),
            "gpu" => Some(MinerBackend::Gpu),
            _ => None,
        }
    }

    /// Which backends the network can use (the proof of work differs).
    pub fn for_network(n: Network) -> &'static [MinerBackend] {
        match n {
            Network::Test => &[MinerBackend::Sha256],
            Network::Dev | Network::Gamma => &[MinerBackend::Gpu, MinerBackend::Cpu],
        }
    }
}

/// Where the miner works: for itself on a node of its own (a block found pays the miner's own address), or for a pool (the reward goes to the
/// POOL, which pays the miner by its own rules).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MiningMode {
    Solo,
    Pool,
}

impl MiningMode {
    pub fn name(self) -> &'static str {
        match self {
            MiningMode::Solo => "solo",
            MiningMode::Pool => "pool",
        }
    }

    pub fn parse(s: &str) -> Option<MiningMode> {
        match s {
            "solo" => Some(MiningMode::Solo),
            "pool" => Some(MiningMode::Pool),
            _ => None,
        }
    }
}

/// Whether `text` is 64 hexadecimal digits (a pool's public key).
pub fn is_key_hex(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub network: Network,
    pub node_kind: NodeKind,
    /// The node's control address (loopback only).
    pub control: SocketAddr,
    /// Peers to start from, `host:port`, one per line in the file as `seed = ...`.
    pub seeds: Vec<String>,
    /// Start the node's listening port (so others can connect), `ip:port`, or none. Only from the settings file (the screen has `inbound_port`):
    /// it listens but tells nobody where to find it.
    pub listen: Option<String>,
    /// "Let other nodes connect to me": the TCP port to accept them on (the router must forward it), or none. The node then also tells each peer
    /// the address it sees us at (`advertise = 0.0.0.0:PORT`), so a changing home IP needs nothing from the user. Wins over `listen`.
    pub inbound_port: Option<u16>,
    /// Do not start a node of our own: use one that is already running (or will be) at `control`, with its data in
    /// `data_dir`.
    pub external_node: bool,
    pub miner_backend: MinerBackend,
    pub miner_cores: usize,
    pub miner_gpu_device: usize,
    pub miner_gpu_auto_batch: bool,
    /// Seconds to wait after a block is found (default 5 on the test network, else 0).
    pub miner_pace_secs: u64,
    /// Which account the block rewards are paid to.
    pub miner_account: usize,
    /// Mining alone on this node (the default) or for a pool.
    pub mining_mode: MiningMode,
    /// The pool to mine for: `HOST:PORT`, or empty for the pool built into the program (if it has one).
    pub pool: String,
    /// That pool's public key, 64 hexadecimal digits (pinned: the miner refuses a pool that proves another). Empty for the built-in pool.
    pub pool_key: String,
    /// A name for this computer, shown to the pool (empty: the miner picks one).
    pub pool_worker: String,
    /// The folder with `tenerod` and `tenero-miner`; empty = next to this program.
    pub program_dir: Option<PathBuf>,
    /// Overrides for where things are; empty = the defaults under the app folder.
    pub data_dir: PathBuf,
    /// The wallet that is selected (opened next): one of the files in `wallets_dir`, or an older file elsewhere.
    pub wallet_file: PathBuf,
    /// Where the wallets are: one `NAME.twl` file each.
    pub wallets_dir: PathBuf,
    /// Where the single wallet of the first versions of the app lived (`wallet-<network>.twl` in the app folder): still listed if
    /// it is there, whichever wallet is selected. Not a setting; worked out from the app folder.
    pub legacy_wallet_file: PathBuf,
}

/// The app folder, from the environment: `%LOCALAPPDATA%\Tenero`, else `$HOME/.tenero`.
pub fn default_app_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("LOCALAPPDATA").filter(|d| !d.is_empty()) {
        return PathBuf::from(d).join("Tenero");
    }
    if let Some(h) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        return PathBuf::from(h).join(".tenero");
    }
    PathBuf::from(".tenero")
}

pub fn default_control(n: Network) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], n.default_control_port()))
}

/// The port the first release suggests for "let other nodes connect to me" (`docs/RUNNING.md`): the one the seed server of the network uses.
pub fn default_inbound_port(n: Network) -> u16 {
    match n {
        Network::Test => 18331,
        Network::Dev => 28333,
        Network::Gamma => 38353,
    }
}

impl Settings {
    /// The defaults for a network: a pruned node (the 0.3.0 default, `config::DEFAULT_PRUNE_KEEP`), no extra seeds, the first backend the network can use, the
    /// miner paying account 0, data and wallet under `app_dir`.
    pub fn defaults(app_dir: &Path, network: Network) -> Settings {
        Settings {
            network,
            node_kind: NodeKind::Pruned {
                keep: tenero_app::config::DEFAULT_PRUNE_KEEP,
            },
            control: default_control(network),
            seeds: Vec::new(),
            listen: None,
            inbound_port: None,
            external_node: false,
            miner_backend: MinerBackend::for_network(network)[0],
            miner_cores: 2,
            miner_gpu_device: 0,
            miner_gpu_auto_batch: false,
            miner_pace_secs: if network == Network::Test { 5 } else { 0 },
            miner_account: 0,
            mining_mode: MiningMode::Solo,
            pool: String::new(),
            pool_key: String::new(),
            pool_worker: String::new(),
            program_dir: None,
            data_dir: app_dir.join(network.name()).join("node"),
            wallet_file: app_dir.join(format!("wallet-{}.twl", network.name())),
            wallets_dir: app_dir.join(format!("wallets-{}", network.name())),
            legacy_wallet_file: app_dir.join(format!("wallet-{}.twl", network.name())),
        }
    }

    /// Reads settings text over the defaults. `app_dir` is where relative defaults come from.
    pub fn parse(app_dir: &Path, text: &str) -> Result<Settings, String> {
        // the network first, because the other defaults depend on it
        let mut network = Network::Test;
        let mut pairs: Vec<(String, String)> = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (k, v) = line
                .split_once('=')
                .ok_or_else(|| format!("line {}: expected `key = value`", i + 1))?;
            let (k, v) = (k.trim().to_string(), v.trim().to_string());
            if k == "network" {
                network = Network::parse(&v)
                    .ok_or_else(|| format!("network: `{v}` is not gamma, dev or test"))?;
            }
            pairs.push((k, v));
        }
        let mut s = Settings::defaults(app_dir, network);
        let mut seen = std::collections::BTreeSet::new();
        for (k, v) in pairs {
            if k != "seed" && !seen.insert(k.clone()) {
                return Err(format!("{k} is given twice"));
            }
            let bad = |why: &str| format!("{k}: `{v}` {why}");
            match k.as_str() {
                "network" => {}
                "node_kind" => {
                    s.node_kind = if v == "archive" {
                        NodeKind::Archive
                    } else if let Some(n) = v.strip_prefix("pruned:") {
                        let keep: u64 = n.parse().map_err(|_| bad("is not `pruned:<blocks>`"))?;
                        if keep < 1_000 {
                            return Err(bad("keeps too few blocks (at least 1000)"));
                        }
                        NodeKind::Pruned { keep }
                    } else {
                        return Err(bad("is not `archive` or `pruned:<blocks>`"));
                    }
                }
                "control" => {
                    let a: SocketAddr = v.parse().map_err(|_| bad("is not ip:port"))?;
                    if !a.ip().is_loopback() {
                        return Err(bad("must be a loopback address (this computer only)"));
                    }
                    s.control = a;
                }
                "seed" => {
                    if v.is_empty() || v.len() > 255 || v.chars().any(char::is_whitespace) {
                        return Err(bad("is not a host:port"));
                    }
                    s.seeds.push(v.clone());
                }
                "listen" => s.listen = (!v.is_empty()).then(|| v.clone()),
                "inbound_port" => {
                    s.inbound_port = if v.is_empty() {
                        None
                    } else {
                        let p: u16 = v.parse().map_err(|_| bad("is not a port (1 to 65535)"))?;
                        if p == 0 {
                            return Err(bad("is not a port (1 to 65535)"));
                        }
                        Some(p)
                    }
                }
                "external_node" => {
                    s.external_node = parse_bool(&v).ok_or_else(|| bad("is not yes or no"))?
                }
                "miner_backend" => {
                    s.miner_backend =
                        MinerBackend::parse(&v).ok_or_else(|| bad("is not sha256, cpu or gpu"))?
                }
                "miner_cores" => {
                    let n: usize = v.parse().map_err(|_| bad("is not a number"))?;
                    if n == 0 || n > MAX_CORES {
                        return Err(bad("must be 1 to 6"));
                    }
                    s.miner_cores = n;
                }
                "miner_gpu_device" => {
                    s.miner_gpu_device = v.parse().map_err(|_| bad("is not a number"))?
                }
                "miner_gpu_auto_batch" => {
                    s.miner_gpu_auto_batch =
                        parse_bool(&v).ok_or_else(|| bad("is not yes or no"))?
                }
                "miner_pace_secs" => {
                    s.miner_pace_secs = v.parse().map_err(|_| bad("is not a number"))?
                }
                "miner_account" => {
                    s.miner_account = v.parse().map_err(|_| bad("is not a number"))?
                }
                "mining_mode" => {
                    s.mining_mode =
                        MiningMode::parse(&v).ok_or_else(|| bad("is not solo or pool"))?
                }
                "pool" => {
                    if v.len() > 255
                        || v.chars().any(char::is_whitespace)
                        || (!v.is_empty() && !v.contains(':'))
                    {
                        return Err(bad("is not HOST:PORT (or empty for the built-in pool)"));
                    }
                    s.pool = v.clone();
                }
                "pool_key" => {
                    if !v.is_empty() && !is_key_hex(&v) {
                        return Err(bad("is not 64 hexadecimal digits"));
                    }
                    s.pool_key = v.to_ascii_lowercase();
                }
                "pool_worker" => {
                    if v.chars().count() > 32 || v.chars().any(char::is_control) {
                        return Err(bad("is more than 32 characters or has a control character"));
                    }
                    s.pool_worker = v.clone();
                }
                "program_dir" => s.program_dir = (!v.is_empty()).then(|| PathBuf::from(&v)),
                "data_dir" => s.data_dir = PathBuf::from(&v),
                "wallet_file" => s.wallet_file = PathBuf::from(&v),
                "wallets_dir" => s.wallets_dir = PathBuf::from(&v),
                other => return Err(format!("unknown setting `{other}`")),
            }
        }
        if !MinerBackend::for_network(network).contains(&s.miner_backend) {
            return Err(format!(
                "miner_backend: {} cannot mine the {} network",
                s.miner_backend.name(),
                network.name()
            ));
        }
        Ok(s)
    }

    /// What the user changed on the Settings screen, laid over the settings as they are NOW (`self`). `base` is the settings when the screen's draft
    /// was made and `draft` is the draft as edited: a field the user left alone (`draft == base`) keeps whatever `self` has, and only the fields
    /// they changed are taken from the draft. The draft can be many minutes old, and meanwhile other screens change the real settings (the wallet
    /// file when a wallet is created, picked or opened; the mining account on the Mining tab): sending the whole draft put the OLD wallet file back,
    /// which the core refuses while a wallet is unlocked, and it refused every other change with it ("settings do not apply unless the wallet is locked").
    pub fn with_changes(&self, base: &Settings, draft: &Settings) -> Settings {
        let mut s = self.clone();
        macro_rules! take {
            ($($f:ident),*) => { $( if draft.$f != base.$f { s.$f = draft.$f.to_owned(); } )* };
        }
        take!(
            network,
            node_kind,
            control,
            seeds,
            listen,
            inbound_port,
            external_node,
            miner_backend,
            miner_cores,
            miner_gpu_device,
            miner_gpu_auto_batch,
            miner_pace_secs,
            miner_account,
            mining_mode,
            pool,
            pool_key,
            pool_worker,
            program_dir,
            data_dir,
            wallet_file,
            wallets_dir,
            legacy_wallet_file
        );
        s
    }

    pub fn to_text(&self) -> String {
        let mut t = String::new();
        t.push_str("# Tenero wallet app settings. No secrets in this file.\n");
        t.push_str(&format!("network = {}\n", self.network.name()));
        t.push_str(&format!(
            "node_kind = {}\n",
            match self.node_kind {
                NodeKind::Archive => "archive".to_string(),
                NodeKind::Pruned { keep } => format!("pruned:{keep}"),
            }
        ));
        t.push_str(&format!("control = {}\n", self.control));
        for s in &self.seeds {
            t.push_str(&format!("seed = {s}\n"));
        }
        if let Some(l) = &self.listen {
            t.push_str(&format!("listen = {l}\n"));
        }
        if let Some(p) = self.inbound_port {
            t.push_str(&format!("inbound_port = {p}\n"));
        }
        t.push_str(&format!("external_node = {}\n", yes_no(self.external_node)));
        t.push_str(&format!("miner_backend = {}\n", self.miner_backend.name()));
        t.push_str(&format!("miner_cores = {}\n", self.miner_cores));
        t.push_str(&format!("miner_gpu_device = {}\n", self.miner_gpu_device));
        t.push_str(&format!(
            "miner_gpu_auto_batch = {}\n",
            yes_no(self.miner_gpu_auto_batch)
        ));
        t.push_str(&format!("miner_pace_secs = {}\n", self.miner_pace_secs));
        t.push_str(&format!("miner_account = {}\n", self.miner_account));
        t.push_str(&format!("mining_mode = {}\n", self.mining_mode.name()));
        if !self.pool.is_empty() {
            t.push_str(&format!("pool = {}\n", self.pool));
        }
        if !self.pool_key.is_empty() {
            t.push_str(&format!("pool_key = {}\n", self.pool_key));
        }
        if !self.pool_worker.is_empty() {
            t.push_str(&format!("pool_worker = {}\n", self.pool_worker));
        }
        if let Some(d) = &self.program_dir {
            t.push_str(&format!("program_dir = {}\n", d.display()));
        }
        t.push_str(&format!("data_dir = {}\n", self.data_dir.display()));
        t.push_str(&format!("wallet_file = {}\n", self.wallet_file.display()));
        t.push_str(&format!("wallets_dir = {}\n", self.wallets_dir.display()));
        t
    }

    /// Reads [`SETTINGS_FILE`] from the app folder; a missing file gives the `gamma` network's defaults.
    pub fn load(app_dir: &Path) -> Result<Settings, String> {
        let path = app_dir.join(SETTINGS_FILE);
        match std::fs::read_to_string(&path) {
            Ok(t) => Settings::parse(app_dir, &t).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(Settings::defaults(app_dir, Network::Gamma))
            }
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    pub fn save(&self, app_dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(app_dir)
            .map_err(|e| format!("cannot create {}: {e}", app_dir.display()))?;
        write_atomic(&app_dir.join(SETTINGS_FILE), self.to_text().as_bytes())
            .map_err(|e| format!("cannot write the settings: {e}"))
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// The most CPU threads the miner may be given (`tenero_miner::MAX_CORES`; CLAUDE.md rule 8).
const MAX_CORES: usize = 6;

fn parse_bool(v: &str) -> Option<bool> {
    match v {
        "yes" | "true" | "on" => Some(true),
        "no" | "false" | "off" => Some(false),
        _ => None,
    }
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}
