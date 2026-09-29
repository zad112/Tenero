//! Tenero consensus code, the native rewrite. Experimental and unaudited.
//!
//! Every rule here must reproduce `tests/vectors/` exactly (see `docs/CONSENSUS.md`).

pub mod chacha20;
pub mod difficulty;
pub mod emission;
pub mod fees;
pub mod hash;
pub mod matmulhash;
pub mod u256;
pub mod units;
pub mod v2;
pub mod vectors;
