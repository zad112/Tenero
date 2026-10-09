//! The wallet on the `gamma` network: scanning the chain for one's Carrot outputs, balances, and building a payment
//! proven with FCMP++.
//!
//! **Unaudited.** The Carrot derivations are `tenero-carrot` (our transcription, checked bit for bit against Monero's
//! `carrot_core`), the proofs `tenero-crypto::fcmp` (monero-oxide's FCMP++, only partly audited). Coin selection here is a
//! simple policy (see [`select_coins`]) that has not been studied for what it reveals. There are no decoys: an FCMP++
//! proof hides the spent output among every output in the curve tree.

use std::collections::{HashMap, HashSet};

use curve25519_dalek::scalar::Scalar;
use rand_core::{CryptoRng, RngCore};
use tenero_carrot::account::{AccountPublic, AccountSecrets, AddressIndex, ViewAll, ViewReceived};
use tenero_carrot::output::{
    additional_payment_proposal, output_set, Additional, PaymentProposal, SelfSendKey,
};
use tenero_carrot::scan::{
    key_image, scan_coinbase, scan_external, scan_internal, shared_secret, spend_keys, Found,
    Received,
};
use tenero_carrot::{CoinbaseEnote, Enote, JanusAnchor, PaymentId, NULL_PAYMENT_ID};
use tenero_core::v3::{
    ids, n_ephemeral_keys, rules, Input, Output, Prunable, Transaction, TxPrefix, Wire,
    MAX_OUTPUTS, MAX_TX_SIZE, VERSION,
};
use tenero_crypto::fcmp::{self, OutputSecret, Spend};
use zeroize::Zeroizing;

use crate::address::{carrot_master, Address, Kind, Network};
use crate::chain::{is_mature, ChainView, Rules, Submitter};

/// How many scanned block ids the wallet remembers to notice a reorganisation. A reorganisation deeper than
/// this makes the wallet rescan from its birth height.
pub const RECENT_BLOCKS: usize = 100;

/// The most recipients one transaction can pay: 16 outputs, and one is always the change.
pub const MAX_RECIPIENTS: usize = MAX_OUTPUTS - 1;

/// How many blocks the wallet asks a node for at a time while scanning.
pub const SCAN_BATCH: u64 = 64;

/// A transaction the wallet has sent keeps its inputs reserved for this many blocks (so that a second payment
/// does not pick the same coins while the first is still waiting); after that, if the coins are unspent, they
/// are free again (the transaction was probably dropped).
pub const RESERVE_BLOCKS: u64 = 20;

/// The fee is the minimum the next block needs, plus this many percent (the minimum moves with the block-weight
/// median between sending and mining), and at least one unit more.
pub const FEE_MARGIN_PERCENT: u64 = 25;

/// How many subaddresses past the highest one used the wallet watches for (subaddress `(0, 1)` to `(0, n)` at first).
pub const SUBADDRESS_LOOKAHEAD: u32 = 50;

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
    /// The payment would need more coins than one transaction can carry (`MAX_TX_SIZE`): `max` is how many fit.
    TooManyInputs {
        max: usize,
    },
    /// A view-only wallet cannot spend (it has no spend key).
    ViewOnly,
    /// A transaction has at most `MAX_RECIPIENTS` recipients (16 outputs, one of them the change).
    TooManyRecipients {
        max: usize,
    },
    /// There is nothing worth combining: fewer than two coins, or coins worth less than the fee they would add.
    NothingToCombine,
    /// The recipient address cannot be paid this way (another network; a subaddress or an integrated address for a block
    /// reward; two integrated addresses in one transaction).
    BadAddress,
    /// The prover refused (a coin's path or secret does not fit the tree).
    Prove,
    /// The transaction the wallet built does not verify, or is not the size it should be: a bug, never sent.
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
            WalletError::BadAddress => write!(f, "that address cannot be paid this way"),
            WalletError::ViewOnly => write!(
                f,
                "this is a view-only wallet: it can see, but it cannot spend or sign (that needs the wallet with its 24 words)"
            ),
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

/// One output that belongs to the wallet, with what spending it needs (no spend secret: that is recomputed from the
/// account when it is spent).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owned {
    pub global_index: u64,
    pub height: u64,
    pub coinbase: bool,
    pub onetime_address: [u8; 32],
    /// The commitment as the curve tree has it (a coinbase output's is `1*G + amount*H`).
    pub commitment: [u8; 32],
    pub amount: u64,
    /// The commitment's blinding factor.
    pub blinding: [u8; 32],
    /// The address it was sent to.
    pub address: AddressIndex,
    /// The two sender extensions (`k^o_g`, `k^o_t`): with the account's keys they give the output's secrets.
    pub extension_g: [u8; 32],
    pub extension_t: [u8; 32],
    pub key_image: [u8; 32],
    /// The payment ID it came with (all zero for none).
    pub payment_id: PaymentId,
    /// Change or a self-send of this wallet (found with the view-balance secret), not a payment received.
    pub internal: bool,
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
    /// Of that, what can go into a transaction now (in the curve tree and not promised to a pending payment).
    pub spendable: u64,
    /// Waiting to enter the curve tree.
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

/// One recipient's output of a transaction: what proves that payment later (with the transaction's first key image, the
/// anchor re-derives the shared secret: `tenero_carrot::scan::scan_external_as_sender`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentPart {
    pub to: Address,
    pub amount: u64,
    pub onetime: [u8; 32],
    /// The Janus anchor chosen for this payment (`anchor_norm`): the sender's secret for it.
    pub anchor: JanusAnchor,
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
    /// The one-time address of the change output.
    pub change_onetime: [u8; 32],
    /// Every payment output, in the order the recipients were given.
    pub parts: Vec<PaymentPart>,
}

/// A payment broken into transactions that spend different coins (so they can all be sent at once): what
/// [`Wallet::build_batch`] makes.
#[derive(Debug)]
pub struct Plan {
    pub txs: Vec<Built>,
    /// What could not be built now (the coins that were left, or their count, ran out): the recipients and amounts still to
    /// pay. Change takes 10 blocks to be spendable, so these are for a later batch.
    pub unsent: Vec<(Address, u64)>,
}

/// One transaction of a payment as [`Wallet::quote_batch`] works it out, before it is made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxQuote {
    /// The coins it spends.
    pub inputs: usize,
    pub outputs: usize,
    /// Its exact size in bytes, proofs included.
    pub size: u64,
    pub fee: u64,
}

/// A transaction on the chain that spends coins of this wallet, as scanning finds it: what a wallet that did not send it
/// (a view-all wallet, or a wallet restored from its words) still knows of a payment out. **Whom it paid is not known**:
/// Carrot hides the recipient from everyone but the sender's own record. Only a wallet that can compute its coins' key
/// images finds these (a full or view-all wallet, not a view-received one).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outgoing {
    /// The block it is in.
    pub height: u64,
    /// The key images of this wallet's coins it spends (they identify the transaction).
    pub spends: Vec<[u8; 32]>,
    /// What those coins held.
    pub spent: u64,
    /// What came back to this wallet in the same transaction: change, or what it paid itself.
    pub returned: u64,
    pub fee: u64,
}

