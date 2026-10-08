//! The seed health check (M9, threat model C1 and K; `docs/SEED_POLICY.md`): connects to each seed the way a new node would, and says
//! which of them are fit to be on a list.
//!
//! **What it checks, for each seed:** it can be reached; the encrypted handshake succeeds for THIS chain; it says hello in the same
//! protocol version; it answers an address request with enough routable addresses in several network groups; it is not far behind the
//! other seeds; it is not pruned (a seed should be able to serve the whole chain to a new node). **For the list as a whole:** at least
//! three seeds, no two in one network group (a new node counts a group once), no address twice.
//!
//! **What it cannot tell you:** whether a seed is HONEST. A seed that answers promptly with real-looking addresses and the right tip passes.
//! The policy's protection is a mostly-honest list of independent operators, and nothing here can check who runs a seed. A check is also
//! one moment: use `--history` to keep results and see how often a seed was up.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use tenero_net::addrbook::{group_of, is_routable, peer_addr_to_string};
use tenero_net::noise::{
    handshake_initiator, prologue, NodeKey, NoiseError, SecureReader, SecureWriter,
};
use tenero_net::wire::{encode, FrameDecoder};
use tenero_net::{Hello, Message, PROTOCOL_VERSION};

/// How a probe is made.
#[derive(Clone, Debug)]
pub struct ProbeConfig {
    /// The longest each step (connect, handshake, hello) may take.
    pub timeout: Duration,
    /// How long to collect the answer to the address request (it ends early once an answer has come and nothing more follows).
    pub addr_wait: Duration,
}

impl Default for ProbeConfig {
    fn default() -> ProbeConfig {
        ProbeConfig {
            timeout: Duration::from_secs(8),
            addr_wait: Duration::from_secs(3),
        }
    }
}

/// What one connection to one seed showed.
#[derive(Clone, Debug, Default)]
pub struct Probe {
    /// The seed as it was given (`host:port`).
    pub seed: String,
    /// The last step reached: `resolve`, `connect`, `handshake`, `hello` or `addrs` (the last means all of them were done).
    pub stage: &'static str,
    /// Why it stopped, if it did.
    pub error: Option<String>,
    pub connect_ms: u64,
    pub handshake_ms: u64,
    pub hello_ms: u64,
    pub hello: Option<Hello>,
    /// The distinct addresses it gave (`ip:port`).
    pub addrs: Vec<String>,
}

impl Probe {
    fn failed(mut self, stage: &'static str, why: impl ToString) -> Probe {
        self.stage = stage;
        self.error = Some(why.to_string());
        self
    }

    /// Milliseconds from the start of the connection to its hello.
    pub fn latency_ms(&self) -> u64 {
        self.connect_ms + self.handshake_ms + self.hello_ms
    }
}

/// What a failed read or handshake means, in words: a seed that hangs up or stays silent is the common case, and "failed to fill whole
/// buffer" tells nobody that.
fn describe(e: &NoiseError) -> String {
    use std::io::ErrorKind::*;
    match e {
        NoiseError::Io(io) => match io.kind() {
            UnexpectedEof | ConnectionReset | ConnectionAborted | BrokenPipe => {
                "the seed closed the connection".to_string()
            }
            TimedOut | WouldBlock => "timed out waiting for the seed".to_string(),
            _ => e.to_string(),
        },
        _ => e.to_string(),
    }
}

struct Wire {
    stream: TcpStream,
    r: SecureReader,
    w: SecureWriter,
    dec: FrameDecoder,
}

impl Wire {
    fn send(&mut self, m: &Message) -> Result<(), String> {
        use std::io::Write;
        let bytes = encode(m).map_err(|e| e.to_string())?;
        let sealed = self.w.seal(&bytes).map_err(|e| e.to_string())?;
        self.stream.write_all(&sealed).map_err(|e| e.to_string())
    }

