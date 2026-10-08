//! Tenero's use of the proof libraries (milestone M7, then G1-G4). **Experimental.**
//!
//! * [`fcmp`]: the proofs of a version 3 (`gamma`) transaction (`docs/CONSENSUS_V2.md` 15.7): the pseudo-outputs and the
//!   balance, one aggregated Bulletproofs+ range proof, and the FCMP++ proof against the curve tree; checked a block at a
//!   time by [`fcmp::FcmpProofs`], which plugs into the block validator as its `ProofCheck`; and the prover.
//! * [`curve_tree`]: the FCMP++ curve tree of the `gamma` network (`docs/FCMP_CARROT_PLAN.md` 4.3): its leaves, growing and
//!   trimming it, its root, and the path a proof needs. Its bookkeeping is ours and unaudited; its roots match Monero's code.
//!
//! The curve mathematics and both proof systems are `monero-oxide`'s (see `Cargo.toml` for the pin and
//! what its audit covers). What is ours, and is not audited: the layout of `proof_data`, the message the
//! signatures cover, the checks around the libraries, and the prover used by tests and the wallet.

pub use tenero_tree as curve_tree;
pub mod fcmp;