impl Outgoing {
    /// What left the wallet besides the fee: 0 for a transaction that only moved its own coins (a combine).
    pub fn sent(&self) -> u64 {
        self.spent
            .saturating_sub(self.returned)
            .saturating_sub(self.fee)
    }
}

/// How a [`Wallet::send_batch`] went: the first `sent` transactions were handed to the node (and their coins reserved);
/// `failed` is why the next one was not.
#[derive(Debug)]
pub struct BatchSent {
    pub sent: usize,
    pub failed: Option<WalletError>,
}

pub struct Wallet {
    /// The seed: `None` for a view-only wallet, which never had it.
    seed: Option<Zeroizing<[u8; 32]>>,
    network: Network,
    access: Access,
    /// Every address the wallet watches, by its spend key: the main address and subaddresses (0, 1) up to (0, `watched`).
    addresses: HashMap<[u8; 32], AddressIndex>,
    watched: u32,
    /// The height scanning starts from on a fresh wallet (so a new wallet does not read the whole chain).
    pub(crate) birth_height: u64,
    /// The last height scanned.
    pub(crate) scanned: Option<u64>,
    /// The ids of the last scanned blocks, oldest first.
    pub(crate) recent: Vec<(u64, [u8; 32])>,
    pub(crate) owned: Vec<Owned>,
    /// The transactions found spending this wallet's coins, oldest first.
    pub(crate) outgoing: Vec<Outgoing>,
    pub(crate) reserved: Vec<Reserved>,
    /// Key images the chain has said are spent (a spent output stays spent, so it is asked once). In memory only; forgotten
    /// when a reorganisation is seen, which is the only way a spend can undo.
    spent_cache: HashSet<[u8; 32]>,
    /// Key images the chain said were NOT spent, and the tip they were asked at.
    unspent_cache: HashSet<[u8; 32]>,
    unspent_tip: Option<[u8; 32]>,
}

/// How much of an account a wallet holds (Carrot 5.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewTier {
    /// Every secret: it sees everything and can spend.
    Full,
    /// The view-balance secret and the partial spend key: it sees incoming payments, its own change, and which of its
    /// outputs are spent (it computes key images), so its balance is right. It cannot spend or sign.
    ViewAll,
    /// The incoming view key: it sees incoming payments only. It cannot see change or spends, so what it shows is what was
    /// RECEIVED, not a balance. It cannot spend or sign.
    ViewReceived,
}

/// The secrets a wallet holds, one tier of [`ViewTier`].
#[derive(Clone)]
pub(crate) enum Access {
    Full(Box<AccountSecrets>),
    ViewAll(Box<ViewAll>),
    ViewReceived(Box<ViewReceived>),
}

impl Access {
    pub(crate) fn tier(&self) -> ViewTier {
        match self {
            Access::Full(_) => ViewTier::Full,
            Access::ViewAll(_) => ViewTier::ViewAll,
            Access::ViewReceived(_) => ViewTier::ViewReceived,
        }
    }

    pub(crate) fn public(&self) -> &AccountPublic {
        match self {
            Access::Full(a) => &a.public,
            Access::ViewAll(v) => &v.public,
            Access::ViewReceived(v) => &v.public,
        }
    }

    fn k_view(&self) -> Scalar {
        match self {
            Access::Full(a) => a.k_view_incoming,
            Access::ViewAll(v) => v.view_received().k_view_incoming,
            Access::ViewReceived(v) => v.k_view_incoming,
        }
    }

    fn s_generate_address(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(match self {
            Access::Full(a) => a.s_generate_address,
            Access::ViewAll(v) => v.view_received().s_generate_address,
            Access::ViewReceived(v) => v.s_generate_address,
        })
    }

    fn s_view_balance(&self) -> Option<Zeroizing<[u8; 32]>> {
        match self {
            Access::Full(a) => Some(Zeroizing::new(a.s_view_balance)),
            Access::ViewAll(v) => Some(Zeroizing::new(v.s_view_balance)),
            Access::ViewReceived(_) => None,
        }
    }

    fn full(&self) -> Option<&AccountSecrets> {
        match self {
            Access::Full(a) => Some(a),
            _ => None,
        }
    }

    pub(crate) fn address(
        &self,
        index: AddressIndex,
    ) -> Result<tenero_carrot::account::Destination, tenero_carrot::CarrotError> {
        tenero_carrot::account::address(self.public(), &self.s_generate_address(), index)
    }

    /// The key image of an output this tier found at `index`, if the output is really this account's: `Some(key image)`,
    /// `Some([0; 32])` for a view-received wallet (which cannot compute it), `None` if the output is not ours.
    fn key_image_of(
        &self,
        index: AddressIndex,
        r: &Received,
        onetime_address: &[u8; 32],
    ) -> Option<[u8; 32]> {
        match self {
            Access::Full(a) => {
                let keys = spend_keys(a, index, r, onetime_address)?;
                Some(key_image(&keys.x, onetime_address))
            }
            Access::ViewAll(v) => {
                // the output must be the address's key plus the sender's extensions
                self.opens_at(index, r, onetime_address).then(|| {
                    tenero_carrot::scan::key_image_view_all(
                        v,
                        &self.s_generate_address(),
                        index,
                        r,
                        onetime_address,
                    )
                })
            }
            Access::ViewReceived(_) => self.opens_at(index, r, onetime_address).then_some([0; 32]),
        }
    }

    /// Whether `Ko = K^j_s + k_g G + k_t T` (the output is the address's), checked with public keys only.
    fn opens_at(&self, index: AddressIndex, r: &Received, onetime_address: &[u8; 32]) -> bool {
        let Ok(d) = self.address(index) else {
            return false;
        };
        let Some(k) = tenero_carrot::points::decompress(&d.spend_pubkey) else {
            return false;
        };
        let ko = k
            + curve25519_dalek::edwards::EdwardsPoint::mul_base(&r.sender_extension_g)
            + *tenero_carrot::points::T * r.sender_extension_t;
        tenero_carrot::points::compress(&ko) == *onetime_address
    }
}

const VIEW_KEY_CHECKSUM_TAG: &[u8] = b"tenero view key v1";

/// The view-all tier from `s_vb` and `K_ps`; `None` if `K_ps` is not a point.
pub(crate) fn view_all_access(
    s_view_balance: [u8; 32],
    partial_spend_pubkey: [u8; 32],
) -> Option<Access> {
    ViewAll::new(s_view_balance, partial_spend_pubkey)
        .ok()
        .map(|v| Access::ViewAll(Box::new(v)))
}

