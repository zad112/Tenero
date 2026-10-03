//! Tenero's wallet (milestone M8.6). **Experimental and unaudited. The output scheme is an INTERIM stand-in
//! for Carrot** ([`interim`]; read its module documentation for what it lacks).
//!
//! * [`amount`]: coins as people write them (8 decimals), parsed strictly.
//! * [`interim`]: keys, addresses, making an output for a recipient, recognising one's own outputs.
//! * [`chain`]: what the wallet needs from a node (two small traits; `Node` implements them).
//! * [`wallet`]: scanning (with reorganisations), balances, building and sending a payment.
//! * [`file`]: the encrypted wallet file.
//! * [`mnemonic`]: the seed as 24 words. [`purse`]: several accounts from one seed, in one file.

pub mod amount;
pub mod chain;
pub mod file;
pub mod interim;
pub mod mnemonic;
pub mod purse;
pub mod wallet;

use rand_core::{CryptoRng, RngCore};
use tenero_node::Payout;

pub use chain::{ChainView, Rules, ScanBlock, Submitter};
pub use file::{FileError, KdfParams};
pub use interim::{Address, AddressError, Keys, BANNER};
pub use mnemonic::{phrase_of, seed_of, PhraseError};
pub use purse::{Entry, EntryKind, Purse, PurseError, SentRecord, SentStatus};
pub use wallet::{Balance, Built, FeeLevel, Owned, SyncReport, Wallet, WalletError};

/// [`coinbase_payout`] with the operating system's randomness.
pub fn coinbase_payout_random(to: &Address, height: u64) -> Option<Payout> {
    coinbase_payout(&mut rand_core::OsRng, to, height)
}

/// The coinbase payout of the block at `height` to `to`: what a miner puts in the block it is building so that
/// the reward goes to this address. Fresh randomness for every block, so rewards to one address are not linked.
pub fn coinbase_payout(
    rng: &mut (impl RngCore + CryptoRng),
    to: &Address,
    height: u64,
) -> Option<Payout> {
    let e = interim::create_enote(rng, to, 0, &interim::coinbase_context(height), 0, true)?;
    Some(Payout {
        onetime_address: e.onetime_address,
        view_tag: e.view_tag,
        ephemeral_pubkey: e.ephemeral_pubkey,
        anchor_enc: e.anchor_enc,
    })
}
