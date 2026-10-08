//! The CONTROL protocol: how the wallet (and anything else on this machine) talks to a running node.
//! `docs/CONTROL_PROTOCOL.md` is the description, `tests/vectors/control.json` the golden vectors.
//!
//! A frame is `length u32 little-endian | body`, the body `kind u8 | payload`, the length at most [`MAX_FRAME`].
//! Requests have kinds 1 to 20 (4, 5 and 15, the ring members' outputs of version 2, are retired and unknown); the answer
//! to request `k` has kind `k | 0x80`; an error answer is `0xFF`.
//! Decoding is strict: an unknown kind, a short payload, a trailing byte, a count out of range or text that is not
//! UTF-8 is an error, and one message has one encoding. **Experimental and unaudited.**

use tenero_core::v2::{MAX_BLOCK_TXS, MAX_COINBASE_OUTPUTS, MAX_EXTRA};
use tenero_core::v3::{
    Block, BlockHeader, Coinbase, CoinbaseOutput, DecodeError, EncodeError, Reader, Transaction,
    TxPrefix, Wire, Writer,
};
use tenero_node::PoolEntry;
use tenero_store::TreeState;
use tenero_tree::{PathBytes, HELIOS_WIDTH, LEAF_CHUNK, SELENE_WIDTH};
use tenero_wallet::{Rules, ScanBlock, SpendPaths};

/// The biggest body a frame may carry (a block of the biggest allowed size fits).
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
/// The most blocks one `Blocks` request may ask for, and the most bytes of blocks one answer carries (a request that
/// would go over is answered with fewer blocks, never an error, so a client just asks again from where it stopped).
pub const MAX_BLOCKS_PER_REQUEST: u16 = 64;
pub const MAX_BLOCKS_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_TEXT: usize = 512;
/// The most pooled transactions one `Mempool` answer lists (the best fee rates; the answer also says how many there are).
pub const MAX_MEMPOOL_LIST: usize = 4096;
pub const MAX_NAME: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    /// Keeps every block's proofs.
    Archive,
    /// Throws away the proofs of old blocks (`CONSENSUS_V2.md` 14).
    Pruned,
}

/// What a block explorer shows of a block: its header's time, the target it met, the work so far, its weight and what its
/// coinbase paid. Nothing a wallet's privacy rests on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockSummary {
    pub height: u64,
    pub id: [u8; 32],
    /// The header's timestamp (Unix seconds, as the miner wrote it).
    pub timestamp: u64,
    /// The target the block met (big-endian; all zeros for the genesis block).
    pub target: [u8; 32],
    /// The chain's total work up to and including this block (big-endian).
    pub cumulative_work: [u8; 32],
    /// The weight of its transactions (`CONSENSUS_V2.md` 15.4: what the block limit counts; 0 for a block with none).
    pub weight: u64,
    /// Transactions besides the coinbase.
    pub tx_count: u32,
    /// The coinbase's outputs added up: the reward and the fees, less any penalty.
    pub coinbase_total: u64,
}

/// The chain's numbers for a block explorer: what the next block must meet and what the schedule has paid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainStats {
    /// The tip's height.
    pub height: u64,
    /// The target the next block's id must be below (big-endian).
    pub next_target: [u8; 32],
    /// The chain's total work up to the tip (big-endian).
    pub cumulative_work: [u8; 32],
    /// The next block's base reward.
    pub next_reward: u64,
    /// The base rewards of blocks 1 to the tip added up (`Emission::paid_through`): the schedule, with no penalty
    /// subtracted.
    pub emitted: u64,
    /// The main emission's cap, and the reward each block pays once it is reached.
    pub max_supply: u64,
    pub tail_reward: u64,
    /// The time between blocks the difficulty aims at, in seconds.
    pub block_time: u64,
}

