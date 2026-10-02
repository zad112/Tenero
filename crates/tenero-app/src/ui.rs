//! What the programs show on the screen (M10.1): one place that decides every line, so the output cannot drift from program to
//! program, and tests with the exact text of each kind of line.
//!
//! **The screen is the summary; the log file keeps the detail.** The log (`log.rs`) is unchanged: every line, with nonces and ids, to
//! a file. This module is for the person at the terminal: a banner, a status block that redraws in place when the output is a
//! terminal (plain lines when it is not: a file, a pipe, a service), events in plain words, and errors that say what to do next.
//!
//! * **ASCII only.** The older Windows console shows anything else as garbage, and a status line should not depend on a code page.
//! * **Colour only on a terminal**, never with `NO_COLOR`, and never when the terminal is `dumb` (`--color always` forces it).
//! * Nothing here is ever given a secret: it formats heights, counts, ids and addresses, as the log does.

use std::io::Write;
use std::sync::Mutex;

use tenero_miner::gpu_stats::{implied_read_bytes_per_sec, GpuReading, SLICE_BYTES};
use tenero_miner::rate::Rates;

/// The coin's ticker as the screen writes it (the owner chose `TNR`), kept in this one place.
pub const TICKER: &str = "TNR";

// ---- colour ------------------------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

impl ColorChoice {
    pub fn parse(s: &str) -> Option<ColorChoice> {
        match s {
            "auto" => Some(ColorChoice::Auto),
            "always" => Some(ColorChoice::Always),
            "never" => Some(ColorChoice::Never),
            _ => None,
        }
    }
}

/// Whether to use colour. `Always` and `Never` are what the user asked for and win over everything else. `Auto` is: not with
/// `NO_COLOR` set; yes with `CLICOLOR_FORCE`; otherwise only on a terminal that is not `dumb`.
pub fn color_enabled(
    choice: ColorChoice,
    is_terminal: bool,
    no_color: bool,
    clicolor_force: bool,
    term: Option<&str>,
) -> bool {
    match choice {
        ColorChoice::Never => false,
        ColorChoice::Always => true,
        ColorChoice::Auto => {
            if no_color {
                false
            } else if clicolor_force {
                true
            } else {
                is_terminal && term != Some("dumb")
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub color: bool,
}

impl Theme {
    fn paint(&self, code: &str, s: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    pub fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    pub fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    pub fn green(&self, s: &str) -> String {
        self.paint("32", s)
    }
    pub fn yellow(&self, s: &str) -> String {
        self.paint("33", s)
    }
    pub fn red(&self, s: &str) -> String {
        self.paint("31", s)
    }
    pub fn cyan(&self, s: &str) -> String {
        self.paint("36", s)
    }
}

// ---- numbers and times ---------------------------------------------------------------------------------------------------------

/// `1204` as `1,204`.
pub fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A span of time as people say it: `41s`, `3m 05s`, `2h 05m`, `3d 04h`.
pub fn format_duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {:02}s", secs / 60, secs % 60),
        3600..=86_399 => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
        _ => format!("{}d {:02}h", secs / 86_400, (secs % 86_400) / 3600),
    }
}

/// A size in bytes: `512 B`, `1.5 KiB`, `12.3 MiB`, `4.0 GiB`.
pub fn format_bytes(b: u64) -> String {
    if b < 1024 {
        return format!("{b} B");
    }
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64 / 1024.0;
    let mut i = 0;
    // a value that would print as 1024.0 is the next unit's 1.0
    while i + 1 < UNITS.len() && (v * 10.0).round() / 10.0 >= 1024.0 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNITS[i])
}

/// An amount of coins in units, as the wallet writes it, with the ticker: `12.3 TNR`.
pub fn format_amount(units: u64) -> String {
    format!("{} {TICKER}", tenero_wallet::amount::format_coins(units))
}

/// `HH:MM:SS` (UTC) for a time in seconds since 1970.
pub fn clock(secs: u64) -> String {
    let r = secs % 86_400;
    format!("{:02}:{:02}:{:02}", r / 3600, (r % 3600) / 60, r % 60)
}

// ---- sync progress ---------------------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SyncProgress {
    /// Our height and the height of the best peer.
    pub current: u64,
    pub target: u64,
    /// Blocks a second over the last half minute or so, if it can be told yet.
    pub rate: Option<f64>,
}