/// The view-received tier from `k_v`, `s_ga` and the account's spend key; `None` if a key is not one.
pub(crate) fn view_received_access(
    k_view: [u8; 32],
    s_generate_address: [u8; 32],
    spend_pubkey: [u8; 32],
) -> Option<Access> {
    let k_v = Option::<Scalar>::from(Scalar::from_canonical_bytes(k_view))?;
    let spend = tenero_carrot::points::decompress(&spend_pubkey)?;
    let public = AccountPublic {
        spend_pubkey,
        view_pubkey: tenero_carrot::points::compress(&(spend * k_v)),
        main_view_pubkey: tenero_carrot::points::compress(
            &curve25519_dalek::edwards::EdwardsPoint::mul_base(&k_v),
        ),
    };
    Some(Access::ViewReceived(Box::new(ViewReceived {
        k_view_incoming: k_v,
        s_generate_address,
        public,
    })))
}

fn scalar(b: &[u8; 32]) -> Scalar {
    Scalar::from_bytes_mod_order(*b)
}

impl Wallet {
    /// A new wallet with a fresh random seed. It will scan from `birth_height` (the chain's tip when it is made,
    /// so it never reads old blocks that cannot hold its coins).
    pub fn create(
        rng: &mut (impl RngCore + CryptoRng + Send),
        network: Network,
        birth_height: u64,
    ) -> Wallet {
        let mut seed = Zeroizing::new([0u8; 32]);
        rng.fill_bytes(&mut *seed);
        Wallet::from_seed(&seed, network, birth_height)
    }

    /// The wallet of a seed (to restore one: use a birth height at or before its first coin, or 0). Its Carrot master
    /// secret is [`carrot_master`] of the seed.
    pub fn from_seed(seed: &[u8; 32], network: Network, birth_height: u64) -> Wallet {
        let account = AccountSecrets::from_master(&carrot_master(seed));
        Wallet::with_access(
            Some(Zeroizing::new(*seed)),
            Access::Full(Box::new(account)),
            network,
            birth_height,
        )
    }

    /// The text form of a view key: this, then base58 of `network u8 | tier u8 (1 view-all, 2 view-received) | birth height
    /// u64 | keys | checksum (4)`. The keys are `s_vb | K_ps` for view-all and `k_v | s_ga | K_s` for view-received. **A
    /// view key is a secret**: whoever has it sees what it shows (a view-all key: every incoming and outgoing payment and the
    /// balance). It cannot spend.
    pub const VIEW_KEY_PREFIX: &'static str = "TENview1";

    /// This wallet's view key of `tier` (a full wallet gives either; a view-all wallet a view-all or view-received one; a
    /// view-received wallet only its own). `None` for a tier it does not hold, or [`ViewTier::Full`].
    pub fn view_key(&self, tier: ViewTier) -> Option<Zeroizing<String>> {
        let mut body = Zeroizing::new(vec![
            crate::file::network_byte(self.network),
            match tier {
                ViewTier::ViewAll => 1,
                ViewTier::ViewReceived => 2,
                ViewTier::Full => return None,
            },
        ]);
        body.extend_from_slice(&self.birth_height.to_le_bytes());
        match (tier, &self.access) {
            (ViewTier::ViewAll, Access::Full(a)) => {
                body.extend_from_slice(&a.s_view_balance);
                body.extend_from_slice(&tenero_carrot::derive::make_partial_spend_pubkey(
                    &a.k_prove_spend,
                ));
            }
            (ViewTier::ViewAll, Access::ViewAll(v)) => {
                body.extend_from_slice(&v.s_view_balance);
                body.extend_from_slice(&v.partial_spend_pubkey);
            }
            (ViewTier::ViewReceived, access) => {
                body.extend_from_slice(access.k_view().as_bytes());
                body.extend_from_slice(&*access.s_generate_address());
                body.extend_from_slice(&access.public().spend_pubkey);
            }
            _ => return None,
        }
        let check = tenero_core::hash::sha256(&[VIEW_KEY_CHECKSUM_TAG, &body]);
        body.extend_from_slice(&check[..4]);
        Some(Zeroizing::new(format!(
            "{}{}",
            Wallet::VIEW_KEY_PREFIX,
            crate::address::b58encode(&body)
        )))
    }

    /// A view-only wallet from a view key's text, for `network` (a key of another network is refused). It scans from the
    /// birth height the key carries.
    pub fn from_view_key(text: &str, network: Network) -> Result<Wallet, String> {
        let body = text
            .trim()
            .strip_prefix(Wallet::VIEW_KEY_PREFIX)
            .ok_or("a view key starts with TENview1")?;
        let data = Zeroizing::new(
            crate::address::b58decode(body)
                .map_err(|_| "not a view key (its characters)".to_string())?,
        );
        if data.len() < 14 {
            return Err("not a view key (too short)".into());
        }
        let (body, check) = data.split_at(data.len() - 4);
        if tenero_core::hash::sha256(&[VIEW_KEY_CHECKSUM_TAG, body])[..4] != *check {
            return Err(
                "not a view key (a character is wrong: the checksum does not match)".into(),
            );
        }
        if crate::file::network_of_byte(body[0]) != Some(network) {
            return Err(format!(
                "that view key is not for the {} network",
                network.name()
            ));
        }
        let birth = u64::from_le_bytes(body[2..10].try_into().expect("8"));
        let keys = &body[10..];
        let b32 = |i: usize| -> [u8; 32] { keys[i..i + 32].try_into().expect("32") };
        let access = match (body[1], keys.len()) {
            (1, 64) => view_all_access(b32(0), b32(32)),
            (2, 96) => view_received_access(b32(0), b32(32), b32(64)),
            _ => None,
        }
        .ok_or("not a view key (its keys)")?;
        Ok(Wallet::with_access(None, access, network, birth))
    }

    /// A wallet of one of the view-only tiers, from its keys ([`Wallet::from_view_key`] reads them from text).
    pub(crate) fn with_access(
        seed: Option<Zeroizing<[u8; 32]>>,
        access: Access,
        network: Network,
        birth_height: u64,
    ) -> Wallet {
        let mut w = Wallet {
            seed,
            network,
            addresses: HashMap::from([(access.public().spend_pubkey, AddressIndex::MAIN)]),
            access,
            watched: 0,
            birth_height,
            scanned: None,
            recent: Vec::new(),
            owned: Vec::new(),
            outgoing: Vec::new(),
            reserved: Vec::new(),
            spent_cache: HashSet::new(),
            unspent_cache: HashSet::new(),
            unspent_tip: None,
        };
        w.watch_up_to(SUBADDRESS_LOOKAHEAD);
        w
    }

    /// Watches subaddresses (0, 1) to (0, `n`).
    fn watch_up_to(&mut self, n: u32) {
        while self.watched < n {
            self.watched += 1;
            let index = AddressIndex {
                major: 0,
                minor: self.watched,
            };
            let d = self.access.address(index).expect("a subaddress index");
            self.addresses.insert(d.spend_pubkey, index);
        }
    }

    pub fn network(&self) -> Network {
        self.network
    }

