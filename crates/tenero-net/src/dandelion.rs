//! Dandelion++ (`docs/DANDELION_PLAN.md`): the state a node keeps for it, and its random choices. The engine
//! (`engine.rs`) does the sending; this holds the epoch (the node's role and its stem peers), the routes from each
//! incoming peer to a stem peer, and the **stem pool**: transactions in their stem, kept apart from the mempool, never
//! announced and never served, each with its embargo timer.
//!
//! Every random choice is SHA-256 of a secret (made at start) with the epoch and what is chosen: a peer cannot predict a
//! node's role, its stem peers or its timers from what it sees, and a simulation is still a function of its seed.

use std::collections::{BTreeMap, HashMap};

use tenero_core::hash::sha256;
use tenero_core::v3::Transaction;

use crate::engine::PeerId;

/// The parameters (`docs/DANDELION_PLAN.md`, "Parameters").
#[derive(Clone, Debug)]
pub struct DandelionConfig {
    /// Off: every transaction is announced at once, as before version 3 (a peer's stem transactions are fluffed).
    pub enabled: bool,
    /// How long a node keeps its role and its stem peers.
    pub epoch_ms: u64,
    /// The chance, in percent, that a node is a diffuser for an epoch (fluffs what it is given).
    pub fluff_percent: u64,
    /// How many outbound peers a node stems to.
    pub stems: usize,
    /// The embargo: a stem transaction not announced to the node within this (exponential, this mean) is fluffed by it.
    pub embargo_mean_ms: u64,
    pub embargo_max_ms: u64,
    /// The most transactions held in the stem pool; past it a new one is fluffed.
    pub max_stem_txs: usize,
    /// The secret every choice is drawn from. `tenerod` makes it from the operating system at start.
    pub secret: [u8; 32],
}

impl Default for DandelionConfig {
    fn default() -> DandelionConfig {
        DandelionConfig {
            enabled: true,
            epoch_ms: 10 * 60 * 1000,
            fluff_percent: 20,
            stems: 2,
            embargo_mean_ms: 39_000,
            embargo_max_ms: 4 * 60 * 1000,
            max_stem_txs: 1024,
            secret: [0x5d; 32],
        }
    }
}

/// A transaction in its stem, held here.
pub struct StemEntry {
    pub tx: Transaction,
    /// When the node fluffs it itself if nobody has announced it (the engine's clock, ms).
    pub embargo_at: u64,
}

/// What a node knows of its Dandelion++ state.
pub struct Dandelion {
    pub cfg: DandelionConfig,
    /// The epoch: its number, when it started, the role, and the stem peers chosen for it.
    epoch: u64,
    epoch_start: Option<u64>,
    diffuser: bool,
    stems: Vec<PeerId>,
    /// Incoming peer -> the stem peer its stem transactions go to this epoch (`None`: the node's own transactions).
    routes: HashMap<Option<PeerId>, PeerId>,
    pub pool: BTreeMap<[u8; 32], StemEntry>,
}

impl Dandelion {
    pub fn new(cfg: DandelionConfig) -> Dandelion {
        Dandelion {
            cfg,
            epoch: 0,
            epoch_start: None,
            diffuser: false,
            stems: Vec::new(),
            routes: HashMap::new(),
            pool: BTreeMap::new(),
        }
    }

    /// A random number for `what`, from the secret: the same inputs always give the same number.
    fn draw(&self, what: &[u8], extra: &[u8]) -> u64 {
        let h = sha256(&[
            &self.cfg.secret,
            b"tenero dandelion++",
            what,
            &self.epoch.to_le_bytes(),
            extra,
        ]);
        u64::from_le_bytes(h[..8].try_into().expect("8 bytes"))
    }