impl SyncProgress {
    /// Whole percent, never 100 until we are there.
    pub fn percent(&self) -> u64 {
        if self.target == 0 || self.current >= self.target {
            return 100;
        }
        (self.current * 100 / self.target).min(99)
    }

    /// Seconds left at the current rate (rounded up), if there is a rate.
    pub fn eta_secs(&self) -> Option<u64> {
        let rate = self.rate?;
        if rate <= 0.0 || self.current >= self.target {
            return None;
        }
        Some(((self.target - self.current) as f64 / rate).ceil() as u64)
    }

    pub fn describe(&self) -> String {
        let mut s = format!(
            "syncing {} of {} ({}%)",
            group_digits(self.current),
            group_digits(self.target),
            self.percent()
        );
        match (self.rate, self.eta_secs()) {
            (Some(r), Some(eta)) => {
                let rate = if r >= 10.0 {
                    format!("{r:.0}")
                } else {
                    format!("{r:.1}")
                };
                s.push_str(&format!(
                    " | {rate} blocks/s | {} left",
                    format_duration(eta)
                ));
            }
            _ => s.push_str(" | estimating the time left"),
        }
        s
    }
}

// ---- the status block ----------------------------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MiningStatus {
    /// What is mining, in words (`cpu, 2 threads`).
    pub backend: String,
    pub blocks_found: u64,
    pub blocks_accepted: u64,
    /// Waiting for the node to finish syncing.
    pub paused: bool,
    /// Attempts a second over 10 s, 60 s, 15 min and the run (searching time only; see `tenero_miner::rate`).
    pub rates: Rates,
    /// The card's health, when mining on a GPU and NVML can be read.
    pub gpu: Option<GpuReading>,
}

/// The rates in words: `10s 35,012 | 60s 34,980 | 15m - | avg 34,990` (a window with no figure yet is `-`; nothing at all is `starting`).
pub fn rates_text(r: &Rates) -> String {
    if r.s10.is_none() && r.s60.is_none() && r.m15.is_none() && r.average.is_none() {
        return "starting".to_string();
    }
    let f = |v: Option<f64>| v.map_or("-".to_string(), |v| group_digits(v.round() as u64));
    format!(
        "10s {} | 60s {} | 15m {} | avg {}",
        f(r.s10),
        f(r.s60),
        f(r.m15),
        f(r.average)
    )
}

/// The rows about the card: its health, what holds it back, and the memory reads the work implies. Nothing for a figure the card does
/// not report; no rows at all when there is no reading. **The memory reads are worked out from the attempt rate, not measured**, and say so.
pub fn gpu_rows(g: &GpuReading, rates: &Rates, t: &Theme) -> Vec<String> {
    let mut health = vec![];
    if let Some(v) = g.temp_c {
        health.push(format!("{v} C"));
    }
    if let Some(v) = g.power_w {
        health.push(format!("{} W", v.round() as u64));
    }
    if let Some(v) = g.fan_pct {
        health.push(format!("fan {v}%"));
    }
    if let Some(v) = g.core_mhz {
        health.push(format!("core {} MHz", group_digits(u64::from(v))));
    }
    if let Some(v) = g.mem_mhz {
        health.push(format!("mem {} MHz", group_digits(u64::from(v))));
    }
    let mut rows = vec![];
    if !health.is_empty() {
        rows.push(cut(&format!("  gpu      {}", health.join(" | "))));
    }
    if let Some(why) = g.limited_by {
        rows.push(cut(&format!(
            "  limited  {}",
            t.yellow(&format!("the driver is holding the clocks down: {why}"))
        )));
    }
    let mut memory = vec![];
    if let (Some(used), Some(total)) = (g.mem_used_mib, g.mem_total_mib) {
        memory.push(format!(
            "{} of {} GiB used",
            format_gib(used),
            format_gib(total)
        ));
    }
    if let Some(v) = g.mem_busy_pct {
        memory.push(format!("controller busy {v}%"));
    }
    if !memory.is_empty() {
        rows.push(cut(&format!("  memory   {}", memory.join(" | "))));
    }
    if let Some(rate) = rates.s10.or(rates.average) {
        let gb = implied_read_bytes_per_sec(rate, SLICE_BYTES) / 1e9;
        rows.push(cut(&format!(
            "  reads    ~{} GB/s implied by the rate (an estimate, not measured)",
            group_digits(gb.round() as u64)
        )));
    }
    rows
}