    /// The main address: where block rewards go (Carrot pays a coinbase output to a main address only).
    pub fn address(&self) -> Address {
        Address::of(self.network, &self.access.public().main_address())
    }

    /// Subaddress `(0, minor)` (`minor` from 1): an address of its own for each payer, that nobody can link to the others.
    /// The wallet watches every subaddress up to the highest asked for, plus [`SUBADDRESS_LOOKAHEAD`].
    pub fn subaddress(&mut self, minor: u32) -> Option<Address> {
        if minor == 0 {
            return None;
        }
        self.watch_up_to(minor.saturating_add(SUBADDRESS_LOOKAHEAD));
        let d = self.access.address(AddressIndex { major: 0, minor }).ok()?;
        Some(Address::of(self.network, &d))
    }

    /// The address of one of the wallet's indexes (the main address for `(0, 0)`).
    pub fn address_of(&self, index: AddressIndex) -> Option<Address> {
        let d = self.access.address(index).ok()?;
        Some(Address::of(self.network, &d))
    }

    /// The two secrets of an address's spend key, `K^j_s = a G + b T`: `a = s_j k_gi`, `b = s_j k_ps`, with `s_j` the
    /// subaddress scalar (one for the main address). What signs as that address.
    fn signing_secrets(
        &self,
        index: AddressIndex,
    ) -> Option<(Zeroizing<Scalar>, Zeroizing<Scalar>)> {
        let account = self.access.full()?;
        let s = Zeroizing::new(tenero_carrot::account::subaddress_scalar(
            &account.public,
            &account.s_generate_address,
            index,
        ));
        Some((
            Zeroizing::new(*s * account.k_generate_image),
            Zeroizing::new(*s * account.k_prove_spend),
        ))
    }

    /// Signs `message` as the address of `index` (the main address for `(0, 0)`): our own construction, unreviewed
    /// (`proofs.rs`). Gives the address too, which is what a verifier checks it against. `None` for a view-only wallet
    /// (signing needs the spend key's secrets).
    pub fn sign_message(
        &self,
        index: AddressIndex,
        message: &[u8],
        rng: &mut (impl RngCore + CryptoRng),
    ) -> Option<(Address, crate::proofs::Signature)> {
        let address = self.address_of(index)?;
        let (a, b) = self.signing_secrets(index)?;
        let sig = crate::proofs::sign_message(&a, &b, &address, message, rng);
        Some((address, sig))
    }

    /// A RECEIVED proof of the output with `global_index`: its anchor, decrypted with the view key, and the receiving
    /// address's signature over it and `message`. Refused for the wallet's own change. A view-only wallet makes the proof
    /// without the signature (a payment proof: it cannot sign).
    pub fn prove_received(
        &self,
        chain: &impl ChainView,
        global_index: u64,
        message: &[u8],
        rng: &mut (impl RngCore + CryptoRng),
    ) -> Result<crate::proofs::PaymentProof, crate::proofs::ProofError> {
        use crate::proofs::ProofError;
        let o = self
            .owned
            .iter()
            .find(|o| o.global_index == global_index)
            .ok_or(ProofError::Unknown)?;
        if o.internal {
            return Err(ProofError::Change);
        }
        let block = chain
            .block(o.height)
            .map_err(ProofError::Chain)?
            .ok_or(ProofError::NoBlock(o.height))?;
        let anchor =
            crate::proofs::received_anchor(&self.access.k_view(), &block, &o.onetime_address)?;
        let mut proof = crate::proofs::PaymentProof {
            address: self.address_of(o.address).ok_or(ProofError::Unknown)?,
            height: o.height,
            onetime_address: o.onetime_address,
            anchor,
            signature: None,
        };
        // a payment to an integrated address proves as the integrated address (its payment ID is checked too)
        if o.payment_id != NULL_PAYMENT_ID && !o.address.is_subaddress() {
            proof.address = proof
                .address
                .with_payment_id(o.payment_id)
                .ok_or(ProofError::Unknown)?;
        }
        if let Some((a, b)) = self.signing_secrets(o.address) {
            proof.sign(&a, &b, message, rng)?;
        }
        // the proof must check before it is handed out
        crate::proofs::check_payment(chain, &proof, message)?;
        Ok(proof)
    }

    /// The main address with a payment ID: an integrated address.
    pub fn integrated_address(&self, payment_id: PaymentId) -> Option<Address> {
        self.address().with_payment_id(payment_id)
    }

    #[cfg(test)]
    pub(crate) fn account(&self) -> &AccountSecrets {
        self.access.full().expect("a full wallet")
    }

    /// How much of the account this wallet holds.
    pub fn tier(&self) -> ViewTier {
        self.access.tier()
    }

    pub(crate) fn access(&self) -> &Access {
        &self.access
    }

    /// The highest subaddress minor index watched.
    pub fn watched_subaddresses(&self) -> u32 {
        self.watched
    }

    pub(crate) fn watch_subaddresses(&mut self, n: u32) {
        self.watch_up_to(n);
    }

    /// Whether this account's keys open `o` (its secrets give its one-time address, and its key image is right).
    pub(crate) fn opens(&self, o: &Owned) -> bool {
        let r = Received {
            found: Found::External,
            address_spend_pubkey: [0; 32],
            amount: o.amount,
            blinding_factor: scalar(&o.blinding),
            enote_type: tenero_carrot::EnoteType::Payment,
            payment_id: NULL_PAYMENT_ID,
            sender_extension_g: scalar(&o.extension_g),
            sender_extension_t: scalar(&o.extension_t),
            internal_message: None,
        };
        self.access
            .key_image_of(o.address, &r, &o.onetime_address)
            .is_some_and(|k| k == o.key_image)
    }

    /// The seed: the wallet's whole secret. Handle with care; never log it. `None` for a view-only wallet.
    pub fn seed(&self) -> Option<&[u8; 32]> {
        self.seed.as_deref()
    }

    pub fn birth_height(&self) -> u64 {
        self.birth_height
    }

    /// The last height scanned, if any.
    pub fn scanned_height(&self) -> Option<u64> {
        self.scanned
    }

