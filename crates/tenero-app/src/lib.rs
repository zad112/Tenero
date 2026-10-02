//! Tenero's programs (milestone M8.7): the node daemon `tenerod` and the wallet command line `tenero-wallet`, with the
//! local control interface between them. **Experimental and unaudited.**
//!
//! * [`control`]: the control protocol's messages and framing (`docs/CONTROL_PROTOCOL.md`).
//! * [`server`]: the control server inside the node. [`client`]: the wallet's end of it.
//! * [`config`], [`log`]: settings (file and command line) and logging.
//! * [`daemon`]: the node program's body. [`wallet_cli`]: the wallet program's commands.

pub mod client;
pub mod config;
pub mod control;
pub mod daemon;
pub mod log;
pub mod private_dir;
pub mod remote_miner;
pub mod server;
pub mod wallet_cli;
