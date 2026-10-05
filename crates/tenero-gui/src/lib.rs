//! Tenero's wallet app (milestone M10.3): a desktop window for the wallet that starts and stops the node and the miner.
//! **Experimental and unaudited. Nothing on any network it uses has value.**
//!
//! * [`settings`]: the app's settings file. [`procs`]: starting and stopping the node and miner as processes.
//! * [`view`]: what the window shows and asks for. [`core`]: the logic behind it, tested without a window.
//! * [`backend`]: runs the logic on a thread of its own so the window never waits. [`ui`]: the window itself.

pub mod backend;
pub mod core;
pub mod movedata;
pub mod procs;
pub mod qr;
pub mod settings;
pub mod text;
pub mod ui;
pub mod view;
pub mod wallets;
