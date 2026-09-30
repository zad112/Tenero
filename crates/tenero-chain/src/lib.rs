//! Block validation for the native rewrite (`docs/CONSENSUS_V2.md` section 8). Experimental and unaudited.
//!
//! [`Validator`] checks a block that extends the tip against every rule that needs no cryptographic proof, and
//! adds it to the [`tenero_store::Store`]. The proofs themselves (milestone M7) go through [`proofs`], and
//! are **not checked yet**: see [`ProofsNotChecked`].

pub mod chain;
pub mod params;
pub mod pow;
pub mod proofs;
pub mod validate;

pub use chain::{Chain, Submitted};
pub use params::{ChainParams, COINBASE_MATURITY, FUTURE_LIMIT_SECONDS, RING_SIZE, SPEND_MATURITY};
pub use pow::{MatmulPow, PowCheck, Sha256Pow};
pub use proofs::{ProofCheck, ProofsNotChecked, TxContext};
pub use validate::{Accepted, BlockError, NextBlock, Outcome, ValidatedBlock, Validator};
