//! `tenero-poolcheck`: a program a pool's developer runs against their pool, as `tenero-seedcheck` is run against a seed
//! (`docs/POOL_PROTOCOL.md`, "A conformance kit"). It connects as a miner and checks what the protocol requires of a pool, one thing at a time, and
//! says which failed. **It cannot prove a pool is right** (a pool can pass every check and still not pay), and a pool that passes has only shown that it
//! speaks the protocol the way these checks look for it. Experimental and unaudited.
//!
//! What needs a valid share (a share accepted, a duplicate refused) is checked only where the tool can make one without the 4 GiB dataset: on the
//! SHA-256 test network. On a network with the real proof of work those two checks are reported as skipped, not passed.

use std::io::Write as _;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tenero_core::u256::U256;
use tenero_core::v2::ids::{self, PowKind};
use tenero_core::v2::{BlockHeader, VERSION};

use crate::pool::{Hello, HelloOk, Job, MinerMessage, PoolMessage};
use crate::pool_net::{self, read_message, write_message, ReadHalf, WriteHalf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail(String),
    /// Not checked, and why.
    Skipped(String),
}

#[derive(Clone, Debug)]
pub struct Check {
    pub name: &'static str,
    pub verdict: Verdict,
}

impl Check {
    fn pass(name: &'static str) -> Check {
        Check {
            name,
            verdict: Verdict::Pass,
        }
    }
    fn fail(name: &'static str, why: impl Into<String>) -> Check {
        Check {
            name,
            verdict: Verdict::Fail(why.into()),
        }
    }
    fn skipped(name: &'static str, why: impl Into<String>) -> Check {
        Check {
            name,
            verdict: Verdict::Skipped(why.into()),
        }
    }
}

pub struct Options {
    pub addr: SocketAddr,
    pub pin: Option<[u8; 32]>,
    /// The network's name, as a miner says it in `hello`.
    pub network: String,
    pub pow: PowKind,
    /// An address to say in the hello (any valid one: the pool is not asked to pay it).
    pub address: String,
    pub wait: Duration,
}

struct Conn {
    r: ReadHalf,
    w: WriteHalf,
    wait: Duration,
}

impl Conn {
    fn open(o: &Options) -> Result<Conn, String> {
        let (r, w) = pool_net::connect(o.addr, o.pin.as_ref())?;
        Ok(Conn { r, w, wait: o.wait })
    }
    fn send(&mut self, m: &MinerMessage) -> Result<(), String> {
        let body = m.to_body().map_err(|e| e.to_string())?;
        write_message(&mut self.w, &body).map_err(|e| e.to_string())
    }
    /// The next message; `Err(None)` at the end of the connection, `Err(Some(why))` for a message that does not decode or a silence.
    fn recv(&mut self) -> Result<PoolMessage, Option<String>> {
        self.r
            .set_timeout(Some(self.wait))
            .map_err(|e| Some(e.to_string()))?;
        let body = read_message(&mut self.r).map_err(|e| match e.kind() {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                Some("nothing came in time".to_string())
            }
            std::io::ErrorKind::UnexpectedEof => None,
            _ => None,
        })?;
        PoolMessage::from_body(&body)
            .map_err(|e| Some(format!("a message that does not decode: {e}")))
    }
    /// Reads until a message satisfies `pick`, skipping jobs and target changes.
    fn until<T>(&mut self, mut pick: impl FnMut(&PoolMessage) -> Option<T>) -> Result<T, String> {
        loop {
            match self.recv() {
                Ok(m) => {
                    if let Some(t) = pick(&m) {
                        return Ok(t);
                    }
                    if !matches!(
                        m,
                        PoolMessage::Job(_)
                            | PoolMessage::SetShareTarget { .. }
                            | PoolMessage::SetPayout { .. }
                    ) {
                        return Err(format!("an unexpected message: {m:?}"));
                    }
                }
                Err(None) => return Err("the pool closed the connection".into()),
                Err(Some(e)) => return Err(e),
            }
        }
    }
    /// True if the pool ends the connection (after any messages saying why) within the wait.
    fn ends(&mut self) -> bool {
        loop {
            match self.recv() {
                Ok(
                    PoolMessage::Error(_)
                    | PoolMessage::Job(_)
                    | PoolMessage::SetShareTarget { .. },
                ) => continue,
                Ok(_) => return false,
                Err(None) => return true,
                Err(Some(_)) => return false,
            }
        }
    }
}

fn hello(o: &Options, network: &str, address: &str) -> MinerMessage {
    MinerMessage::Hello(Hello {
        min_version: 1,
        max_version: 1,
        capabilities: 0,
        network: network.to_string(),
        address: address.to_string(),
        worker: "poolcheck".into(),
        agent: format!("tenero-poolcheck {}", crate::daemon::VERSION),
    })
    .clone_with(o)
}

trait CloneWith {
    fn clone_with(self, o: &Options) -> Self;
}

impl CloneWith for MinerMessage {
    fn clone_with(self, _o: &Options) -> MinerMessage {
        self
    }
}

