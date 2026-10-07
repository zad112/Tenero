//! The wallet: scanning the chain for one's outputs, balances, and building a payment.
//!
//! **Interim and unaudited.** The output scheme is [`crate::interim`]; decoy selection and coin selection here are
//! simple policies (see the notes on each), not Monero's, and have not been studied for how much they reveal.

use rand_core::{CryptoRng, RngCore};
use tenero_core::fees;
use tenero_core::v2::{ids, Input, Output, Transaction, TxPrefix, Wire, MAX_TX_SIZE, VERSION};
use tenero_crypto::ringct::{self, OutputSecret, SpendInput};
use tenero_store::StoredOutput;
use zeroize::Zeroizing;

use crate::chain::{ChainView, Rules, Submitter};
use crate::interim::{
    create_enote, scan_coinbase_output, scan_output, tx_context, Address, Keys, ViewKeys,
};

/// How many scanned block ids the wallet remembers to notice a reorganisation. A reorganisation deeper than
/// this makes the wallet rescan from its birth height.
pub const RECENT_BLOCKS: usize = 100;

/// The most recipients one transaction can pay: 16 outputs, and one is always the change.
pub const MAX_RECIPIENTS: usize = tenero_core::v2::MAX_OUTPUTS - 1;

/// How many blocks the wallet asks a node for at a time while scanning.
pub const SCAN_BATCH: u64 = 64;

/// A transaction the wallet has sent keeps its inputs reserved for this many blocks (so that a second payment
/// does not pick the same coins while the first is still waiting); after that, if the coins are unspent, they
/// are free again (the transaction was probably dropped).
pub const RESERVE_BLOCKS: u64 = 20;

/// The fee is the minimum the next block needs, plus this many percent (the minimum moves with the block-size
/// median between sending and mining), and at least one unit more.
pub const FEE_MARGIN_PERCENT: u64 = 25;

/// How much to pay for a payment, as a multiple of the minimum fee the next block needs. The minimum is a rule of
/// the chain; a higher fee only buys a better place when the pool is full (a full pool drops the lowest fee rate
/// first), and nothing else. Chosen by the owner 2026-10-03.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeeLevel {
    /// 1.25 times the minimum: what the wallet always paid (the margin covers the minimum rising before a block).
    Low,
    /// 2 times the minimum.
    Normal,
    /// 5 times the minimum.
    High,
}

impl FeeLevel {
    pub const ALL: [FeeLevel; 3] = [FeeLevel::Low, FeeLevel::Normal, FeeLevel::High];

    /// The fee as a percentage of the minimum.
    pub fn percent_of_minimum(self) -> u64 {
        match self {
            FeeLevel::Low => 100 + FEE_MARGIN_PERCENT,
            FeeLevel::Normal => 200,
            FeeLevel::High => 500,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            FeeLevel::Low => "Low",
            FeeLevel::Normal => "Normal",
            FeeLevel::High => "High",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum WalletError {
    /// The node or the store said no.
    Chain(String),
    /// A payment of zero.
    ZeroAmount,
    NotEnough {
        spendable: u64,
        needed: u64,
    },
    /// The payment would need more coins than one transaction can carry: a transaction may take at most `MAX_TX_SIZE` bytes,
    /// and every coin it spends adds about 780 of them. `max` is how many fit.
    TooManyInputs {
        max: usize,
    },
    /// A transaction has at most `MAX_RECIPIENTS` recipients (16 outputs, one of them the change).
    TooManyRecipients {
        max: usize,
    },
    /// There is nothing worth combining: fewer than two coins, or coins worth less than the fee they would add.
    NothingToCombine,
    /// Not enough mature outputs on the chain to hide among.
    NotEnoughDecoys,
    /// The recipient address is not valid.
    BadAddress,
    /// The prover refused (a bad secret or a malformed ring).
    Prove,
    /// The transaction the wallet built does not verify: a bug, never sent.
    SelfCheck(String),
    /// The node refused the transaction.
    Submit(String),
}

impl std::fmt::Display for WalletError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalletError::Chain(e) => write!(f, "the node said: {e}"),
            WalletError::ZeroAmount => write!(f, "cannot pay nothing"),
            WalletError::NotEnough { spendable, needed } => {
                write!(
                    f,
                    "not enough spendable coins: {spendable} units, need {needed}"
                )
            }
            WalletError::TooManyInputs { max } => write!(
                f,
                "that payment needs more pieces than one transaction can carry (at most {max}): send a smaller amount, or send some of your balance to yourself first to combine it into fewer, larger pieces"
            ),
            WalletError::TooManyRecipients { max } => write!(
                f,
                "one transaction can pay at most {max} recipients"
            ),
            WalletError::NothingToCombine => write!(
                f,
                "there are no pieces worth combining (a piece worth less than the fee it adds is left alone)"
            ),
            WalletError::NotEnoughDecoys => write!(
                f,
                "the chain does not have enough matured outputs yet to hide a payment among (block rewards need time to mature): try again after more blocks"
            ),
            WalletError::BadAddress => write!(f, "invalid recipient address"),
            WalletError::Prove => write!(f, "could not build the proofs"),
            WalletError::SelfCheck(e) => {
                write!(f, "the wallet built an invalid transaction (a bug): {e}")
            }
            WalletError::Submit(e) => write!(f, "the node refused the transaction: {e}"),
        }
    }
}

impl std::error::Error for WalletError {}

fn chain_err(e: String) -> WalletError {
    WalletError::Chain(e)
}

/// One output that belongs to the wallet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owned {
    pub global_index: u64,
    pub height: u64,
    pub coinbase: bool,
    pub onetime_address: [u8; 32],
    pub amount: u64,
    pub mask: [u8; 32],
    /// The one-time offset (the output's secret is this plus the spend secret).
    pub offset: [u8; 32],
    pub key_image: [u8; 32],
}

/// Coins the wallet has promised to a transaction it sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reserved {
    pub key_image: [u8; 32],
    pub until_height: u64,
}

/// What the wallet holds, in units.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Balance {
    /// Every unspent output the wallet recognises.
    pub total: u64,
    /// Of that, what can go into a transaction now (mature and not promised to a pending payment).
    pub spendable: u64,
    /// Waiting for maturity.
    pub immature: u64,
    /// Promised to a payment the wallet sent that is not yet in the chain.
    pub reserved: u64,
}

