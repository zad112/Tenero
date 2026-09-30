//! Tenero's use of the audited proof libraries (milestone M7). **Experimental.**
//!
//! * [`ringct`]: the proofs of a version 2 transaction (`docs/CONSENSUS_V2.md` section 7): CLSAG ring
//!   signatures, one aggregated Bulletproofs+ range proof and the balance equation, checked by
//!   [`ringct::RingCtProofs`], which plugs into the block validator as its `ProofCheck`.
//!
//! The curve mathematics and both proof systems are `monero-oxide`'s (see `Cargo.toml` for the pin and
//! what its audit covers). What is ours, and is not audited: the layout of `proof_data`, the message the
//! signatures cover, the checks around the libraries, and the prover used by tests and the wallet.

pub mod ringct;