/// What a node says about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeInfo {
    pub height: u64,
    pub tip_id: [u8; 32],
    pub peers: u32,
    pub inbound: u32,
    pub pruned_below: u64,
    pub mempool_txs: u32,
    pub syncing: bool,
    pub kind: NodeKind,
    pub network: String,
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// The first message of a connection: the contents of the node's cookie file.
    Auth {
        cookie: [u8; 32],
    },
    Tip,
    Block {
        height: u64,
    },
    /// The full proof-of-work check of a header at a height (`PowCheck::check_full`): the answer says whether the mix is the one the proof of work gives for the
    /// header's nonce. The node does the work on its own thread with the dataset it already holds, so a program that checks many headers (a mining pool) needs no
    /// dataset of its own. It does NOT say the id meets any target: that is a comparison the asker makes.
    CheckPow {
        height: u64,
        header: BlockHeader,
    },
    /// The paths in the curve tree of the outputs with these global indexes (1 to [`MAX_SPEND_PATHS`]), all in the tree of
    /// one reference block (the tip): what a wallet proves a spend with. The node learns which outputs are about to be
    /// spent, so a wallet asks only a node on its own machine (`docs/FCMP_CARROT_PLAN.md` 7).
    SpendPaths {
        indexes: Vec<u64>,
    },
    KeyImageSpent {
        key_image: [u8; 32],
    },
    /// Many key images at once (1 to [`MAX_KEY_IMAGES`]): the answer has one flag for each, in order. A wallet with
    /// thousands of coins asked one by one (a round trip each) took twenty seconds to find which are spent.
    KeyImagesSpent {
        key_images: Vec<[u8; 32]>,
    },
    Rules,
    SubmitTx(Transaction),
    Info,
    /// Asks the node to shut down cleanly.
    Stop,
    /// Up to `count` blocks from height `from` on (`count` is 1 to [`MAX_BLOCKS_PER_REQUEST`]).
    Blocks {
        from: u64,
        count: u16,
    },
    /// An unmined block on the node's tip, with the coinbase paying the main address whose keys these are, for a miner in
    /// another process to search. A Carrot coinbase output depends on its amount, which only the node knows (the reward
    /// and the fees of what it puts in), so the node makes the output, with fresh randomness each time; it therefore
    /// knows which of its templates' outputs are this address's, as the node a miner mines through always did.
    /// `max_weight` is the most transaction weight it wants (the node may give less).
    BlockTemplate {
        spend_pubkey: [u8; 32],
        view_pubkey: [u8; 32],
        max_weight: u64,
    },
    /// A mined block. The answer says whether it is in the chain, or on a side branch (it lost a race); a block the
    /// node refuses is an error answer.
    SubmitBlock(Block),
    /// Up to `count` [`BlockSummary`]s from height `from` on (`count` is 1 to [`MAX_BLOCKS_PER_REQUEST`]): a block explorer's
    /// list of blocks.
    Headers {
        from: u64,
        count: u16,
    },
    /// The transactions in the node's pool, best fee rate first.
    Mempool,
    /// The chain's numbers: difficulty, work, reward and emission.
    ChainStats,
}

/// What a miner searches: the block with an empty nonce and mix, the height, and the target its id must be below
/// (big-endian). `anchor` is the randomness the node made the coinbase output with (Carrot's Janus anchor): with it the
/// miner makes the same output from its own address and the amount, and so knows the template pays it
/// (`remote_miner::check_template`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Template {
    pub block: Block,
    pub height: u64,
    pub target: [u8; 32],
    pub anchor: [u8; 16],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    Authed,
    Tip {
        height: u64,
        id: [u8; 32],
    },
    Block(Option<ScanBlock>),
    /// The answer to `CheckPow`: the mix is right, or not.
    PowChecked(bool),
    /// The reference block, its tree, and one path for each index asked about, in order (`None` for an output not in
    /// that tree).
    SpendPaths(SpendPaths),
    Spent(bool),
    /// One flag for each key image asked about, in order.
    SpentMany(Vec<bool>),
    Rules(Rules),
    TxAccepted {
        id: [u8; 32],
    },
    Info(NodeInfo),
    Stopping,
    /// Blocks in order from the requested height: fewer than asked at the tip, or when they would not fit a frame.
    Blocks(Vec<ScanBlock>),
    Template(Template),
    /// The block was taken: `in_chain` is true when it is part of the node's chain, false when it is on a side branch
    /// because another block took its place first.
    BlockSubmitted {
        id: [u8; 32],
        in_chain: bool,
    },
    /// Block summaries in order from the requested height: fewer than asked at the tip.
    Headers(Vec<BlockSummary>),
    /// `total` is how many transactions the pool holds; `txs` the best of them by fee rate, at most
    /// [`MAX_MEMPOOL_LIST`], never more than `total`.
    Mempool {
        total: u32,
        txs: Vec<PoolEntry>,
    },
    ChainStats(ChainStats),
    Error(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum ControlError {
    /// The frame's length is 0 or over [`MAX_FRAME`].
    BadLength(usize),
    UnknownKind(u8),
    Decode(DecodeError),
    /// A byte after the end of the message.
    Trailing,
    Encode(String),
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControlError::BadLength(n) => write!(f, "a frame of {n} bytes is not allowed"),
            ControlError::UnknownKind(k) => write!(f, "unknown message kind {k}"),
            ControlError::Decode(e) => write!(f, "malformed message: {}", e.as_str()),
            ControlError::Trailing => write!(f, "bytes after the end of the message"),
            ControlError::Encode(e) => write!(f, "cannot encode: {e}"),
        }
    }
}