/// Runs every check against the pool in `o`.
pub fn run(o: &Options) -> Vec<Check> {
    let mut out = Vec::new();
    // 1. the handshake (with the pinned key, if there is one)
    let mut c = match Conn::open(o) {
        Ok(c) => {
            out.push(Check::pass(
                "the handshake finishes (and, if a key is pinned, the pool proves it)",
            ));
            c
        }
        Err(e) => {
            out.push(Check::fail(
                "the handshake finishes (and, if a key is pinned, the pool proves it)",
                e,
            ));
            return out;
        }
    };
    // 2. hello and hello_ok
    if let Err(e) = c.send(&hello(o, &o.network, &o.address)) {
        out.push(Check::fail("the pool answers a good hello", e));
        return out;
    }
    let ok: HelloOk = match c.until(|m| match m {
        PoolMessage::HelloOk(ok) => Some(ok.clone()),
        _ => None,
    }) {
        Ok(ok) => ok,
        Err(e) => {
            out.push(Check::fail(
                "the pool answers a good hello with hello_ok",
                e,
            ));
            return out;
        }
    };
    let mut problems = Vec::new();
    if ok.version != 1 {
        problems.push(format!("it chose version {}", ok.version));
    }
    if ok.prefix_bits > 32 || (ok.prefix_bits < 64 && ok.prefix >> ok.prefix_bits != 0) {
        problems.push("the nonce prefix does not fit its bits".to_string());
    }
    if ok.pool_name.is_empty() {
        problems.push("the pool has no name".to_string());
    }
    if !ok.pays_pool {
        problems.push("it does not say the reward goes to the pool".to_string());
    }
    out.push(if problems.is_empty() {
        Check::pass("hello_ok is sane (version 1, a prefix that fits, a name, says the pool keeps the reward)")
    } else {
        Check::fail("hello_ok is sane", problems.join("; "))
    });
    // 3. a first job
    let job: Option<Job> = c
        .until(|m| match m {
            PoolMessage::Job(j) => Some(j.clone()),
            _ => None,
        })
        .map_err(|e| out.push(Check::fail("a job arrives after the hello", e)))
        .ok();
    let Some(job) = job else { return out };
    let mut problems = Vec::new();
    if job.header.version != VERSION {
        problems.push(format!("the header's version is {}", job.header.version));
    }
    if !job.clean {
        problems.push("the first job is not clean".to_string());
    }
    if job.block_target == [0; 32] {
        problems.push("the block target is zero".to_string());
    }
    if !(1..=3600).contains(&job.ttl) {
        problems.push(format!("the time to live is {}", job.ttl));
    }
    out.push(if problems.is_empty() {
        Check::pass("the first job is clean and well formed (empty nonce and mix, a target, a time to live)")
    } else {
        Check::fail("the first job is clean and well formed", problems.join("; "))
    });
    // 4. ping
    let _ = c.send(&MinerMessage::Ping { token: 0xC0FFEE });
    out.push(
        match c.until(|m| match m {
            PoolMessage::Pong { token } => Some(*token),
            _ => None,
        }) {
            Ok(0xC0FFEE) => Check::pass("a ping is answered with a pong carrying the same token"),
            Ok(t) => Check::fail(
                "a ping is answered with a pong carrying the same token",
                format!("the token was {t:#x}"),
            ),
            Err(e) => Check::fail("a ping is answered with a pong carrying the same token", e),
        },
    );
    // 5. a share above the target
    let share_target = U256::from_be_bytes(&ok.share_target);
    let start = crate::pool_core::first_nonce_of(ok.prefix, ok.prefix_bits);
    let above = find(&job.header, o.pow, start, &share_target, true);
    out.push(share_check(
        &mut c,
        "a share above the share target is refused (reason 3)",
        job.job_id,
        above,
        |a, r| !a && r == 3,
    ));
    // 6. a share for a job that does not exist
    out.push(share_check(
        &mut c,
        "a share for a job the pool never made is refused (reason 5 or 1)",
        job.job_id ^ 0x5A5A_5A5A_5A5A_5A5A,
        start,
        |a, r| !a && (r == 5 || r == 1),
    ));
    // 7. a nonce outside the miner's slice (if it has one)
    if ok.prefix_bits > 0 {
        let other = (ok.prefix + 1) & ((1u64 << ok.prefix_bits) - 1);
        let outside = find(
            &job.header,
            o.pow,
            crate::pool_core::first_nonce_of(other, ok.prefix_bits),
            &share_target,
            false,
        );
        out.push(share_check(
            &mut c,
            "a share outside this miner's slice of the nonces is refused (reason 3)",
            job.job_id,
            outside,
            |a, r| !a && r == 3,
        ));
    } else {
        out.push(Check::skipped(
            "a share outside this miner's slice is refused",
            "the pool gave a prefix of no bits",
        ));
    }
    // 8. a good share, and the same again
    if o.pow == PowKind::Sha256 {
        let good = find(&job.header, o.pow, start, &share_target, false);
        out.push(share_check(
            &mut c,
            "a share that meets the target is accepted",
            job.job_id,
            good,
            |a, r| a && r == 0,
        ));
        out.push(share_check(
            &mut c,
            "the same share again is refused as a duplicate (reason 2)",
            job.job_id,
            good,
            |a, r| !a && r == 2,
        ));
    } else {
        for name in [
            "a share that meets the target is accepted",
            "the same share again is refused as a duplicate (reason 2)",
        ] {
            out.push(Check::skipped(
                name,
                "needs a valid share, which on this network needs the 4 GiB proof-of-work dataset: test it with a real miner",
            ));
        }
    }
    // 9. a message that does not exist ends the connection
    let _ = write_message(&mut c.w, &[0x7E, 1, 2, 3]);
    out.push(if c.ends() {
        Check::pass("a message of a kind that does not exist ends the connection")
    } else {
        Check::fail(
            "a message of a kind that does not exist ends the connection",
            "the pool kept the connection open",
        )
    });
    // 10. a hello for another network
    out.push(refused_hello(
        o,
        "a hello for another network is refused with an error",
        "no-such-network",
        &o.address,
    ));
    // 11. a bad address
    out.push(refused_hello(
        o,
        "a hello with an address that is not valid is refused with an error",
        &o.network,
        "tni1notanaddress",
    ));
    // 12. the first message must be a hello
    out.push(match Conn::open(o) {
        Ok(mut c2) => {
            let _ = c2.send(&MinerMessage::Ping { token: 1 });
            if c2.ends() {
                Check::pass("a first message that is not a hello ends the connection")
            } else {
                Check::fail(
                    "a first message that is not a hello ends the connection",
                    "the pool kept the connection open",
                )
            }
        }
        Err(e) => Check::fail("a first message that is not a hello ends the connection", e),
    });
    // 13. a second hello
    out.push(match Conn::open(o) {
        Ok(mut c2) => {
            let _ = c2.send(&hello(o, &o.network, &o.address));
            let _ = c2.until(|m| matches!(m, PoolMessage::HelloOk(_)).then_some(()));
            let _ = c2.send(&hello(o, &o.network, &o.address));
            if c2.ends() {
                Check::pass("a second hello ends the connection")
            } else {
                Check::fail(
                    "a second hello ends the connection",
                    "the pool kept the connection open",
                )
            }
        }
        Err(e) => Check::fail("a second hello ends the connection", e),
    });
    // job declaration is optional: say what the pool offers
    out.push(if ok.capabilities & crate::pool::CAP_JOB_DECLARATION != 0 {
        Check::skipped(
            "job declaration (optional)",
            "the pool offers it: this tool does not test it yet",
        )
    } else {
        Check::pass("the pool offers no capability beyond version 1 (job declaration is optional)")
    });
    let _ = c.w.flush();
    out
}

