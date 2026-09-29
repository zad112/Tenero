//! Chain storage on redb, laid out so the chain can run pruned (`docs/CONSENSUS_V2.md` section 14).
//!
//! See [`store`] for the table layout. The store checks only what would corrupt its own indexes; the
//! consensus rules (proofs, fees, rewards, difficulty, maturity) belong to the validator above it.

pub mod error;
pub mod records;
pub mod store;

pub use error::{Result, StoreError};
pub use records::{AppendInfo, BlockIndex, PruneStats, StoredBlock, StoredOutput, StoredTx};
pub use store::{Store, FORMAT_VERSION};