/// MiB as GiB with one decimal (`15.9`).
fn format_gib(mib: u64) -> String {
    let tenths = (mib * 10 + 512) / 1024;
    format!("{}.{}", tenths / 10, tenths % 10)
}

/// The row of rates, with a mark when the miner is not searching at this moment (paused, building a dataset, waiting for a job).
fn rates_row(r: &Rates) -> String {
    format!(
        "  rate/s   {}{}",
        if r.searching { "" } else { "(idle) " },
        rates_text(r)
    )
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeStatus {
    pub height: u64,
    /// The first eight hex digits of the tip id.
    pub tip: String,
    pub last_block_age_secs: u64,
    pub peers_in: usize,
    pub peers_out: usize,
    pub out_groups: usize,
    pub sync: Option<SyncProgress>,
    pub mempool: usize,
    pub uptime_secs: u64,
    pub disk_bytes: Option<u64>,
    pub pruned_below: u64,
    /// The names of the alarms that are raised (`stale-tip`, ...).
    pub alarms: Vec<String>,
    pub mining: Option<MiningStatus>,
}

/// The widest a line of the block is: it fits an 80-column window with room to spare.
pub const MAX_LINE: usize = 78;

/// A line cut to `MAX_LINE` visible characters: colour codes take no room, and a cut never falls inside one (a colour left open is
/// closed).
fn cut(s: &str) -> String {
    let visible = |s: &str| -> usize {
        let mut n = 0;
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            if c == '\x1b' && it.peek() == Some(&'[') {
                for d in it.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                n += 1;
            }
        }
        n
    };
    if visible(s) <= MAX_LINE {
        return s.to_string();
    }
    let keep = MAX_LINE - 3;
    let (mut out, mut n, mut colored) = (String::new(), 0, false);
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' && it.peek() == Some(&'[') {
            out.push(c);
            let mut code = String::new();
            for d in it.by_ref() {
                out.push(d);
                code.push(d);
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
            colored = code != "[0m";
        } else if n < keep {
            out.push(c);
            n += 1;
        } else {
            break;
        }
    }
    if colored {
        out.push_str("\x1b[0m");
    }
    out.push_str("...");
    out
}

/// The status block: a handful of lines that are redrawn in place. Colour only marks (a sync in progress, an alarm, a pause); the
/// words alone say everything.
pub fn render_status_block(s: &NodeStatus, t: &Theme) -> Vec<String> {
    let mut lines =
        vec![t.dim("-- status ------------------------------------------------------------")];
    lines.push(cut(&format!(
        "  chain    height {} ({}) | last block {} ago",
        group_digits(s.height),
        s.tip,
        format_duration(s.last_block_age_secs)
    )));
    let sync = match &s.sync {
        Some(p) => t.yellow(&p.describe()),
        None => t.green("in sync"),
    };
    lines.push(cut(&format!("  sync     {sync}")));
    lines.push(cut(&format!(
        "  peers    {} (in {}, out {}) | outbound in {} network group{}",
        s.peers_in + s.peers_out,
        s.peers_in,
        s.peers_out,
        s.out_groups,
        if s.out_groups == 1 { "" } else { "s" }
    )));
    let mut third = format!(
        "  node     mempool {} | up {}",
        group_digits(s.mempool as u64),
        format_duration(s.uptime_secs)
    );
    if let Some(d) = s.disk_bytes {
        third.push_str(&format!(" | disk {}", format_bytes(d)));
    }
    if s.pruned_below > 0 {
        third.push_str(&format!(" | pruned below {}", group_digits(s.pruned_below)));
    }
    lines.push(cut(&third));
    if let Some(m) = &s.mining {
        let state = if m.paused {
            t.yellow("paused")
        } else {
            t.green("mining")
        };
        lines.push(cut(&format!(
            "  mining   {state} ({}) | {} found, {} in the chain",
            m.backend, m.blocks_found, m.blocks_accepted
        )));
        lines.push(cut(&rates_row(&m.rates)));
        if let Some(g) = &m.gpu {
            lines.extend(gpu_rows(g, &m.rates, t));
        }
    }
    let alarms = if s.alarms.is_empty() {
        "none".to_string()
    } else {
        t.red(&s.alarms.join(", "))
    };
    lines.push(cut(&format!("  alarms   {alarms}")));
    lines
}