    /// The transactions found spending this wallet's coins (see [`Outgoing`]), oldest first.
    pub fn outgoing(&self) -> &[Outgoing] {
        &self.outgoing
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
                    self.outgoing.retain(|o| o.height <= h);
                }
                None => {
                    self.owned.clear();
                    self.outgoing.clear();
                }
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
        let k_v = self.access.k_view();
        let s_vb = self.access.s_view_balance();
        let main = self.access.public().spend_pubkey;
        let main_view = self.access.public().main_view_pubkey;
        let mut found = 0;
        let mut index = block.first_output_index;
        // this wallet's coins by key image, to see which transactions spend them (a view-received wallet has no key
        // images, so it sees none: it watches what comes in only)
        let mine: HashMap<[u8; 32], u64> =
            if block.txs.is_empty() || matches!(self.access, Access::ViewReceived(_)) {
                HashMap::new()
            } else {
                self.owned.iter().map(|o| (o.key_image, o.amount)).collect()
            };
        for o in &block.coinbase.outputs {
            let enote = CoinbaseEnote {
                onetime_address: o.onetime_address,
                amount: o.amount,
                view_tag: o.view_tag,
                ephemeral_pubkey: o.ephemeral_pubkey,
                anchor_enc: o.anchor_enc,
                block_index: block.height,
            };
            let s_sr = shared_secret(&k_v, &o.ephemeral_pubkey);
            if let Some(r) = scan_coinbase(&enote, &s_sr, &[main], &main_view) {
                let commitment = tenero_tree::coinbase_commitment(o.amount);
                found += u64::from(self.keep(
                    index,
                    block.height,
                    true,
                    o.onetime_address,
                    commitment,
                    &r,
                ));
            }
            index += 1;
        }
        for t in &block.txs {
            let Some(first) = t.inputs.first() else {
                index += t.outputs.len() as u64;
                continue;
            };
            let spends: Vec<([u8; 32], u64)> = t
                .inputs
                .iter()
                .filter_map(|i| mine.get(&i.key_image).map(|a| (i.key_image, *a)))
                .collect();
            let kept_before = self.owned.len();
            // one shared secret per ephemeral key (a 2-output transaction has one key for both)
            let secrets: Vec<[u8; 32]> = t
                .ephemeral_pubkeys
                .iter()
                .map(|d| shared_secret(&k_v, d))
                .collect();
            for (j, o) in t.outputs.iter().enumerate() {
                let k = if t.ephemeral_pubkeys.len() == 1 { 0 } else { j };
                let (Some(d_e), Some(s_sr)) = (t.ephemeral_pubkeys.get(k), secrets.get(k)) else {
                    index += 1;
                    continue;
                };
                let enote = Enote {
                    onetime_address: o.onetime_address,
                    amount_commitment: o.amount_commitment,
                    amount_enc: o.amount_enc,
                    view_tag: o.view_tag,
                    ephemeral_pubkey: *d_e,
                    anchor_enc: o.anchor_enc,
                    tx_first_key_image: first.key_image,
                };
                let internal = s_vb.as_ref().and_then(|s| scan_internal(&enote, s));
                let r = internal.or_else(|| {
                    scan_external(&enote, Some(&t.encrypted_payment_id), s_sr, &[main], &k_v)
                });
                if let Some(r) = r {
                    found += u64::from(self.keep(
                        index,
                        block.height,
                        false,
                        o.onetime_address,
                        o.amount_commitment,
                        &r,
                    ));
                }
                index += 1;
            }
            if !spends.is_empty() {
                self.outgoing.push(Outgoing {
                    height: block.height,
                    spends: spends.iter().map(|(k, _)| *k).collect(),
                    spent: spends.iter().map(|(_, a)| *a).sum(),
                    returned: self.owned[kept_before..].iter().map(|o| o.amount).sum(),
                    fee: t.fee,
                });
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
        commitment: [u8; 32],
        r: &Received,
    ) -> bool {
        // an output worth nothing (a change of zero, a dummy) is of no use to anyone
        if r.amount == 0 || self.owned.iter().any(|o| o.global_index == global_index) {
            return false;
        }
        let Some(&address) = self.addresses.get(&r.address_spend_pubkey) else {
            return false;
        };
        // it must open to that address: then it is really ours there (and the key image, where the tier can compute one,
        // is right)
        let Some(key_image) = self.access.key_image_of(address, r, &onetime_address) else {
            return false;
        };
        if address.minor + SUBADDRESS_LOOKAHEAD > self.watched {
            self.watch_up_to(address.minor + SUBADDRESS_LOOKAHEAD);
        }
        self.owned.push(Owned {
            global_index,
            height,
            coinbase,
            onetime_address,
            commitment,
            amount: r.amount,
            blinding: r.blinding_factor.to_bytes(),
            address,
            extension_g: r.sender_extension_g.to_bytes(),
            extension_t: r.sender_extension_t.to_bytes(),
            key_image,
            payment_id: r.payment_id,
            internal: r.found == Found::Internal,
        });
        true
    }

    // --------------------------------------------------------------------------------------------
    // balances
    // --------------------------------------------------------------------------------------------

    /// Asks the chain about every key image whose answer is not already known, in one request.
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

    /// The unspent outputs, after asking the chain.
    fn unspent(&mut self, chain: &impl ChainView) -> Result<Vec<Owned>, WalletError> {
        self.note_tip(chain)?;
        let images: Vec<[u8; 32]> = self.owned.iter().map(|o| o.key_image).collect();
        self.refresh_spent(chain, &images)?;
        Ok(self
            .owned
            .iter()
            .filter(|o| !self.spent_cache.contains(&o.key_image))
            .cloned()
            .collect())
    }

    /// Which outputs can go into a transaction sent now: unspent, in the tree, not promised elsewhere. Also forgets
    /// reservations that have run out or whose coins have been spent.
    fn spendable(
        &mut self,
        chain: &impl ChainView,
        rules: &Rules,
    ) -> Result<Vec<Owned>, WalletError> {
        // every payment starts here: a view-only wallet cannot spend, and says so before it does anything else
        if self.access.full().is_none() {
            return Err(WalletError::ViewOnly);
        }
        let unspent = self.unspent(chain)?;
        self.reserved.retain(|r| {
            r.until_height >= rules.next_height
                && unspent.iter().any(|o| o.key_image == r.key_image)
        });
        Ok(unspent
            .into_iter()
            .filter(|o| {
                is_mature(rules, o.height, o.coinbase)
                    && !self.reserved.iter().any(|r| r.key_image == o.key_image)
            })
            .collect())
    }

    pub fn balance(&mut self, chain: &impl ChainView) -> Result<Balance, WalletError> {
        let rules = chain.rules().map_err(chain_err)?;
        // a view-received wallet cannot compute key images, so it cannot tell what is spent: it shows what was RECEIVED
        // (as `total`), and nothing as spendable
        if self.access.tier() == ViewTier::ViewReceived {
            let mut b = Balance::default();
            for o in &self.owned {
                b.total += o.amount;
                if !is_mature(&rules, o.height, o.coinbase) {
                    b.immature += o.amount;
                }
            }
            return Ok(b);
        }
        let unspent = self.unspent(chain)?;
        self.reserved.retain(|r| {
            r.until_height >= rules.next_height
                && unspent.iter().any(|o| o.key_image == r.key_image)
        });
        let mut b = Balance::default();
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
        rng: &mut (impl RngCore + CryptoRng + Send),
        to: &Address,
        amount: u64,
    ) -> Result<Built, WalletError> {
        self.build_payment_at(chain, rng, to, amount, FeeLevel::Low)
    }

    /// [`Wallet::build_payment`] at a chosen fee level.
    pub fn build_payment_at(
        &mut self,
        chain: &impl ChainView,
        rng: &mut (impl RngCore + CryptoRng + Send),
        to: &Address,
        amount: u64,
        level: FeeLevel,
    ) -> Result<Built, WalletError> {
        if amount == 0 {
            return Err(WalletError::ZeroAmount);
        }
        self.build_to_at(chain, rng, &[(*to, amount)], level)
    }

    /// One transaction paying several recipients (at most [`MAX_RECIPIENTS`]), with the change coming back to this wallet.
    /// Nothing is sent and nothing is reserved. To pay more recipients than that, or more than the coins of one transaction
    /// can, use [`Wallet::build_batch`].
    pub fn build_to_at(
        &mut self,
        chain: &impl ChainView,
        rng: &mut (impl RngCore + CryptoRng + Send),
        dests: &[(Address, u64)],
        level: FeeLevel,
    ) -> Result<Built, WalletError> {
        self.check_dests(dests)?;
        let rules = chain.rules().map_err(chain_err)?;
        let candidates = self.spendable(chain, &rules)?;
        let amount = dests.iter().map(|(_, v)| *v).sum::<u64>();
        let chosen = choose(&rules, level, &candidates, amount, dests.len() + 1)?;
        self.assemble(chain, rng, &rules, level, chosen, dests)
    }

    /// Builds, proves and checks a transaction that spends exactly `chosen` and pays `dests` (and what is left, after the
    /// fee, back to this wallet). The fee is the level's share of the minimum for the transaction's exact size, known before
    /// proving ([`transaction_size`]).
    fn assemble(
        &self,
        chain: &impl ChainView,
        rng: &mut (impl RngCore + CryptoRng + Send),
        rules: &Rules,
        level: FeeLevel,
        mut chosen: Vec<Owned>,
        dests: &[(Address, u64)],
    ) -> Result<Built, WalletError> {
        let account = self.access.full().ok_or(WalletError::ViewOnly)?;
        // key images strictly ascending: the rules' one canonical order; the first is the input context's
        chosen.sort_by_key(|o| o.key_image);
        let paths = chain
            .spend_paths(&chosen.iter().map(|o| o.global_index).collect::<Vec<_>>())
            .map_err(chain_err)?;
        if paths.paths.len() != chosen.len() {
            return Err(WalletError::Chain(
                "the node answered about a different number of outputs than were asked".into(),
            ));
        }
        let layers = usize::from(paths.tree.n_layers);
        let n_outputs = dests.len() + 1;
        let size = transaction_size(chosen.len(), n_outputs, layers);
        let fee = fee_for_size(rules, level, size as u64)?;
        let amount: u64 = dests.iter().map(|(_, v)| *v).sum();
        let total: u64 = chosen.iter().map(|o| o.amount).sum();
        let change = total
            .checked_sub(amount)
            .and_then(|r| r.checked_sub(fee))
            .ok_or(WalletError::NotEnough {
                spendable: total,
                needed: amount.saturating_add(fee),
            })?;

        // the outputs: the payments and the change (a self-send found with the view-balance secret), sorted by one-time
        // address by `output_set`, so nothing about a position says which is which
        let normal: Vec<PaymentProposal> = dests
            .iter()
            .map(|(a, v)| PaymentProposal::new(a.destination(), *v, rng))
            .collect();
        let selfsend = match additional_payment_proposal(
            normal.len(),
            0,
            change,
            false,
            &self.access.public().spend_pubkey,
            false,
            rng,
        )
        .map_err(|_| WalletError::BadAddress)?
        {
            Some(Additional::SelfSend(s)) => vec![s],
            _ => return Err(WalletError::SelfCheck("no change output".into())),
        };
        let mut dummy_pid = [0u8; 8];
        rng.fill_bytes(&mut dummy_pid);
        let first_key_image = chosen[0].key_image;
        let (proposals, encrypted_payment_id, order) = output_set(
            &normal,
            &selfsend,
            Some(dummy_pid),
            SelfSendKey::ViewBalance(&account.s_view_balance),
            &first_key_image,
        )
        .map_err(|_| WalletError::BadAddress)?;
        let outputs: Vec<Output> = proposals
            .iter()
            .map(|p| Output {
                onetime_address: p.enote.onetime_address,
                amount_commitment: p.enote.amount_commitment,
                amount_enc: p.enote.amount_enc,
                view_tag: p.enote.view_tag,
                anchor_enc: p.enote.anchor_enc,
            })
            .collect();
        let ephemeral_pubkeys: Vec<[u8; 32]> = proposals
            .iter()
            .take(n_ephemeral_keys(proposals.len()))
            .map(|p| p.enote.ephemeral_pubkey)
            .collect();
        let mut tx = Transaction {
            prefix: TxPrefix {
                version: VERSION,
                inputs: chosen
                    .iter()
                    .map(|o| Input {
                        key_image: o.key_image,
                    })
                    .collect(),
                outputs,
                ephemeral_pubkeys,
                fee,
                encrypted_payment_id,
            },
            prunable: Prunable {
                reference_height: paths.reference_height,
                proof_data: vec![],
            },
        };

        // the spends: each coin's secrets (recomputed from the account), its leaf and its path
        let mut spends = Vec::with_capacity(chosen.len());
        for (o, path) in chosen.iter().zip(&paths.paths) {
            let path = path
                .as_ref()
                .and_then(tenero_tree::path_from_bytes)
                .ok_or(WalletError::Prove)?;
            let r = Received {
                found: Found::External,
                address_spend_pubkey: [0; 32],
                amount: o.amount,
                blinding_factor: scalar(&o.blinding),
                enote_type: tenero_carrot::EnoteType::Payment,
                payment_id: NULL_PAYMENT_ID,
                sender_extension_g: scalar(&o.extension_g),
                sender_extension_t: scalar(&o.extension_t),
                internal_message: None,
            };
            let keys =
                spend_keys(account, o.address, &r, &o.onetime_address).ok_or(WalletError::Prove)?;
            spends.push(Spend {
                x: keys.x,
                y: keys.y,
                mask: scalar(&o.blinding),
                amount: o.amount,
                leaf: tenero_tree::Leaf::from_output(&o.onetime_address, &o.commitment)
                    .ok_or(WalletError::Prove)?,
                path,
            });
        }
        let secrets: Vec<OutputSecret> = proposals
            .iter()
            .map(|p| OutputSecret {
                amount: p.amount,
                mask: p.blinding_factor,
            })
            .collect();
        tx.prunable.proof_data = fcmp::prove(rng, &rules.chain_id, &tx, &spends, &secrets, layers)
            .ok_or(WalletError::Prove)?;

        // never send what does not verify, or is not the size the fee was paid for
        let real = tx
            .to_bytes()
            .map_err(|e| WalletError::SelfCheck(e.to_string()))?
            .len();
        if real != size {
            return Err(WalletError::SelfCheck(format!(
                "{real} bytes, not the {size} the fee was worked out for"
            )));
        }
        let root = tenero_tree::root_from_bytes(layers, &paths.tree.root)
            .ok_or_else(|| WalletError::SelfCheck("the tree's root is not a point".into()))?;
        fcmp::verify_tx(&rules.chain_id, &tx, root, layers)
            .map_err(|e| WalletError::SelfCheck(e.to_string()))?;
        let id = ids::tx_id(&tx).map_err(|e| WalletError::SelfCheck(e.to_string()))?;

        let mut parts: Vec<Option<PaymentPart>> = vec![None; dests.len()];
        let mut change_onetime = [0u8; 32];
        for (p, (is_self, i)) in proposals.iter().zip(&order) {
            if *is_self {
                change_onetime = p.enote.onetime_address;
            } else {
                parts[*i] = Some(PaymentPart {
                    to: dests[*i].0,
                    amount: dests[*i].1,
                    onetime: p.enote.onetime_address,
                    anchor: normal[*i].randomness,
                });
            }
        }
        Ok(Built {
            tx,
            id,
            fee,
            amount,
            change,
            spends: chosen.iter().map(|o| o.key_image).collect(),
            change_onetime,
            parts: parts.into_iter().flatten().collect(),
        })
    }

    /// A payment to any number of recipients of any total the wallet can cover, as **several transactions that spend
    /// different coins**, so that all of them can be sent at once. Each pays at most [`MAX_RECIPIENTS`] recipients and spends
    /// at most as many coins as fit in `MAX_TX_SIZE`; when one recipient's amount needs more coins than that, it is paid in
    /// parts, in different transactions.
    ///
    /// **What comes out may be less than was asked for**: change cannot be spent for 10 blocks, so when the coins there at the
    /// start run out, the rest is returned in [`Plan::unsent`] for a later batch. Nothing is sent and nothing is reserved. An
    /// error is returned only when not even the first transaction can be built.
    pub fn build_batch(
        &mut self,
        chain: &impl ChainView,
        rng: &mut (impl RngCore + CryptoRng + Send),
        dests: &[(Address, u64)],
        level: FeeLevel,
    ) -> Result<Plan, WalletError> {
        let (txs, unsent) = self.batch_with(chain, dests, level, |w, rules, chosen, group| {
            let b = w.assemble(chain, rng, rules, level, chosen, group)?;
            let spends = b.spends.clone();
            Ok((b, spends))
        })?;
        Ok(Plan { txs, unsent })
    }

    /// What [`Wallet::build_batch`] would make, worked out WITHOUT making it: the same coins chosen and grouped the same way,
    /// each transaction's exact size and fee, but no proofs (seconds per coin) and no request to the node for spend paths.
    /// For showing the fees before the person decides. (A tree that gains a layer between the quote and the build makes the
    /// built transaction slightly bigger; the build works its fee out again.)
    pub fn quote_batch(
        &mut self,
        chain: &impl ChainView,
        dests: &[(Address, u64)],
        level: FeeLevel,
    ) -> Result<Vec<TxQuote>, WalletError> {
        self.access.full().ok_or(WalletError::ViewOnly)?;
        let (quotes, _) = self.batch_with(chain, dests, level, |_, rules, chosen, group| {
            let outputs = group.len() + 1;
            let size = transaction_size(chosen.len(), outputs, rules.tree_layers) as u64;
            let q = TxQuote {
                inputs: chosen.len(),
                outputs,
                size,
                fee: fee_for_size(rules, level, size)?,
            };
            Ok((q, chosen.iter().map(|o| o.key_image).collect()))
        })?;
        Ok(quotes)
    }

    /// The batching of [`Wallet::build_batch`] and [`Wallet::quote_batch`]: groups the recipients, chooses each group's
    /// coins, and hands them to `make`, which gives what it made and the key images it spent.
    #[allow(clippy::type_complexity)]
    fn batch_with<T>(
        &mut self,
        chain: &impl ChainView,
        dests: &[(Address, u64)],
        level: FeeLevel,
        mut make: impl FnMut(
            &Wallet,
            &Rules,
            Vec<Owned>,
            &[(Address, u64)],
        ) -> Result<(T, Vec<[u8; 32]>), WalletError>,
    ) -> Result<(Vec<T>, Vec<(Address, u64)>), WalletError> {
        if dests.is_empty() || dests.iter().any(|(_, v)| *v == 0) {
            return Err(WalletError::ZeroAmount);
        }
        if dests.iter().any(|(a, _)| a.network != self.network) {
            return Err(WalletError::BadAddress);
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
        let mut txs: Vec<T> = Vec::new();
        while !pending.is_empty() {
            let mut group: Vec<(Address, u64)> =
                pending.iter().take(MAX_RECIPIENTS).copied().collect();
            // at most one integrated address in a transaction (Carrot carries one payment ID)
            if let Some(second) = group
                .iter()
                .enumerate()
                .filter(|(_, (a, _))| a.kind == Kind::Integrated)
                .nth(1)
                .map(|(i, _)| i)
            {
                group.truncate(second);
            }
            let limit = max_inputs_for(group.len() + 1, rules.tree_layers);
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
            let amount: u64 = group.iter().map(|(_, v)| *v).sum();
            let built = choose(&rules, level, &available, amount, group.len() + 1)
                .and_then(|chosen| make(self, &rules, chosen, &group));
            match built {
                Ok((b, spends)) => {
                    available.retain(|o| !spends.contains(&o.key_image));
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
        Ok((txs, pending.into_iter().collect()))
    }

    /// Combines coins: every spendable coin that is worth more than the fee it adds, grouped as many to a transaction as
    /// fit, each group paid to `to` (this wallet's own address when it is `None`) in one output. For a wallet that has been
    /// paid in many small amounts (a miner's block rewards), this is what makes them one coin. Coins that are alone in their
    /// group are left alone when the destination is this wallet. Nothing is sent and nothing is reserved. **The result of a
    /// combine cannot be spent for 10 blocks.**
    pub fn build_sweep(
        &mut self,
        chain: &impl ChainView,
        rng: &mut (impl RngCore + CryptoRng + Send),
        to: Option<&Address>,
        level: FeeLevel,
    ) -> Result<Vec<Built>, WalletError> {
        let rules = chain.rules().map_err(chain_err)?;
        let mut coins = self.spendable(chain, &rules)?;
        let dest = to.copied().unwrap_or_else(|| self.address());
        if dest.network != self.network {
            return Err(WalletError::BadAddress);
        }
        let limit = max_inputs_for(2, rules.tree_layers);
        let per_input = per_input_fee(&rules, level)?;
        coins.retain(|o| o.amount > per_input);
        coins.sort_by_key(|o| o.amount);
        let mut out = Vec::new();
        for group in coins.chunks(limit) {
            if to.is_none() && group.len() < 2 {
                continue;
            }
            out.push(self.consolidate(chain, rng, &rules, group.to_vec(), dest, level)?);
        }
        if out.is_empty() {
            return Err(WalletError::NothingToCombine);
        }
        Ok(out)
    }

    /// Combines `count` coins (the smallest ones that are worth more than the fee they add) into one coin of this wallet:
    /// the manual form of [`Wallet::build_sweep`]. `count` is from 2 up to what one transaction can spend.
    pub fn build_combine(
        &mut self,
        chain: &impl ChainView,
        rng: &mut (impl RngCore + CryptoRng + Send),
        count: usize,
        level: FeeLevel,
    ) -> Result<Built, WalletError> {
        let rules = chain.rules().map_err(chain_err)?;
        let limit = max_inputs_for(2, rules.tree_layers);
        if count > limit {
            return Err(WalletError::TooManyInputs { max: limit });
        }
        if count < 2 {
            return Err(WalletError::NothingToCombine);
        }
        let mut coins = self.spendable(chain, &rules)?;
        let per_input = per_input_fee(&rules, level)?;
        coins.retain(|o| o.amount > per_input);
        coins.sort_by_key(|o| o.amount);
        if coins.len() < count {
            return Err(WalletError::NothingToCombine);
        }
        coins.truncate(count);
        let dest = self.address();
        self.consolidate(chain, rng, &rules, coins, dest, level)
    }

    /// One transaction that spends exactly `coins` and pays everything but the fee to `dest`.
    fn consolidate(
        &self,
        chain: &impl ChainView,
        rng: &mut (impl RngCore + CryptoRng + Send),
        rules: &Rules,
        coins: Vec<Owned>,
        dest: Address,
        level: FeeLevel,
    ) -> Result<Built, WalletError> {
        let total: u64 = coins.iter().map(|o| o.amount).sum();
        // the fee of exactly this transaction (the tree may grow a layer before the paths are read: then the change, not
        // the payment, absorbs the difference, and a combine with no change left fails rather than overpays)
        let fee = fee_for_size(
            rules,
            level,
            transaction_size(coins.len(), 2, rules.tree_layers) as u64,
        )?;
        let amount = total
            .checked_sub(fee)
            .filter(|a| *a > 0)
            .ok_or(WalletError::NothingToCombine)?;
        self.assemble(chain, rng, rules, level, coins, &[(dest, amount)])
    }

    /// Hands the transactions of a batch to the node one after another and reserves the coins of each. It stops at the first
    /// the node refuses: the ones before it are sent, and the report says how many.
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
        rng: &mut (impl RngCore + CryptoRng + Send),
        to: &Address,
        amount: u64,
    ) -> Result<Built, WalletError> {
        let built = self.build_payment(&*node, rng, to, amount)?;
        self.send_built(node, &built)?;
        Ok(built)
    }

    /// Hands a payment built by [`Wallet::build_payment_at`] to the node and reserves the coins it spends. (Build, show the
    /// person the fee, and then send: nothing is reserved by building.)
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

    /// Whether a list of recipients can go in one transaction: at least one, every amount more than nothing, at most
    /// [`MAX_RECIPIENTS`], every address of this network, at most one integrated address, a total that fits.
    fn check_dests(&self, dests: &[(Address, u64)]) -> Result<(), WalletError> {
        if dests.is_empty() || dests.iter().any(|(_, v)| *v == 0) {
            return Err(WalletError::ZeroAmount);
        }
        if dests.len() > MAX_RECIPIENTS {
            return Err(WalletError::TooManyRecipients {
                max: MAX_RECIPIENTS,
            });
        }
        if dests.iter().any(|(a, _)| a.network != self.network)
            || dests
                .iter()
                .filter(|(a, _)| a.kind == Kind::Integrated)
                .count()
                > 1
        {
            return Err(WalletError::BadAddress);
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
}

/// The fee for a transaction of `size` bytes at a fee level: the level's share of the minimum the next block needs, and
/// at least one unit more.
fn fee_for_size(rules: &Rules, level: FeeLevel, size: u64) -> Result<u64, WalletError> {
    let min = rules::min_fee(size, rules.reward, rules.median).map_err(WalletError::Chain)?;
    Ok(min.saturating_mul(level.percent_of_minimum()) / 100 + 1)
}

/// What one more input adds to the fee of a 2-output transaction.
fn per_input_fee(rules: &Rules, level: FeeLevel) -> Result<u64, WalletError> {
    let one = transaction_size(1, 2, rules.tree_layers);
    let two = transaction_size(2, 2, rules.tree_layers);
    fee_for_size(rules, level, (two - one) as u64)
}

/// The coins to spend for `amount` plus the fee of the transaction they make (which depends on how many there are):
/// tried with one coin's fee, then with the fee of the count chosen, until they agree.
fn choose(
    rules: &Rules,
    level: FeeLevel,
    candidates: &[Owned],
    amount: u64,
    n_outputs: usize,
) -> Result<Vec<Owned>, WalletError> {
    let limit = max_inputs_for(n_outputs, rules.tree_layers);
    let mut n = 1;
    for _ in 0..8 {
        let fee = fee_for_size(
            rules,
            level,
            transaction_size(n, n_outputs, rules.tree_layers) as u64,
        )?;
        let needed = amount.checked_add(fee).ok_or(WalletError::NotEnough {
            spendable: 0,
            needed: u64::MAX,
        })?;
        let chosen = select_coins(candidates, needed, limit)?;
        if chosen.len() <= n {
            return Ok(chosen);
        }
        n = chosen.len();
    }
    Err(WalletError::Chain("could not settle on a fee".into()))
}

/// The exact size in bytes of a transaction of `n_inputs` inputs and `n_outputs` outputs proven against a tree of `layers`
/// layers (`docs/CONSENSUS_V2.md` 15.2; the proof's length is `tenero_crypto::fcmp::proof_data_size`). A wallet knows its fee
/// before it proves; it checks the real size afterwards.
pub fn transaction_size(n_inputs: usize, n_outputs: usize, layers: usize) -> usize {
    // version, input count, key images, output count, the outputs of 91, the ephemeral keys, fee, payment ID
    let prefix =
        2 + 4 + 32 * n_inputs + 4 + 91 * n_outputs + 32 * n_ephemeral_keys(n_outputs) + 8 + 8;
    // reference height, the proof's length and the proof
    let prunable = 8 + 4 + fcmp::proof_data_size(n_inputs, n_outputs, layers);
    prefix + prunable
}

/// How many coins a transaction of `n_outputs` outputs can spend: the most whose [`transaction_size`] is within
/// `MAX_TX_SIZE` and whose proof is within `MAX_PROOF`.
pub fn max_inputs_for(n_outputs: usize, layers: usize) -> usize {
    let fits = |n: usize| {
        transaction_size(n, n_outputs, layers) <= MAX_TX_SIZE
            && fcmp::proof_data_size(n, n_outputs, layers) <= tenero_core::v3::MAX_PROOF
    };
    let mut n = 1;
    while fits(n + 1) {
        n += 1;
    }
    n
}

/// Which outputs to spend: the smallest single one that covers `needed`, else the largest first until it is covered.
/// Fewer inputs mean a smaller transaction and a smaller fee. (Spending the same way every time is a pattern a chain
/// observer could use; a randomised policy is future work.)
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