impl std::error::Error for ControlError {}

impl From<DecodeError> for ControlError {
    fn from(e: DecodeError) -> Self {
        ControlError::Decode(e)
    }
}

impl From<EncodeError> for ControlError {
    fn from(e: EncodeError) -> Self {
        ControlError::Encode(e.to_string())
    }
}

pub const K_AUTH: u8 = 1;
pub const K_TIP: u8 = 2;
pub const K_BLOCK: u8 = 3;
pub const K_KEY_IMAGE_SPENT: u8 = 6;
pub const K_KEY_IMAGES_SPENT: u8 = 14;
/// `check_pow`: is this header's mix what the proof of work gives (the full check, with the node's own dataset)? A pool asks it so that it needs no 4 GiB dataset of its own.
pub const K_CHECK_POW: u8 = 16;
/// `spend_paths`: the curve-tree paths a spend is proven with (version 3).
pub const K_SPEND_PATHS: u8 = 20;
/// The most outputs one `SpendPaths` request may ask about. A path is at most about 12 KiB (a leaf chunk of 38 outputs
/// and a chunk for each of up to eight layers), so the answer stays well inside a frame.
pub const MAX_SPEND_PATHS: usize = 512;
/// The most layers a path may have: a tree of `u64::MAX` leaves has fewer.
pub const MAX_PATH_LAYERS: usize = 32;
/// The most key images one `KeyImagesSpent` request may ask about (and the most flags one answer carries).
pub const MAX_KEY_IMAGES: usize = 4096;
pub const K_RULES: u8 = 7;
pub const K_SUBMIT_TX: u8 = 8;
pub const K_INFO: u8 = 9;
pub const K_STOP: u8 = 10;
pub const K_BLOCKS: u8 = 11;
pub const K_BLOCK_TEMPLATE: u8 = 12;
pub const K_SUBMIT_BLOCK: u8 = 13;
/// `headers`, `mempool` and `chain_stats`: what a block explorer shows (added 2026-10-07).
pub const K_HEADERS: u8 = 17;
pub const K_MEMPOOL: u8 = 18;
pub const K_CHAIN_STATS: u8 = 19;
pub const K_ERROR: u8 = 0xFF;
const ANSWER: u8 = 0x80;

fn text(w: &mut Writer, s: &str, max: usize) -> Result<(), EncodeError> {
    w.var(s.as_bytes(), max)
}

fn read_text(r: &mut Reader<'_>, max: usize) -> Result<String, DecodeError> {
    String::from_utf8(r.var(max)?).map_err(|_| DecodeError::CountOutOfRange)
}

fn flag(r: &mut Reader<'_>) -> Result<bool, DecodeError> {
    match r.take(1)?[0] {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(DecodeError::CountOutOfRange),
    }
}

fn put_flag(w: &mut Writer, b: bool) {
    w.raw(&[u8::from(b)]);
}

