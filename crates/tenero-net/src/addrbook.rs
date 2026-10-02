//! The address book and the ban list: what a node knows about other nodes' addresses, and whom it refuses.
//!
//! **What it is for.** A node that wants 50 peers must find them, and must not be steered to an attacker's
//! addresses. The book therefore
//! * keeps only routable, parseable `ip:port` addresses (a peer cannot make us dial `127.0.0.1` or `10.0.0.5`
//!   from the public network, unless the operator says the network is private);
//! * remembers who told us each address (its *source group*) and lets one source group fill only a bounded part
//!   of the book, so one peer, or one network range, cannot fill it with its own addresses;
//! * is bounded in total and drops the worst entries first (never-worked, often-failed, stale) before any address
//!   that has worked;
//! * keeps a *tried* flag (an address that once connected) and prefers a mix of tried and new when choosing whom
//!   to dial;
//! * backs off after a failure, exponentially, and forgets addresses that never worked;
//! * can be saved and loaded, with a checksum, so a restarted node does not start from its seeds alone.
//!
//! **What it does not do:** it is not the connection manager (that is in the engine), it cannot tell whether an
//! address is honest, and a determined attacker with many source groups and many addresses can still crowd it
//! (the mitigation is diversity when dialling, not the book).

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use tenero_core::hash::sha256;

use crate::message::PeerAddr;

/// A small deterministic generator: address selection must be a function of the configured seed.
#[derive(Clone, Debug)]
pub struct XorShift(pub u64);

impl XorShift {
    pub fn next_u64(&mut self) -> u64 {
        if self.0 == 0 {
            self.0 = 0x9e37_79b9_7f4a_7c15;
        }
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A number in `0..n` (`n` must be positive).
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i + 1);
            items.swap(i, j);
        }
    }
}

/// Is this a public address a node could reasonably dial from the Internet?
pub fn is_routable(addr: &SocketAddr) -> bool {
    if addr.port() == 0 {
        return false;
    }
    let ip = match addr.ip() {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    };
    match ip {
        IpAddr::V4(a) => {
            !(a.is_unspecified()
                || a.is_loopback()
                || a.is_private()
                || a.is_link_local()
                || a.is_broadcast()
                || a.is_multicast()
                || a.is_documentation())
        }
        IpAddr::V6(a) => {
            let s = a.segments();
            !(a.is_unspecified()
                || a.is_loopback()
                || a.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80 // link local
                || (s[0] == 0x2001 && s[1] == 0x0db8)) // documentation
        }
    }
}

/// The network range an address belongs to, for diversity: an IPv4 /16 or an IPv6 /32. An address that is
/// not an `ip:port` (a test name, say) is its own group.
pub fn group_of(addr: &str) -> String {
    match addr.parse::<SocketAddr>() {
        Ok(sa) => match sa.ip() {
            IpAddr::V4(a) => format!("v4:{}.{}", a.octets()[0], a.octets()[1]),
            IpAddr::V6(a) => match a.to_ipv4_mapped() {
                Some(v4) => format!("v4:{}.{}", v4.octets()[0], v4.octets()[1]),
                None => format!("v6:{:x}:{:x}", a.segments()[0], a.segments()[1]),
            },
        },
        Err(_) => format!("raw:{addr}"),
    }
}

/// What a ban applies to: the IP address, whatever port the peer connected from (an inbound peer's port is
/// arbitrary, so banning `ip:port` would ban nothing). An address that is not `ip:port` is its own host.
pub fn host_of(addr: &str) -> String {
    match addr.parse::<SocketAddr>() {
        Ok(sa) => match sa.ip() {
            IpAddr::V6(a) => match a.to_ipv4_mapped() {
                Some(v4) => v4.to_string(),
                None => a.to_string(),
            },
            v4 => v4.to_string(),
        },
        Err(_) => format!("raw:{addr}"),
    }
}

/// A gossiped address as text, or `None` if it is not one we can use.
pub fn peer_addr_to_string(a: &PeerAddr) -> Option<String> {
    let v6 = Ipv6Addr::from(a.ip);
    let ip = match v6.to_ipv4_mapped() {
        Some(v4) => IpAddr::V4(v4),
        None => IpAddr::V6(v6),
    };
    Some(SocketAddr::new(ip, a.port).to_string())
}