    /// Starts a new epoch when the last one is over, when there is none yet, or when it has no stem peer and there are
    /// `candidates` now (a node that just started, or lost its peers). `candidates` are the peers that may be stem
    /// peers (the engine's ready outbound peers), in a stable order.
    pub fn refresh(&mut self, now: u64, candidates: &[PeerId]) {
        let over = self
            .epoch_start
            .is_none_or(|s| now >= s.saturating_add(self.cfg.epoch_ms));
        if !over && !(self.stems.is_empty() && !candidates.is_empty()) {
            return;
        }
        self.epoch += 1;
        self.epoch_start = Some(now);
        self.diffuser = self.draw(b"role", &[]) % 100 < self.cfg.fluff_percent;
        self.routes.clear();
        self.stems.clear();
        let mut pool: Vec<PeerId> = candidates.to_vec();
        while self.stems.len() < self.cfg.stems && !pool.is_empty() {
            let i = (self.draw(b"stem", &(self.stems.len() as u64).to_le_bytes())
                % pool.len() as u64) as usize;
            self.stems.push(pool.swap_remove(i));
        }
    }

    /// A number in `0..n` (`n` positive) for `what` and `key`, this epoch: for a choice the engine makes itself.
    pub fn pick(&self, what: &[u8], key: u64, n: usize) -> usize {
        (self.draw(what, &key.to_le_bytes()) % n as u64) as usize
    }

    pub fn is_diffuser(&self) -> bool {
        self.diffuser
    }

    pub fn stems(&self) -> &[PeerId] {
        &self.stems
    }

    /// A peer went away: it is no longer a stem peer (another is chosen from `candidates`, which no longer hold it), and
    /// no route leads from it or to it.
    pub fn peer_gone(&mut self, peer: PeerId, candidates: &[PeerId]) {
        self.routes
            .retain(|from, to| *from != Some(peer) && *to != peer);
        if let Some(i) = self.stems.iter().position(|p| *p == peer) {
            self.stems.remove(i);
            let free: Vec<PeerId> = candidates
                .iter()
                .copied()
                .filter(|c| !self.stems.contains(c))
                .collect();
            if !free.is_empty() {
                let j = (self.draw(b"replace", &peer.to_le_bytes()) % free.len() as u64) as usize;
                self.stems.push(free[j]);
            }
        }
    }

    /// The stem peer for what comes from `from` (`None`: the node's own): the same one all epoch, never `from` itself.
    /// `None` when there is no stem peer to use (the transaction is then fluffed).
    pub fn route(&mut self, from: Option<PeerId>) -> Option<PeerId> {
        if let Some(to) = self.routes.get(&from) {
            return Some(*to);
        }
        let choices: Vec<PeerId> = self
            .stems
            .iter()
            .copied()
            .filter(|s| Some(*s) != from)
            .collect();
        if choices.is_empty() {
            return None;
        }
        let key = from.unwrap_or(u64::MAX);
        let to = choices[(self.draw(b"route", &key.to_le_bytes()) % choices.len() as u64) as usize];
        self.routes.insert(from, to);
        Some(to)
    }

    /// When a stem transaction `id` that arrived `now` is fluffed if nobody announces it: exponential, mean
    /// `embargo_mean_ms`, at most `embargo_max_ms`.
    pub fn embargo_at(&self, now: u64, id: &[u8; 32]) -> u64 {
        // a uniform number in (0, 1], from 53 random bits
        let u = ((self.draw(b"embargo", id) >> 11) + 1) as f64 / (1u64 << 53) as f64;
        let delay = (-u.ln() * self.cfg.embargo_mean_ms as f64) as u64;
        now + delay.min(self.cfg.embargo_max_ms)
    }

    /// Whether a transaction spending any of these key images is already held in the stem pool.
    pub fn conflicts(&self, tx: &Transaction) -> bool {
        self.pool.values().any(|e| {
            e.tx.prefix
                .inputs
                .iter()
                .any(|i| tx.prefix.inputs.iter().any(|j| i.key_image == j.key_image))
        })
    }

