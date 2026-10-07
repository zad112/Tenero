//! The pool's bookkeeping, with no network in it: the share targets, the nonce prefixes, the adjusting of a miner's difficulty, the
//! PPLNS accounts and the file they are kept in. **Experimental and unaudited; nothing on any network it serves has value.**
//!
//! What it decides, and the rules (`docs/POOL_PROTOCOL.md`, `docs/RUNNING_A_POOL.md`):
//!
//! * **A share's worth** is the work its target stands for: `floor(2^256 / share target)` attempts, so a share at a target 16 times
//!   easier than the block's is worth a sixteenth of a block's work, whatever the miner's speed.
//! * **PPLNS** (pay per last N shares, the owner's choice of 2026-10-07): when a block is found, its reward goes to the miners of the
//!   last shares whose work adds up to `window_factor` times the block's work, in proportion to the work of each, **whatever rounds the
//!   shares fell in**. The pool's fee comes off first. The shares are kept across blocks (a window slides, it is not emptied).
//! * **A block's reward is credited in two steps.** When the block is found, who gets what is fixed (a snapshot of the window) and kept
//!   as *pending*. Only when the block is `maturity` blocks deep **and still the block of the chain at its height** are the credits added
//!   to the balances; a block another one has taken the place of pays nobody, because the pool never received the coins.
//! * **Payouts** are for balances of at least the minimum, at most once an interval (a time kept in the file, so a restart does not
//!   bring a payout forward).

use std::collections::{BTreeMap, HashMap, VecDeque};

use tenero_core::hash::sha256;
use tenero_core::u256::{U256, U320};
use tenero_core::v2::codec::{DecodeError, EncodeError, Reader, Writer};
use tenero_wallet::Address;

/// The most miners (addresses) the pool keeps accounts for.
pub const MAX_ADDRESSES: usize = 200_000;
/// The most shares kept in the window.
pub const MAX_WINDOW: usize = 2_000_000;
/// The most blocks waiting to mature.
pub const MAX_PENDING: usize = 4096;
/// The most miners one block's credits list.
pub const MAX_CREDITS: usize = MAX_ADDRESSES;

/// A miner's number in the pool's books.
pub type AddrId = u32;

// ---- share targets and their worth ---------------------------------------------------------------------------------------

/// The share target is never easier than this (2^255: half of all attempts would be shares, which counts nothing).
pub fn easiest_share_target() -> U256 {
    U256::pow2(255).expect("2^255 fits")
}

/// The most the ratio of a share target to the block target may be.
pub const MAX_RATIO: u64 = 1 << 40;

/// The target of a share at `ratio` times the block target (ratio 1: the share is a block), never easier than 2^255 and never zero.
pub fn share_target(block_target: &U256, ratio: u64) -> U256 {
    let ratio = ratio.clamp(1, MAX_RATIO);
    let t = U320::from_u256(block_target)
        .checked_mul_u64(ratio)
        .and_then(|x| x.to_u256())
        .unwrap_or_else(easiest_share_target);
    let cap = easiest_share_target();
    let t = if t > cap { cap } else { t };
    if t == U256::ZERO {
        U256::ONE
    } else {
        t
    }
}

/// What a share at this target is worth: the attempts it stands for (`floor(2^256 / target)`), at most `u64::MAX`.
pub fn work_of(target: &U256) -> u64 {
    let Some(w) = U256::work_of_target(target) else {
        return u64::MAX;
    };
    let b = w.to_be_bytes();
    if b[..24].iter().any(|x| *x != 0) {
        u64::MAX
    } else {
        u64::from_be_bytes(b[24..].try_into().expect("8 bytes"))
    }
}

// ---- nonce prefixes ----------------------------------------------------------------------------------------------------

/// Hands out each connected miner its own slice of the nonce space: the top `bits` bits of the nonce are fixed to the miner's prefix.
/// Two miners connected at once never have the same prefix, so no work is searched twice.
pub struct Prefixes {
    bits: u8,
    taken: Vec<bool>,
    next: usize,
}

impl Prefixes {
    /// Room for at least `max_miners` miners at once: the fewest bits that give that many prefixes (and at least one bit).
    pub fn for_miners(max_miners: usize) -> Prefixes {
        let mut bits = 1u8;
        while (1usize << bits) < max_miners.max(2) && bits < 24 {
            bits += 1;
        }
        Prefixes {
            bits,
            taken: vec![false; 1usize << bits],
            next: 0,
        }
    }