/// An address as it goes on the wire, or `None` if it is not `ip:port`.
pub fn string_to_peer_addr(addr: &str, last_seen: u64) -> Option<PeerAddr> {
    let sa = addr.parse::<SocketAddr>().ok()?;
    let ip = match sa.ip() {
        IpAddr::V4(v4) => v4.to_ipv6_mapped().octets(),
        IpAddr::V6(v6) => v6.octets(),
    };
    Some(PeerAddr {
        ip,
        port: sa.port(),
        last_seen,
    })
}

#[derive(Clone, Debug)]
pub struct AddrBookConfig {
    /// The most addresses kept in all.
    pub max_entries: usize,
    /// The most *new* (never connected) addresses one source group may have in the book.
    pub max_new_per_source: usize,
    /// First retry delay after a failure; it doubles each time.
    pub backoff_base_ms: u64,
    pub backoff_max_ms: u64,
    /// The shortest wait before dialling an address again, even if the last connection worked: a peer that
    /// connects and is dropped at once must not be redialled in a tight loop.
    pub min_redial_ms: u64,
    /// A never-worked address is forgotten after this many failures.
    pub max_failures: u32,
    /// An address not seen for this long is forgotten.
    pub stale_secs: u64,
    /// Accept private and loopback addresses (a private test network, or a simulation).
    pub accept_private: bool,
    /// When choosing among never-tried addresses, those reported by two or more different source groups come first. Off by
    /// default: it only helps when honest sources report overlapping lists (see `docs/SEED_POLICY.md` for the measurement).
    pub prefer_corroborated: bool,
    pub seed: u64,
}

impl Default for AddrBookConfig {
    fn default() -> AddrBookConfig {
        AddrBookConfig {
            max_entries: 4096,
            max_new_per_source: 64,
            backoff_base_ms: 30_000,
            backoff_max_ms: 6 * 3600 * 1000,
            min_redial_ms: 30_000,
            max_failures: 10,
            stale_secs: 30 * 24 * 3600,
            accept_private: false,
            prefer_corroborated: false,
            seed: 0x5eed_5eed_5eed_5eed,
        }
    }
}

/// How many different source groups are remembered per address.
pub const MAX_REPORTERS: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub addr: String,
    /// When the address was last known alive, Unix seconds (as claimed by whoever told us, clamped to now).
    pub last_seen: u64,
    /// Who FIRST told us (a network group), or `seed`.
    pub source: String,
    /// The configured seed this address descends from: the network group of the seed itself, or, for an address learned from a
    /// peer, that peer's own origin (an address told by a peer that a seed told us of has that seed's origin). A source can be
    /// multiplied by an attacker at no cost (every peer of his is a new one); an origin cannot, because only the seeds start one.
    /// Not saved to disk: after a restart it is the first source.
    pub origin: String,
    /// Every source group that has told us this address (the first one included), at most `MAX_REPORTERS`. Not saved to disk: after
    /// a restart an entry has only its first source.
    pub reporters: Vec<String>,
    /// It has connected at least once.
    pub tried: bool,
    pub failures: u32,
    pub last_attempt_ms: u64,
}

pub struct AddrBook {
    cfg: AddrBookConfig,
    entries: BTreeMap<String, Entry>,
    rng: XorShift,
}