/// The same facts on one plain line, for output that is not a terminal (and for the log).
pub fn render_status_line(s: &NodeStatus) -> String {
    let sync = match &s.sync {
        Some(p) => p.describe(),
        None => "in sync".to_string(),
    };
    let mut line = format!(
        "height {} ({}) | {sync} | peers {} (in {}, out {}) | mempool {} | up {}",
        group_digits(s.height),
        s.tip,
        s.peers_in + s.peers_out,
        s.peers_in,
        s.peers_out,
        s.mempool,
        format_duration(s.uptime_secs)
    );
    if let Some(m) = &s.mining {
        line.push_str(&format!(
            " | mining {}: {} found, {} in the chain{}",
            m.backend,
            m.blocks_found,
            m.blocks_accepted,
            if m.paused { " (paused)" } else { "" }
        ));
        line.push_str(&format!(" | rate/s {}", rates_text(&m.rates)));
        if let Some(g) = &m.gpu {
            line.push_str(&gpu_plain(g));
        }
    }
    if !s.alarms.is_empty() {
        line.push_str(&format!(" | ALARMS: {}", s.alarms.join(", ")));
    }
    line
}

// ---- the miner's status block -------------------------------------------------------------------------------------------------------

/// How the miner stands with its node.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NodeLink {
    /// Not (yet) connected, or lost: trying again.
    #[default]
    Down,
    Connected,
    /// Connected, but the node is catching up, so mining waits.
    Syncing,
}

/// What the separate miner program shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MinerStatus {
    pub backend: String,
    pub link: NodeLink,
    pub node_height: u64,
    /// Attempts a second over 10 s, 60 s, 15 min and the run.
    pub rates: Rates,
    pub gpu: Option<GpuReading>,
    pub found: u64,
    pub accepted: u64,
    pub lost_race: u64,
    pub refused: u64,
    pub uptime_secs: u64,
}

pub fn render_miner_block(s: &MinerStatus, t: &Theme) -> Vec<String> {
    let node = match s.link {
        NodeLink::Connected => t.green(&format!(
            "connected (height {})",
            group_digits(s.node_height)
        )),
        NodeLink::Syncing => t.yellow(&format!(
            "syncing (height {}): mining waits",
            group_digits(s.node_height)
        )),
        NodeLink::Down => t.red("not reachable: trying again"),
    };
    vec![
        t.dim("-- status ------------------------------------------------------------"),
        cut(&format!("  node     {node}")),
        cut(&format!("  mining   {}", s.backend)),
        cut(&rates_row(&s.rates)),
    ]
    .into_iter()
    .chain(s.gpu.iter().flat_map(|g| gpu_rows(g, &s.rates, t)))
    .chain([
        cut(&format!(
            "  blocks   {} found | {} in the chain | {} lost a race | {} refused",
            s.found, s.accepted, s.lost_race, s.refused
        )),
        cut(&format!("  up       {}", format_duration(s.uptime_secs))),
    ])
    .collect()
}

/// The card in a plain line: ` | gpu 62 C 212 W 94% busy` (what is there; nothing if nothing is).
fn gpu_plain(g: &GpuReading) -> String {
    let mut parts = vec![];
    if let Some(v) = g.temp_c {
        parts.push(format!("{v} C"));
    }
    if let Some(v) = g.power_w {
        parts.push(format!("{} W", v.round() as u64));
    }
    if let Some(why) = g.limited_by {
        parts.push(format!("limited by {why}"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" | gpu {}", parts.join(", "))
    }
}

pub fn render_miner_line(s: &MinerStatus) -> String {
    let link = match s.link {
        NodeLink::Connected => format!("node connected (height {})", group_digits(s.node_height)),
        NodeLink::Syncing => format!(
            "node syncing (height {}), mining waits",
            group_digits(s.node_height)
        ),
        NodeLink::Down => "node not reachable".to_string(),
    };
    let rate = format!(
        "rate/s {}{}",
        rates_text(&s.rates),
        s.gpu.as_ref().map(gpu_plain).unwrap_or_default()
    );
    format!(
        "{link} | {} | {rate} | blocks: {} found, {} in the chain, {} lost a race, {} refused | up {}",
        s.backend,
        s.found,
        s.accepted,
        s.lost_race,
        s.refused,
        format_duration(s.uptime_secs)
    )
}

// ---- the banner ----------------------------------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Banner {
    /// `node` or `miner`.
    pub role: String,
    pub version: String,
    /// `test` or `dev`, and what it means.
    pub network: String,
    pub network_note: String,
    /// Extra lines (where the data is, what it listens on, where the log is).
    pub details: Vec<String>,
}

