//! The messages two nodes exchange, and the limits that keep a hostile peer from making us do unbounded
//! work. Typed here; the wire encoding is M8.2.

use tenero_core::v2::{Block, Transaction};

pub const PROTOCOL_VERSION: u32 = 1;

/// The ceilings the wire format enforces (`docs/WIRE_PROTOCOL.md` section 2). The engine's own [`Limits`] may be
/// lower, never higher.
pub const MAX_LOCATOR: usize = 32;
pub const MAX_IDS: usize = 500;
pub const MAX_BLOCKS: usize = 32;
pub const MAX_TXS: usize = 64;
pub const MAX_NOT_FOUND: usize = 64;
/// Addresses in one `Addrs` message.
pub const MAX_ADDRS: usize = 100;

/// Hard limits on what one message may contain. A message over a limit is a protocol violation.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Ids in a block locator (newest first, ending with the genesis id).
    pub max_locator: usize,
    /// Ids in one `BlockIds` reply.
    pub max_ids: usize,
    /// Ids in one `GetBlocks` request, and blocks in one `Blocks` reply.
    pub max_blocks: usize,
    /// Ids in one `NewTx` announcement or `GetTxs` request, and transactions in one `Txs` reply.
    pub max_txs: usize,
    /// Addresses in one `Addrs` message.
    pub max_addrs: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_locator: MAX_LOCATOR,
            max_ids: MAX_IDS,
            max_blocks: MAX_BLOCKS,
            max_txs: MAX_TXS,
            max_addrs: MAX_ADDRS,
        }
    }
}

/// A peer's address as gossiped: an IPv6 address (an IPv4 one is written IPv4-mapped, `::ffff:a.b.c.d`), a port,
/// and when it was last seen alive, in Unix seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerAddr {
    pub ip: [u8; 16],
    pub port: u16,
    pub last_seen: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub version: u32,
    pub chain_id: [u8; 32],
    pub tip_height: u64,
    /// The tip's cumulative work, big-endian.
    pub cumulative_work: [u8; 32],
    pub tip_id: [u8; 32],
    /// Blocks below this height have lost their proofs here, so this node cannot serve them.
    pub pruned_below: u64,
    /// Random, chosen once per run. Two connections whose peers show the same nonce are two links to the same
    /// node: to ourselves, or a duplicate. 0 means "not set" and is never taken as a match.
    pub nonce: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Hello(Hello),
    Ping(u64),
    Pong(u64),
    /// "Which blocks do you have after the newest of these that you know?" Newest first, genesis last.
    GetBlockIds {
        locator: Vec<[u8; 32]>,
    },
    /// The ids of the chain after the common block, oldest first; `first_height` is the first one's height.
    BlockIds {
        first_height: u64,
        ids: Vec<[u8; 32]>,
    },
    GetBlocks {
        ids: Vec<[u8; 32]>,
    },
    Blocks {
        blocks: Vec<Block>,
    },
    /// Requested ids the sender cannot serve (unknown, or pruned).
    NotFound {
        ids: Vec<[u8; 32]>,
    },
    /// "I have a new tip": announced by id and work, and fetched from ONE peer, never pushed in full.
    NewBlock {
        id: [u8; 32],
        height: u64,
        cumulative_work: [u8; 32],
    },
    /// Transaction ids the sender has in its pool.
    NewTx {
        ids: Vec<[u8; 32]>,
    },
    GetTxs {
        ids: Vec<[u8; 32]>,
    },
    Txs {
        txs: Vec<Transaction>,
    },
    /// "Which peers do you know?" Answered once per connection.
    GetAddrs,
    /// Peers the sender knows (an answer to `GetAddrs`), or, once, the sender's own address.
    Addrs {
        addrs: Vec<PeerAddr>,
    },
}

impl Message {
    /// A short name for statistics and logs.
    pub fn kind(&self) -> &'static str {
        match self {
            Message::Hello(_) => "hello",
            Message::Ping(_) => "ping",
            Message::Pong(_) => "pong",
            Message::GetBlockIds { .. } => "get_block_ids",
            Message::BlockIds { .. } => "block_ids",
            Message::GetBlocks { .. } => "get_blocks",
            Message::Blocks { .. } => "blocks",
            Message::NotFound { .. } => "not_found",
            Message::NewBlock { .. } => "new_block",
            Message::NewTx { .. } => "new_tx",
            Message::GetTxs { .. } => "get_txs",
            Message::Txs { .. } => "txs",
            Message::GetAddrs => "get_addrs",
            Message::Addrs { .. } => "addrs",
        }
    }
}