/// What a scan did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub blocks_scanned: u64,
    pub outputs_found: u64,
    /// Heights given up because of a reorganisation.
    pub blocks_rolled_back: u64,
    /// The scan started again from the birth height (a reorganisation deeper than [`RECENT_BLOCKS`]).
    pub rescanned: bool,
}

/// A payment the wallet built (not yet sent).
#[derive(Clone, Debug)]
pub struct Built {
    pub tx: Transaction,
    pub id: [u8; 32],
    pub fee: u64,
    pub amount: u64,
    pub change: u64,
    /// The key images of the outputs it spends (to reserve them).
    pub spends: Vec<[u8; 32]>,
    /// The one-time address of the change output (so the wallet can tell its own change from a payment it
    /// received: change comes back to the wallet and scanning finds it like any other output).
    pub change_onetime: [u8; 32],
    /// The secret of the PAYMENT output (not the change) and its one-time address: what proves the payment later.
    pub payment_secret: crate::interim::TxSecret,
    pub payment_onetime: [u8; 32],
    /// Every payment output, in the order the recipients were given (the first is the one `payment_secret` and `payment_onetime` are of).
    pub parts: Vec<PaymentPart>,
}

/// One recipient's output of a transaction: what proves that payment later.
#[derive(Clone, Debug)]
pub struct PaymentPart {
    pub to: Address,
    pub amount: u64,
    pub onetime: [u8; 32],
    pub secret: crate::interim::TxSecret,
}

/// A payment broken into transactions that spend different coins (so they can all be sent at once): what [`Wallet::build_batch`] makes.
#[derive(Debug)]
pub struct Plan {
    pub txs: Vec<Built>,
    /// What could not be built now (the coins that were left, or their count, ran out): the recipients and amounts still to pay. Coins that come
    /// back as change take 10 blocks to be spendable, so these are for a later batch.
    pub unsent: Vec<(Address, u64)>,
}

/// How a [`Wallet::send_batch`] went: the first `sent` transactions were handed to the node (and their coins reserved); `failed` is why the next one was not.
#[derive(Debug)]
pub struct BatchSent {
    pub sent: usize,
    pub failed: Option<WalletError>,
}

pub struct Wallet {
    seed: Zeroizing<[u8; 32]>,
    keys: Keys,
    view: ViewKeys,
    /// The height scanning starts from on a fresh wallet (so a new wallet does not read the whole chain).
    pub(crate) birth_height: u64,
    /// The last height scanned.
    pub(crate) scanned: Option<u64>,
    /// The ids of the last scanned blocks, oldest first.
    pub(crate) recent: Vec<(u64, [u8; 32])>,
    pub(crate) owned: Vec<Owned>,
    pub(crate) reserved: Vec<Reserved>,
    /// Key images the chain has said are spent (a spent output stays spent, so it is asked once). In memory only; forgotten
    /// when a reorganisation is seen, which is the only way a spend can undo.
    spent_cache: std::collections::HashSet<[u8; 32]>,
    /// Key images the chain said were NOT spent, and the tip they were asked at: the answer only changes when the tip does,
    /// so on an unchanged tip a coin is not asked about again (a wallet with hundreds of coins asked hundreds of questions
    /// for every balance and every payment).
    unspent_cache: std::collections::HashSet<[u8; 32]>,
    unspent_tip: Option<[u8; 32]>,
}

fn is_mature(rules: &Rules, height: u64, coinbase: bool) -> bool {
    let wait = if coinbase {
        rules.coinbase_maturity
    } else {
        rules.spend_maturity
    };
    rules.next_height >= height.saturating_add(wait)
}

/// A uniform number in `0..n` (`n > 0`), without modulo bias.
fn below(rng: &mut impl RngCore, n: u64) -> u64 {
    let zone = u64::MAX - (u64::MAX % n);
    loop {
        let x = rng.next_u64();
        if x < zone {
            return x % n;
        }
    }
}

/// The commitment a ring member counts for in a signature.
fn ring_commitment(o: &StoredOutput) -> [u8; 32] {
    if o.coinbase {
        ringct::public_amount_commitment(o.public_amount)
    } else {
        o.amount_commitment
    }
}

impl Wallet {
    /// A new wallet with a fresh random seed. It will scan from `birth_height` (the chain's tip when it is made,
    /// so it never reads old blocks that cannot hold its coins).
    pub fn create(rng: &mut (impl RngCore + CryptoRng), birth_height: u64) -> Wallet {
        let mut seed = [0u8; 32];
        rng.fill_bytes(&mut seed);
        let w = Wallet::from_seed(&seed, birth_height);
        zeroize::Zeroize::zeroize(&mut seed);
        w
    }

    /// The wallet of a seed (to restore one: use a birth height at or before its first coin, or 0).
    pub fn from_seed(seed: &[u8; 32], birth_height: u64) -> Wallet {
        let keys = Keys::from_seed(seed);
        let view = keys.view_keys();
        Wallet {
            seed: Zeroizing::new(*seed),
            keys,
            view,
            birth_height,
            scanned: None,
            recent: Vec::new(),
            owned: Vec::new(),
            reserved: Vec::new(),
            spent_cache: std::collections::HashSet::new(),
            unspent_cache: std::collections::HashSet::new(),
            unspent_tip: None,
        }
    }

    pub fn address(&self) -> Address {
        self.keys.address()
    }

    pub(crate) fn keys(&self) -> &Keys {
        &self.keys
    }

    pub(crate) fn view_keys(&self) -> &ViewKeys {
        &self.view
    }

    /// The seed: the wallet's whole secret. Handle with care; never log it.
    pub fn seed(&self) -> &[u8; 32] {
        &self.seed
    }

    pub fn birth_height(&self) -> u64 {
        self.birth_height
    }

    /// The last height scanned, if any.
    pub fn scanned_height(&self) -> Option<u64> {
        self.scanned
    }

    pub fn owned(&self) -> &[Owned] {
        &self.owned
    }

    /// Whether any of these key images is still promised to a payment this wallet sent.
    pub fn is_reserved(&self, key_images: &[[u8; 32]]) -> bool {
        self.reserved
            .iter()
            .any(|r| key_images.contains(&r.key_image))
    }

    // --------------------------------------------------------------------------------------------
    // scanning
    // --------------------------------------------------------------------------------------------

