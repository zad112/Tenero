//! A purse: several accounts from one seed, in one encrypted file.
//!
//! An account is an ordinary one-account [`Wallet`] whose seed is derived from the purse's *master seed*, so all the
//! scanning, balance and payment code is the code that already exists and is tested. **Account 0 is the master seed
//! itself**: a wallet made before purses existed is a purse with one account, and its file still opens.
//!
//! * The master seed is written as 24 words ([`crate::mnemonic`]) and restores every account.
//! * Account `i > 0` has the seed `SHA-256("tenero account v1" || master || i as u32 LE)`.
//! * **Accounts are separate wallets, not subaddresses.** Each has its own address, its own scan state and its own
//!   balance, and **a payment spends from one account only**. Nothing on the chain says two accounts belong together
//!   (no more and no less than for two wallets), except what scanning costs you: each account is scanned
//!   separately, so a purse with ten accounts reads the chain ten times.
//! * Restoring from the words does not know how many accounts there were: [`Purse::discover`] scans account after
//!   account until [`GAP`] in a row have never received anything. An account used later than a gap of that many
//!   empty ones would be missed, and the app can add it by hand.
//! * Account names are kept in the file only (the words do not carry them).
//!
//! The output scheme is still the INTERIM one: this is not Carrot, and the accounts here are not Carrot subaddresses.

use std::path::Path;

use rand_core::{CryptoRng, RngCore};
use tenero_core::hash::sha256;
use tenero_core::v2::{Reader, Writer};
use zeroize::Zeroizing;

use crate::chain::{ChainView, Submitter};
use crate::file::{open, seal, FileError, KdfParams, MAGIC_PURSE};
use crate::interim::Address;
use crate::wallet::{Balance, Built, FeeLevel, SyncReport, Wallet, WalletError};

/// The most accounts one purse holds.
pub const MAX_ACCOUNTS: usize = 64;
/// The longest account name, in bytes.
pub const MAX_LABEL: usize = 48;
/// How many unused accounts in a row end the search on a restore.
pub const GAP: usize = 3;

/// Version 2 added the record of sent payments after the accounts; version 1 files still open (with no records).
const PURSE_VERSION: u16 = 2;
/// The most sent-payment records a purse keeps (the oldest are dropped past this).
pub const MAX_SENT_RECORDS: usize = 20_000;
const MAX_SPENDS: usize = 64;
const MAX_WALLET_STATE: usize = 256 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum PurseError {
    /// The purse already has [`MAX_ACCOUNTS`].
    TooManyAccounts,
    /// No account with that number.
    NoSuchAccount(usize),
    /// A name that is empty, longer than [`MAX_LABEL`] bytes, or holds a control character.
    BadLabel,
    Wallet(WalletError),
}

impl std::fmt::Display for PurseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PurseError::TooManyAccounts => {
                write!(f, "a wallet holds at most {MAX_ACCOUNTS} accounts")
            }
            PurseError::NoSuchAccount(i) => write!(f, "there is no account {i}"),
            PurseError::BadLabel => write!(
                f,
                "an account name must be 1 to {MAX_LABEL} bytes with no control characters"
            ),
            PurseError::Wallet(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for PurseError {}

impl From<WalletError> for PurseError {
    fn from(e: WalletError) -> Self {
        PurseError::Wallet(e)
    }
}

/// The seed of account `index` of a master seed (account 0 is the master seed).
pub fn account_seed(master: &[u8; 32], index: u32) -> Zeroizing<[u8; 32]> {
    if index == 0 {
        return Zeroizing::new(*master);
    }
    Zeroizing::new(sha256(&[
        b"tenero account v1",
        master,
        &index.to_le_bytes(),
    ]))
}

fn check_label(label: &str) -> Result<(), PurseError> {
    if label.trim().is_empty() || label.len() > MAX_LABEL || label.chars().any(char::is_control) {
        return Err(PurseError::BadLabel);
    }
    Ok(())
}

/// A payment this wallet sent. Nothing on the chain says to whom a payment went (the interim scheme has no outgoing
/// view key), so the wallet writes it down when it sends; **a wallet restored from the words has no such records**,
/// and its history shows what it received but not whom it paid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentRecord {
    pub account: u32,
    pub id: [u8; 32],
    pub to: Address,
    pub amount: u64,
    pub fee: u64,
    /// Which fee level it was sent at.
    pub level: FeeLevel,
    /// Seconds since 1970, as the computer's clock said when it was sent.
    pub time: u64,
    /// The height the next block would have had when it was sent.
    pub height: u64,
    pub spends: Vec<[u8; 32]>,
    pub change_onetime: [u8; 32],
}