    pub fn bits(&self) -> u8 {
        self.bits
    }

    pub fn capacity(&self) -> usize {
        self.taken.len()
    }

    pub fn in_use(&self) -> usize {
        self.taken.iter().filter(|t| **t).count()
    }

    /// A free prefix, or `None` when every one is in use.
    pub fn take(&mut self) -> Option<u64> {
        let n = self.taken.len();
        for k in 0..n {
            let i = (self.next + k) % n;
            if !self.taken[i] {
                self.taken[i] = true;
                self.next = (i + 1) % n;
                return Some(i as u64);
            }
        }
        None
    }

    pub fn give_back(&mut self, prefix: u64) {
        if let Some(t) = self.taken.get_mut(prefix as usize) {
            *t = false;
        }
    }
}

/// The first nonce of the slice a prefix owns.
pub fn first_nonce_of(prefix: u64, bits: u8) -> u64 {
    if bits == 0 {
        0
    } else {
        prefix << (64 - u32::from(bits))
    }
}

/// Whether a nonce is inside the slice of `prefix` (the top `bits` bits equal it).
pub fn nonce_in_prefix(nonce: u64, prefix: u64, bits: u8) -> bool {
    bits == 0 || nonce >> (64 - u32::from(bits)) == prefix
}

// ---- adjusting a miner's difficulty ---------------------------------------------------------------------------------------

/// How often (seconds) a miner's ratio is looked at.
pub const RETARGET_SECS: u64 = 30;
/// The pool aims at a share about this often (seconds): often enough to count the work, rarely enough not to flood the pool.
pub const DESIRED_SHARE_SECS: u64 = 15;

/// One miner's difficulty: its share target is the block target times `ratio`. After each [`RETARGET_SECS`] it moves by the factor by which the
/// shares came faster or slower than [`DESIRED_SHARE_SECS`], at most 4 times either way, so a miner finds its level in a few minutes.
#[derive(Clone, Debug)]
pub struct VarDiff {
    pub ratio: u64,
    window_start: u64,
    shares: u64,
}

impl VarDiff {
    pub fn new(ratio: u64, now_secs: u64) -> VarDiff {
        VarDiff {
            ratio: ratio.clamp(1, MAX_RATIO),
            window_start: now_secs,
            shares: 0,
        }
    }

    /// A share was accepted.
    pub fn share(&mut self) {
        self.shares += 1;
    }

    /// Looks at the shares of the last window if it is over. `Some(new ratio)` when the ratio changed.
    pub fn tick(&mut self, now_secs: u64) -> Option<u64> {
        let elapsed = now_secs.saturating_sub(self.window_start);
        if elapsed < RETARGET_SECS {
            return None;
        }
        // seconds per share seen, against the wanted: a miner whose shares come too fast gets a harder target (a smaller ratio)
        let factor = if self.shares == 0 {
            4.0
        } else {
            ((elapsed as f64 / self.shares as f64) / DESIRED_SHARE_SECS as f64).clamp(0.25, 4.0)
        };
        self.window_start = now_secs;
        self.shares = 0;
        let new = ((self.ratio as f64 * factor).round() as u64).clamp(1, MAX_RATIO);
        // a change under 10% is not worth telling the miner about
        let change = new.abs_diff(self.ratio) as f64 / self.ratio as f64;
        if change < 0.10 {
            return None;
        }
        self.ratio = new;
        Some(new)
    }
}

// ---- the accounts -------------------------------------------------------------------------------------------------------

/// A block the pool found, whose reward is waiting to mature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingBlock {
    pub height: u64,
    pub id: [u8; 32],
    /// The whole reward of the block (what its coinbase pays the pool).
    pub reward: u64,
    /// Who is owed what, fixed when the block was found.
    pub credits: Vec<(AddrId, u64)>,
}

/// What a payout owes someone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owed {
    pub id: AddrId,
    pub address: Address,
    pub amount: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AccountError {
    TooManyAddresses,
    Format(String),
}

impl std::fmt::Display for AccountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AccountError::TooManyAddresses => write!(
                f,
                "the pool keeps accounts for {MAX_ADDRESSES} addresses at most"
            ),
            AccountError::Format(e) => write!(f, "the pool's state file is not valid: {e}"),
        }
    }
}

impl std::error::Error for AccountError {}