impl AddrBook {
    pub fn new(cfg: AddrBookConfig) -> AddrBook {
        let rng = XorShift(cfg.seed);
        AddrBook {
            cfg,
            entries: BTreeMap::new(),
            rng,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, addr: &str) -> Option<&Entry> {
        self.entries.get(addr)
    }

    /// The origin of an address in the book.
    pub fn origin_of(&self, addr: &str) -> Option<&str> {
        self.entries.get(addr).map(|e| e.origin.as_str())
    }

    /// How many different origins the book's entries have.
    pub fn origin_count(&self) -> usize {
        self.entries
            .values()
            .map(|e| e.origin.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    }

    pub fn tried_count(&self) -> usize {
        self.entries.values().filter(|e| e.tried).count()
    }

    /// How many entries were told to us by this source group.
    pub fn from_source(&self, source: &str) -> usize {
        self.entries.values().filter(|e| e.source == source).count()
    }

    pub fn addrs(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    fn usable(&self, addr: &str) -> Option<SocketAddr> {
        let sa = addr.parse::<SocketAddr>().ok();
        match sa {
            Some(sa) if self.cfg.accept_private || is_routable(&sa) => {
                if sa.port() == 0 {
                    None
                } else {
                    Some(sa)
                }
            }
            _ => None,
        }
    }

    /// Adds an address told to us by `source` (a group name, or `"seed"`). Returns whether it is in the book
    /// afterwards. An address already known keeps its record, and its `last_seen` only moves forward.
    pub fn add(&mut self, addr: &str, last_seen: u64, source: &str, now_secs: u64) -> bool {
        self.add_from(addr, last_seen, source, source, now_secs)
    }

    /// `add`, saying also which seed the telling peer descends from (`origin`; ignored for the seeds themselves, whose origin is
    /// their own network group).
    pub fn add_from(
        &mut self,
        addr: &str,
        last_seen: u64,
        source: &str,
        origin: &str,
        now_secs: u64,
    ) -> bool {
        // a peer cannot claim an address was seen in the future
        let last_seen = last_seen.min(now_secs);
        if now_secs.saturating_sub(last_seen) > self.cfg.stale_secs {
            return false;
        }
        if self.usable(addr).is_none() {
            return false;
        }
        if let Some(e) = self.entries.get_mut(addr) {
            e.last_seen = e.last_seen.max(last_seen);
            if source != "seed"
                && e.reporters.len() < MAX_REPORTERS
                && !e.reporters.iter().any(|r| r == source)
            {
                e.reporters.push(source.to_string());
            }
            return true;
        }
        if source != "seed" && self.new_from(source) >= self.cfg.max_new_per_source {
            return false;
        }
        if self.entries.len() >= self.cfg.max_entries && !self.evict_one(now_secs) {
            return false;
        }
        self.entries.insert(
            addr.to_string(),
            Entry {
                addr: addr.to_string(),
                last_seen,
                source: source.to_string(),
                origin: if source == "seed" {
                    group_of(addr)
                } else {
                    origin.to_string()
                },
                reporters: vec![source.to_string()],
                tried: false,
                failures: 0,
                last_attempt_ms: 0,
            },
        );
        true
    }

    fn new_from(&self, source: &str) -> usize {
        self.entries
            .values()
            .filter(|e| !e.tried && e.source == source)
            .count()
    }

    /// Makes room: drops the worst entry (a new one that failed most, then the stalest). Tried addresses go
    /// only if nothing else can. Returns false if the book holds only entries it will not drop.
    fn evict_one(&mut self, _now_secs: u64) -> bool {
        let victim = self
            .entries
            .values()
            .filter(|e| e.source != "seed")
            .min_by_key(|e| {
                (
                    e.tried,                       // never-worked first
                    std::cmp::Reverse(e.failures), // the most failures first
                    e.last_seen,                   // then the stalest
                    e.addr.clone(),
                )
            })
            .map(|e| e.addr.clone());
        match victim {
            Some(a) => {
                self.entries.remove(&a);
                true
            }
            None => false,
        }
    }

    pub fn remove(&mut self, addr: &str) {
        self.entries.remove(addr);
    }

    /// Drops what has been stale too long.
    pub fn expire(&mut self, now_secs: u64) {
        let stale = self.cfg.stale_secs;
        self.entries
            .retain(|_, e| e.source == "seed" || now_secs.saturating_sub(e.last_seen) <= stale);
    }

    pub fn mark_attempt(&mut self, addr: &str, now_ms: u64) {
        if let Some(e) = self.entries.get_mut(addr) {
            e.last_attempt_ms = now_ms;
        }
    }

    /// The address connected (and is alive): it is "tried" from now on and its failures are forgotten.
    pub fn mark_success(&mut self, addr: &str, now_secs: u64) {
        if let Some(e) = self.entries.get_mut(addr) {
            e.tried = true;
            e.failures = 0;
            e.last_seen = now_secs;
        }
    }

    /// The address could not be reached (or the connection failed the handshake).
    pub fn mark_failure(&mut self, addr: &str, now_ms: u64) {
        let forget = match self.entries.get_mut(addr) {
            Some(e) => {
                e.failures += 1;
                e.last_attempt_ms = now_ms;
                !e.tried && e.source != "seed" && e.failures >= self.cfg.max_failures
            }
            None => false,
        };
        if forget {
            self.entries.remove(addr);
        }
    }

    /// How long to wait after `failures` failures: the base, doubling, up to the maximum.
    pub fn backoff_ms(&self, failures: u32) -> u64 {
        if failures == 0 {
            return 0;
        }
        let shift = (failures - 1).min(30);
        self.cfg
            .backoff_base_ms
            .saturating_mul(1u64 << shift)
            .min(self.cfg.backoff_max_ms)
    }

    /// Up to `limit` addresses worth dialling now, best first. `skip` says which addresses to leave out (already
    /// connected, connecting, banned, our own) and `group_full` which network groups already have all the
    /// outbound peers allowed. Tried and new addresses are mixed so that neither can crowd the other out.
    pub fn candidates(
        &mut self,
        now_ms: u64,
        limit: usize,
        skip: &dyn Fn(&str) -> bool,
        group_full: &dyn Fn(&str) -> bool,
    ) -> Vec<String> {
        self.candidates_with(now_ms, limit, skip, group_full, &|_| false, false)
    }

    /// `candidates`, with two more rules: `source_full` says which ORIGINS (the seed an address descends from) already have all
    /// the outbound peers allowed (the seeds' own addresses are not limited by it), and `seeds_only`
    /// leaves out everything but the configured seeds (a node that is still bootstrapping).
    pub fn candidates_with(
        &mut self,
        now_ms: u64,
        limit: usize,
        skip: &dyn Fn(&str) -> bool,
        group_full: &dyn Fn(&str) -> bool,
        source_full: &dyn Fn(&str) -> bool,
        seeds_only: bool,
    ) -> Vec<String> {
        let mut tried: Vec<String> = Vec::new();
        let mut fresh: Vec<(String, bool)> = Vec::new();
        for e in self.entries.values() {
            let wait = self.backoff_ms(e.failures).max(self.cfg.min_redial_ms);
            if e.last_attempt_ms > 0 && now_ms < e.last_attempt_ms.saturating_add(wait) {
                continue;
            }
            let is_seed = e.source == "seed";
            if (seeds_only && !is_seed)
                || skip(&e.addr)
                || group_full(&group_of(&e.addr))
                || (!is_seed && source_full(&e.origin))
            {
                continue;
            }
            if e.tried {
                tried.push(e.addr.clone());
            } else {
                fresh.push((e.addr.clone(), e.reporters.len() >= 2));
            }
        }
        self.rng.shuffle(&mut tried);
        self.rng.shuffle(&mut fresh);
        if self.cfg.prefer_corroborated {
            // a stable sort: the shuffle's order is kept within each class
            fresh.sort_by_key(|(_, corroborated)| !*corroborated);
        }
        let fresh: Vec<String> = fresh.into_iter().map(|(a, _)| a).collect();
        let mut out = Vec::new();
        let (mut t, mut f) = (tried.into_iter(), fresh.into_iter());
        while out.len() < limit {
            // alternate, starting from a random side, so both kinds are represented
            let first_tried = self.rng.below(2) == 0;
            let pick = if first_tried {
                t.next().or_else(|| f.next())
            } else {
                f.next().or_else(|| t.next())
            };
            match pick {
                Some(a) => out.push(a),
                None => break,
            }
        }
        out
    }

    /// A random sample to send in answer to `GetAddrs`: at most `n`, none stale.
    pub fn sample(&mut self, n: usize, now_secs: u64) -> Vec<PeerAddr> {
        let stale = self.cfg.stale_secs;
        let mut all: Vec<&Entry> = self
            .entries
            .values()
            .filter(|e| now_secs.saturating_sub(e.last_seen) <= stale)
            .collect();
        // a stable order first, so the shuffle depends only on the seed
        all.sort_by(|a, b| a.addr.cmp(&b.addr));
        let mut idx: Vec<usize> = (0..all.len()).collect();
        self.rng.shuffle(&mut idx);
        idx.into_iter()
            .filter_map(|i| string_to_peer_addr(&all[i].addr, all[i].last_seen))
            .take(n)
            .collect()
    }

    // ---- persistence -------------------------------------------------------------------------------

    /// `"TAB1" | count u32 | entries | checksum 4` where an entry is `addr_len u8, addr, last_seen u64,
    /// source_len u8, source, tried u8, failures u32`. The checksum is the first 4 bytes of SHA-256 of
    /// everything before it: it detects a damaged file, it is not a defence against an attacker with write access.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = b"TAB1".to_vec();
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for e in self.entries.values() {
            out.push(e.addr.len() as u8);
            out.extend_from_slice(e.addr.as_bytes());
            out.extend_from_slice(&e.last_seen.to_le_bytes());
            out.push(e.source.len() as u8);
            out.extend_from_slice(e.source.as_bytes());
            out.push(u8::from(e.tried));
            out.extend_from_slice(&e.failures.to_le_bytes());
        }
        let sum = sha256(&[&out]);
        out.extend_from_slice(&sum[..4]);
        out
    }

    /// Strict: a damaged, truncated or trailing-garbage file is refused (and the node starts from its seeds).
    pub fn from_bytes(cfg: AddrBookConfig, data: &[u8]) -> Result<AddrBook, String> {
        if data.len() < 12 || &data[..4] != b"TAB1" {
            return Err("not an address book".into());
        }
        let (body, sum) = data.split_at(data.len() - 4);
        if sha256(&[body])[..4] != *sum {
            return Err("the address book is damaged (checksum)".into());
        }
        let mut r = Cursor { data: body, pos: 4 };
        let n = r.u32()? as usize;
        if n > cfg.max_entries {
            return Err("more entries than the book may hold".into());
        }
        let mut book = AddrBook::new(cfg);
        for _ in 0..n {
            let addr = r.string()?;
            let last_seen = r.u64()?;
            let source = r.string()?;
            let tried = match r.u8()? {
                0 => false,
                1 => true,
                _ => return Err("bad flag".into()),
            };
            let failures = r.u32()?;
            let origin = if source == "seed" {
                group_of(&addr)
            } else {
                source.clone()
            };
            book.entries.insert(
                addr.clone(),
                Entry {
                    addr,
                    last_seen,
                    reporters: vec![source.clone()],
                    origin,
                    source,
                    tried,
                    failures,
                    last_attempt_ms: 0,
                },
            );
        }
        if r.pos != body.len() {
            return Err("trailing bytes".into());
        }
        Ok(book)
    }
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], String> {
        if self.data.len() - self.pos < n {
            return Err("truncated".into());
        }
        let out = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn string(&mut self) -> Result<String, String> {
        let n = self.u8()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| "not text".to_string())
    }
}

/// Banned hosts (see [`host_of`]) and when each ban ends, in Unix milliseconds.
#[derive(Clone, Debug, Default)]
pub struct BanList {
    until: BTreeMap<String, u64>,
}

impl BanList {
    pub fn new() -> BanList {
        BanList::default()
    }