    /// The next message, an error if the connection ended, or `Ok(None)` if nothing came within `wait`.
    fn recv(&mut self, wait: Duration) -> Result<Option<Message>, String> {
        let end = Instant::now() + wait;
        loop {
            if let Some(m) = self.dec.next_message().map_err(|e| e.to_string())? {
                return Ok(Some(m));
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            self.stream
                .set_read_timeout(Some(left))
                .map_err(|e| e.to_string())?;
            match self.r.read_chunk(&mut self.stream) {
                Ok(chunk) => self.dec.push(&chunk),
                Err(tenero_net::noise::NoiseError::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    return Ok(None)
                }
                Err(e) => return Err(describe(&e)),
            }
        }
    }
}

/// Connects to `seed` (`host:port`) as a new node of the chain `chain_id` would, reads its hello and asks it for addresses.
pub fn probe(seed: &str, chain_id: [u8; 32], cfg: &ProbeConfig) -> Probe {
    let mut p = Probe {
        seed: seed.to_string(),
        stage: "resolve",
        ..Probe::default()
    };
    let addr: SocketAddr = match seed.to_socket_addrs() {
        Ok(mut it) => match it.next() {
            Some(a) => a,
            None => return p.failed("resolve", "the name has no address"),
        },
        Err(e) => return p.failed("resolve", e),
    };
    let t = Instant::now();
    let stream = match TcpStream::connect_timeout(&addr, cfg.timeout) {
        Ok(s) => s,
        Err(e) => return p.failed("connect", e),
    };
    p.connect_ms = t.elapsed().as_millis() as u64;
    p.stage = "connect";
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(cfg.timeout));
    let _ = stream.set_write_timeout(Some(cfg.timeout));
    let mut stream = stream;
    let key = NodeKey::generate();
    let t = Instant::now();
    let secured =
        match handshake_initiator(&mut stream, &key, &prologue(PROTOCOL_VERSION, &chain_id)) {
            Ok(s) => s,
            Err(e) => {
                return p.failed(
                    "handshake",
                    format!(
                        "{} (a node of another chain or protocol version fails here)",
                        describe(&e)
                    ),
                )
            }
        };
    p.handshake_ms = t.elapsed().as_millis() as u64;
    p.stage = "handshake";
    let mut wire = Wire {
        stream,
        r: secured.reader,
        w: secured.writer,
        dec: FrameDecoder::new(),
    };
    let t = Instant::now();
    let nonce = u64::from_le_bytes(key.public()[..8].try_into().unwrap()) | 1;
    let ours = Hello {
        version: PROTOCOL_VERSION,
        chain_id,
        tip_height: 0,
        cumulative_work: [0; 32],
        tip_id: [0; 32],
        pruned_below: 0,
        nonce,
    };
    if let Err(e) = wire.send(&Message::Hello(ours)) {
        return p.failed("hello", e);
    }
    let end = Instant::now() + cfg.timeout;
    let hello = loop {
        let left = end.saturating_duration_since(Instant::now());
        match wire.recv(left) {
            Ok(Some(Message::Hello(h))) => break h,
            Ok(Some(Message::Ping(n))) => {
                let _ = wire.send(&Message::Pong(n));
            }
            Ok(Some(_)) => {}
            Ok(None) => return p.failed("hello", "no hello within the time allowed"),
            Err(e) => return p.failed("hello", e),
        }
    };
    p.hello_ms = t.elapsed().as_millis() as u64;
    p.hello = Some(hello);
    p.stage = "hello";
    if let Err(e) = wire.send(&Message::GetAddrs) {
        return p.failed("addrs", e);
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let end = Instant::now() + cfg.addr_wait;
    let mut got_answer = false;
    loop {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        // once an answer has come, a short quiet ends the wait
        let wait = if got_answer {
            left.min(Duration::from_millis(400))
        } else {
            left
        };
        match wire.recv(wait) {
            Ok(Some(Message::Addrs { addrs })) => {
                got_answer = true;
                seen.extend(addrs.iter().filter_map(peer_addr_to_string));
            }
            Ok(Some(Message::Ping(n))) => {
                let _ = wire.send(&Message::Pong(n));
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    p.addrs = seen.into_iter().collect();
    p.stage = "addrs";
    p
}

// ---- judging ------------------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Ok,
    Warn,
    Fail,
}

impl Severity {
    pub fn word(self) -> &'static str {
        match self {
            Severity::Ok => "OK",
            Severity::Warn => "WARN",
            Severity::Fail => "FAIL",
        }
    }

    /// The exit code of the tool: 0 all well, 1 warnings, 2 a failure.
    pub fn exit_code(self) -> i32 {
        match self {
            Severity::Ok => 0,
            Severity::Warn => 1,
            Severity::Fail => 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct EvalConfig {
    /// The chain the seeds must be on.
    pub chain_id: [u8; 32],
    /// A seed this many blocks behind the middle of the answering seeds fails; this many ahead is a warning.
    pub max_lag_blocks: u64,
    /// Fewer addresses than this in an answer is a warning.
    pub min_addrs: usize,
    /// Fewer seeds than this (listed, or answering) is a warning: `SEED_POLICY.md` wants at least three operators.
    pub min_seeds: usize,
    /// More than this many milliseconds to reach the hello is a warning.
    pub slow_ms: u64,
    /// A private network (the test network on loopback): addresses need not be routable, and groups are not compared.
    pub private_network: bool,
    /// A seed is expected to serve the whole chain: a pruned one is a warning.
    pub expect_archive: bool,
}

impl EvalConfig {
    pub fn new(chain_id: [u8; 32]) -> EvalConfig {
        EvalConfig {
            chain_id,
            max_lag_blocks: 3,
            min_addrs: 5,
            min_seeds: 3,
            slow_ms: 3000,
            private_network: false,
            expect_archive: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SeedReport {
    pub seed: String,
    pub severity: Severity,
    pub findings: Vec<Finding>,
    pub tip_height: Option<u64>,
    pub latency_ms: Option<u64>,
    /// How many addresses it gave, and how many of those are routable, and in how many network groups.
    pub addrs: usize,
    pub routable: usize,
    pub groups: usize,
}

#[derive(Clone, Debug)]
pub struct Report {
    pub seeds: Vec<SeedReport>,
    /// Findings about the list as a whole.
    pub list: Vec<Finding>,
    pub answered: usize,
    pub severity: Severity,
}

fn finding(severity: Severity, text: impl Into<String>) -> Finding {
    Finding {
        severity,
        text: text.into(),
    }
}

/// Judges the probes (one per seed, in the order of the list). No I/O: everything it says follows from what the probes hold.
pub fn evaluate(probes: &[Probe], cfg: &EvalConfig) -> Report {
    let mut seeds: Vec<SeedReport> = Vec::new();
    for p in probes {
        let mut f: Vec<Finding> = Vec::new();
        let mut routable = 0;
        let mut groups = 0;
        if let Some(e) = &p.error {
            let what = match p.stage {
                "resolve" => "the name does not resolve",
                "connect" => "cannot connect",
                "handshake" => "the encrypted handshake did not complete",
                "hello" => "no hello",
                _ => "failed",
            };
            f.push(finding(Severity::Fail, format!("{what}: {e}")));
        }
        if let Some(h) = &p.hello {
            if h.version != PROTOCOL_VERSION {
                f.push(finding(
                    Severity::Fail,
                    format!(
                        "protocol version {} (this tool speaks {PROTOCOL_VERSION})",
                        h.version
                    ),
                ));
            }
            if h.chain_id != cfg.chain_id {
                f.push(finding(Severity::Fail, "a different chain"));
            }
            if cfg.expect_archive && h.pruned_below > 0 {
                f.push(finding(
                    Severity::Warn,
                    format!(
                        "pruned below height {}: it cannot serve the whole chain to a new node",
                        h.pruned_below
                    ),
                ));
            }
            if p.latency_ms() > cfg.slow_ms {
                f.push(finding(
                    Severity::Warn,
                    format!(
                        "slow: {} ms to its hello (more than {} ms)",
                        p.latency_ms(),
                        cfg.slow_ms
                    ),
                ));
            }
        }
        if p.error.is_none() && p.stage == "addrs" {
            let parsed: Vec<SocketAddr> = p.addrs.iter().filter_map(|a| a.parse().ok()).collect();
            routable = parsed.iter().filter(|a| is_routable(a)).count();
            groups = p
                .addrs
                .iter()
                .map(|a| group_of(a))
                .collect::<BTreeSet<_>>()
                .len();
            if p.addrs.is_empty() {
                f.push(finding(
                    Severity::Warn,
                    "gave no addresses: a new node cannot get started from it",
                ));
            } else if p.addrs.len() < cfg.min_addrs {
                f.push(finding(
                    Severity::Warn,
                    format!(
                        "gave only {} addresses (at least {} wanted)",
                        p.addrs.len(),
                        cfg.min_addrs
                    ),
                ));
            }
            if !cfg.private_network && !p.addrs.is_empty() {
                if routable * 2 < p.addrs.len() {
                    f.push(finding(
                        Severity::Warn,
                        format!(
                            "only {routable} of its {} addresses are routable",
                            p.addrs.len()
                        ),
                    ));
                }
                if p.addrs.len() >= cfg.min_addrs && groups < 2 {
                    f.push(finding(
                        Severity::Warn,
                        "all of its addresses are in one network group",
                    ));
                }
            }
        }
        seeds.push(SeedReport {
            seed: p.seed.clone(),
            severity: Severity::Ok,
            findings: f,
            tip_height: p.hello.as_ref().map(|h| h.tip_height),
            latency_ms: p.hello.as_ref().map(|_| p.latency_ms()),
            addrs: p.addrs.len(),
            routable,
            groups,
        });
    }

    // the seeds that said hello are compared with one another
    let heard: Vec<(usize, &Hello)> = probes
        .iter()
        .enumerate()
        .filter_map(|(i, p)| p.hello.as_ref().map(|h| (i, h)))
        .filter(|(_, h)| h.chain_id == cfg.chain_id && h.version == PROTOCOL_VERSION)
        .collect();
    if heard.len() >= 2 {
        let mut heights: Vec<u64> = heard.iter().map(|(_, h)| h.tip_height).collect();
        heights.sort_unstable();
        // the middle one (the upper of the two middle ones when there is an even number)
        let median = heights[heights.len() / 2];
        let mut by_tip: BTreeMap<[u8; 32], usize> = BTreeMap::new();
        for (_, h) in &heard {
            *by_tip.entry(h.tip_id).or_default() += 1;
        }
        let top = by_tip.values().copied().max().unwrap_or(0);
        let majority_tip: Option<[u8; 32]> = by_tip
            .iter()
            .find(|(_, &n)| n == top && n * 2 > heard.len())
            .map(|(id, _)| *id);
        for (i, h) in &heard {
            let r = &mut seeds[*i].findings;
            if h.tip_height + cfg.max_lag_blocks < median {
                r.push(finding(
                    Severity::Fail,
                    format!(
                        "behind the other seeds: tip {} against about {median}",
                        h.tip_height
                    ),
                ));
            } else if h.tip_height > median + cfg.max_lag_blocks {
                r.push(finding(
                    Severity::Warn,
                    format!(
                        "ahead of the other seeds: tip {} against about {median} (or they are the ones behind)",
                        h.tip_height
                    ),
                ));
            } else if let Some(m) = majority_tip {
                if h.tip_id != m && h.tip_height == median {
                    r.push(finding(
                        Severity::Warn,
                        "on a different tip from most seeds at the same height (a fork, or a block still arriving)",
                    ));
                }
            }
        }
    }
    for s in &mut seeds {
        s.severity = s
            .findings
            .iter()
            .map(|f| f.severity)
            .max()
            .unwrap_or(Severity::Ok);
    }

    // the list as a whole
    let mut list: Vec<Finding> = Vec::new();
    let answered = probes.iter().filter(|p| p.hello.is_some()).count();
    if probes.len() < cfg.min_seeds {
        list.push(finding(
            Severity::Warn,
            format!(
                "only {} seeds listed: the policy wants at least {} operators in different network groups",
                probes.len(),
                cfg.min_seeds
            ),
        ));
    } else if answered < cfg.min_seeds {
        list.push(finding(
            Severity::Warn,
            format!(
                "only {answered} of {} seeds answered: a new node needs at least {}",
                probes.len(),
                cfg.min_seeds
            ),
        ));
    }
    let mut seen_names: BTreeSet<&str> = BTreeSet::new();
    for p in probes {
        if !seen_names.insert(p.seed.as_str()) {
            list.push(finding(
                Severity::Warn,
                format!("{} is listed twice", p.seed),
            ));
        }
    }
    if !cfg.private_network {
        let mut by_group: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for p in probes {
            by_group
                .entry(group_of(&p.seed))
                .or_default()
                .push(p.seed.as_str());
        }
        for (g, names) in by_group {
            let distinct: BTreeSet<&str> = names.iter().copied().collect();
            if distinct.len() > 1 {
                list.push(finding(
                    Severity::Warn,
                    format!(
                        "{} share the network group {g}: a new node counts them as one answer",
                        distinct.into_iter().collect::<Vec<_>>().join(", ")
                    ),
                ));
            }
        }
    }
    let severity = seeds
        .iter()
        .map(|s| s.severity)
        .chain(list.iter().map(|f| f.severity))
        .max()
        .unwrap_or(Severity::Ok);
    Report {
        seeds,
        list,
        answered,
        severity,
    }
}

impl Report {
    /// The report as text for a person.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{} seeds checked, {} answered\n",
            self.seeds.len(),
            self.answered
        ));
        for s in &self.seeds {
            let detail = match (s.tip_height, s.latency_ms) {
                (Some(h), Some(ms)) => format!(
                    "tip {h}, {ms} ms, {} addresses ({} routable, {} groups)",
                    s.addrs, s.routable, s.groups
                ),
                _ => "no hello".to_string(),
            };
            out.push_str(&format!(
                "  {:<4}  {}  {}\n",
                s.severity.word(),
                s.seed,
                detail
            ));
            for f in &s.findings {
                out.push_str(&format!("          {}: {}\n", f.severity.word(), f.text));
            }
        }
        for f in &self.list {
            out.push_str(&format!("  list  {}: {}\n", f.severity.word(), f.text));
        }
        out.push_str(&format!(
            "result: {} (exit code {})\n",
            self.severity.word(),
            self.severity.exit_code()
        ));
        out
    }
}

// ---- history -------------------------------------------------------------------------------------------------------------------

/// One line for a history file: `unix_seconds <TAB> seed <TAB> ok|fail <TAB> severity <TAB> latency_ms <TAB> tip_height`.
pub fn history_line(unix_secs: u64, report: &SeedReport) -> String {
    format!(
        "{unix_secs}\t{}\t{}\t{}\t{}\t{}",
        report.seed,
        if report.tip_height.is_some() {
            "up"
        } else {
            "down"
        },
        report.severity.word(),
        report.latency_ms.map_or("-".to_string(), |m| m.to_string()),
        report.tip_height.map_or("-".to_string(), |h| h.to_string()),
    )
}

/// Of the last `last` checks of `seed` in a history file's text: how many found it up, and how many checks there were.
pub fn uptime(history: &str, seed: &str, last: usize) -> (usize, usize) {
    let mut marks: Vec<bool> = history
        .lines()
        .filter_map(|l| {
            let mut it = l.split('\t');
            let _ts = it.next()?;
            if it.next()? != seed {
                return None;
            }
            Some(it.next()? == "up")
        })
        .collect();
    if marks.len() > last {
        marks.drain(..marks.len() - last);
    }
    (marks.iter().filter(|&&u| u).count(), marks.len())
}

// ---- the command line ----------------------------------------------------------------------------------------------------------

pub const USAGE: &str = "\
tenero-seedcheck: checks the seeds of a Tenero network the way a new node would use them (EXPERIMENTAL, UNAUDITED; test networks only, nothing on them has value)

  tenero-seedcheck --network gamma|test|dev --seed HOST:PORT [--seed HOST:PORT ...] [options]
  tenero-seedcheck --network gamma --seeds-file seeds.txt

  --network N        gamma, test or dev: the chain the seeds must be on
  --seed ADDR        a seed to check (repeat for each)       --seeds-file F   one seed per line, blank lines and # comments ignored
  --timeout S        seconds each step may take (default 8)  --addr-wait S    seconds to collect the address answer (default 3)
  --max-lag N        blocks a seed may be behind the others before it FAILS (default 3)
  --min-addrs N      fewer addresses than this is a warning (default 5)      --min-seeds N   fewer seeds than this is a warning (default 3)
  --private yes|no   a private network (addresses need not be routable, groups are not compared): default yes for test, no for gamma and dev
  --history FILE     append one line per seed to FILE and show how often each was up in its last 50 checks

Exit code: 0 all well, 1 warnings, 2 a failure. It cannot tell whether a seed is HONEST: only whether it answers like a good one.";

#[derive(Clone, Debug)]
pub struct Options {
    pub network: String,
    pub seeds: Vec<String>,
    pub timeout_s: u64,
    pub addr_wait_s: u64,
    pub max_lag: u64,
    pub min_addrs: usize,
    pub min_seeds: usize,
    pub private: bool,
    pub history: Option<String>,
}

/// The seeds in a seeds file text: one per line, blank lines and `#` comments (also after a seed) ignored. Duplicates are kept (the
/// check says so).
pub fn parse_seeds_file(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect()
}

/// Reads the command line (`args` without the program name). `read_file` reads a seeds file (a parameter so that this needs no files).
pub fn parse_args(
    args: &[String],
    read_file: &dyn Fn(&str) -> Result<String, String>,
) -> Result<Options, String> {
    let mut o = Options {
        network: String::new(),
        seeds: Vec::new(),
        timeout_s: 8,
        addr_wait_s: 3,
        max_lag: 3,
        min_addrs: 5,
        min_seeds: 3,
        private: false,
        history: None,
    };
    let mut private: Option<bool> = None;
    let mut seen = BTreeSet::new();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let Some(key) = flag.strip_prefix("--") else {
            return Err(format!("unexpected argument `{flag}`"));
        };
        let v = it
            .next()
            .ok_or_else(|| format!("--{key} needs a value"))?
            .clone();
        // a seed may be repeated; nothing else may
        if key != "seed" && !seen.insert(key.to_string()) {
            return Err(format!("--{key} given twice"));
        }
        let num = |what: &str| {
            v.parse::<u64>()
                .map_err(|_| format!("--{what}: `{v}` is not a number"))
        };
        match key {
            "network" => o.network = v.clone(),
            "seed" => o.seeds.push(v.clone()),
            "seeds-file" => {
                let text = read_file(&v).map_err(|e| format!("--seeds-file: {e}"))?;
                o.seeds.extend(parse_seeds_file(&text));
            }
            "timeout" => o.timeout_s = num("timeout")?.max(1),
            "addr-wait" => o.addr_wait_s = num("addr-wait")?.max(1),
            "max-lag" => o.max_lag = num("max-lag")?,
            "min-addrs" => o.min_addrs = num("min-addrs")? as usize,
            "min-seeds" => o.min_seeds = num("min-seeds")? as usize,
            "private" => {
                private = Some(match v.as_str() {
                    "yes" => true,
                    "no" => false,
                    _ => return Err(format!("--private: `{v}` is not yes or no")),
                })
            }
            "history" => o.history = Some(v.clone()),
            other => return Err(format!("unknown option `--{other}`")),
        }
    }
    if !matches!(o.network.as_str(), "gamma" | "test" | "dev") {
        return Err("--network must be gamma, dev or test".into());
    }
    if o.seeds.is_empty() {
        return Err("no seeds: give --seed or --seeds-file".into());
    }
    o.private = private.unwrap_or(o.network == "test");
    Ok(o)
}
