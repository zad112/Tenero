//! The node core (milestone M8.0): the chain, the mempool, and the rules for using them together. No
//! networking yet. **Experimental and unaudited.**

pub mod mempool;
pub mod node;

pub use mempool::{AddOutcome, Mempool, MempoolConfig, PoolError};
pub use node::{Node, NodeConfig, NodeError, Payout, PoolLoad};