pub fn render_banner(b: &Banner, t: &Theme) -> Vec<String> {
    let mut v = vec![
        t.bold(&format!(
            "TENERO {} {} | network: {} ({})",
            b.role, b.version, b.network, b.network_note
        )),
        t.yellow("EXPERIMENTAL and UNAUDITED. Nothing on this network has any value."),
    ];
    v.extend(b.details.iter().map(|d| cut(d)));
    v.push(t.dim("Times are UTC. Press Ctrl-C to stop cleanly."));
    v
}

// ---- events --------------------------------------------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The node is up: where it listens.
    Listening {
        p2p: Option<String>,
        control: String,
    },
    /// A block this program mined is in the chain.
    BlockMined {
        height: u64,
        secs: f64,
        reward: Option<u64>,
    },
    /// A block this program mined lost a race to another block.
    BlockLostRace {
        height: u64,
    },
    /// A block this program mined was refused.
    BlockRefused {
        height: u64,
        why: String,
    },
    Synced {
        height: u64,
    },
    AlarmBegan(String),
    AlarmEnded(String),
    MiningPaused,
    MiningResumed,
    Connected(String),
    Lost(String),
    Warn(String),
    /// What went wrong and, if known, what to do about it.
    Error {
        what: String,
        hint: Option<String>,
    },
    ShuttingDown,
    Stopped {
        height: u64,
        tip: String,
    },
    /// A plain note (the summary a program prints as it ends).
    Info(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Good,
    Warn,
    Error,
}

impl Event {
    pub fn severity(&self) -> Severity {
        match self {
            Event::BlockMined { .. } => Severity::Good,
            Event::BlockLostRace { .. }
            | Event::AlarmBegan(_)
            | Event::Lost(_)
            | Event::Warn(_)
            | Event::MiningPaused => Severity::Warn,
            Event::BlockRefused { .. } | Event::Error { .. } => Severity::Error,
            _ => Severity::Info,
        }
    }
}

/// The text of an event, without the time. Errors take a second line that says what to do.
pub fn render_event(e: &Event, t: &Theme) -> Vec<String> {
    match e {
        Event::Listening { p2p, control } => vec![match p2p {
            Some(p) => format!("listening for peers on {p}; control on {control}"),
            None => format!("not listening for peers (outbound only); control on {control}"),
        }],
        Event::BlockMined {
            height,
            secs,
            reward,
        } => vec![t.green(&format!(
            "block {} mined in {:.1} s{}",
            group_digits(*height),
            secs,
            reward
                .map(|r| format!(", reward {}", format_amount(r)))
                .unwrap_or_default()
        ))],
        Event::BlockLostRace { height } => vec![t.yellow(&format!(
            "block {} was found but another block won the race: no reward for it",
            group_digits(*height)
        ))],
        Event::BlockRefused { height, why } => vec![
            t.red(&format!(
                "block {} was refused by the node: {why}",
                group_digits(*height)
            )),
            "  what to do: this should not happen; keep the log file and report it".to_string(),
        ],
        Event::Synced { height } => vec![t.green(&format!(
            "synced: the chain is up to date at height {}",
            group_digits(*height)
        ))],
        Event::AlarmBegan(text) => vec![t.red(&format!("WARNING: {text}"))],
        Event::AlarmEnded(kind) => vec![format!("alarm ended: {kind}")],
        Event::MiningPaused => vec![t.yellow("mining paused: the node is syncing")],
        Event::MiningResumed => vec!["mining resumed".to_string()],
        Event::Connected(what) => vec![format!("connected to {what}")],
        Event::Lost(what) => vec![t.yellow(&format!("lost {what}; trying again"))],
        Event::Warn(text) => vec![t.yellow(&format!("warning: {text}"))],
        Event::Error { what, hint } => {
            let mut v = vec![t.red(&format!("error: {what}"))];
            if let Some(h) = hint {
                v.push(format!("  what to do: {h}"));
            }
            v
        }
        Event::ShuttingDown => vec!["shutting down...".to_string()],
        Event::Info(text) => vec![text.clone()],
        Event::Stopped { height, tip } => vec![format!(
            "stopped at height {} ({tip})",
            group_digits(*height)
        )],
    }
}

