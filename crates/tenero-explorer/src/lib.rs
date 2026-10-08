//! Tenero's block explorer: a window onto a node running on this machine. It reads, through the node's control interface
//! (`docs/CONTROL_PROTOCOL.md`), the chain's height, difficulty and emission, the transactions waiting in the node's pool,
//! and the latest blocks, and estimates the network's hash rate from them. It never sends anything and never changes the node.
//!
//! * `core.rs`: where the node is, what is fetched, and the numbers worked out from it (tested without a window);
//! * `worker.rs`: asks the node on a thread of its own, so the window never waits;
//! * `ui.rs`: the window.
//!
//! **Experimental and unaudited. What it shows is what ONE node says, and nothing on any network it shows has value.**

pub mod core;
pub mod ui;
pub mod worker;