    /// Brings the wallet up to the chain's tip: first it gives up any scanned blocks the chain no longer
    /// has (a reorganisation), then it reads the new ones.
    pub fn sync(&mut self, chain: &impl ChainView) -> Result<SyncReport, WalletError> {
        let mut report = SyncReport::default();
        // 1. a reorganisation: forget the blocks that are no longer on the chain
        while let Some(&(h, id)) = self.recent.last() {
            let same = chain
                .block(h)
                .map_err(chain_err)?
                .is_some_and(|b| b.id == id);
            if same {
                break;
            }
            self.recent.pop();
            report.blocks_rolled_back += 1;
        }
        let keep = if let Some(&(h, _)) = self.recent.last() {
            Some(h)
        } else if self.scanned.is_some() {
            // every remembered block is gone: a deeper reorganisation than we remember
            report.rescanned = true;
            None
        } else {
            None
        };
        if self.scanned != keep {
            match keep {
                Some(h) => {
                    self.owned.retain(|o| o.height <= h);
                }
                None => self.owned.clear(),
            }
            self.scanned = keep;
        }
        if report.blocks_rolled_back > 0 || report.rescanned {
            self.spent_cache.clear();
            self.unspent_cache.clear();
            self.unspent_tip = None;
        }
        // 2. new blocks
        let (tip, _) = chain.tip().map_err(chain_err)?;
        let mut from = self.scanned.map_or(self.birth_height, |h| h + 1);
        while from <= tip {
            let batch = chain.blocks(from, SCAN_BATCH).map_err(chain_err)?;
            if batch.is_empty() {
                break;
            }
            for block in batch {
                // blocks must arrive in order, one after another; anything else is the node's bug, not a chain
                if block.height != from {
                    return Err(WalletError::Chain(format!(
                        "asked for block {from}, got block {}",
                        block.height
                    )));
                }
                report.outputs_found += self.scan_block(&block);
                report.blocks_scanned += 1;
                self.scanned = Some(block.height);
                self.recent.push((block.height, block.id));
                if self.recent.len() > RECENT_BLOCKS {
                    self.recent.remove(0);
                }
                from += 1;
            }
        }
        Ok(report)
    }

    fn scan_block(&mut self, block: &crate::chain::ScanBlock) -> u64 {
        let mut found = 0;
        let mut index = block.first_output_index;
        for (j, o) in block.coinbase.outputs.iter().enumerate() {
            if let Some(r) = scan_coinbase_output(&self.view, o, block.height, j as u32) {
                found += self.keep(index, block.height, true, o.onetime_address, &r) as u64;
            }
            index += 1;
        }
        for t in &block.txs {
            let ctx = tx_context(&t.inputs[0].key_image);
            for (j, o) in t.outputs.iter().enumerate() {
                if let Some(r) = scan_output(&self.view, o, &ctx, j as u32) {
                    found += self.keep(index, block.height, false, o.onetime_address, &r) as u64;
                }
                index += 1;
            }
        }
        found
    }

    fn keep(
        &mut self,
        global_index: u64,
        height: u64,
        coinbase: bool,
        onetime_address: [u8; 32],
        r: &crate::interim::Recognised,
    ) -> bool {
        // an output worth nothing (the change of a payment that used its coins exactly, or of a combine) is of no use to anyone
        if r.amount == 0 || self.owned.iter().any(|o| o.global_index == global_index) {
            return false;
        }
        let Some(secret) = self.keys.onetime_secret(&r.offset) else {
            return false;
        };
        let Some(key_image) = ringct::key_image(&secret) else {
            return false;
        };
        self.owned.push(Owned {
            global_index,
            height,
            coinbase,
            onetime_address,
            amount: r.amount,
            mask: r.mask,
            offset: r.offset,
            key_image,
        });
        true
    }

    // --------------------------------------------------------------------------------------------
    // balances
    // --------------------------------------------------------------------------------------------

    /// Whether the chain has this key image, asking only if it has not already said yes.
    fn is_spent(&mut self, chain: &impl ChainView, ki: &[u8; 32]) -> Result<bool, WalletError> {
        if self.spent_cache.contains(ki) {
            return Ok(true);
        }
        if self.unspent_cache.contains(ki) {
            return Ok(false);
        }
        let spent = chain.key_image_spent(ki).map_err(chain_err)?;
        if spent {
            self.spent_cache.insert(*ki);
        } else {
            self.unspent_cache.insert(*ki);
        }
        Ok(spent)
    }

    /// Asks the chain about every key image whose answer is not already known, **in one request** (`ChainView::key_images_spent`):
    /// a wallet that has mined thousands of blocks owns thousands of coins, and asking about each in turn took twenty seconds every
    /// time the tip moved (the "not spent" answers are forgotten then).
    fn refresh_spent(
        &mut self,
        chain: &impl ChainView,
        images: &[[u8; 32]],
    ) -> Result<(), WalletError> {
        let unknown: Vec<[u8; 32]> = images
            .iter()
            .filter(|k| !self.spent_cache.contains(*k) && !self.unspent_cache.contains(*k))
            .copied()
            .collect();
        if unknown.is_empty() {
            return Ok(());
        }
        let flags = chain.key_images_spent(&unknown).map_err(chain_err)?;
        if flags.len() != unknown.len() {
            return Err(WalletError::Chain(
                "the node answered about a different number of key images than were asked".into(),
            ));
        }
        for (k, spent) in unknown.iter().zip(flags) {
            if spent {
                self.spent_cache.insert(*k);
            } else {
                self.unspent_cache.insert(*k);
            }
        }
        Ok(())
    }

    /// Forgets the "not spent" answers if the chain has moved since they were given.
    fn note_tip(&mut self, chain: &impl ChainView) -> Result<(), WalletError> {
        let (_, id) = chain.tip().map_err(chain_err)?;
        if self.unspent_tip != Some(id) {
            self.unspent_cache.clear();
            self.unspent_tip = Some(id);
        }
        Ok(())
    }