/// Where a sent payment stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SentStatus {
    /// The node has it and no block has taken it yet.
    Pending,
    /// The coins it spent are spent on the chain: a block took it in.
    Confirmed,
    /// Not in the chain and no longer waiting: the node probably dropped it. The coins are free again.
    NotConfirmed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// Coins that arrived from someone else.
    Received,
    /// A block reward.
    Mined,
    Sent {
        to: Address,
        fee: u64,
        status: SentStatus,
        time: u64,
    },
}

/// One line of the history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub account: usize,
    pub kind: EntryKind,
    pub amount: u64,
    /// The block it arrived in, or for a sent payment, the height when it was sent.
    pub height: u64,
    pub id: Option<[u8; 32]>,
}

pub struct Account {
    label: String,
    wallet: Wallet,
}

impl Account {
    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn wallet(&self) -> &Wallet {
        &self.wallet
    }

    pub fn address(&self) -> Address {
        self.wallet.address()
    }

    /// Whether this account has ever received anything.
    pub fn used(&self) -> bool {
        !self.wallet.owned().is_empty()
    }
}

pub struct Purse {
    master: Zeroizing<[u8; 32]>,
    birth_height: u64,
    accounts: Vec<Account>,
    sent: Vec<SentRecord>,
}

impl Purse {
    /// A new purse with a fresh random master seed and one account, "Main".
    pub fn create(rng: &mut (impl RngCore + CryptoRng), birth_height: u64) -> Purse {
        let mut seed = Zeroizing::new([0u8; 32]);
        rng.fill_bytes(&mut *seed);
        Purse::from_seed(&seed, birth_height)
    }

    /// The purse of a master seed, with account 0 only (see [`Purse::discover`] to find the others). To restore,
    /// use a birth height at or before the first coin, or 0.
    pub fn from_seed(master: &[u8; 32], birth_height: u64) -> Purse {
        Purse {
            master: Zeroizing::new(*master),
            birth_height,
            accounts: vec![Account {
                label: "Main".to_string(),
                wallet: Wallet::from_seed(master, birth_height),
            }],
            sent: Vec::new(),
        }
    }

    /// One account of an existing one-account wallet (how an old wallet file is read).
    fn from_wallet(wallet: Wallet) -> Purse {
        Purse {
            master: Zeroizing::new(*wallet.seed()),
            birth_height: wallet.birth_height(),
            accounts: vec![Account {
                label: "Main".to_string(),
                wallet,
            }],
            sent: Vec::new(),
        }
    }

    /// The master seed: the whole secret of every account. Never log it.
    pub fn master_seed(&self) -> &[u8; 32] {
        &self.master
    }

    /// The 24 words of the master seed.
    pub fn phrase(&self) -> Zeroizing<String> {
        crate::mnemonic::phrase_of(&self.master)
    }