    /// The transactions whose embargo has run out by `now`, taken out of the stem pool.
    pub fn take_expired(&mut self, now: u64) -> Vec<Transaction> {
        let due: Vec<[u8; 32]> = self
            .pool
            .iter()
            .filter(|(_, e)| e.embargo_at <= now)
            .map(|(id, _)| *id)
            .collect();
        due.into_iter()
            .filter_map(|id| self.pool.remove(&id).map(|e| e.tx))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(fluff: u64, secret: u8) -> Dandelion {
        Dandelion::new(DandelionConfig {
            fluff_percent: fluff,
            secret: [secret; 32],
            ..DandelionConfig::default()
        })
    }

    #[test]
    fn an_epoch_keeps_its_role_stems_and_routes_until_it_ends() {
        let mut x = d(20, 1);
        x.refresh(0, &[10, 11, 12, 13]);
        let (stems, role) = (x.stems().to_vec(), x.is_diffuser());
        assert_eq!(stems.len(), 2);
        let r = x.route(Some(50)).unwrap();
        assert!(stems.contains(&r));
        // the same epoch: nothing changes, whatever the candidates now
        x.refresh(9 * 60 * 1000, &[20, 21]);
        assert_eq!((x.stems(), x.is_diffuser()), (&stems[..], role));
        assert_eq!(x.route(Some(50)), Some(r));
        // a new one: chosen again
        x.refresh(10 * 60 * 1000, &[20, 21, 22]);
        assert!(x.stems().iter().all(|s| [20, 21, 22].contains(s)));
    }

    #[test]
    fn a_route_never_goes_back_to_where_it_came_from() {
        let mut x = d(0, 2);
        x.refresh(0, &[10, 11]);
        let s = x.stems().to_vec();
        assert_eq!(x.route(Some(s[0])), Some(s[1]));
        assert_eq!(x.route(Some(s[1])), Some(s[0]));
        // with one stem peer, what came from it has nowhere to go: it is fluffed
        let mut y = d(0, 3);
        y.refresh(0, &[10]);
        assert_eq!(y.route(Some(10)), None);
        assert_eq!(y.route(None), Some(10));
        // and with none, nothing goes anywhere
        let mut z = d(0, 4);
        z.refresh(0, &[]);
        assert_eq!(z.route(None), None);
    }

    #[test]
    fn a_lost_stem_peer_is_replaced_and_its_routes_forgotten() {
        let mut x = d(0, 5);
        x.refresh(0, &[10, 11, 12]);
        let gone = x.route(None).unwrap();
        let left: Vec<PeerId> = [10, 11, 12].into_iter().filter(|p| *p != gone).collect();
        x.peer_gone(gone, &left);
        assert_eq!(x.stems().len(), 2);
        assert!(!x.stems().contains(&gone));
        assert_ne!(x.route(None), Some(gone));
    }

    #[test]
    fn a_node_with_no_stem_peer_starts_an_epoch_as_soon_as_it_has_one() {
        let mut x = d(0, 6);
        x.refresh(0, &[]);
        assert!(x.stems().is_empty());
        x.refresh(1000, &[10]);
        assert_eq!(x.stems(), &[10]);
    }

    #[test]
    fn about_a_fifth_of_epochs_are_diffusers_and_embargoes_average_39_seconds() {
        let mut x = d(20, 7);
        let mut diffusers = 0;
        for e in 0..2000u64 {
            x.refresh(e * 600_000, &[1, 2]);
            diffusers += u64::from(x.is_diffuser());
        }
        assert!((300..500).contains(&diffusers), "{diffusers} of 2000");
        let n = 5000u64;
        let total: u64 = (0..n)
            .map(|i| {
                let mut id = [0u8; 32];
                id[..8].copy_from_slice(&i.to_le_bytes());
                x.embargo_at(0, &id)
            })
            .sum();
        let mean = total / n;
        assert!((35_000..43_000).contains(&mean), "{mean} ms");
        assert!((0..n).all(|i| {
            let mut id = [1u8; 32];
            id[..8].copy_from_slice(&i.to_le_bytes());
            x.embargo_at(0, &id) <= 240_000
        }));
    }

    #[test]
    fn two_secrets_choose_differently() {
        let (mut a, mut b) = (d(50, 8), d(50, 9));
        let mut same = 0;
        for e in 0..64u64 {
            a.refresh(e * 600_000, &[1, 2, 3, 4, 5, 6]);
            b.refresh(e * 600_000, &[1, 2, 3, 4, 5, 6]);
            same += u64::from(a.stems() == b.stems() && a.is_diffuser() == b.is_diffuser());
        }
        assert!(same < 16, "{same} of 64 epochs the same");
    }
}
