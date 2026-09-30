//! Peer-to-peer protocol logic (milestone M8.1) as a **state machine with no I/O**: events go in (a peer
//! connected, a message arrived, the clock ticked), actions come out (send this, disconnect that). The same
//! engine will be driven by real sockets in M8.4; today it is driven by [`sim`], a deterministic simulated
//! network, so partitions, delays, message loss and hostile peers can be tested without timing luck.
//!
//! The messages are typed values; their byte encoding, with golden vectors from an independent Python
//! reference, is [`wire`] (`docs/WIRE_PROTOCOL.md`).
//! **Experimental and unaudited.**

pub mod addrbook;
pub mod engine;
pub mod message;
pub mod noise;
pub mod sim;
pub mod transport;
pub mod wire;

pub use engine::{Action, AssumeValid, Engine, EngineConfig, Event, PeerId, Stats};
pub use message::{Hello, Limits, Message, PROTOCOL_VERSION};
pub use wire::{decode_frame, encode, FrameDecoder, WireError, MAX_FRAME};