    pub fn birth_height(&self) -> u64 {
        self.birth_height
    }

    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }

    pub fn account(&self, index: usize) -> Result<&Account, PurseError> {
        self.accounts
            .get(index)
            .ok_or(PurseError::NoSuchAccount(index))
    }

    /// Adds the next account. It scans from `birth_height`: the chain's tip for an account made now (it cannot hold
    /// older coins), 0 or the purse's own birth height when looking for old ones.
    pub fn add_account(&mut self, label: &str, birth_height: u64) -> Result<usize, PurseError> {
        check_label(label)?;
        if self.accounts.len() >= MAX_ACCOUNTS {
            return Err(PurseError::TooManyAccounts);
        }
        let index = self.accounts.len();
        let seed = account_seed(&self.master, index as u32);
        self.accounts.push(Account {
            label: label.trim().to_string(),
            wallet: Wallet::from_seed(&seed, birth_height),
        });
        Ok(index)
    }

    pub fn rename(&mut self, index: usize, label: &str) -> Result<(), PurseError> {
        check_label(label)?;
        let a = self
            .accounts
            .get_mut(index)
            .ok_or(PurseError::NoSuchAccount(index))?;
        a.label = label.trim().to_string();
        Ok(())
    }

    /// Scans every account up to the chain's tip.
    pub fn sync(&mut self, chain: &impl ChainView) -> Result<SyncReport, PurseError> {
        let mut total = SyncReport::default();
        for a in &mut self.accounts {
            let r = a.wallet.sync(chain)?;
            total.blocks_scanned += r.blocks_scanned;
            total.outputs_found += r.outputs_found;
            total.blocks_rolled_back += r.blocks_rolled_back;
            total.rescanned |= r.rescanned;
        }
        Ok(total)
    }

    /// After a restore: adds accounts, scanning each, until [`GAP`] in a row have never received anything. Names
    /// them "Account N". Returns how many accounts the purse has.
    pub fn discover(&mut self, chain: &impl ChainView) -> Result<usize, PurseError> {
        self.sync(chain)?;
        loop {
            let after_last_used = self.accounts.len()
                - self
                    .accounts
                    .iter()
                    .rposition(Account::used)
                    .map_or(0, |i| i + 1);
            if after_last_used >= GAP {
                return Ok(self.accounts.len());
            }
            for _ in after_last_used..GAP {
                let n = self.accounts.len();
                self.add_account(&format!("Account {n}"), self.birth_height)?;
            }
            self.sync(chain)?;
        }
    }

    pub fn balance(&mut self, index: usize, chain: &impl ChainView) -> Result<Balance, PurseError> {
        let a = self
            .accounts
            .get_mut(index)
            .ok_or(PurseError::NoSuchAccount(index))?;
        Ok(a.wallet.balance(chain)?)
    }

    /// The balances of every account added up.
    pub fn total_balance(&mut self, chain: &impl ChainView) -> Result<Balance, PurseError> {
        let mut t = Balance::default();
        for a in &mut self.accounts {
            let b = a.wallet.balance(chain)?;
            t.total += b.total;
            t.spendable += b.spendable;
            t.immature += b.immature;
            t.reserved += b.reserved;
        }
        Ok(t)
    }

    /// Builds a payment from one account (nothing is sent, nothing reserved). The change returns to the same
    /// account.
    pub fn build_payment(
        &mut self,
        index: usize,
        chain: &impl ChainView,
        rng: &mut (impl RngCore + CryptoRng),
        to: &Address,
        amount: u64,
        level: FeeLevel,
    ) -> Result<Built, PurseError> {
        let a = self
            .accounts
            .get_mut(index)
            .ok_or(PurseError::NoSuchAccount(index))?;
        Ok(a.wallet.build_payment_at(chain, rng, to, amount, level)?)
    }

    /// Sends a payment built by [`Purse::build_payment`]: hands it to the node, reserves its coins and writes it in
    /// the record of sent payments. `now` is the time in seconds since 1970 (the caller's clock).
    pub fn send<C: ChainView + Submitter>(
        &mut self,
        index: usize,
        node: &mut C,
        built: &Built,
        to: &Address,
        level: FeeLevel,
        now: u64,
    ) -> Result<(), PurseError> {
        let height = node
            .rules()
            .map_err(|e| PurseError::Wallet(WalletError::Chain(e)))?
            .next_height;
        let a = self
            .accounts
            .get_mut(index)
            .ok_or(PurseError::NoSuchAccount(index))?;
        a.wallet.send_built(node, built)?;
        self.sent.push(SentRecord {
            account: index as u32,
            id: built.id,
            to: *to,
            amount: built.amount,
            fee: built.fee,
            level,
            time: now,
            height,
            spends: built.spends.clone(),
            change_onetime: built.change_onetime,
        });
        if self.sent.len() > MAX_SENT_RECORDS {
            self.sent.remove(0);
        }
        Ok(())
    }

    /// Builds, sends and records a payment from one account in one step.
    #[allow(clippy::too_many_arguments)]
    pub fn pay<C: ChainView + Submitter>(
        &mut self,
        index: usize,
        node: &mut C,
        rng: &mut (impl RngCore + CryptoRng),
        to: &Address,
        amount: u64,
        level: FeeLevel,
        now: u64,
    ) -> Result<Built, PurseError> {
        let built = self.build_payment(index, &*node, rng, to, amount, level)?;
        self.send(index, node, &built, to, level, now)?;
        Ok(built)
    }

    pub fn sent_records(&self) -> &[SentRecord] {
        &self.sent
    }

    /// Everything the wallet knows happened, newest first: what it received (block rewards marked), and what it sent
    /// with where each payment stands. The change of a payment is not listed as received.
    pub fn history(&self, chain: &impl ChainView) -> Result<Vec<Entry>, PurseError> {
        let chain_err = |e: String| PurseError::Wallet(WalletError::Chain(e));
        let mut out = Vec::new();
        for (i, a) in self.accounts.iter().enumerate() {
            for o in a.wallet.owned() {
                let is_change = self
                    .sent
                    .iter()
                    .any(|r| r.account as usize == i && r.change_onetime == o.onetime_address);
                if is_change {
                    continue;
                }
                out.push(Entry {
                    account: i,
                    kind: if o.coinbase {
                        EntryKind::Mined
                    } else {
                        EntryKind::Received
                    },
                    amount: o.amount,
                    height: o.height,
                    id: None,
                });
            }
        }
        for r in &self.sent {
            let mut spent = false;
            for ki in &r.spends {
                if chain.key_image_spent(ki).map_err(chain_err)? {
                    spent = true;
                    break;
                }
            }
            let waiting = self
                .accounts
                .get(r.account as usize)
                .is_some_and(|a| a.wallet.is_reserved(&r.spends));
            let status = if spent {
                SentStatus::Confirmed
            } else if waiting {
                SentStatus::Pending
            } else {
                SentStatus::NotConfirmed
            };
            out.push(Entry {
                account: r.account as usize,
                kind: EntryKind::Sent {
                    to: r.to,
                    fee: r.fee,
                    status,
                    time: r.time,
                },
                amount: r.amount,
                height: r.height,
                id: Some(r.id),
            });
        }
        out.sort_by_key(|e| std::cmp::Reverse(e.height));
        Ok(out)
    }

    // --------------------------------------------------------------------------------------------
    // the file
    // --------------------------------------------------------------------------------------------

    fn state_bytes(&self) -> Result<Zeroizing<Vec<u8>>, FileError> {
        let bad = |e: tenero_core::v2::EncodeError| FileError::Corrupt(e.to_string());
        let mut w = Writer::new();
        w.u16(PURSE_VERSION);
        w.raw(&*self.master);
        w.u64(self.birth_height);
        w.count(self.accounts.len(), 1, MAX_ACCOUNTS).map_err(bad)?;
        for a in &self.accounts {
            w.var(a.label.as_bytes(), MAX_LABEL).map_err(bad)?;
            let mut inner = Writer::new();
            a.wallet.write_state(&mut inner)?;
            let inner = Zeroizing::new(inner.into_bytes());
            w.var(&inner, MAX_WALLET_STATE).map_err(bad)?;
        }
        w.count(self.sent.len(), 0, MAX_SENT_RECORDS).map_err(bad)?;
        for r in &self.sent {
            w.u32(r.account);
            w.raw(&r.id);
            w.raw(&r.to.spend);
            w.raw(&r.to.view);
            w.u64(r.amount);
            w.u64(r.fee);
            w.raw(&[match r.level {
                FeeLevel::Low => 0,
                FeeLevel::Normal => 1,
                FeeLevel::High => 2,
            }]);
            w.u64(r.time);
            w.u64(r.height);
            w.count(r.spends.len(), 0, MAX_SPENDS).map_err(bad)?;
            for ki in &r.spends {
                w.raw(ki);
            }
            w.raw(&r.change_onetime);
        }
        Ok(Zeroizing::new(w.into_bytes()))
    }

    fn from_state_bytes(data: &[u8]) -> Result<Purse, FileError> {
        let bad = |e: tenero_core::v2::DecodeError| FileError::Corrupt(e.as_str().to_string());
        let mut r = Reader::new(data);
        let version = r.u16().map_err(bad)?;
        if version != 1 && version != PURSE_VERSION {
            return Err(FileError::Corrupt("unknown purse version".into()));
        }
        let master: [u8; 32] = r.array().map_err(bad)?;
        let master = Zeroizing::new(master);
        let birth_height = r.u64().map_err(bad)?;
        let n = r.count(1, MAX_ACCOUNTS).map_err(bad)?;
        let mut accounts = Vec::with_capacity(n);
        for i in 0..n {
            let label = String::from_utf8(r.var(MAX_LABEL).map_err(bad)?)
                .map_err(|_| FileError::Corrupt("an account name is not text".into()))?;
            check_label(&label)
                .map_err(|_| FileError::Corrupt("an account name is not allowed".into()))?;
            let inner = Zeroizing::new(r.var(MAX_WALLET_STATE).map_err(bad)?);
            let mut ir = Reader::new(&inner);
            let wallet = Wallet::read_state(&mut ir)?;
            ir.finish().map_err(bad)?;
            // an account's seed must be the one its number derives from the master seed
            if *wallet.seed() != *account_seed(&master, i as u32) {
                return Err(FileError::Corrupt(
                    "an account does not belong to this seed".into(),
                ));
            }
            accounts.push(Account { label, wallet });
        }
        let mut sent = Vec::new();
        if version >= 2 {
            let m = r.count(0, MAX_SENT_RECORDS).map_err(bad)?;
            for _ in 0..m {
                let account = r.u32().map_err(bad)?;
                if account as usize >= accounts.len() {
                    return Err(FileError::Corrupt(
                        "a sent payment names an account that is not there".into(),
                    ));
                }
                let id: [u8; 32] = r.array().map_err(bad)?;
                let to = Address {
                    spend: r.array().map_err(bad)?,
                    view: r.array().map_err(bad)?,
                };
                let amount = r.u64().map_err(bad)?;
                let fee = r.u64().map_err(bad)?;
                let level = match r.take(1).map_err(bad)?[0] {
                    0 => FeeLevel::Low,
                    1 => FeeLevel::Normal,
                    2 => FeeLevel::High,
                    _ => return Err(FileError::Corrupt("a bad fee level".into())),
                };
                let time = r.u64().map_err(bad)?;
                let height = r.u64().map_err(bad)?;
                let n = r.count(0, MAX_SPENDS).map_err(bad)?;
                let mut spends = Vec::with_capacity(n);
                for _ in 0..n {
                    spends.push(r.array().map_err(bad)?);
                }
                let change_onetime: [u8; 32] = r.array().map_err(bad)?;
                sent.push(SentRecord {
                    account,
                    id,
                    to,
                    amount,
                    fee,
                    level,
                    time,
                    height,
                    spends,
                    change_onetime,
                });
            }
        }
        r.finish().map_err(bad)?;
        Ok(Purse {
            master,
            birth_height,
            accounts,
            sent,
        })
    }

    /// Writes the purse to `path`, encrypted with the passphrase (an empty passphrase is allowed here: the app
    /// decides whether to permit it), replacing any file there atomically.
    pub fn save(
        &self,
        path: &Path,
        passphrase: &[u8],
        kdf: KdfParams,
        rng: &mut (impl RngCore + CryptoRng),
    ) -> Result<(), FileError> {
        let state = self.state_bytes()?;
        seal(path, MAGIC_PURSE, &state, passphrase, kdf, rng)
    }

    /// Reads a purse file, or an older one-account wallet file (which becomes a purse with one account; saving
    /// writes the new format).
    pub fn load(path: &Path, passphrase: &[u8]) -> Result<Purse, FileError> {
        let (magic, plain) = open(path, passphrase)?;
        if &magic == MAGIC_PURSE {
            Purse::from_state_bytes(&plain)
        } else {
            // TWL1: the plaintext is one wallet's state
            let mut r = Reader::new(&plain);
            let w = Wallet::read_state(&mut r)?;
            r.finish()
                .map_err(|e| FileError::Corrupt(e.as_str().to_string()))?;
            Ok(Purse::from_wallet(w))
        }
    }
}
