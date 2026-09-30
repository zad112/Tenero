//! Peer-to-peer protocol logic (milestone M8.1) as a **state machine with no I/O**: events go in (a peer
//! connected, a message arrived, the clock ticked), actions come out (send this, disconnect that). The same
//! engine will be driven by real sockets in M8.4; today it is driven by [`sim`], a deterministic simulated
//! network, so partitions, delays, message loss and hostile peers can be tested without timing luck.
//!
//! The messages here are typed values; their byte encoding, with golden vectors, is M8.2.
//! **Experimental and unaudited.**

pub mod engine;
pub mod message;
pub mod sim;

pub use engine::{Action, Engine, EngineConfig, Event, PeerId, Stats};
pub use message::{Hello, Limits, Message, PROTOCOL_VERSION};