/// What the screen says of something a miner reported (`None` for what is only counted).
pub fn miner_event_to_ui(e: &tenero_miner::MinerEvent) -> Option<Event> {
    use tenero_miner::MinerEvent as M;
    Some(match e {
        M::Started { .. } => return None,
        M::InChain {
            height,
            secs,
            reward,
        } => Event::BlockMined {
            height: *height,
            secs: *secs,
            reward: Some(*reward),
        },
        M::LostRace { height } => Event::BlockLostRace { height: *height },
        M::Refused { height } => Event::BlockRefused {
            height: *height,
            why: "the block or its proof of work is wrong".to_string(),
        },
        M::Paused => Event::MiningPaused,
        M::Resumed => Event::MiningResumed,
        M::BackendFailed { why } => error_event(&format!(
            "the mining backend failed and mining has stopped: {why}"
        )),
        M::NodeConnected => Event::Connected("the node".to_string()),
        M::NodeLost { .. } => Event::Lost("the node".to_string()),
    })
}

/// What to do about an error, for the failures people meet first. `None` if nothing useful can be said.
pub fn hint_for(error: &str) -> Option<String> {
    let e = error.to_lowercase();
    let h = if e.contains("address already in use")
        || e.contains("only one usage of each socket address")
        || e.contains("cannot listen")
    {
        "another program (perhaps another node) is using that port: stop it, or give this node a different `listen` or `control` address"
    } else if e.contains("being used by another process")
        || e.contains("already open")
        || e.contains("could not acquire lock")
        || e.contains("database is locked")
    {
        "another node may be running on this data folder: stop it first (`tenerod stop --data FOLDER`) or use a different folder"
    } else if e.contains("cookie") {
        "the node writes `control.cookie` when it starts: start the node first, and check that --data is the node's own folder"
    } else if e.contains("cannot reach the node") || e.contains("connection refused") {
        "is the node running? start it with `tenerod`, and check --control and --data"
    } else if e.contains("invalid key") || e.contains("--address") {
        "an address starts with `tni1`: make one with `tenero-wallet`"
    } else if e.contains("different chain") || e.contains("wrong chain") {
        "this data folder belongs to another network: use a new folder, or the other network name"
    } else {
        return None;
    };
    Some(h.to_string())
}

/// An error as an event: the hint is looked up from the text.
pub fn error_event(what: &str) -> Event {
    Event::Error {
        what: what.to_string(),
        hint: hint_for(what),
    }
}

// ---- the screen ----------------------------------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verbosity {
    /// Only warnings and errors.
    Quiet,
    Normal,
    /// Also every line of the log, as it is written to the log file.
    Verbose,
}

struct State {
    out: Box<dyn Write + Send>,
    /// The status block as last drawn (interactive mode), and how many lines it takes.
    block: Vec<String>,
    /// When a plain status line was last written (plain mode).
    last_plain: Option<u64>,
}

/// The screen: where banner, events and the status block are written. One at a time (a lock), so lines of two threads do not mix.
pub struct Screen {
    state: Mutex<State>,
    theme: Theme,
    /// A terminal that redraws in place; otherwise plain lines, each with its time.
    interactive: bool,
    verbosity: Verbosity,
    /// Plain mode: the seconds between status lines.
    plain_every: u64,
    now: Box<dyn Fn() -> u64 + Send + Sync>,
}

impl Screen {
    /// `now` gives the time in seconds since 1970 (a parameter so that tests can say what time it is).
    pub fn new(
        out: Box<dyn Write + Send>,
        interactive: bool,
        theme: Theme,
        verbosity: Verbosity,
        plain_every: u64,
        now: Box<dyn Fn() -> u64 + Send + Sync>,
    ) -> Screen {
        Screen {
            state: Mutex::new(State {
                out,
                block: Vec::new(),
                last_plain: None,
            }),
            theme,
            interactive,
            verbosity,
            plain_every: plain_every.max(1),
            now,
        }
    }