    pub fn ban(&mut self, addr: &str, until_ms: u64) {
        let host = host_of(addr);
        let e = self.until.entry(host).or_insert(0);
        *e = (*e).max(until_ms);
    }

    pub fn is_banned(&self, addr: &str, now_ms: u64) -> bool {
        self.until
            .get(&host_of(addr))
            .is_some_and(|&until| until > now_ms)
    }

    pub fn expire(&mut self, now_ms: u64) {
        self.until.retain(|_, &mut until| until > now_ms);
    }

    pub fn len(&self) -> usize {
        self.until.len()
    }

    pub fn is_empty(&self) -> bool {
        self.until.is_empty()
    }

    /// `"TBN1" | count u32 | (host_len u8, host, until u64)* | checksum 4`
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = b"TBN1".to_vec();
        out.extend_from_slice(&(self.until.len() as u32).to_le_bytes());
        for (host, until) in &self.until {
            out.push(host.len() as u8);
            out.extend_from_slice(host.as_bytes());
            out.extend_from_slice(&until.to_le_bytes());
        }
        let sum = sha256(&[&out]);
        out.extend_from_slice(&sum[..4]);
        out
    }

    pub fn from_bytes(data: &[u8]) -> Result<BanList, String> {
        if data.len() < 12 || &data[..4] != b"TBN1" {
            return Err("not a ban list".into());
        }
        let (body, sum) = data.split_at(data.len() - 4);
        if sha256(&[body])[..4] != *sum {
            return Err("the ban list is damaged (checksum)".into());
        }
        let mut r = Cursor { data: body, pos: 4 };
        let n = r.u32()? as usize;
        if n > 1_000_000 {
            return Err("implausible ban count".into());
        }
        let mut list = BanList::new();
        for _ in 0..n {
            let host = r.string()?;
            let until = r.u64()?;
            list.until.insert(host, until);
        }
        if r.pos != body.len() {
            return Err("trailing bytes".into());
        }
        Ok(list)
    }
}

/// `Ipv4Addr` helper for tests that build addresses from numbers.
pub fn v4(a: u8, b: u8, c: u8, d: u8, port: u16) -> String {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(a, b, c, d)), port).to_string()
}