    /// Which outputs can go into a transaction sent now: unspent, mature, not promised elsewhere. Also forgets
    /// reservations that have run out or whose coins have been spent.
    fn spendable(
        &mut self,
        chain: &impl ChainView,
        rules: &Rules,
    ) -> Result<Vec<Owned>, WalletError> {
        self.note_tip(chain)?;
        let mut spent = Vec::new();
        let images: Vec<[u8; 32]> = self.owned.iter().map(|o| o.key_image).collect();
        self.refresh_spent(chain, &images)?;
        for ki in images {
            if self.is_spent(chain, &ki)? {
                spent.push(ki);
            }
        }
        self.reserved
            .retain(|r| r.until_height >= rules.next_height && !spent.contains(&r.key_image));
        Ok(self
            .owned
            .iter()
            .filter(|o| {
                !spent.contains(&o.key_image)
                    && is_mature(rules, o.height, o.coinbase)
                    && !self.reserved.iter().any(|r| r.key_image == o.key_image)
            })
            .cloned()
            .collect())
    }

    pub fn balance(&mut self, chain: &impl ChainView) -> Result<Balance, WalletError> {
        let rules = chain.rules().map_err(chain_err)?;
        self.note_tip(chain)?;
        let mut b = Balance::default();
        let mut unspent = Vec::new();
        let owned = self.owned.clone();
        let images: Vec<[u8; 32]> = owned.iter().map(|o| o.key_image).collect();
        self.refresh_spent(chain, &images)?;
        for o in owned {
            if !self.is_spent(chain, &o.key_image)? {
                unspent.push(o);
            }
        }
        self.reserved.retain(|r| {
            r.until_height >= rules.next_height
                && unspent.iter().any(|o| o.key_image == r.key_image)
        });
        for o in &unspent {
            b.total += o.amount;
            if !is_mature(&rules, o.height, o.coinbase) {
                b.immature += o.amount;
            } else if self.reserved.iter().any(|r| r.key_image == o.key_image) {
                b.reserved += o.amount;
            } else {
                b.spendable += o.amount;
            }
        }
        Ok(b)
    }

    // --------------------------------------------------------------------------------------------
    // paying
    // --------------------------------------------------------------------------------------------

    /// Builds a payment of `amount` to `to`, with the change coming back to this wallet, and checks it. Nothing is
    /// sent and nothing is reserved.
    pub fn build_payment(
        &mut self,
        chain: &impl ChainView,
        rng: &mut (impl RngCore + CryptoRng),
        to: &Address,
        amount: u64,
    ) -> Result<Built, WalletError> {
        self.build_payment_at(chain, rng, to, amount, FeeLevel::Low)
    }

    /// [`Wallet::build_payment`] at a chosen fee level.
    pub fn build_payment_at(
        &mut self,
        chain: &impl ChainView,
        rng: &mut (impl RngCore + CryptoRng),
        to: &Address,
        amount: u64,
        level: FeeLevel,
    ) -> Result<Built, WalletError> {
        if amount == 0 {
            return Err(WalletError::ZeroAmount);
        }
        self.build_to_at(chain, rng, &[(*to, amount)], level)
    }

