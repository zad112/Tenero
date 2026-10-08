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
//! Each account is a Carrot account of its own (`crate::address::carrot_master` of its seed), with its own subaddresses. The
//! purse file of `beta` (versions 1 to 4, the interim scheme) is not read: the same 24 words restore a `gamma` purse.

use std::path::Path;

use rand_core::{CryptoRng, RngCore};
use tenero_carrot::JanusAnchor;
use tenero_core::hash::sha256;
use tenero_core::v3::{Reader, Writer};
use zeroize::Zeroizing;

use crate::address::{Address, Kind, Network};
use crate::chain::{ChainView, Submitter};
use crate::file::{open, seal, FileError, KdfParams, MAGIC_PURSE};
use crate::request::{check_text, PaymentRequest, MAX_LABEL as MAX_REQUEST_LABEL, MAX_MESSAGE};
use crate::wallet::{Balance, BatchSent, Built, FeeLevel, Plan, SyncReport, Wallet, WalletError};

/// The most accounts one purse holds.
pub const MAX_ACCOUNTS: usize = 64;
/// The longest account name, in bytes.
pub const MAX_LABEL: usize = 48;
/// How many unused accounts in a row end the search on a restore.
pub const GAP: usize = 3;

/// Version 5: the `gamma` network (Carrot accounts; a sent payment keeps its Janus anchor). Versions 1 to 4 were `beta`'s.
const PURSE_VERSION: u16 = 5;
/// The most sent-payment records a purse keeps (the oldest are dropped past this).
pub const MAX_SENT_RECORDS: usize = 20_000;
/// The most saved payment requests a purse keeps.
pub const MAX_REQUESTS: usize = 1_000;
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
    /// A payment request that cannot be made or found (why).
    Request(String),
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
            PurseError::Request(e) => write!(f, "{e}"),
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

/// A payment this wallet sent, written down when it is sent (the address it went to is on no chain; **a wallet restored
/// from the words has no such records**, and its history shows what it received and its own change, not whom it paid).
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
    /// The Janus anchor chosen for the payment output (`anchor_norm`) and its one-time address: with the transaction's
    /// first key image (`spends[0]`), what lets the sender prove the payment later. `None` once forgotten.
    pub anchor: Option<JanusAnchor>,
    pub payment_onetime: Option<[u8; 32]>,
    /// What the payment was for: the label of the payment request it answered, if it answered one.
    pub note: Option<String>,
}

/// A payment request this wallet made, kept so it can be shown again (as a link and a QR code). **It is not marked paid when a
/// payment arrives.**
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedRequest {
    pub account: u32,
    pub amount: Option<u64>,
    pub label: Option<String>,
    pub message: Option<String>,
    /// Seconds since 1970 when it was made.
    pub time: u64,
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
    /// For something received or mined: the output's global index (what a proof of receipt names).
    pub global_index: Option<u64>,
    /// For a sent payment: the wallet still holds its anchor, so the payment can be proved.
    pub has_secret: bool,
    /// For a sent payment: what it was for, if it answered a request.
    pub note: Option<String>,
    /// For something received at an integrated address: its payment ID (which says what the payment was for).
    pub payment_id: Option<[u8; 8]>,
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
    network: Network,
    birth_height: u64,
    accounts: Vec<Account>,
    sent: Vec<SentRecord>,
    requests: Vec<SavedRequest>,
}

impl Purse {
    /// A new purse with a fresh random master seed and one account, "Main".
    pub fn create(
        rng: &mut (impl RngCore + CryptoRng + Send),
        network: Network,
        birth_height: u64,
    ) -> Purse {
        let mut seed = Zeroizing::new([0u8; 32]);
        rng.fill_bytes(&mut *seed);
        Purse::from_seed(&seed, network, birth_height)
    }

    /// The purse of a master seed, with account 0 only (see [`Purse::discover`] to find the others). To restore,
    /// use a birth height at or before the first coin, or 0.
    pub fn from_seed(master: &[u8; 32], network: Network, birth_height: u64) -> Purse {
        Purse {
            master: Zeroizing::new(*master),
            network,
            birth_height,
            accounts: vec![Account {
                label: "Main".to_string(),
                wallet: Wallet::from_seed(master, network, birth_height),
            }],
            sent: Vec::new(),
            requests: Vec::new(),
        }
    }