    /// The screen for standard error: a terminal if it is one (colour as the environment and `choice` say), else plain lines.
    pub fn for_stderr(choice: ColorChoice, verbosity: Verbosity, plain_every: u64) -> Screen {
        use std::io::IsTerminal;
        let is_tty = std::io::stderr().is_terminal();
        // the Windows console needs to be told to understand escape codes; if it cannot, it is plain
        let ansi_ok = if is_tty {
            anstyle_query::windows::enable_ansi_colors() != Some(false)
        } else {
            false
        };
        let color = ansi_ok
            && color_enabled(
                choice,
                is_tty,
                anstyle_query::no_color(),
                anstyle_query::clicolor_force(),
                std::env::var("TERM").ok().as_deref(),
            )
            || (!is_tty && choice == ColorChoice::Always);
        Screen::new(
            Box::new(std::io::stderr()),
            is_tty && ansi_ok,
            Theme { color },
            verbosity,
            plain_every,
            Box::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            }),
        )
    }

    pub fn theme(&self) -> Theme {
        self.theme
    }

    pub fn verbosity(&self) -> Verbosity {
        self.verbosity
    }

    pub fn is_interactive(&self) -> bool {
        self.interactive
    }

    /// Writes `lines` above the status block (the block is wiped, the lines written, the block drawn again).
    fn write_lines(&self, st: &mut State, lines: &[String]) {
        let mut buf = String::new();
        if self.interactive && !st.block.is_empty() {
            buf.push_str(&format!("\x1b[{}A\r\x1b[J", st.block.len()));
        }
        for l in lines {
            buf.push_str(l);
            buf.push('\n');
        }
        if self.interactive {
            for l in &st.block {
                buf.push_str(l);
                buf.push('\n');
            }
        }
        let _ = st.out.write_all(buf.as_bytes());
        let _ = st.out.flush();
    }

    /// With the time in front, in plain mode (a terminal's own clock is the person's; a file has none).
    fn stamped(&self, lines: Vec<String>) -> Vec<String> {
        if self.interactive {
            return lines;
        }
        let t = clock((self.now)());
        lines.into_iter().map(|l| format!("{t}  {l}")).collect()
    }

    pub fn banner(&self, b: &Banner) {
        if self.verbosity == Verbosity::Quiet {
            return;
        }
        let lines = render_banner(b, &self.theme);
        if let Ok(mut st) = self.state.lock() {
            self.write_lines(&mut st, &lines);
        }
    }

    /// An event; in quiet mode only warnings and errors are shown.
    pub fn event(&self, e: &Event) {
        if self.verbosity == Verbosity::Quiet && e.severity() < Severity::Warn {
            return;
        }
        let lines = self.stamped(render_event(e, &self.theme));
        if let Ok(mut st) = self.state.lock() {
            self.write_lines(&mut st, &lines);
        }
    }

    /// A line of the log, shown as it is written to the file: only in verbose mode.
    pub fn detail(&self, line: &str) {
        if self.verbosity != Verbosity::Verbose {
            return;
        }
        let lines = vec![self.theme.dim(line)];
        if let Ok(mut st) = self.state.lock() {
            self.write_lines(&mut st, &lines);
        }
    }

    /// The node's status (see `show_status`).
    pub fn status(&self, s: &NodeStatus) {
        self.show_status(render_status_block(s, &self.theme), render_status_line(s));
    }

    /// The miner's status (see `show_status`).
    pub fn miner_status(&self, s: &MinerStatus) {
        self.show_status(render_miner_block(s, &self.theme), render_miner_line(s));
    }

    /// A status: `block` redrawn in place on a terminal, or `plain` as one line every `plain_every` seconds when plain. Nothing in
    /// quiet mode.
    pub fn show_status(&self, block: Vec<String>, plain: String) {
        if self.verbosity == Verbosity::Quiet {
            return;
        }
        let now = (self.now)();
        let Ok(mut st) = self.state.lock() else {
            return;
        };
        if self.interactive {
            let mut buf = String::new();
            if !st.block.is_empty() {
                buf.push_str(&format!("\x1b[{}A\r\x1b[J", st.block.len()));
            }
            for l in &block {
                buf.push_str(l);
                buf.push('\n');
            }
            st.block = block;
            let _ = st.out.write_all(buf.as_bytes());
            let _ = st.out.flush();
        } else {
            if st
                .last_plain
                .is_some_and(|t| now.saturating_sub(t) < self.plain_every)
            {
                return;
            }
            st.last_plain = Some(now);
            let line = format!("{}  status: {plain}\n", clock(now));
            let _ = st.out.write_all(line.as_bytes());
            let _ = st.out.flush();
        }
    }
}
