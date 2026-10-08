//! **Carrot**, Monero's addressing protocol, in Rust, for the `gamma` network (`docs/FCMP_CARROT_PLAN.md`, G2).
//!
//! **Experimental and UNAUDITED.** This is a transcription of the specification (`jeffro256/carrot`, `carrot.md`) and of
//! Monero's C++ `carrot_core` (stressnet release `v0.19.0.0-beta.3.0`), written for this project. The specification and
//! Monero's C++ code have had audits (Cypher Stack); **this Rust code has had none.** It is checked against Monero's own
//! values (`tests/vectors/upstream_monero_carrot_*.json`), which shows agreement with Monero, not security.
//!
//! The parts, in the order of the specification:
//! * [`derive`]: every derivation, one function each (keys, addresses, the enote's fields).
//! * [`account`]: an account's secrets from its master secret, its public keys and its addresses.
//! * [`output`]: making outputs (payments, change and other self-sends, coinbase outputs) and a transaction's output set.
//! * [`scan`]: finding one's outputs (with the Janus checks), and what is needed to spend them.
//!
//! What this crate does NOT decide: where the master secret comes from (the wallet derives it from its seed phrase), the
//! text form of addresses, and the consensus rules of `gamma` (G4).

pub mod account;
pub mod derive;
mod hash;
pub mod output;
pub mod points;
pub mod scan;

/// What the program must say wherever it shows a Carrot address or balance.
pub const BANNER: &str =
    "Carrot addressing (Monero's design), written for Tenero in Rust: experimental and UNAUDITED.";

/// The input context: `"R"` and the transaction's first key image, or `"C"` and the block height (33 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputContext(pub [u8; 33]);

/// The 16-byte Janus anchor (also the "internal message" of an internal self-send).
pub type JanusAnchor = [u8; 16];

/// An 8-byte payment ID (all zero means none).
pub type PaymentId = [u8; 8];

pub const NULL_PAYMENT_ID: PaymentId = [0; 8];

pub type ViewTag = [u8; 3];

/// Whether an output pays someone or returns change: it is bound into the commitment's blinding factor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum EnoteType {
    Payment = 0,
    Change = 1,
}

/// A non-coinbase output ("enote"), with the transaction's first key image it was made against. The first six fields
/// are the 123-byte output of `docs/CONSENSUS_V2.md` 6.1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Enote {
    pub onetime_address: [u8; 32],
    pub amount_commitment: [u8; 32],
    pub amount_enc: [u8; 8],
    pub view_tag: ViewTag,
    pub ephemeral_pubkey: [u8; 32],
    pub anchor_enc: JanusAnchor,
    pub tx_first_key_image: [u8; 32],
}

/// A coinbase output: its amount is public.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoinbaseEnote {
    pub onetime_address: [u8; 32],
    pub amount: u64,
    pub view_tag: ViewTag,
    pub ephemeral_pubkey: [u8; 32],
    pub anchor_enc: JanusAnchor,
    pub block_index: u64,
}

/// Why an output or an output set could not be made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CarrotError {
    /// A public key given (an address's spend or view key) is not a valid point.
    InvalidPoint,
    /// A subaddress index of (0, 0): that is the main address.
    BadAddressType(&'static str),
    /// Randomness that must be fresh is zero or repeated.
    MissingRandomness(&'static str),
    /// The output set breaks one of Carrot's rules (too few outputs, no self-send, ...).
    BadOutputSet(&'static str),
}

impl std::fmt::Display for CarrotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CarrotError::InvalidPoint => write!(f, "a public key is not a valid point"),
            CarrotError::BadAddressType(why) => write!(f, "wrong kind of address: {why}"),
            CarrotError::MissingRandomness(why) => {
                write!(f, "missing or repeated randomness: {why}")
            }
            CarrotError::BadOutputSet(why) => write!(f, "invalid output set: {why}"),
        }
    }
}

impl std::error::Error for CarrotError {}