fn refused_hello(o: &Options, name: &'static str, network: &str, address: &str) -> Check {
    match Conn::open(o) {
        Ok(mut c) => {
            let _ = c.send(&hello(o, network, address));
            match c.recv() {
                Ok(PoolMessage::Error(_)) => {
                    if c.ends() {
                        Check::pass(name)
                    } else {
                        Check::fail(name, "it said why but kept the connection open")
                    }
                }
                Ok(PoolMessage::HelloOk(_)) => Check::fail(name, "the pool accepted it"),
                Ok(other) => Check::fail(name, format!("it answered with {other:?}")),
                Err(None) => Check::pass(name),
                Err(Some(e)) => Check::fail(name, e),
            }
        }
        Err(e) => Check::fail(name, e),
    }
}

/// The first nonce at or after `start` whose block id is under `target` (or, with `above`, not under it). Not for a proof of work whose id needs a
/// dataset: the id of a matmulhash header is computed from the header alone, so "not under" is found at once, and "under" is only searched for on the
/// SHA-256 test chain.
fn find(header: &BlockHeader, pow: PowKind, start: u64, target: &U256, above: bool) -> u64 {
    let mut h = header.clone();
    let mut n = start;
    for _ in 0..50_000_000u64 {
        h.nonce = n;
        let id = U256::from_be_bytes(&ids::block_id(&h, pow));
        if (id < *target) != above {
            return n;
        }
        n = n.wrapping_add(1);
    }
    start
}

fn share_check(
    c: &mut Conn,
    name: &'static str,
    job_id: u64,
    nonce: u64,
    expect: impl Fn(bool, u8) -> bool,
) -> Check {
    if let Err(e) = c.send(&MinerMessage::SubmitShare {
        job_id,
        nonce,
        mix: [0; 64],
    }) {
        return Check::fail(name, e);
    }
    let started = Instant::now();
    match c.until(|m| match m {
        PoolMessage::ShareResult {
            accepted, reason, ..
        } => Some((*accepted, *reason)),
        _ => None,
    }) {
        Ok((a, r)) if expect(a, r) => Check::pass(name),
        Ok((a, r)) => Check::fail(name, format!("the pool answered accepted={a}, reason {r}")),
        Err(e) => Check::fail(name, format!("{e} (after {:?})", started.elapsed())),
    }
}

/// How many checks failed.
pub fn failures(checks: &[Check]) -> usize {
    checks
        .iter()
        .filter(|c| matches!(c.verdict, Verdict::Fail(_)))
        .count()
}