impl Request {
    pub fn kind(&self) -> u8 {
        match self {
            Request::Auth { .. } => K_AUTH,
            Request::Tip => K_TIP,
            Request::Block { .. } => K_BLOCK,
            Request::SpendPaths { .. } => K_SPEND_PATHS,
            Request::CheckPow { .. } => K_CHECK_POW,
            Request::KeyImageSpent { .. } => K_KEY_IMAGE_SPENT,
            Request::KeyImagesSpent { .. } => K_KEY_IMAGES_SPENT,
            Request::Rules => K_RULES,
            Request::SubmitTx(_) => K_SUBMIT_TX,
            Request::Info => K_INFO,
            Request::Stop => K_STOP,
            Request::Blocks { .. } => K_BLOCKS,
            Request::BlockTemplate { .. } => K_BLOCK_TEMPLATE,
            Request::SubmitBlock(_) => K_SUBMIT_BLOCK,
            Request::Headers { .. } => K_HEADERS,
            Request::Mempool => K_MEMPOOL,
            Request::ChainStats => K_CHAIN_STATS,
        }
    }

    /// The body (kind and payload), without the length prefix.
    pub fn to_body(&self) -> Result<Vec<u8>, ControlError> {
        let mut w = Writer::new();
        w.raw(&[self.kind()]);
        match self {
            Request::Auth { cookie } => w.raw(cookie),
            Request::Tip
            | Request::Rules
            | Request::Info
            | Request::Stop
            | Request::Mempool
            | Request::ChainStats => {}
            Request::Block { height } => w.u64(*height),
            Request::CheckPow { height, header } => {
                w.u64(*height);
                header.write(&mut w)?;
            }
            Request::SpendPaths { indexes } => {
                w.count(indexes.len(), 1, MAX_SPEND_PATHS)?;
                for i in indexes {
                    w.u64(*i);
                }
            }
            Request::KeyImageSpent { key_image } => w.raw(key_image),
            Request::KeyImagesSpent { key_images } => {
                w.count(key_images.len(), 1, MAX_KEY_IMAGES)?;
                for k in key_images {
                    w.raw(k);
                }
            }
            Request::SubmitTx(tx) => tx.write(&mut w)?,
            Request::Blocks { from, count } | Request::Headers { from, count } => {
                w.u64(*from);
                w.u16(*count);
            }
            Request::BlockTemplate {
                spend_pubkey,
                view_pubkey,
                max_weight,
            } => {
                w.raw(spend_pubkey);
                w.raw(view_pubkey);
                w.u64(*max_weight);
            }
            Request::SubmitBlock(b) => b.write(&mut w)?,
        }
        Ok(w.into_bytes())
    }

    pub fn from_body(body: &[u8]) -> Result<Request, ControlError> {
        let mut r = Reader::new(body);
        let kind = r.take(1).map_err(|_| ControlError::BadLength(0))?[0];
        let req = match kind {
            K_AUTH => Request::Auth { cookie: r.array()? },
            K_TIP => Request::Tip,
            K_BLOCK => Request::Block { height: r.u64()? },
            K_CHECK_POW => Request::CheckPow {
                height: r.u64()?,
                header: BlockHeader::read(&mut r)?,
            },
            K_SPEND_PATHS => {
                let n = r.count(1, MAX_SPEND_PATHS)?;
                let indexes = (0..n).map(|_| r.u64()).collect::<Result<Vec<u64>, _>>()?;
                Request::SpendPaths { indexes }
            }
            K_KEY_IMAGES_SPENT => {
                let n = r.count(1, MAX_KEY_IMAGES)?;
                let key_images = (0..n)
                    .map(|_| r.array())
                    .collect::<Result<Vec<[u8; 32]>, _>>()?;
                Request::KeyImagesSpent { key_images }
            }
            K_KEY_IMAGE_SPENT => Request::KeyImageSpent {
                key_image: r.array()?,
            },
            K_RULES => Request::Rules,
            K_SUBMIT_TX => Request::SubmitTx(Transaction::read(&mut r)?),
            K_INFO => Request::Info,
            K_STOP => Request::Stop,
            K_BLOCKS | K_HEADERS => {
                let from = r.u64()?;
                let count = r.u16()?;
                if count == 0 || count > MAX_BLOCKS_PER_REQUEST {
                    return Err(DecodeError::CountOutOfRange.into());
                }
                if kind == K_BLOCKS {
                    Request::Blocks { from, count }
                } else {
                    Request::Headers { from, count }
                }
            }
            K_MEMPOOL => Request::Mempool,
            K_CHAIN_STATS => Request::ChainStats,
            K_BLOCK_TEMPLATE => Request::BlockTemplate {
                spend_pubkey: r.array()?,
                view_pubkey: r.array()?,
                max_weight: r.u64()?,
            },
            K_SUBMIT_BLOCK => Request::SubmitBlock(Block::read(&mut r)?),
            other => return Err(ControlError::UnknownKind(other)),
        };
        r.finish().map_err(|_| ControlError::Trailing)?;
        Ok(req)
    }
}