    /// One transaction paying several recipients (at most [`MAX_RECIPIENTS`]), with the change coming back to this wallet. Nothing is sent and
    /// nothing is reserved. To pay more recipients than that, or more than the coins of one transaction can, use [`Wallet::build_batch`].
    pub fn build_to_at(
        &mut self,
        chain: &impl ChainView,
        rng: &mut (impl RngCore + CryptoRng),
        dests: &[(Address, u64)],
        level: FeeLevel,
    ) -> Result<Built, WalletError> {
        check_dests(dests)?;
        let rules = chain.rules().map_err(chain_err)?;
        let candidates = self.spendable(chain, &rules)?;
        let mut cache = BuildCache::default();
        settle(&rules, level, |fee| {
            self.build_with_fee(chain, rng, &rules, &candidates, dests, fee, &mut cache)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn build_with_fee(
        &self,
        chain: &impl ChainView,
        rng: &mut (impl RngCore + CryptoRng),
        rules: &Rules,
        candidates: &[Owned],
        dests: &[(Address, u64)],
        fee: u64,
        cache: &mut BuildCache,
    ) -> Result<Built, WalletError> {
        let amount = dests
            .iter()
            .try_fold(0u64, |a, (_, v)| a.checked_add(*v))
            .ok_or(WalletError::NotEnough {
                spendable: 0,
                needed: u64::MAX,
            })?;
        let needed = amount.checked_add(fee).ok_or(WalletError::NotEnough {
            spendable: 0,
            needed: u64::MAX,
        })?;
        let limit = payment_input_limit(rules, dests.len() + 1);
        let chosen = select_coins(candidates, needed, limit)?;
        self.assemble(chain, rng, rules, chosen, dests, fee, cache)
    }

    /// Builds, proves and checks a transaction that spends exactly `chosen` and pays `dests` (and what is left, after the fee, back to this wallet).
    #[allow(clippy::too_many_arguments)]
    fn assemble(
        &self,
        chain: &impl ChainView,
        rng: &mut (impl RngCore + CryptoRng),
        rules: &Rules,
        mut chosen: Vec<Owned>,
        dests: &[(Address, u64)],
        fee: u64,
        cache: &mut BuildCache,
    ) -> Result<Built, WalletError> {
        let amount: u64 = dests.iter().map(|(_, v)| *v).sum();
        // key images strictly ascending: the rules' one canonical order
        chosen.sort_by_key(|o| o.key_image);
        let total: u64 = chosen.iter().map(|o| o.amount).sum();
        let change = total
            .checked_sub(amount)
            .and_then(|r| r.checked_sub(fee))
            .ok_or(WalletError::NotEnough {
                spendable: total,
                needed: amount.saturating_add(fee),
            })?;

        // the outputs: the payments and the change, in a random order (nothing about the position says which is which)
        let ctx = tx_context(&chosen[0].key_image);
        let mut slots: Vec<(Address, u64, Option<usize>)> = dests
            .iter()
            .enumerate()
            .map(|(i, (a, v))| (*a, *v, Some(i)))
            .collect();
        slots.push((self.address(), change, None));
        for i in (1..slots.len()).rev() {
            let j = below(rng, i as u64 + 1) as usize;
            slots.swap(i, j);
        }
        let mut enotes = Vec::new();
        for (i, (addr, value, _)) in slots.iter().enumerate() {
            enotes.push(
                create_enote(rng, addr, *value, &ctx, i as u32, false)
                    .ok_or(WalletError::BadAddress)?,
            );
        }
        let outputs: Vec<Output> = enotes.iter().map(|e| e.to_output()).collect();
        let prefix = TxPrefix {
            version: VERSION,
            inputs: chosen
                .iter()
                .map(|o| Input {
                    key_image: o.key_image,
                })
                .collect(),
            outputs,
            fee,
            extra: vec![],
        };

        // the rings: decoys for the coins that have none yet (the rounds that settle the fee keep the rings they made), then every
        // ring member not yet known fetched in ONE request
        let mut pending: Vec<(u64, Vec<u64>)> = Vec::new();
        for o in &chosen {
            if !cache.rings.contains_key(&o.global_index) {
                if cache.pool.is_none() {
                    cache.pool = Some(DecoyPool::new(chain, rules)?);
                }
                let pool = cache.pool.as_mut().expect("just made");
                let mut indexes = pool.pick(chain, rules, rng, o.global_index)?;
                indexes.push(o.global_index);
                indexes.sort_unstable();
                pending.push((o.global_index, indexes));
            }
        }
        if !pending.is_empty() {
            let mut want: Vec<u64> = pending
                .iter()
                .flat_map(|(_, ix)| ix.iter().copied())
                .collect();
            want.sort_unstable();
            want.dedup();
            let got = chain.outputs(&want).map_err(chain_err)?;
            if got.len() != want.len() {
                return Err(WalletError::Chain(
                    "the node answered about a different number of outputs than were asked".into(),
                ));
            }
            let known: std::collections::HashMap<u64, StoredOutput> = want
                .iter()
                .copied()
                .zip(got)
                .filter_map(|(i, o)| o.map(|o| (i, o)))
                .collect();
            for (real, indexes) in pending {
                let members = indexes
                    .iter()
                    .map(|i| known.get(i).cloned().ok_or(WalletError::NotEnoughDecoys))
                    .collect::<Result<Vec<_>, _>>()?;
                cache.rings.insert(real, (indexes, members));
            }
        }
        let mut spend_inputs = Vec::new();
        let mut ring_members: Vec<Vec<StoredOutput>> = Vec::new();
        for o in &chosen {
            let (indexes, members) = cache
                .rings
                .get(&o.global_index)
                .cloned()
                .expect("a ring was made for every chosen coin");
            let signer = indexes
                .iter()
                .position(|&i| i == o.global_index)
                .expect("the real output is in its ring");
            let secret = self
                .keys
                .onetime_secret(&o.offset)
                .ok_or(WalletError::Prove)?;
            spend_inputs.push(SpendInput {
                secret_key: secret,
                mask: o.mask,
                amount: o.amount,
                ring_indexes: indexes,
                ring: members
                    .iter()
                    .map(|m| [m.onetime_address, ring_commitment(m)])
                    .collect(),
                signer,
            });
            ring_members.push(members);
        }
        let secrets: Vec<OutputSecret> = enotes
            .iter()
            .zip(&slots)
            .map(|(e, (_, v, _))| OutputSecret {
                amount: *v,
                mask: e.mask,
            })
            .collect();
        let prunable = ringct::prove(rng, &rules.chain_id, &prefix, &spend_inputs, &secrets)
            .ok_or(WalletError::Prove)?;
        let tx = Transaction { prefix, prunable };
        // never send what does not verify
        ringct::verify_tx(&rules.chain_id, &tx, &ring_members)
            .map_err(|e| WalletError::SelfCheck(e.to_string()))?;
        let id = ids::tx_id(&tx).map_err(|e| WalletError::SelfCheck(e.to_string()))?;
        let mut parts: Vec<Option<PaymentPart>> = vec![None; dests.len()];
        let mut change_onetime = [0u8; 32];
        for (e, (addr, value, tag)) in enotes.iter().zip(&slots) {
            match tag {
                Some(i) => {
                    parts[*i] = Some(PaymentPart {
                        to: *addr,
                        amount: *value,
                        onetime: e.onetime_address,
                        secret: e.tx_secret.clone(),
                    })
                }
                None => change_onetime = e.onetime_address,
            }
        }
        let parts: Vec<PaymentPart> = parts.into_iter().flatten().collect();
        Ok(Built {
            tx,
            id,
            fee,
            amount,
            change,
            spends: chosen.iter().map(|o| o.key_image).collect(),
            change_onetime,
            payment_secret: parts[0].secret.clone(),
            payment_onetime: parts[0].onetime,
            parts,
        })
    }

    /// A payment to any number of recipients of any total the wallet can cover, as **several transactions that spend different coins**, so that all
    /// of them can be sent at once. Each pays at most [`MAX_RECIPIENTS`] recipients and spends at most as many coins as fit in `MAX_TX_SIZE`; when
    /// one recipient's amount needs more coins than that, it is paid in parts, in different transactions (the recipient gets several outputs).
    ///
    /// **What comes out may be less than was asked for**: the change of a transaction cannot be spent for 10 blocks, so when the coins that were
    /// there at the start run out before every recipient is paid, the rest is returned in [`Plan::unsent`] for a later batch. Nothing is sent
    /// and nothing is reserved (see [`Wallet::send_batch`]). An error is returned only when not even the first transaction can be built.
    pub fn build_batch(
        &mut self,
        chain: &impl ChainView,
        rng: &mut (impl RngCore + CryptoRng),
        dests: &[(Address, u64)],
        level: FeeLevel,
    ) -> Result<Plan, WalletError> {
        if dests.is_empty() || dests.iter().any(|(_, v)| *v == 0) {
            return Err(WalletError::ZeroAmount);
        }
        let rules = chain.rules().map_err(chain_err)?;
        let mut available = self.spendable(chain, &rules)?;
        let total = dests
            .iter()
            .try_fold(0u64, |a, (_, v)| a.checked_add(*v))
            .ok_or(WalletError::NotEnough {
                spendable: 0,
                needed: u64::MAX,
            })?;
        let spendable: u64 = available.iter().map(|o| o.amount).sum();
        if spendable < total {
            return Err(WalletError::NotEnough {
                spendable,
                needed: total,
            });
        }
        // the dearest a transaction can be: one of the largest size the rules allow
        let reserve = fee_for_size(&rules, level, MAX_TX_SIZE as u64)?;
        let mut pending: std::collections::VecDeque<(Address, u64)> =
            dests.iter().copied().collect();
        let mut txs: Vec<Built> = Vec::new();
        let mut cache = BuildCache::default();
        while !pending.is_empty() {
            let mut group: Vec<(Address, u64)> =
                pending.iter().take(MAX_RECIPIENTS).copied().collect();
            let limit = payment_input_limit(&rules, group.len() + 1);
            let mut amounts: Vec<u64> = available.iter().map(|o| o.amount).collect();
            amounts.sort_unstable_by(|a, b| b.cmp(a));
            let capacity: u64 = amounts
                .iter()
                .take(limit)
                .fold(0, |a, v| a.saturating_add(*v));
            let room = capacity.saturating_sub(reserve);
            if room == 0 {
                break;
            }
            let mut consumed = group.len();
            let mut leftover: Option<(Address, u64)> = None;
            if group.iter().map(|(_, v)| *v).sum::<u64>() > room {
                // pay what the coins of one transaction can: the recipients in order, the last one in part
                let mut left = room;
                let mut kept = Vec::new();
                consumed = 0;
                for (a, v) in &group {
                    if left == 0 {
                        break;
                    }
                    consumed += 1;
                    if *v <= left {
                        kept.push((*a, *v));
                        left -= *v;
                    } else {
                        kept.push((*a, left));
                        leftover = Some((*a, *v - left));
                        break;
                    }
                }
                group = kept;
            }
            let built = settle(&rules, level, |fee| {
                self.build_with_fee(chain, rng, &rules, &available, &group, fee, &mut cache)
            });
            match built {
                Ok(b) => {
                    available.retain(|o| !b.spends.contains(&o.key_image));
                    for _ in 0..consumed {
                        pending.pop_front();
                    }
                    if let Some(l) = leftover {
                        pending.push_front(l);
                    }
                    txs.push(b);
                }
                Err(e) if txs.is_empty() => return Err(e),
                Err(_) => break,
            }
        }
        Ok(Plan {
            txs,
            unsent: pending.into_iter().collect(),
        })
    }

    /// Combines coins: every spendable coin that is worth more than the fee it adds, grouped as many to a transaction as fit, each group paid to
    /// `to` (this wallet's own address when it is `None`) in one output. For a wallet that has been paid in many small amounts (a miner's block
    /// rewards), this is what makes them one coin. Coins that are alone in their group are left alone when the destination is this wallet. Nothing
    /// is sent and nothing is reserved. **The result of a combine cannot be spent for 10 blocks.**
    pub fn build_sweep(
        &mut self,
        chain: &impl ChainView,
        rng: &mut (impl RngCore + CryptoRng),
        to: Option<&Address>,
        level: FeeLevel,
    ) -> Result<Vec<Built>, WalletError> {
        let rules = chain.rules().map_err(chain_err)?;
        let mut coins = self.spendable(chain, &rules)?;
        let dest = to.copied().unwrap_or_else(|| self.address());
        let limit = payment_input_limit(&rules, 2);
        let per_input = fee_for_size(
            &rules,
            level,
            (transaction_size(2, 2, rules.ring_size) - transaction_size(1, 2, rules.ring_size))
                as u64,
        )?;
        coins.retain(|o| o.amount > per_input);
        coins.sort_by_key(|o| o.amount);
        let mut cache = BuildCache::default();
        let mut out = Vec::new();
        for group in coins.chunks(limit) {
            if to.is_none() && group.len() < 2 {
                continue;
            }
            out.push(self.consolidate(
                chain,
                rng,
                &rules,
                group.to_vec(),
                dest,
                level,
                &mut cache,
            )?);
        }
        if out.is_empty() {
            return Err(WalletError::NothingToCombine);
        }
        Ok(out)
    }

    /// Combines `count` coins (the smallest ones that are worth more than the fee they add) into one coin of this wallet: the manual form of
    /// [`Wallet::build_sweep`]. `count` is from 2 up to what one transaction can spend.
    pub fn build_combine(
        &mut self,
        chain: &impl ChainView,
        rng: &mut (impl RngCore + CryptoRng),
        count: usize,
        level: FeeLevel,
    ) -> Result<Built, WalletError> {
        let rules = chain.rules().map_err(chain_err)?;
        let limit = payment_input_limit(&rules, 2);
        if count > limit {
            return Err(WalletError::TooManyInputs { max: limit });
        }
        if count < 2 {
            return Err(WalletError::NothingToCombine);
        }
        let mut coins = self.spendable(chain, &rules)?;
        let per_input = fee_for_size(
            &rules,
            level,
            (transaction_size(2, 2, rules.ring_size) - transaction_size(1, 2, rules.ring_size))
                as u64,
        )?;
        coins.retain(|o| o.amount > per_input);
        coins.sort_by_key(|o| o.amount);
        if coins.len() < count {
            return Err(WalletError::NothingToCombine);
        }
        coins.truncate(count);
        let dest = self.address();
        self.consolidate(
            chain,
            rng,
            &rules,
            coins,
            dest,
            level,
            &mut BuildCache::default(),
        )
    }

    /// One transaction that spends exactly `coins` and pays everything but the fee to `dest`.
    #[allow(clippy::too_many_arguments)]
    fn consolidate(
        &self,
        chain: &impl ChainView,
        rng: &mut (impl RngCore + CryptoRng),
        rules: &Rules,
        coins: Vec<Owned>,
        dest: Address,
        level: FeeLevel,
        cache: &mut BuildCache,
    ) -> Result<Built, WalletError> {
        let total: u64 = coins.iter().map(|o| o.amount).sum();
        settle(rules, level, |fee| {
            let amount = total
                .checked_sub(fee)
                .filter(|a| *a > 0)
                .ok_or(WalletError::NothingToCombine)?;
            self.assemble(
                chain,
                rng,
                rules,
                coins.clone(),
                &[(dest, amount)],
                fee,
                cache,
            )
        })
    }

    /// Hands the transactions of a batch to the node one after another and reserves the coins of each. It stops at the first the node refuses: the ones
    /// before it are sent, and the report says how many.
    pub fn send_batch<C: ChainView + Submitter>(
        &mut self,
        node: &mut C,
        builts: &[Built],
    ) -> BatchSent {
        for (i, b) in builts.iter().enumerate() {
            if let Err(e) = self.send_built(node, b) {
                return BatchSent {
                    sent: i,
                    failed: Some(e),
                };
            }
        }
        BatchSent {
            sent: builts.len(),
            failed: None,
        }
    }

    /// Builds a payment, hands it to the node, and reserves the coins it spends.
    pub fn pay<C: ChainView + Submitter>(
        &mut self,
        node: &mut C,
        rng: &mut (impl RngCore + CryptoRng),
        to: &Address,
        amount: u64,
    ) -> Result<Built, WalletError> {
        let built = self.build_payment(&*node, rng, to, amount)?;
        self.send_built(node, &built)?;
        Ok(built)
    }

    /// Hands a payment built by [`Wallet::build_payment_at`] to the node and reserves the coins it spends. (Build,
    /// show the person the fee, and then send: nothing is reserved by building.)
    pub fn send_built<C: ChainView + Submitter>(
        &mut self,
        node: &mut C,
        built: &Built,
    ) -> Result<(), WalletError> {
        let next_height = node.rules().map_err(chain_err)?.next_height;
        node.submit(built.tx.clone()).map_err(WalletError::Submit)?;
        for ki in &built.spends {
            self.reserved.push(Reserved {
                key_image: *ki,
                until_height: next_height + RESERVE_BLOCKS,
            });
        }
        Ok(())
    }
}

/// Whether a list of recipients can go in one transaction: at least one, every amount more than nothing, at most [`MAX_RECIPIENTS`], a total that fits.
fn check_dests(dests: &[(Address, u64)]) -> Result<(), WalletError> {
    if dests.is_empty() || dests.iter().any(|(_, v)| *v == 0) {
        return Err(WalletError::ZeroAmount);
    }
    if dests.len() > MAX_RECIPIENTS {
        return Err(WalletError::TooManyRecipients {
            max: MAX_RECIPIENTS,
        });
    }
    dests
        .iter()
        .try_fold(0u64, |a, (_, v)| a.checked_add(*v))
        .map(|_| ())
        .ok_or(WalletError::NotEnough {
            spendable: 0,
            needed: u64::MAX,
        })
}

/// The fee for a transaction of `size` bytes at a fee level: the level's share of the minimum the next block needs, and at least one unit more.
fn fee_for_size(rules: &Rules, level: FeeLevel, size: u64) -> Result<u64, WalletError> {
    let min =
        fees::dynamic_min_fee(size, rules.reward, rules.median).map_err(WalletError::Chain)?;
    Ok(min.saturating_mul(level.percent_of_minimum()) / 100 + 1)
}

/// Builds a transaction whose fee is exactly the level's share of the minimum for the size it ends up with. The fee depends on the size, which
/// depends on which coins are spent, which depends on the fee: it starts from a guess and rebuilds if the size shows the guess was too low (the fee
/// field has a fixed width, so a different fee does not change the size).
fn settle(
    rules: &Rules,
    level: FeeLevel,
    mut build: impl FnMut(u64) -> Result<Built, WalletError>,
) -> Result<Built, WalletError> {
    let mut fee =
        fees::dynamic_min_fee(2_000, rules.reward, rules.median).map_err(WalletError::Chain)?;
    for round in 0..6 {
        let built = build(fee)?;
        let size = built
            .tx
            .to_bytes()
            .map_err(|e| WalletError::SelfCheck(e.to_string()))?
            .len() as u64;
        let wanted = fee_for_size(rules, level, size)?;
        // A different fee can change which coins are chosen and so the size; if that has not settled after a few rounds, a fee that is at least
        // the level's is taken rather than failing.
        if fee == wanted || (round >= 3 && fee >= wanted) {
            return Ok(built);
        }
        fee = wanted;
    }
    Err(WalletError::Chain("could not settle on a fee".into()))
}

/// The size in bytes of a transaction this wallet builds, spending `n_inputs` coins with rings of `ring_size` and paying `n_outputs` outputs (the
/// payments and the change) with an empty `extra`. Worked out from the layouts of `CONSENSUS_V2.md` 6.2 and of `proof_data` (`tenero-crypto`,
/// `ringct.rs`), not measured at run time; tests build real transactions and check it to the byte.
pub fn transaction_size(n_inputs: usize, n_outputs: usize, ring_size: usize) -> usize {
    // version, input count, key images, output count, the outputs of 123, fee, extra length
    let prefix = 2 + 4 + 32 * n_inputs + 4 + 123 * n_outputs + 8 + 4;
    // a Bulletproofs+ proof of n 64-bit outputs, padded to a power of two: 6 32-byte values, 2 * log2(64 * padded) curve points, 2 one-byte lengths
    let rounds = (64 * n_outputs.next_power_of_two()).ilog2() as usize;
    let range_proof = 32 * 6 + 32 * 2 * rounds + 2;
    // a pseudo-output and a CLSAG (s: one scalar per ring member, c1, D) for every input
    let proof = n_inputs * (32 + 32 * ring_size + 64) + range_proof;
    // the ring count, each ring (its length and its indexes), the proof length and the proof
    let prunable = 4 + n_inputs * (4 + 8 * ring_size) + 4 + proof;
    prefix + prunable
}

/// [`transaction_size`] of a payment with one recipient: two outputs.
pub fn payment_size(n_inputs: usize, ring_size: usize) -> usize {
    transaction_size(n_inputs, 2, ring_size)
}

/// How many coins a transaction of `n_outputs` outputs can spend: the most whose [`transaction_size`] is within `MAX_TX_SIZE`.
pub fn max_inputs_for(n_outputs: usize, ring_size: usize) -> usize {
    let mut n = 1;
    while transaction_size(n + 1, n_outputs, ring_size) <= MAX_TX_SIZE {
        n += 1;
    }
    n
}

/// How many coins one payment can spend: [`max_inputs_for`] two outputs.
pub fn max_payment_inputs(ring_size: usize) -> usize {
    max_inputs_for(2, ring_size)
}

/// How many coins a transaction of `n_outputs` outputs can spend on this chain: what fits in `MAX_TX_SIZE`, and no more than the chain's own limit
/// on inputs if it has one (`Rules::max_inputs`).
pub fn payment_input_limit(rules: &Rules, n_outputs: usize) -> usize {
    let by_size = max_inputs_for(n_outputs, rules.ring_size);
    rules.max_inputs.map_or(by_size, |m| m.min(by_size))
}

/// Which outputs to spend: the smallest single one that covers `needed`, else the largest first until it is
/// covered. Fewer inputs mean a smaller transaction and a smaller fee. (Spending the same way every time is a
/// pattern a chain observer could use; a randomised policy is future work.)
fn select_coins(
    candidates: &[Owned],
    needed: u64,
    max_inputs: usize,
) -> Result<Vec<Owned>, WalletError> {
    let spendable: u64 = candidates
        .iter()
        .map(|o| o.amount)
        .fold(0, u64::saturating_add);
    if spendable < needed {
        return Err(WalletError::NotEnough { spendable, needed });
    }
    if let Some(one) = candidates
        .iter()
        .filter(|o| o.amount >= needed)
        .min_by_key(|o| o.amount)
    {
        return Ok(vec![one.clone()]);
    }
    let mut sorted = candidates.to_vec();
    sorted.sort_by_key(|o| std::cmp::Reverse(o.amount));
    let mut chosen = Vec::new();
    let mut sum = 0u64;
    for o in sorted {
        sum = sum.saturating_add(o.amount);
        chosen.push(o);
        if sum >= needed {
            break;
        }
    }
    if chosen.len() > max_inputs {
        return Err(WalletError::TooManyInputs { max: max_inputs });
    }
    Ok(chosen)
}

/// What is learned while one payment is built, so that the rounds that settle the fee (and every coin of the payment) do not
/// ask the node the same things again. A payment of 32 coins used to ask it about 1,400 outputs, each a round trip of about
/// 15 ms over the control socket: twenty seconds of waiting for half a second of work. Nothing is kept between payments.
#[derive(Default)]
struct BuildCache {
    pool: Option<DecoyPool>,
    /// The ring made for a coin (by its global index): the sorted indexes, the real one among them, and the outputs.
    rings: std::collections::HashMap<u64, (Vec<u64>, Vec<StoredOutput>)>,
}

/// Which outputs a ring may use besides the real one, worked out once for a payment.
///
/// **The policy, and its limit:** an output is picked by how far it is from the newest *usable* one, with the distance
/// log-uniform (`exp(U * ln N)`), so newer outputs are far more likely than old ones, as real spends are. This is a
/// simplification of Monero's gamma distribution of output ages and has not been checked against how real spends behave
/// on this chain (which has none yet). The usable ones are found first so that no pick is wasted on an output that has
/// not matured: on a young chain most of the newest ones have not, and picking among all of them and throwing most away
/// cost hundreds of round trips to the node for every payment.
///
/// Heights never decrease with the output index, which is what makes the search work: outputs at or below
/// `next_height - max(maturities)` are mature whatever they are (`prefix` counts them: found in about `log2(total)`
/// requests); between that and `next_height - min(maturities)` only the ones that are not block rewards are (`after`, a
/// short walk, made only when the prefix alone is too small).
struct DecoyPool {
    total: u64,
    prefix: u64,
    after: Option<Vec<u64>>,
}

impl DecoyPool {
    fn new(chain: &impl ChainView, rules: &Rules) -> Result<DecoyPool, WalletError> {
        let total = chain.output_count().map_err(chain_err)?;
        if total < rules.ring_size as u64 {
            return Err(WalletError::NotEnoughDecoys);
        }
        let max_wait = rules.coinbase_maturity.max(rules.spend_maturity);
        let mut prefix = 0;
        if let Some(safe) = rules.next_height.checked_sub(max_wait) {
            let (mut lo, mut hi) = (0u64, total);
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                match chain.output(mid).map_err(chain_err)? {
                    Some(o) if o.height <= safe => lo = mid + 1,
                    _ => hi = mid,
                }
            }
            prefix = lo;
        }
        Ok(DecoyPool {
            total,
            prefix,
            after: None,
        })
    }

    /// The mature outputs just after the prefix: enough of them that, with the prefix, a ring can still be made when the
    /// real coin is one of them.
    fn walk_after(
        &mut self,
        chain: &impl ChainView,
        rules: &Rules,
        enough: u64,
    ) -> Result<&[u64], WalletError> {
        if self.after.is_none() {
            let min_wait = rules.coinbase_maturity.min(rules.spend_maturity);
            let mut after = Vec::new();
            let (mut index, mut walked) = (self.prefix, 0);
            while self.prefix + (after.len() as u64) < enough + 1
                && index < self.total
                && walked < 5_000
            {
                let Some(o) = chain.output(index).map_err(chain_err)? else {
                    break;
                };
                if o.height.saturating_add(min_wait) > rules.next_height {
                    break;
                }
                if is_mature(rules, o.height, o.coinbase) {
                    after.push(index);
                }
                index += 1;
                walked += 1;
            }
            self.after = Some(after);
        }
        Ok(self.after.as_deref().unwrap_or(&[]))
    }

    /// `ring_size - 1` other outputs to hide `real` among: distinct, existing and mature (the rules refuse an immature ring
    /// member).
    fn pick(
        &mut self,
        chain: &impl ChainView,
        rules: &Rules,
        rng: &mut impl RngCore,
        real: u64,
    ) -> Result<Vec<u64>, WalletError> {
        let want = rules.ring_size.saturating_sub(1);
        let mut picked: Vec<u64> = Vec::new();
        if want == 0 {
            return Ok(picked);
        }
        let p = self.prefix;
        if p - u64::from(real < p) >= want as u64 {
            // every index below `p` is mature: no question to the node is needed to know it
            let ln = (p as f64).ln();
            let mut tries = 0;
            while picked.len() < want && tries < 200 * rules.ring_size {
                tries += 1;
                let u = (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
                let distance = ((u * ln).exp() as u64).clamp(1, p);
                let index = p - distance;
                if index != real && !picked.contains(&index) {
                    picked.push(index);
                }
            }
            // the distribution can keep landing on the same few: fill the rest from a random start, in order
            let start = below(rng, p);
            let mut k = 0;
            while picked.len() < want && k < p {
                let index = (start + k) % p;
                if index != real && !picked.contains(&index) {
                    picked.push(index);
                }
                k += 1;
            }
        } else {
            // few usable outputs: choose among them at random
            let after: Vec<u64> = self.walk_after(chain, rules, want as u64)?.to_vec();
            let mut all: Vec<u64> = (0..p).chain(after).filter(|&i| i != real).collect();
            if (all.len() as u64) < want as u64 {
                return Err(WalletError::NotEnoughDecoys);
            }
            while picked.len() < want && !all.is_empty() {
                let i = below(rng, all.len() as u64) as usize;
                picked.push(all.swap_remove(i));
            }
        }
        if picked.len() == want {
            Ok(picked)
        } else {
            Err(WalletError::NotEnoughDecoys)
        }
    }
}
