//! The wallet: scanning the chain for one's outputs, balances, and building a payment.
//!
//! **Interim and unaudited.** The output scheme is [`crate::interim`]; decoy selection and coin selection here are
//! simple policies (see the notes on each), not Monero's, and have not been studied for how much they reveal.

use rand_core::{CryptoRng, RngCore};
use tenero_core::fees;
use tenero_core::v2::{ids, Input, Output, Transaction, TxPrefix, Wire, MAX_INPUTS, VERSION};
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

/// A transaction the wallet has sent keeps its inputs reserved for this many blocks (so that a second payment
/// does not pick the same coins while the first is still waiting); after that, if the coins are unspent, they
/// are free again (the transaction was probably dropped).
pub const RESERVE_BLOCKS: u64 = 20;

/// The fee is the minimum the next block needs, plus this many percent (the minimum moves with the block-size
/// median between sending and mining), and at least one unit more.
pub const FEE_MARGIN_PERCENT: u64 = 25;

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
    /// The payment would need more than `MAX_INPUTS` outputs.
    TooManyInputs,
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
            WalletError::TooManyInputs => {
                write!(f, "that payment needs more than {MAX_INPUTS} outputs")
            }
            WalletError::NotEnoughDecoys => {
                write!(f, "the chain has too few mature outputs to hide among")
            }
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
        }
    }

    pub fn address(&self) -> Address {
        self.keys.address()
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
        // 2. new blocks
        let (tip, _) = chain.tip().map_err(chain_err)?;
        let from = self.scanned.map_or(self.birth_height, |h| h + 1);
        for height in from..=tip {
            let Some(block) = chain.block(height).map_err(chain_err)? else {
                break;
            };
            report.outputs_found += self.scan_block(&block);
            report.blocks_scanned += 1;
            self.scanned = Some(height);
            self.recent.push((height, block.id));
            if self.recent.len() > RECENT_BLOCKS {
                self.recent.remove(0);
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
        if self.owned.iter().any(|o| o.global_index == global_index) {
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

    /// Which outputs can go into a transaction sent now: unspent, mature, not promised elsewhere. Also forgets
    /// reservations that have run out or whose coins have been spent.
    fn spendable(
        &mut self,
        chain: &impl ChainView,
        rules: &Rules,
    ) -> Result<Vec<Owned>, WalletError> {
        let mut spent = Vec::new();
        for o in &self.owned {
            if chain.key_image_spent(&o.key_image).map_err(chain_err)? {
                spent.push(o.key_image);
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
        let mut b = Balance::default();
        let mut unspent = Vec::new();
        for o in &self.owned {
            if !chain.key_image_spent(&o.key_image).map_err(chain_err)? {
                unspent.push(o.clone());
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
        if amount == 0 {
            return Err(WalletError::ZeroAmount);
        }
        let rules = chain.rules().map_err(chain_err)?;
        let candidates = self.spendable(chain, &rules)?;
        // The fee depends on the size, which depends on how many outputs are spent, which depends on the fee: start
        // from a guess and rebuild if the size shows it was too low (the fee field has a fixed width, so a
        // different fee does not change the size).
        let mut fee =
            fees::dynamic_min_fee(2_000, rules.reward, rules.median).map_err(WalletError::Chain)?;
        for _ in 0..4 {
            let built = self.build_with_fee(chain, rng, &rules, &candidates, to, amount, fee)?;
            let size = built
                .tx
                .to_bytes()
                .map_err(|e| WalletError::SelfCheck(e.to_string()))?
                .len() as u64;
            let min = fees::dynamic_min_fee(size, rules.reward, rules.median)
                .map_err(WalletError::Chain)?;
            let wanted = min + min * FEE_MARGIN_PERCENT / 100 + 1;
            // accept a fee that covers the minimum and the margin without being wasteful (the first guess is only
            // a guess); otherwise take the fee the size asks for and build again
            if fee >= wanted && fee <= wanted.saturating_mul(2) {
                return Ok(built);
            }
            fee = wanted;
        }
        Err(WalletError::Chain("could not settle on a fee".into()))
    }

    #[allow(clippy::too_many_arguments)]
    fn build_with_fee(
        &self,
        chain: &impl ChainView,
        rng: &mut (impl RngCore + CryptoRng),
        rules: &Rules,
        candidates: &[Owned],
        to: &Address,
        amount: u64,
        fee: u64,
    ) -> Result<Built, WalletError> {
        let needed = amount.checked_add(fee).ok_or(WalletError::NotEnough {
            spendable: 0,
            needed: u64::MAX,
        })?;
        let mut chosen = select_coins(candidates, needed)?;
        // key images strictly ascending: the rules' one canonical order
        chosen.sort_by_key(|o| o.key_image);
        let total: u64 = chosen.iter().map(|o| o.amount).sum();
        let change = total - needed;

        // the outputs: the payment and the change, in a random order (nothing about the position says which
        // is which)
        let ctx = tx_context(&chosen[0].key_image);
        let mut slots = [(*to, amount), (self.address(), change)];
        if rng.next_u32() & 1 == 1 {
            slots.swap(0, 1);
        }
        let mut enotes = Vec::new();
        for (i, (addr, value)) in slots.iter().enumerate() {
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

        // the rings
        let mut spend_inputs = Vec::new();
        let mut ring_members: Vec<Vec<StoredOutput>> = Vec::new();
        for o in &chosen {
            let mut indexes = pick_decoys(chain, rules, rng, o.global_index)?;
            indexes.push(o.global_index);
            indexes.sort_unstable();
            let signer = indexes
                .iter()
                .position(|&i| i == o.global_index)
                .expect("the real output is in its ring");
            let mut members = Vec::new();
            for &i in &indexes {
                members.push(
                    chain
                        .output(i)
                        .map_err(chain_err)?
                        .ok_or(WalletError::NotEnoughDecoys)?,
                );
            }
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
            .map(|(e, (_, v))| OutputSecret {
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
        Ok(Built {
            tx,
            id,
            fee,
            amount,
            change,
            spends: chosen.iter().map(|o| o.key_image).collect(),
        })
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
        let next_height = node.rules().map_err(chain_err)?.next_height;
        node.submit(built.tx.clone()).map_err(WalletError::Submit)?;
        for ki in &built.spends {
            self.reserved.push(Reserved {
                key_image: *ki,
                until_height: next_height + RESERVE_BLOCKS,
            });
        }
        Ok(built)
    }
}

/// Which outputs to spend: the smallest single one that covers `needed`, else the largest first until it is
/// covered. Fewer inputs mean a smaller transaction and a smaller fee. (Spending the same way every time is a
/// pattern a chain observer could use; a randomised policy is future work.)
fn select_coins(candidates: &[Owned], needed: u64) -> Result<Vec<Owned>, WalletError> {
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
    if chosen.len() > MAX_INPUTS {
        return Err(WalletError::TooManyInputs);
    }
    Ok(chosen)
}

/// `ring_size - 1` other outputs to hide `real` among: distinct, existing and mature (the rules refuse an
/// immature ring member).
///
/// **The policy, and its limit:** an output is picked by how far it is from the newest one, with the distance
/// log-uniform (`exp(U * ln N)`), so recent outputs are far more likely than old ones, as real spends are. This
/// is a simplification of Monero's gamma distribution of output ages and has not been checked against how
/// real spends behave on this chain (which has none yet).
fn pick_decoys(
    chain: &impl ChainView,
    rules: &Rules,
    rng: &mut impl RngCore,
    real: u64,
) -> Result<Vec<u64>, WalletError> {
    let want = rules.ring_size.saturating_sub(1);
    let total = chain.output_count().map_err(chain_err)?;
    let mut picked: Vec<u64> = Vec::new();
    if want == 0 {
        return Ok(picked);
    }
    if total < rules.ring_size as u64 {
        return Err(WalletError::NotEnoughDecoys);
    }
    let ln_total = (total as f64).ln();
    let mut tries = 0;
    while picked.len() < want {
        tries += 1;
        if tries > 200 * rules.ring_size {
            // the chain may have enough mature outputs but the distribution keeps missing them: take any
            return pick_any(chain, rules, rng, real, total, picked);
        }
        let u = (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        let distance = ((u * ln_total).exp() as u64).clamp(1, total);
        let index = total - distance;
        if index == real || picked.contains(&index) {
            continue;
        }
        let Some(o) = chain.output(index).map_err(chain_err)? else {
            continue;
        };
        if is_mature(rules, o.height, o.coinbase) {
            picked.push(index);
        }
    }
    Ok(picked)
}

/// The fallback of [`pick_decoys`]: walk every output from a random start until enough mature ones are found.
fn pick_any(
    chain: &impl ChainView,
    rules: &Rules,
    rng: &mut impl RngCore,
    real: u64,
    total: u64,
    mut picked: Vec<u64>,
) -> Result<Vec<u64>, WalletError> {
    let want = rules.ring_size - 1;
    let start = below(rng, total);
    for k in 0..total {
        if picked.len() == want {
            break;
        }
        let index = (start + k) % total;
        if index == real || picked.contains(&index) {
            continue;
        }
        if let Some(o) = chain.output(index).map_err(chain_err)? {
            if is_mature(rules, o.height, o.coinbase) {
                picked.push(index);
            }
        }
    }
    if picked.len() == want {
        Ok(picked)
    } else {
        Err(WalletError::NotEnoughDecoys)
    }
}