/// Everything the pool owes and has done, kept in a file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Accounts {
    addrs: Vec<Address>,
    index: HashMap<String, AddrId>,
    balances: Vec<u64>,
    paid: Vec<u64>,
    /// The newest share is at the back.
    window: VecDeque<(AddrId, u64)>,
    window_sum: u128,
    pending: Vec<PendingBlock>,
    /// When the next payout may be made, seconds since 1970 (0: now).
    pub next_payout: u64,
    pub blocks_found: u64,
    pub shares_accepted: u64,
    pub total_paid: u64,
}

impl Accounts {
    pub fn new() -> Accounts {
        Accounts::default()
    }

    /// The miner's number, adding it to the books if it is new.
    pub fn id_of(&mut self, address: &Address) -> Result<AddrId, AccountError> {
        let text = address.to_text();
        if let Some(id) = self.index.get(&text) {
            return Ok(*id);
        }
        if self.addrs.len() >= MAX_ADDRESSES {
            return Err(AccountError::TooManyAddresses);
        }
        let id = self.addrs.len() as AddrId;
        self.addrs.push(*address);
        self.balances.push(0);
        self.paid.push(0);
        self.index.insert(text, id);
        Ok(id)
    }

    pub fn address(&self, id: AddrId) -> Option<&Address> {
        self.addrs.get(id as usize)
    }

    pub fn addresses(&self) -> usize {
        self.addrs.len()
    }

    pub fn balance(&self, id: AddrId) -> u64 {
        self.balances.get(id as usize).copied().unwrap_or(0)
    }

    pub fn paid(&self, id: AddrId) -> u64 {
        self.paid.get(id as usize).copied().unwrap_or(0)
    }

    pub fn total_balances(&self) -> u128 {
        self.balances.iter().map(|b| u128::from(*b)).sum()
    }

    pub fn pending(&self) -> &[PendingBlock] {
        &self.pending
    }

    pub fn window_len(&self) -> usize {
        self.window.len()
    }

    pub fn window_work(&self) -> u128 {
        self.window_sum
    }

    /// An accepted share worth `weight` (see [`work_of`]); the window keeps the newest shares that add up to `need` work, plus the one
    /// that crosses it.
    pub fn add_share(&mut self, id: AddrId, weight: u64, need: u128) {
        self.shares_accepted += 1;
        self.window.push_back((id, weight));
        self.window_sum += u128::from(weight);
        while let Some(&(_, w)) = self.window.front() {
            if self.window_sum - u128::from(w) >= need && self.window.len() > 1 {
                self.window.pop_front();
                self.window_sum -= u128::from(w);
            } else {
                break;
            }
        }
        while self.window.len() > MAX_WINDOW {
            if let Some((_, w)) = self.window.pop_front() {
                self.window_sum -= u128::from(w);
            }
        }
    }

    /// A block was found: fixes who is owed what for it (the newest shares adding up to `need` work, the pool's fee (`fee_ppm`, parts per million of the reward) taken off
    /// first) and keeps that as pending until the block matures. Returns the credits.
    pub fn block_found(
        &mut self,
        height: u64,
        id: [u8; 32],
        reward: u64,
        fee_ppm: u64,
        need: u128,
    ) -> Vec<(AddrId, u64)> {
        self.blocks_found += 1;
        // rounded down: the odd unit goes to the miners, not to the pool
        let fee =
            (u128::from(reward) * u128::from(fee_ppm.min(FEE_ALL)) / u128::from(FEE_ALL)) as u64;
        let net = reward - fee;
        // the newest shares first, until they add up to `need`: the last one counts only for what is still missing
        let mut weights: BTreeMap<AddrId, u128> = BTreeMap::new();
        let mut total: u128 = 0;
        for &(who, w) in self.window.iter().rev() {
            if total >= need {
                break;
            }
            let take = u128::from(w).min(need - total);
            *weights.entry(who).or_default() += take;
            total += take;
        }
        let mut credits: Vec<(AddrId, u64)> = Vec::new();
        // (with no work in the window there are no weights, and nothing below does anything)
        {
            let mut given: u64 = 0;
            for (who, w) in &weights {
                let c = (u128::from(net) * *w).checked_div(total).unwrap_or(0) as u64;
                given += c;
                credits.push((*who, c));
            }
            // what rounding left over goes to the miner with the most work, so the credits add up to exactly the reward less the fee
            if let Some(top) = weights
                .iter()
                .max_by_key(|(who, w)| (**w, std::cmp::Reverse(**who)))
                .map(|(who, _)| *who)
            {
                if let Some(c) = credits.iter_mut().find(|(who, _)| *who == top) {
                    c.1 += net - given;
                }
            }
            credits.retain(|(_, c)| *c > 0);
        }
        self.pending.push(PendingBlock {
            height,
            id,
            reward,
            credits: credits.clone(),
        });
        while self.pending.len() > MAX_PENDING {
            self.pending.remove(0);
        }
        credits
    }