    /// One account of an existing one-account wallet (how an old wallet file is read).
    fn from_wallet(wallet: Wallet) -> Result<Purse, FileError> {
        let seed = wallet.seed().ok_or_else(|| {
            FileError::Corrupt(
                "a view-only wallet: it opens in the wallet program (tenero-wallet), not as a purse".into(),
            )
        })?;
        Ok(Purse {
            master: Zeroizing::new(*seed),
            network: wallet.network(),
            birth_height: wallet.birth_height(),
            accounts: vec![Account {
                label: "Main".to_string(),
                wallet,
            }],
            sent: Vec::new(),
            requests: Vec::new(),
        })
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

    pub fn network(&self) -> Network {
        self.network
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
            wallet: Wallet::from_seed(&seed, self.network, birth_height),
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
        rng: &mut (impl RngCore + CryptoRng + Send),
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
            anchor: built.parts.first().map(|p| p.anchor),
            payment_onetime: built.parts.first().map(|p| p.onetime),
            note: None,
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
        rng: &mut (impl RngCore + CryptoRng + Send),
        to: &Address,
        amount: u64,
        level: FeeLevel,
        now: u64,
    ) -> Result<Built, PurseError> {
        let built = self.build_payment(index, &*node, rng, to, amount, level)?;
        self.send(index, node, &built, to, level, now)?;
        Ok(built)
    }

    /// Builds a payment to any number of recipients from one account, as several transactions that spend different coins when it takes them
    /// (`Wallet::build_batch`). Nothing is sent, nothing is reserved.
    pub fn build_batch(
        &mut self,
        index: usize,
        chain: &impl ChainView,
        rng: &mut (impl RngCore + CryptoRng + Send),
        dests: &[(Address, u64)],
        level: FeeLevel,
    ) -> Result<Plan, PurseError> {
        let a = self
            .accounts
            .get_mut(index)
            .ok_or(PurseError::NoSuchAccount(index))?;
        Ok(a.wallet.build_batch(chain, rng, dests, level)?)
    }

    /// Sends the transactions of a [`Purse::build_batch`] one after another, reserves their coins and writes a record for every payment in them (a
    /// transaction that pays several recipients has a record for each, with the fee on the first; **a payment proof for such a transaction covers the
    /// first recipient only**). It stops at the first transaction the node refuses. `now` is the time in seconds since 1970.
    pub fn send_batch<C: ChainView + Submitter>(
        &mut self,
        index: usize,
        node: &mut C,
        txs: &[Built],
        level: FeeLevel,
        now: u64,
    ) -> Result<BatchSent, PurseError> {
        let height = node
            .rules()
            .map_err(|e| PurseError::Wallet(WalletError::Chain(e)))?
            .next_height;
        let a = self
            .accounts
            .get_mut(index)
            .ok_or(PurseError::NoSuchAccount(index))?;
        let sent = a.wallet.send_batch(node, txs);
        for built in &txs[..sent.sent] {
            for (i, part) in built.parts.iter().enumerate() {
                self.sent.push(SentRecord {
                    account: index as u32,
                    id: built.id,
                    to: part.to,
                    amount: part.amount,
                    fee: if i == 0 { built.fee } else { 0 },
                    level,
                    time: now,
                    height,
                    spends: built.spends.clone(),
                    change_onetime: built.change_onetime,
                    anchor: Some(part.anchor),
                    payment_onetime: Some(part.onetime),
                    note: None,
                });
            }
        }
        while self.sent.len() > MAX_SENT_RECORDS {
            self.sent.remove(0);
        }
        Ok(sent)
    }

    /// Combines an account's coins (`Wallet::build_sweep`): to its own address, or to `to`. Nothing is sent, nothing is reserved.
    pub fn build_sweep(
        &mut self,
        index: usize,
        chain: &impl ChainView,
        rng: &mut (impl RngCore + CryptoRng + Send),
        to: Option<&Address>,
        level: FeeLevel,
    ) -> Result<Vec<Built>, PurseError> {
        let a = self
            .accounts
            .get_mut(index)
            .ok_or(PurseError::NoSuchAccount(index))?;
        Ok(a.wallet.build_sweep(chain, rng, to, level)?)
    }

    /// Combines `count` of an account's coins into one (`Wallet::build_combine`). Nothing is sent, nothing is reserved.
    pub fn build_combine(
        &mut self,
        index: usize,
        chain: &impl ChainView,
        rng: &mut (impl RngCore + CryptoRng + Send),
        count: usize,
        level: FeeLevel,
    ) -> Result<Built, PurseError> {
        let a = self
            .accounts
            .get_mut(index)
            .ok_or(PurseError::NoSuchAccount(index))?;
        Ok(a.wallet.build_combine(chain, rng, count, level)?)
    }

    /// Sends transactions that move an account's own coins (a sweep or a combine): reserves the coins and records each as a payment to `to` marked
    /// with `note`. The coin that comes out is not shown as money received (it is marked as change). Stops at the first the node refuses.
    pub fn send_own<C: ChainView + Submitter>(
        &mut self,
        index: usize,
        node: &mut C,
        txs: &[Built],
        level: FeeLevel,
        note: &str,
        now: u64,
    ) -> Result<BatchSent, PurseError> {
        let height = node
            .rules()
            .map_err(|e| PurseError::Wallet(WalletError::Chain(e)))?
            .next_height;
        let a = self
            .accounts
            .get_mut(index)
            .ok_or(PurseError::NoSuchAccount(index))?;
        let mine = a.wallet.address();
        let sent = a.wallet.send_batch(node, txs);
        for built in &txs[..sent.sent] {
            let part = &built.parts[0];
            // to another address it is a payment; to this account, the coin that comes out must not be listed as received
            let change_onetime = if part.to == mine {
                part.onetime
            } else {
                built.change_onetime
            };
            self.sent.push(SentRecord {
                account: index as u32,
                id: built.id,
                to: part.to,
                amount: part.amount,
                fee: built.fee,
                level,
                time: now,
                height,
                spends: built.spends.clone(),
                change_onetime,
                anchor: Some(part.anchor),
                payment_onetime: Some(part.onetime),
                note: Some(note.to_string()),
            });
        }
        while self.sent.len() > MAX_SENT_RECORDS {
            self.sent.remove(0);
        }
        Ok(sent)
    }

    /// Makes and keeps a payment request for an account. An amount of `None` leaves the payer to choose. `now` is the time in seconds
    /// since 1970. Returns its number in [`Purse::requests`].
    pub fn add_request(
        &mut self,
        account: usize,
        amount: Option<u64>,
        label: Option<String>,
        message: Option<String>,
        now: u64,
    ) -> Result<usize, PurseError> {
        self.account(account)?;
        if amount == Some(0) {
            return Err(PurseError::Request(
                "the amount cannot be zero (leave it out to let the payer choose)".into(),
            ));
        }
        let clean = |t: Option<String>, max, what| -> Result<Option<String>, PurseError> {
            let t = t.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
            if let Some(t) = &t {
                check_text(t, max, what).map_err(|e| PurseError::Request(e.to_string()))?;
            }
            Ok(t)
        };
        let label = clean(label, MAX_REQUEST_LABEL, "label")?;
        let message = clean(message, MAX_MESSAGE, "message")?;
        if self.requests.len() >= MAX_REQUESTS {
            return Err(PurseError::Request(format!(
                "a wallet keeps at most {MAX_REQUESTS} requests: delete some"
            )));
        }
        self.requests.push(SavedRequest {
            account: account as u32,
            amount,
            label,
            message,
            time: now,
        });
        Ok(self.requests.len() - 1)
    }

    pub fn requests(&self) -> &[SavedRequest] {
        &self.requests
    }

    pub fn remove_request(&mut self, index: usize) -> Result<(), PurseError> {
        if index >= self.requests.len() {
            return Err(PurseError::Request("there is no such request".into()));
        }
        self.requests.remove(index);
        Ok(())
    }

    /// The request as a link, with its account's address.
    pub fn request_of(&self, index: usize) -> Result<PaymentRequest, PurseError> {
        let r = self
            .requests
            .get(index)
            .ok_or_else(|| PurseError::Request("there is no such request".into()))?;
        Ok(PaymentRequest {
            address: self.account(r.account as usize)?.address(),
            amount: r.amount,
            label: r.label.clone(),
            message: r.message.clone(),
        })
    }

    /// Notes what a sent payment was for (the label of the request it answered).
    pub fn annotate_sent(&mut self, id: &[u8; 32], note: &str) -> Result<(), PurseError> {
        let note = note.trim();
        check_text(note, MAX_REQUEST_LABEL, "label")
            .map_err(|e| PurseError::Request(e.to_string()))?;
        let rec = self
            .sent
            .iter_mut()
            .find(|r| &r.id == id)
            .ok_or_else(|| PurseError::Request("there is no such sent payment".into()))?;
        rec.note = (!note.is_empty()).then(|| note.to_string());
        Ok(())
    }

    /// Deletes the stored anchor of one sent payment (for the person who does not want it kept). That payment can then never
    /// be proved by this wallet, and nothing can bring the anchor back.
    pub fn forget_payment_secret(&mut self, id: &[u8; 32]) -> Result<(), PurseError> {
        let rec = self
            .sent
            .iter_mut()
            .find(|r| &r.id == id)
            .ok_or_else(|| PurseError::Request("there is no such sent payment".into()))?;
        rec.anchor = None;
        Ok(())
    }

    pub fn sent_records(&self) -> &[SentRecord] {
        &self.sent
    }

    // ---- signatures and payment proofs (`proofs.rs`: the signatures are our own construction, unreviewed) ----

    /// Signs `message` as account `account`'s main address; gives the address with the signature.
    pub fn sign_message(
        &self,
        account: usize,
        message: &[u8],
        rng: &mut (impl RngCore + CryptoRng),
    ) -> Result<(Address, crate::proofs::Signature), PurseError> {
        self.account(account)?
            .wallet
            .sign_message(
                tenero_carrot::account::AddressIndex { major: 0, minor: 0 },
                message,
                rng,
            )
            .ok_or_else(|| PurseError::Request("the address cannot be made".into()))
    }

    /// A RECEIVED proof of an output of account `account` (by its global index), signed over `message`.
    pub fn prove_received(
        &self,
        account: usize,
        global_index: u64,
        chain: &impl ChainView,
        message: &[u8],
        rng: &mut (impl RngCore + CryptoRng),
    ) -> Result<crate::proofs::PaymentProof, PurseError> {
        self.account(account)?
            .wallet
            .prove_received(chain, global_index, message, rng)
            .map_err(|e| PurseError::Request(e.to_string()))
    }

    /// The anchor of a payment this wallet sent: its "payment key", with which anyone who has the recipient's address can
    /// find and check that one output ([`crate::proofs::check_anchor`]).
    pub fn payment_anchor(&self, id: &[u8; 32]) -> Result<JanusAnchor, PurseError> {
        let rec = self
            .sent
            .iter()
            .find(|r| &r.id == id)
            .ok_or_else(|| PurseError::Request("there is no such sent payment".into()))?;
        rec.anchor.ok_or_else(|| {
            PurseError::Request("this payment's anchor was forgotten: it cannot be proved".into())
        })
    }

    /// A proof of a payment this wallet sent: the recipient's address, the block it is in and the payment's anchor. It
    /// does not say who sent it (the receiver could make the same one); it shows that the output pays that address.
    pub fn prove_sent(
        &self,
        id: &[u8; 32],
        chain: &impl ChainView,
    ) -> Result<crate::proofs::PaymentProof, PurseError> {
        let err = |e: crate::proofs::ProofError| PurseError::Request(e.to_string());
        let anchor = self.payment_anchor(id)?;
        let rec = self.sent.iter().find(|r| &r.id == id).expect("found above");
        let onetime = rec.payment_onetime.ok_or_else(|| {
            PurseError::Request(
                "this payment's output was not recorded: it cannot be proved".into(),
            )
        })?;
        // the payment was sent for the block at `height`; it is in that block or a later one
        let block =
            crate::proofs::block_with(chain, &onetime, rec.height.saturating_sub(1), 10_000)
                .map_err(|_| PurseError::Request("the payment is not in a block yet".into()))?;
        let proof = crate::proofs::PaymentProof {
            address: rec.to,
            height: block.height,
            onetime_address: onetime,
            anchor,
            signature: None,
        };
        crate::proofs::check_payment(chain, &proof, b"").map_err(err)?;
        Ok(proof)
    }

    /// Everything the wallet knows happened, newest first: what it received (block rewards marked), and what it sent
    /// with where each payment stands. The change of a payment is not listed as received.
    pub fn history(&self, chain: &impl ChainView) -> Result<Vec<Entry>, PurseError> {
        let chain_err = |e: String| PurseError::Wallet(WalletError::Chain(e));
        let mut out = Vec::new();
        for (i, a) in self.accounts.iter().enumerate() {
            for o in a.wallet.owned() {
                // change is an internal self-send: the wallet knows it even without its record of what it sent
                let is_change = o.internal
                    || self
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
                    global_index: Some(o.global_index),
                    has_secret: false,
                    note: None,
                    payment_id: (o.payment_id != tenero_carrot::NULL_PAYMENT_ID)
                        .then_some(o.payment_id),
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
                global_index: None,
                has_secret: r.anchor.is_some(),
                note: r.note.clone(),
                payment_id: None,
            });
        }
        out.sort_by_key(|e| std::cmp::Reverse(e.height));
        Ok(out)
    }