fn write_scan_block(w: &mut Writer, b: &ScanBlock) -> Result<(), EncodeError> {
    w.u64(b.height);
    w.raw(&b.id);
    w.u64(b.first_output_index);
    // the coinbase is written here, not with its consensus encoding, because the genesis block has none
    // (no outputs), which that encoding forbids
    w.u16(b.coinbase.version);
    w.u64(b.coinbase.height);
    w.count(b.coinbase.outputs.len(), 0, MAX_COINBASE_OUTPUTS)?;
    for o in &b.coinbase.outputs {
        o.write(w)?;
    }
    w.var(&b.coinbase.extra, MAX_EXTRA)?;
    w.count(b.txs.len(), 0, MAX_BLOCK_TXS)?;
    for t in &b.txs {
        t.write(w)?;
    }
    Ok(())
}

fn write_summary(w: &mut Writer, b: &BlockSummary) {
    w.u64(b.height);
    w.raw(&b.id);
    w.u64(b.timestamp);
    w.raw(&b.target);
    w.raw(&b.cumulative_work);
    w.u64(b.weight);
    w.u32(b.tx_count);
    w.u64(b.coinbase_total);
}

fn read_summary(r: &mut Reader<'_>) -> Result<BlockSummary, DecodeError> {
    Ok(BlockSummary {
        height: r.u64()?,
        id: r.array()?,
        timestamp: r.u64()?,
        target: r.array()?,
        cumulative_work: r.array()?,
        weight: r.u64()?,
        tx_count: r.u32()?,
        coinbase_total: r.u64()?,
    })
}

fn read_pool_entry(r: &mut Reader<'_>) -> Result<PoolEntry, DecodeError> {
    Ok(PoolEntry {
        id: r.array()?,
        received: r.u64()?,
        fee: r.u64()?,
        size: r.u64()?,
        weight: r.u64()?,
    })
}

/// The widest chunk a path may carry above the leaves (the curve tree's two branch widths).
const MAX_CHUNK: usize = if SELENE_WIDTH > HELIOS_WIDTH {
    SELENE_WIDTH
} else {
    HELIOS_WIDTH
};

fn write_path(w: &mut Writer, p: &PathBytes) -> Result<(), EncodeError> {
    w.u64(p.position);
    w.count(p.leaves.len(), 1, LEAF_CHUNK)?;
    for (o, c) in &p.leaves {
        w.raw(o);
        w.raw(c);
    }
    w.count(p.layers.len(), 0, MAX_PATH_LAYERS)?;
    for chunk in &p.layers {
        w.count(chunk.len(), 1, MAX_CHUNK)?;
        for x in chunk {
            w.raw(x);
        }
    }
    Ok(())
}

fn read_path(r: &mut Reader<'_>) -> Result<PathBytes, DecodeError> {
    let position = r.u64()?;
    let leaves = r.list(1, LEAF_CHUNK, |r| Ok((r.array()?, r.array()?)))?;
    let layers = r.list(0, MAX_PATH_LAYERS, |r| r.list(1, MAX_CHUNK, |r| r.array()))?;
    Ok(PathBytes {
        position,
        leaves,
        layers,
    })
}

