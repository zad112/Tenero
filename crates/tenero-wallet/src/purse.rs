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
use crate::wallet::{Balance, Built, SyncReport, Wallet, WalletError};

/// The most accounts one purse holds.
pub const MAX_ACCOUNTS: usize = 64;
/// The longest account name, in bytes.
pub const MAX_LABEL: usize = 48;
/// How many unused accounts in a row end the search on a restore.
pub const GAP: usize = 3;

const PURSE_VERSION: u16 = 1;
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

    /// Builds a payment from one account (nothing is sent). The change returns to the same account.
    pub fn build_payment(
        &mut self,
        index: usize,
        chain: &impl ChainView,
        rng: &mut (impl RngCore + CryptoRng),
        to: &Address,
        amount: u64,
    ) -> Result<Built, PurseError> {
        let a = self
            .accounts
            .get_mut(index)
            .ok_or(PurseError::NoSuchAccount(index))?;
        Ok(a.wallet.build_payment(chain, rng, to, amount)?)
    }

    /// Builds, sends and reserves a payment from one account.
    pub fn pay<C: ChainView + Submitter>(
        &mut self,
        index: usize,
        chain: &mut C,
        rng: &mut (impl RngCore + CryptoRng),
        to: &Address,
        amount: u64,
    ) -> Result<Built, PurseError> {
        let a = self
            .accounts
            .get_mut(index)
            .ok_or(PurseError::NoSuchAccount(index))?;
        Ok(a.wallet.pay(chain, rng, to, amount)?)
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
        Ok(Zeroizing::new(w.into_bytes()))
    }

    fn from_state_bytes(data: &[u8]) -> Result<Purse, FileError> {
        let bad = |e: tenero_core::v2::DecodeError| FileError::Corrupt(e.as_str().to_string());
        let mut r = Reader::new(data);
        if r.u16().map_err(bad)? != PURSE_VERSION {
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
        r.finish().map_err(bad)?;
        Ok(Purse {
            master,
            birth_height,
            accounts,
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
