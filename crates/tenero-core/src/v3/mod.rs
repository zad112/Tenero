//! The version 3 data model: the `gamma` network (and `dev`, `test` from 0.3.0), FCMP++ and Carrot from genesis
//! (`docs/CONSENSUS_V2.md` section 15, `docs/FCMP_CARROT_PLAN.md`). Checked against `tests/vectors/v3_*.json`, made by the
//! Python reference `reference/tools/make_vectors_v3.py`.
//!
//! The header, the coinbase and its outputs keep their version 2 encodings (re-exported here); an output, a transaction
//! and a block are new. The codec is version 2's.

pub mod ids;
pub mod rules;
pub mod types;

pub use crate::v2::codec::{DecodeError, EncodeError, Reader, Wire, Writer};
pub use crate::v2::{BlockHeader, Coinbase, CoinbaseOutput, Input};
pub use types::*;