fn read_scan_block(r: &mut Reader<'_>) -> Result<ScanBlock, DecodeError> {
    Ok(ScanBlock {
        height: r.u64()?,
        id: r.array()?,
        first_output_index: r.u64()?,
        coinbase: Coinbase {
            version: r.u16()?,
            height: r.u64()?,
            outputs: r.list(0, MAX_COINBASE_OUTPUTS, CoinbaseOutput::read)?,
            extra: r.var(MAX_EXTRA)?,
        },
        txs: r.list(0, MAX_BLOCK_TXS, TxPrefix::read)?,
    })
}

impl Response {
    /// The kind of the request this answers (or [`K_ERROR`]), with the answer bit set.
    pub fn kind(&self) -> u8 {
        match self {
            Response::Authed => K_AUTH | ANSWER,
            Response::Tip { .. } => K_TIP | ANSWER,
            Response::Block(_) => K_BLOCK | ANSWER,
            Response::SpendPaths(_) => K_SPEND_PATHS | ANSWER,
            Response::PowChecked(_) => K_CHECK_POW | ANSWER,
            Response::Spent(_) => K_KEY_IMAGE_SPENT | ANSWER,
            Response::SpentMany(_) => K_KEY_IMAGES_SPENT | ANSWER,
            Response::Rules(_) => K_RULES | ANSWER,
            Response::TxAccepted { .. } => K_SUBMIT_TX | ANSWER,
            Response::Info(_) => K_INFO | ANSWER,
            Response::Stopping => K_STOP | ANSWER,
            Response::Blocks(_) => K_BLOCKS | ANSWER,
            Response::Template(_) => K_BLOCK_TEMPLATE | ANSWER,
            Response::BlockSubmitted { .. } => K_SUBMIT_BLOCK | ANSWER,
            Response::Headers(_) => K_HEADERS | ANSWER,
            Response::Mempool { .. } => K_MEMPOOL | ANSWER,
            Response::ChainStats(_) => K_CHAIN_STATS | ANSWER,
            Response::Error(_) => K_ERROR,
        }
    }

    pub fn to_body(&self) -> Result<Vec<u8>, ControlError> {
        let mut w = Writer::new();
        w.raw(&[self.kind()]);
        match self {
            Response::Authed | Response::Stopping => {}
            Response::Tip { height, id } => {
                w.u64(*height);
                w.raw(id);
            }
            Response::Block(b) => match b {
                Some(b) => {
                    put_flag(&mut w, true);
                    write_scan_block(&mut w, b)?;
                }
                None => put_flag(&mut w, false),
            },
            Response::PowChecked(b) => put_flag(&mut w, *b),
            Response::SpendPaths(sp) => {
                w.u64(sp.reference_height);
                sp.tree.write(&mut w)?;
                w.count(sp.paths.len(), 1, MAX_SPEND_PATHS)?;
                for p in &sp.paths {
                    match p {
                        Some(p) => {
                            put_flag(&mut w, true);
                            write_path(&mut w, p)?;
                        }
                        None => put_flag(&mut w, false),
                    }
                }
            }
            Response::Spent(b) => put_flag(&mut w, *b),
            Response::SpentMany(v) => {
                w.count(v.len(), 1, MAX_KEY_IMAGES)?;
                for b in v {
                    put_flag(&mut w, *b);
                }
            }
            Response::Rules(r) => {
                w.raw(&r.chain_id);
                w.u64(r.next_height);
                w.u64(r.reward);
                w.u64(r.median);
                w.raw(&[u8::try_from(r.tree_layers)
                    .ok()
                    .filter(|l| usize::from(*l) <= MAX_PATH_LAYERS)
                    .ok_or_else(|| ControlError::Encode("tree layers".into()))?]);
            }
            Response::TxAccepted { id } => w.raw(id),
            Response::Info(i) => {
                w.u64(i.height);
                w.raw(&i.tip_id);
                w.u32(i.peers);
                w.u32(i.inbound);
                w.u64(i.pruned_below);
                w.u32(i.mempool_txs);
                put_flag(&mut w, i.syncing);
                w.raw(&[match i.kind {
                    NodeKind::Archive => 0,
                    NodeKind::Pruned => 1,
                }]);
                text(&mut w, &i.network, MAX_NAME)?;
                text(&mut w, &i.version, MAX_NAME)?;
            }
            Response::Blocks(blocks) => {
                w.count(blocks.len(), 0, usize::from(MAX_BLOCKS_PER_REQUEST))?;
                for b in blocks {
                    write_scan_block(&mut w, b)?;
                }
            }
            Response::Template(t) => {
                w.u64(t.height);
                w.raw(&t.target);
                w.raw(&t.anchor);
                t.block.write(&mut w)?;
            }
            Response::BlockSubmitted { id, in_chain } => {
                w.raw(id);
                put_flag(&mut w, *in_chain);
            }
            Response::Headers(v) => {
                w.count(v.len(), 0, usize::from(MAX_BLOCKS_PER_REQUEST))?;
                for b in v {
                    write_summary(&mut w, b);
                }
            }
            Response::Mempool { total, txs } => {
                if txs.len() > *total as usize {
                    return Err(ControlError::Encode(
                        "more pooled transactions listed than the pool holds".into(),
                    ));
                }
                w.u32(*total);
                w.count(txs.len(), 0, MAX_MEMPOOL_LIST)?;
                for t in txs {
                    w.raw(&t.id);
                    w.u64(t.received);
                    w.u64(t.fee);
                    w.u64(t.size);
                    w.u64(t.weight);
                }
            }
            Response::ChainStats(c) => {
                w.u64(c.height);
                w.raw(&c.next_target);
                w.raw(&c.cumulative_work);
                w.u64(c.next_reward);
                w.u64(c.emitted);
                w.u64(c.max_supply);
                w.u64(c.tail_reward);
                w.u64(c.block_time);
            }
            Response::Error(m) => text(&mut w, m, MAX_TEXT)?,
        }
        Ok(w.into_bytes())
    }