    // --------------------------------------------------------------------------------------------
    // the file
    // --------------------------------------------------------------------------------------------

    fn state_bytes(&self) -> Result<Zeroizing<Vec<u8>>, FileError> {
        self.state_bytes_as(PURSE_VERSION)
    }

    fn state_bytes_as(&self, version: u16) -> Result<Zeroizing<Vec<u8>>, FileError> {
        let bad = |e: tenero_core::v3::EncodeError| FileError::Corrupt(e.to_string());
        let mut w = Writer::new();
        w.u16(version);
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
            w.raw(&[match r.to.kind {
                Kind::Main => 0,
                Kind::Subaddress => 1,
                Kind::Integrated => 2,
            }]);
            w.raw(&r.to.spend_pubkey);
            w.raw(&r.to.view_pubkey);
            w.raw(&r.to.payment_id);
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
            match (&r.anchor, &r.payment_onetime) {
                (Some(anchor), Some(onetime)) => {
                    w.raw(&[1]);
                    w.raw(anchor);
                    w.raw(onetime);
                }
                _ => w.raw(&[0]),
            }
            w.var(
                r.note.as_deref().unwrap_or("").as_bytes(),
                MAX_REQUEST_LABEL,
            )
            .map_err(bad)?;
        }
        {
            w.count(self.requests.len(), 0, MAX_REQUESTS).map_err(bad)?;
            for q in &self.requests {
                w.u32(q.account);
                match q.amount {
                    Some(a) => {
                        w.raw(&[1]);
                        w.u64(a);
                    }
                    None => {
                        w.raw(&[0]);
                        w.u64(0);
                    }
                }
                w.var(
                    q.label.as_deref().unwrap_or("").as_bytes(),
                    MAX_REQUEST_LABEL,
                )
                .map_err(bad)?;
                w.var(q.message.as_deref().unwrap_or("").as_bytes(), MAX_MESSAGE)
                    .map_err(bad)?;
                w.u64(q.time);
            }
        }
        Ok(Zeroizing::new(w.into_bytes()))
    }

    fn from_state_bytes(data: &[u8]) -> Result<Purse, FileError> {
        let bad = |e: tenero_core::v3::DecodeError| FileError::Corrupt(e.as_str().to_string());
        let mut r = Reader::new(data);
        let version = r.u16().map_err(bad)?;
        if (1..PURSE_VERSION).contains(&version) {
            return Err(FileError::Corrupt(
                "a beta wallet: open it with the 0.2 programs (its 24 words also restore a gamma wallet)".into(),
            ));
        }
        if version != PURSE_VERSION {
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
            if wallet.seed() != Some(&*account_seed(&master, i as u32)) {
                return Err(FileError::Corrupt(
                    "an account does not belong to this seed".into(),
                ));
            }
            accounts.push(Account { label, wallet });
        }
        let network = accounts[0].wallet.network();
        if accounts.iter().any(|a| a.wallet.network() != network) {
            return Err(FileError::Corrupt(
                "the accounts are of different networks".into(),
            ));
        }
        let mut sent = Vec::new();
        {
            let m = r.count(0, MAX_SENT_RECORDS).map_err(bad)?;
            for _ in 0..m {
                let account = r.u32().map_err(bad)?;
                if account as usize >= accounts.len() {
                    return Err(FileError::Corrupt(
                        "a sent payment names an account that is not there".into(),
                    ));
                }
                let id: [u8; 32] = r.array().map_err(bad)?;
                let kind = match r.take(1).map_err(bad)?[0] {
                    0 => Kind::Main,
                    1 => Kind::Subaddress,
                    2 => Kind::Integrated,
                    _ => return Err(FileError::Corrupt("a bad address kind".into())),
                };
                let to = Address {
                    network,
                    kind,
                    spend_pubkey: r.array().map_err(bad)?,
                    view_pubkey: r.array().map_err(bad)?,
                    payment_id: r.array().map_err(bad)?,
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
                let (anchor, payment_onetime) = match r.take(1).map_err(bad)?[0] {
                    0 => (None, None),
                    1 => {
                        let anchor: JanusAnchor = r.array().map_err(bad)?;
                        let onetime: [u8; 32] = r.array().map_err(bad)?;
                        (Some(anchor), Some(onetime))
                    }
                    _ => return Err(FileError::Corrupt("a bad anchor marker".into())),
                };
                let note = {
                    let raw = String::from_utf8(r.var(MAX_REQUEST_LABEL).map_err(bad)?)
                        .map_err(|_| FileError::Corrupt("a note is not text".into()))?;
                    check_text(&raw, MAX_REQUEST_LABEL, "label")
                        .map_err(|_| FileError::Corrupt("a note is not allowed".into()))?;
                    (!raw.is_empty()).then_some(raw)
                };
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
                    anchor,
                    payment_onetime,
                    note,
                });
            }
        }
        let mut requests = Vec::new();
        {
            let m = r.count(0, MAX_REQUESTS).map_err(bad)?;
            for _ in 0..m {
                let account = r.u32().map_err(bad)?;
                if account as usize >= accounts.len() {
                    return Err(FileError::Corrupt(
                        "a request names an account that is not there".into(),
                    ));
                }
                let flag = r.take(1).map_err(bad)?[0];
                let a = r.u64().map_err(bad)?;
                let amount = match flag {
                    0 if a == 0 => None,
                    1 if a > 0 => Some(a),
                    _ => return Err(FileError::Corrupt("a bad request amount".into())),
                };
                let mut text = |max: usize,
                                what: &'static str|
                 -> Result<Option<String>, FileError> {
                    let raw = String::from_utf8(r.var(max).map_err(bad)?)
                        .map_err(|_| FileError::Corrupt("a request's text is not text".into()))?;
                    check_text(&raw, max, what).map_err(|_| {
                        FileError::Corrupt("a request's text is not allowed".into())
                    })?;
                    Ok((!raw.is_empty()).then_some(raw))
                };
                let label = text(MAX_REQUEST_LABEL, "label")?;
                let message = text(MAX_MESSAGE, "message")?;
                let time = r.u64().map_err(bad)?;
                requests.push(SavedRequest {
                    account,
                    amount,
                    label,
                    message,
                    time,
                });
            }
        }
        r.finish().map_err(bad)?;
        Ok(Purse {
            master,
            network,
            birth_height,
            accounts,
            sent,
            requests,
        })
    }

    /// Writes the purse to `path`, encrypted with the passphrase (an empty passphrase is allowed here: the app
    /// decides whether to permit it), replacing any file there atomically.
    pub fn save(
        &self,
        path: &Path,
        passphrase: &[u8],
        kdf: KdfParams,
        rng: &mut (impl RngCore + CryptoRng + Send),
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
            Purse::from_wallet(w)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(secret: bool) -> SentRecord {
        SentRecord {
            account: 0,
            id: [1; 32],
            to: Wallet::from_seed(&[2; 32], Network::Test, 0).address(),
            amount: 5,
            fee: 6,
            level: FeeLevel::Normal,
            time: 7,
            height: 8,
            spends: vec![[9; 32]],
            change_onetime: [10; 32],
            anchor: secret.then_some([11; 16]),
            payment_onetime: secret.then_some([12; 32]),
            note: secret.then(|| "Rent".to_string()),
        }
    }

    #[test]
    fn a_purse_round_trips_and_a_beta_purse_is_refused_with_a_reason() {
        let mut p = Purse::from_seed(&[3; 32], Network::Test, 4);
        p.sent.push(record(true));
        p.sent.push(record(false));
        let mut integrated = record(true);
        integrated.to = integrated.to.with_payment_id([7; 8]).unwrap();
        p.sent.push(integrated);
        let back = Purse::from_state_bytes(&p.state_bytes().unwrap()).unwrap();
        assert_eq!(back.sent, p.sent);
        assert_eq!(back.network(), Network::Test);
        for old in 1..=4u16 {
            let mut bytes = p.state_bytes().unwrap().to_vec();
            bytes[..2].copy_from_slice(&old.to_le_bytes());
            match Purse::from_state_bytes(&bytes) {
                Err(FileError::Corrupt(why)) => assert!(why.contains("beta"), "{why}"),
                other => panic!("version {old}: {:?}", other.err()),
            }
        }
        let mut future = p.state_bytes().unwrap().to_vec();
        future[0] = 9;
        assert!(Purse::from_state_bytes(&future).is_err());
        // an anchor marker that is not 0 or 1 is refused
        let mut p0 = Purse::from_seed(&[3; 32], Network::Test, 4);
        p0.sent.push(record(false));
        let mut bytes = p0.state_bytes().unwrap().to_vec();
        // the record ends with its marker (0) and an empty note (a 4-byte length), then the request count (4 bytes)
        let at = bytes.len() - 9;
        assert_eq!(bytes[at], 0);
        bytes[at] = 2;
        assert!(Purse::from_state_bytes(&bytes).is_err());
    }

    #[test]
    fn requests_and_notes_are_kept_in_the_file_and_a_bad_one_is_refused() {
        let mut p = Purse::from_seed(&[3; 32], Network::Test, 4);
        p.add_account("Savings", 0).unwrap();
        p.sent.push(record(true));
        p.add_request(
            0,
            Some(150_000_000),
            Some("Rent".into()),
            Some("October rent".into()),
            1_700_000_000,
        )
        .unwrap();
        p.add_request(1, None, None, None, 1_700_000_001).unwrap();
        let back = Purse::from_state_bytes(&p.state_bytes().unwrap()).unwrap();
        assert_eq!(back.requests, p.requests);
        assert_eq!(back.sent, p.sent);
        assert_eq!(back.sent[0].note.as_deref(), Some("Rent"));
        assert_eq!(
            back.request_of(0).unwrap().to_uri(),
            format!(
                "tenero:{}?amount=1.5&label=Rent&message=October%20rent",
                p.accounts()[0].address().to_text()
            )
        );
        // what makes a request: refused amounts and texts, accounts that are not there, too many, and removing
        assert!(p.add_request(9, None, None, None, 0).is_err());
        assert!(p.add_request(0, Some(0), None, None, 0).is_err());
        assert!(p
            .add_request(
                0,
                None,
                Some(
                    "a
b"
                    .into()
                ),
                None,
                0
            )
            .is_err());
        assert!(p
            .add_request(0, None, Some("x".repeat(65)), None, 0)
            .is_err());
        assert!(p
            .add_request(0, None, None, Some("x".repeat(201)), 0)
            .is_err());
        assert_eq!(p.requests().len(), 2);
        // a blank label is no label
        let i = p
            .add_request(0, None, Some("   ".into()), Some("".into()), 5)
            .unwrap();
        assert_eq!(
            (
                p.requests()[i].label.clone(),
                p.requests()[i].message.clone()
            ),
            (None, None)
        );
        p.remove_request(i).unwrap();
        assert!(p.remove_request(i).is_err());
        // a note on a sent payment
        p.annotate_sent(&[1; 32], "  Groceries ").unwrap();
        assert_eq!(p.sent[0].note.as_deref(), Some("Groceries"));
        assert!(p.annotate_sent(&[1; 32], "a	b").is_err());
        assert!(p.annotate_sent(&[7; 32], "x").is_err());
        p.annotate_sent(&[1; 32], "").unwrap();
        assert_eq!(p.sent[0].note, None);
        // a file whose request names an account that is not there is refused
        let mut q = Purse::from_seed(&[3; 32], Network::Test, 4);
        q.requests.push(SavedRequest {
            account: 5,
            amount: None,
            label: None,
            message: None,
            time: 0,
        });
        assert!(Purse::from_state_bytes(&q.state_bytes().unwrap()).is_err());
        // and so is one with an amount marker that disagrees with its number
        let mut q = Purse::from_seed(&[3; 32], Network::Test, 4);
        q.requests.push(SavedRequest {
            account: 0,
            amount: Some(5),
            label: None,
            message: None,
            time: 0,
        });
        let mut bytes = q.state_bytes().unwrap().to_vec();
        // the amount marker is the byte after the 4-byte account number of the only request, 12 + 4 + 4 + 1... find it by search
        let pos = bytes
            .windows(9)
            .rposition(|w| w[0] == 1 && w[1..] == 5u64.to_le_bytes())
            .unwrap();
        bytes[pos] = 0;
        assert!(Purse::from_state_bytes(&bytes).is_err());
    }
}
