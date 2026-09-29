//! The version 2 data model of the native rewrite (`docs/CONSENSUS_V2.md`, a draft): outputs,
//! transactions with a prunable part, blocks, their canonical wire form, ids and the genesis block.
//!
//! Nothing here checks a cryptographic proof yet (that needs the upstream libraries, milestone M7): it
//! is the data, its exact bytes and its hashes, all checked against `tests/vectors/v2_*.json`.

pub mod codec;
pub mod ids;
pub mod types;

pub use codec::{DecodeError, EncodeError, Reader, Wire, Writer};
pub use types::*;