    pub fn from_body(body: &[u8]) -> Result<Response, ControlError> {
        let mut r = Reader::new(body);
        let kind = r.take(1).map_err(|_| ControlError::BadLength(0))?[0];
        let resp = match kind {
            x if x == K_AUTH | ANSWER => Response::Authed,
            x if x == K_TIP | ANSWER => Response::Tip {
                height: r.u64()?,
                id: r.array()?,
            },
            x if x == K_BLOCK | ANSWER => Response::Block(if flag(&mut r)? {
                Some(read_scan_block(&mut r)?)
            } else {
                None
            }),
            x if x == K_CHECK_POW | ANSWER => Response::PowChecked(flag(&mut r)?),
            x if x == K_SPEND_PATHS | ANSWER => {
                let reference_height = r.u64()?;
                let tree = TreeState::read(&mut r)?;
                let paths = r.list(1, MAX_SPEND_PATHS, |r| {
                    Ok(if flag(r)? { Some(read_path(r)?) } else { None })
                })?;
                Response::SpendPaths(SpendPaths {
                    reference_height,
                    tree,
                    paths,
                })
            }
            x if x == K_KEY_IMAGE_SPENT | ANSWER => Response::Spent(flag(&mut r)?),
            x if x == K_KEY_IMAGES_SPENT | ANSWER => {
                let n = r.count(1, MAX_KEY_IMAGES)?;
                let mut flags = Vec::with_capacity(n);
                for _ in 0..n {
                    flags.push(flag(&mut r)?);
                }
                Response::SpentMany(flags)
            }
            x if x == K_RULES | ANSWER => Response::Rules(Rules {
                chain_id: r.array()?,
                next_height: r.u64()?,
                reward: r.u64()?,
                median: r.u64()?,
                tree_layers: match usize::from(r.take(1)?[0]) {
                    l if l <= MAX_PATH_LAYERS => l,
                    _ => return Err(DecodeError::CountOutOfRange.into()),
                },
            }),
            x if x == K_SUBMIT_TX | ANSWER => Response::TxAccepted { id: r.array()? },
            x if x == K_INFO | ANSWER => Response::Info(NodeInfo {
                height: r.u64()?,
                tip_id: r.array()?,
                peers: r.u32()?,
                inbound: r.u32()?,
                pruned_below: r.u64()?,
                mempool_txs: r.u32()?,
                syncing: flag(&mut r)?,
                kind: match r.take(1)?[0] {
                    0 => NodeKind::Archive,
                    1 => NodeKind::Pruned,
                    _ => return Err(DecodeError::CountOutOfRange.into()),
                },
                network: read_text(&mut r, MAX_NAME)?,
                version: read_text(&mut r, MAX_NAME)?,
            }),
            x if x == K_STOP | ANSWER => Response::Stopping,
            x if x == K_BLOCKS | ANSWER => {
                Response::Blocks(r.list(0, usize::from(MAX_BLOCKS_PER_REQUEST), read_scan_block)?)
            }
            x if x == K_BLOCK_TEMPLATE | ANSWER => Response::Template(Template {
                height: r.u64()?,
                target: r.array()?,
                anchor: r.array()?,
                block: Block::read(&mut r)?,
            }),
            x if x == K_SUBMIT_BLOCK | ANSWER => Response::BlockSubmitted {
                id: r.array()?,
                in_chain: flag(&mut r)?,
            },
            x if x == K_HEADERS | ANSWER => {
                Response::Headers(r.list(0, usize::from(MAX_BLOCKS_PER_REQUEST), read_summary)?)
            }
            x if x == K_MEMPOOL | ANSWER => {
                let total = r.u32()?;
                // a list longer than the pool it lists is not an answer
                let txs = r.list(0, MAX_MEMPOOL_LIST.min(total as usize), read_pool_entry)?;
                Response::Mempool { total, txs }
            }
            x if x == K_CHAIN_STATS | ANSWER => Response::ChainStats(ChainStats {
                height: r.u64()?,
                next_target: r.array()?,
                cumulative_work: r.array()?,
                next_reward: r.u64()?,
                emitted: r.u64()?,
                max_supply: r.u64()?,
                tail_reward: r.u64()?,
                block_time: r.u64()?,
            }),
            K_ERROR => Response::Error(read_text(&mut r, MAX_TEXT)?),
            other => return Err(ControlError::UnknownKind(other)),
        };
        r.finish().map_err(|_| ControlError::Trailing)?;
        Ok(resp)
    }
}