    /// Settles the pending blocks that are `maturity` deep at `tip_height`: a block that is still the chain's at its height gets its
    /// credits added to the balances, and any other pays nobody. `id_at` says which block the chain has at a height. Returns
    /// `(blocks credited, blocks lost)`.
    pub fn settle(
        &mut self,
        tip_height: u64,
        maturity: u64,
        id_at: &mut dyn FnMut(u64) -> Option<[u8; 32]>,
    ) -> (usize, usize) {
        let (mut credited, mut lost) = (0, 0);
        let mut keep = Vec::new();
        for p in std::mem::take(&mut self.pending) {
            if tip_height < p.height.saturating_add(maturity) {
                keep.push(p);
                continue;
            }
            match id_at(p.height) {
                Some(id) if id == p.id => {
                    for (who, c) in &p.credits {
                        if let Some(b) = self.balances.get_mut(*who as usize) {
                            *b = b.saturating_add(*c);
                        }
                    }
                    credited += 1;
                }
                Some(_) => lost += 1,
                // the node could not say: ask again later
                None => keep.push(p),
            }
        }
        self.pending = keep;
        (credited, lost)
    }

    /// Who is owed at least `min` (largest first), at most `max` of them.
    pub fn due(&self, min: u64, max: usize) -> Vec<Owed> {
        let mut v: Vec<Owed> = self
            .balances
            .iter()
            .enumerate()
            .filter(|(_, b)| **b >= min && **b > 0)
            .map(|(i, b)| Owed {
                id: i as AddrId,
                address: self.addrs[i],
                amount: *b,
            })
            .collect();
        v.sort_by(|a, b| b.amount.cmp(&a.amount).then(a.id.cmp(&b.id)));
        v.truncate(max);
        v
    }

    /// `amount` was paid to `id`: takes it off what is owed. A payment bigger than the balance is a bug, and takes the balance to zero.
    pub fn debit(&mut self, id: AddrId, amount: u64) {
        if let Some(b) = self.balances.get_mut(id as usize) {
            *b = b.saturating_sub(amount);
        }
        if let Some(p) = self.paid.get_mut(id as usize) {
            *p = p.saturating_add(amount);
        }
        self.total_paid = self.total_paid.saturating_add(amount);
    }

    pub fn id_by_text(&self, text: &str) -> Option<AddrId> {
        self.index.get(text).copied()
    }

    // ---- the file ----

    const MAGIC: &'static [u8] = b"tenero pool state v1\n";

    /// The state as bytes: a header, the books, and a SHA-256 of all of it so that a damaged file is noticed.
    pub fn to_bytes(&self) -> Result<Vec<u8>, EncodeError> {
        let mut w = Writer::new();
        w.raw(Self::MAGIC);
        w.count(self.addrs.len(), 0, MAX_ADDRESSES)?;
        for (i, a) in self.addrs.iter().enumerate() {
            w.var(a.to_text().as_bytes(), 256)?;
            w.u64(self.balances[i]);
            w.u64(self.paid[i]);
        }
        w.count(self.window.len(), 0, MAX_WINDOW)?;
        for (who, weight) in &self.window {
            w.u32(*who);
            w.u64(*weight);
        }
        w.count(self.pending.len(), 0, MAX_PENDING)?;
        for p in &self.pending {
            w.u64(p.height);
            w.raw(&p.id);
            w.u64(p.reward);
            w.count(p.credits.len(), 0, MAX_CREDITS)?;
            for (who, c) in &p.credits {
                w.u32(*who);
                w.u64(*c);
            }
        }
        w.u64(self.next_payout);
        w.u64(self.blocks_found);
        w.u64(self.shares_accepted);
        w.u64(self.total_paid);
        let mut bytes = w.into_bytes();
        let sum = sha256(&[&bytes]);
        bytes.extend_from_slice(&sum);
        Ok(bytes)
    }

