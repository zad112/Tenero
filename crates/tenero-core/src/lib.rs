//! Tenero consensus code, the native rewrite. Experimental and unaudited.
//!
//! Every rule here must reproduce `tests/vectors/` exactly (see `docs/CONSENSUS.md`).

pub mod chacha20;
pub mod hash;
pub mod matmulhash;
pub mod u256;
pub mod vectors;