/// How many bytes a scanned block takes on the wire.
pub fn scan_block_size(b: &ScanBlock) -> usize {
    let mut w = Writer::new();
    // a block that cannot be encoded is counted as nothing here and refused when the answer is encoded
    let _ = write_scan_block(&mut w, b);
    w.into_bytes().len()
}

/// A body with its length prefix.
pub fn frame(body: &[u8]) -> Result<Vec<u8>, ControlError> {
    if body.is_empty() || body.len() > MAX_FRAME {
        return Err(ControlError::BadLength(body.len()));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    Ok(out)
}

/// The length a frame header announces, checked.
pub fn frame_len(header: [u8; 4]) -> Result<usize, ControlError> {
    let n = u32::from_le_bytes(header) as usize;
    if n == 0 || n > MAX_FRAME {
        return Err(ControlError::BadLength(n));
    }
    Ok(n)
}

/// Reads one frame's body from a stream.
pub fn read_frame(stream: &mut impl std::io::Read) -> std::io::Result<Vec<u8>> {
    let mut h = [0u8; 4];
    stream.read_exact(&mut h)?;
    let n = frame_len(h)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    // read in pieces so an announced length cannot make us allocate before the bytes arrive
    let mut body = Vec::with_capacity(n.min(64 * 1024));
    let mut left = n;
    let mut buf = [0u8; 16 * 1024];
    while left > 0 {
        let take = left.min(buf.len());
        stream.read_exact(&mut buf[..take])?;
        body.extend_from_slice(&buf[..take]);
        left -= take;
    }
    Ok(body)
}

pub fn write_frame(stream: &mut impl std::io::Write, body: &[u8]) -> std::io::Result<()> {
    let f = frame(body)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
    stream.write_all(&f)?;
    stream.flush()
}