    pub fn from_bytes(data: &[u8]) -> Result<Accounts, AccountError> {
        let bad = |e: DecodeError| AccountError::Format(e.as_str().to_string());
        if data.len() < 32 + Self::MAGIC.len() {
            return Err(AccountError::Format("too short".into()));
        }
        let (body, sum) = data.split_at(data.len() - 32);
        if sha256(&[body]) != sum {
            return Err(AccountError::Format("the checksum does not match".into()));
        }
        let mut r = Reader::new(body);
        if r.take(Self::MAGIC.len()).map_err(bad)? != Self::MAGIC {
            return Err(AccountError::Format("not a pool state file".into()));
        }
        let mut a = Accounts::new();
        let n = r.count(0, MAX_ADDRESSES).map_err(bad)?;
        for _ in 0..n {
            let text = String::from_utf8(r.var(256).map_err(bad)?)
                .map_err(|_| AccountError::Format("an address that is not text".into()))?;
            let address = Address::from_text(&text)
                .map_err(|e| AccountError::Format(format!("an address: {e}")))?;
            let (balance, paid) = (r.u64().map_err(bad)?, r.u64().map_err(bad)?);
            let id = a.id_of(&address)?;
            if id as usize != a.addrs.len() - 1 {
                return Err(AccountError::Format("an address listed twice".into()));
            }
            a.balances[id as usize] = balance;
            a.paid[id as usize] = paid;
        }
        let wn = r.count(0, MAX_WINDOW).map_err(bad)?;
        for _ in 0..wn {
            let (who, weight) = (r.u32().map_err(bad)?, r.u64().map_err(bad)?);
            if who as usize >= a.addrs.len() {
                return Err(AccountError::Format("a share of an unknown miner".into()));
            }
            a.window.push_back((who, weight));
            a.window_sum += u128::from(weight);
        }
        let pn = r.count(0, MAX_PENDING).map_err(bad)?;
        for _ in 0..pn {
            let height = r.u64().map_err(bad)?;
            let id: [u8; 32] = r.array().map_err(bad)?;
            let reward = r.u64().map_err(bad)?;
            let cn = r.count(0, MAX_CREDITS).map_err(bad)?;
            let mut credits = Vec::with_capacity(cn.min(1024));
            for _ in 0..cn {
                let (who, c) = (r.u32().map_err(bad)?, r.u64().map_err(bad)?);
                if who as usize >= a.addrs.len() {
                    return Err(AccountError::Format("a credit to an unknown miner".into()));
                }
                credits.push((who, c));
            }
            a.pending.push(PendingBlock {
                height,
                id,
                reward,
                credits,
            });
        }
        a.next_payout = r.u64().map_err(bad)?;
        a.blocks_found = r.u64().map_err(bad)?;
        a.shares_accepted = r.u64().map_err(bad)?;
        a.total_paid = r.u64().map_err(bad)?;
        r.finish().map_err(bad)?;
        Ok(a)
    }
}

/// One percent of a block's reward, in the unit the pool's fee is kept in: parts per million of the reward. So 0.5 % is 5,000 and
/// the smallest step is 0.0001 % (one part in a million). Whole percents were the only fee the first version could take.
pub const FEE_ONE_PERCENT: u64 = 10_000;
/// The whole reward (100 %), in the same unit.
pub const FEE_ALL: u64 = 1_000_000;

/// A fee as a person writes it, a percentage of at most four decimals (`0`, `1`, `0.5`, `0.05`, `0.0001`), in parts per million of the
/// reward. `None` for anything else: a sign, an exponent, a comma, more than four decimals, a missing digit (`.5`, `5.`), more than 100.
pub fn parse_fee_percent(text: &str) -> Option<u64> {
    let (whole, frac) = text.split_once('.').unwrap_or((text, ""));
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty()
        || !digits(whole)
        || !digits(frac)
        || frac.len() > 4
        || (text.contains('.') && frac.is_empty())
    {
        return None;
    }
    let mut ppm = whole.parse::<u64>().ok()?.checked_mul(FEE_ONE_PERCENT)?;
    let mut step = FEE_ONE_PERCENT;
    for b in frac.bytes() {
        step /= 10;
        ppm = ppm.checked_add(u64::from(b - b'0') * step)?;
    }
    (ppm <= FEE_ALL).then_some(ppm)
}

/// A fee in parts per million written as a percentage, for the log and the screen: `0`, `0.5`, `0.05`, `1`, no trailing zeros.
pub fn fee_percent_text(ppm: u64) -> String {
    let ppm = ppm.min(FEE_ALL);
    let (whole, frac) = (ppm / FEE_ONE_PERCENT, ppm % FEE_ONE_PERCENT);
    if frac == 0 {
        return whole.to_string();
    }
    let f = format!("{frac:04}");
    format!("{whole}.{}", f.trim_end_matches('0'))
}
