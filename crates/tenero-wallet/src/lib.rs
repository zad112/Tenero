//! Tenero's wallet for the `gamma` network: Carrot addresses and outputs, FCMP++ spends. **Experimental and unaudited**
//! ([`BANNER`]).
//!
//! * [`address`]: addresses as text (`TENg...`), the networks, and an account's Carrot master secret.
//! * [`amount`]: coins as people write them (8 decimals), parsed strictly.
//! * [`chain`]: what the wallet needs from a node (two small traits; `Node` implements them).
//! * [`wallet`]: scanning (with reorganisations), balances, subaddresses, building and sending a payment.
//! * [`file`]: the encrypted wallet file.
//! * [`mnemonic`]: the seed as 24 words. [`purse`]: several accounts from one seed, in one file.
//! * [`request`]: payment requests as links.
//!
//! Message signatures and payment proofs (the interim scheme's `proofs.rs`) are rebuilt on Carrot in milestone G5
//! (`docs/FCMP_CARROT_PLAN.md`); until then a 0.3.0 wallet keeps, for each payment it sends, what such a proof will need.

pub mod address;
pub mod amount;
pub mod chain;
pub mod file;
pub mod mnemonic;
pub mod purse;
pub mod request;
pub mod wallet;

use rand_core::{CryptoRng, RngCore};
use tenero_carrot::output::{coinbase_enote, PaymentProposal};
use tenero_node::Payout;

pub use address::{carrot_master, Address, AddressError, Kind, Network};
pub use chain::{ChainView, Rules, ScanBlock, SpendPaths, Submitter};
pub use file::{FileError, KdfParams};
pub use mnemonic::{phrase_of, seed_of, PhraseError};
pub use purse::{Entry, EntryKind, Purse, PurseError, SavedRequest, SentRecord, SentStatus};
pub use request::{PaymentRequest, RequestError};
pub use wallet::{
    max_inputs_for, transaction_size, Balance, BatchSent, Built, FeeLevel, Owned, PaymentPart,
    Plan, SyncReport, Wallet, WalletError, MAX_RECIPIENTS,
};

/// What the program must say wherever it shows an address or a balance.
pub const BANNER: &str = "Carrot addresses and FCMP++ proofs, written for Tenero (Rust, from Monero's designs; the FCMP++ \
crates are monero-oxide's): experimental and UNAUDITED. Do not hold value you cannot lose.";

/// [`coinbase_payout`] with the operating system's randomness.
pub fn coinbase_payout_random(to: &Address, height: u64, amount: u64) -> Option<Payout> {
    coinbase_payout(&mut rand_core::OsRng, to, height, amount)
}

/// The coinbase output of the block at `height` paying `amount` to `to`: what a miner puts in the block it is building so
/// that the reward goes to this address (Carrot: a coinbase output's key depends on its amount, so a block template asks
/// for it once the amount is known). Fresh randomness for every block, so rewards to one address are not linked. `None`
/// for anything but a main address (Carrot pays a coinbase output to a main address only).
pub fn coinbase_payout(
    rng: &mut (impl RngCore + CryptoRng),
    to: &Address,
    height: u64,
    amount: u64,
) -> Option<Payout> {
    if to.kind != Kind::Main {
        return None;
    }
    let e = coinbase_enote(&PaymentProposal::new(to.destination(), amount, rng), height).ok()?;
    Some(Payout {
        onetime_address: e.onetime_address,
        view_tag: e.view_tag,
        ephemeral_pubkey: e.ephemeral_pubkey,
        anchor_enc: e.anchor_enc,
    })
}
