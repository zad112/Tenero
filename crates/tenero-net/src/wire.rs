//! The byte encoding of [`Message`] (`docs/WIRE_PROTOCOL.md`), and a streaming decoder.
//!
//! A frame is `length u32 | kind u8 | body`, `length` counting the kind byte and the body. Everything is
//! fixed-width little-endian, counts are checked before any element is read, and a decoder that accepts bytes
//! yields a message that encodes back to the same bytes. The independent Python reference
//! (`tools/make_vectors_wire.py`) makes `tests/vectors/v2_wire.json`, which this code must reproduce.
//!
//! No cryptography lives here: the Noise channel of M8.4 wraps these frames.

use tenero_core::v2::codec::{DecodeError, EncodeError, Reader, Writer};
use tenero_core::v2::{Block, Transaction, Wire};

use crate::message::{
    Hello, Message, PeerAddr, MAX_ADDRS, MAX_BLOCKS, MAX_IDS, MAX_LOCATOR, MAX_NOT_FOUND, MAX_TXS,
};

/// The most a frame's `length` may ever be (blocks and transactions lists).
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Why a frame was refused. [`WireError::as_str`] gives the names the vectors use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WireError {
    /// Fewer bytes than the frame declares (or than a header needs).
    ShortFrame,
    /// A `length` of zero: there is not even a kind byte.
    EmptyFrame,
    UnknownKind(u8),
    /// `length` is over the cap of its kind. Detected from the first 5 bytes.
    FrameTooLarge {
        kind: u8,
        length: u32,
        cap: usize,
    },
    /// Bytes after the frame.
    TrailingBytes,
    /// A body that does not decode: a count out of range, a short read, leftover bytes, or a block or
    /// transaction that fails its own strict decoder.
    Decode(DecodeError),
    /// A message that could not be encoded (it is over a limit a decoder would enforce).
    Encode(String),
}

impl WireError {
    /// The error's name in `tests/vectors/v2_wire.json`.
    pub fn as_str(&self) -> &'static str {
        match self {
            WireError::ShortFrame => "short frame",
            WireError::EmptyFrame => "empty frame",
            WireError::UnknownKind(_) => "unknown kind",
            WireError::FrameTooLarge { .. } => "frame too large",
            WireError::TrailingBytes => "trailing bytes",
            WireError::Decode(e) => e.as_str(),
            WireError::Encode(_) => "encode",
        }
    }
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Encode(why) => write!(f, "cannot encode: {why}"),
            other => write!(f, "{}", other.as_str()),
        }
    }
}

impl std::error::Error for WireError {}

impl From<DecodeError> for WireError {
    fn from(e: DecodeError) -> WireError {
        WireError::Decode(e)
    }
}

impl From<EncodeError> for WireError {
    fn from(e: EncodeError) -> WireError {
        WireError::Encode(e.to_string())
    }
}

const HELLO: u8 = 1;
const PING: u8 = 2;
const PONG: u8 = 3;
const GET_BLOCK_IDS: u8 = 4;
const BLOCK_IDS: u8 = 5;
const GET_BLOCKS: u8 = 6;
const BLOCKS: u8 = 7;
const NOT_FOUND: u8 = 8;
const NEW_BLOCK: u8 = 9;
const NEW_TX: u8 = 10;
const GET_TXS: u8 = 11;
const TXS: u8 = 12;
const GET_ADDRS: u8 = 13;
const ADDRS: u8 = 14;

/// The largest `length` (kind byte and body) each kind may declare; `None` for an unknown kind.
fn cap_of(kind: u8) -> Option<usize> {
    Some(match kind {
        HELLO => 1 + 4 + 32 + 8 + 32 + 32 + 8 + 8,
        PING | PONG => 1 + 8,
        GET_BLOCK_IDS => 1 + 4 + MAX_LOCATOR * 32,
        BLOCK_IDS => 1 + 8 + 4 + MAX_IDS * 32,
        GET_BLOCKS => 1 + 4 + MAX_BLOCKS * 32,
        BLOCKS | TXS => MAX_FRAME,
        NOT_FOUND => 1 + 4 + MAX_NOT_FOUND * 32,
        NEW_BLOCK => 1 + 32 + 8 + 32,
        NEW_TX | GET_TXS => 1 + 4 + MAX_TXS * 32,
        GET_ADDRS => 1,
        ADDRS => 1 + 4 + MAX_ADDRS * 26,
        _ => return None,
    })
}

fn kind_of(msg: &Message) -> u8 {
    match msg {
        Message::Hello(_) => HELLO,
        Message::Ping(_) => PING,
        Message::Pong(_) => PONG,
        Message::GetBlockIds { .. } => GET_BLOCK_IDS,
        Message::BlockIds { .. } => BLOCK_IDS,
        Message::GetBlocks { .. } => GET_BLOCKS,
        Message::Blocks { .. } => BLOCKS,
        Message::NotFound { .. } => NOT_FOUND,
        Message::NewBlock { .. } => NEW_BLOCK,
        Message::NewTx { .. } => NEW_TX,
        Message::GetTxs { .. } => GET_TXS,
        Message::Txs { .. } => TXS,
        Message::GetAddrs => GET_ADDRS,
        Message::Addrs { .. } => ADDRS,
    }
}

fn write_ids(w: &mut Writer, ids: &[[u8; 32]], max: usize) -> Result<(), WireError> {
    w.count(ids.len(), 0, max)?;
    for id in ids {
        w.raw(id);
    }
    Ok(())
}

fn read_ids(r: &mut Reader<'_>, max: usize) -> Result<Vec<[u8; 32]>, WireError> {
    let n = r.count(0, max)?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(r.array()?);
    }
    Ok(out)
}

/// A whole frame for `msg`. Refuses a message a decoder would refuse (a list over its cap, a frame over the
/// cap of its kind).
pub fn encode(msg: &Message) -> Result<Vec<u8>, WireError> {
    let mut w = Writer::new();
    match msg {
        Message::Hello(h) => {
            w.u32(h.version);
            w.raw(&h.chain_id);
            w.u64(h.tip_height);
            w.raw(&h.cumulative_work);
            w.raw(&h.tip_id);
            w.u64(h.pruned_below);
            w.u64(h.nonce);
        }
        Message::Ping(n) | Message::Pong(n) => w.u64(*n),
        Message::GetBlockIds { locator } => write_ids(&mut w, locator, MAX_LOCATOR)?,
        Message::BlockIds { first_height, ids } => {
            w.u64(*first_height);
            write_ids(&mut w, ids, MAX_IDS)?;
        }
        Message::GetBlocks { ids } => write_ids(&mut w, ids, MAX_BLOCKS)?,
        Message::NotFound { ids } => write_ids(&mut w, ids, MAX_NOT_FOUND)?,
        Message::NewTx { ids } | Message::GetTxs { ids } => write_ids(&mut w, ids, MAX_TXS)?,
        Message::Blocks { blocks } => {
            w.count(blocks.len(), 0, MAX_BLOCKS)?;
            for b in blocks {
                b.write(&mut w)?;
            }
        }
        Message::Txs { txs } => {
            w.count(txs.len(), 0, MAX_TXS)?;
            for t in txs {
                t.write(&mut w)?;
            }
        }
        Message::GetAddrs => {}
        Message::Addrs { addrs } => {
            w.count(addrs.len(), 0, MAX_ADDRS)?;
            for a in addrs {
                w.raw(&a.ip);
                w.u16(a.port);
                w.u64(a.last_seen);
            }
        }
        Message::NewBlock {
            id,
            height,
            cumulative_work,
        } => {
            w.raw(id);
            w.u64(*height);
            w.raw(cumulative_work);
        }
    }
    let body = w.into_bytes();
    let kind = kind_of(msg);
    let length = 1 + body.len();
    let cap = cap_of(kind).expect("every kind we encode has a cap");
    if length > cap {
        return Err(WireError::Encode(format!(
            "frame of {length} bytes is over the cap of {cap} for kind {kind}"
        )));
    }
    let mut frame = Vec::with_capacity(4 + length);
    frame.extend_from_slice(&(length as u32).to_le_bytes());
    frame.push(kind);
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// What a header of at least 4 or 5 bytes already says, before the body exists. `None` means "wait for more".
fn header_error(prefix: &[u8]) -> Option<WireError> {
    if prefix.len() < 4 {
        return None;
    }
    let length = u32::from_le_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]);
    if length == 0 {
        return Some(WireError::EmptyFrame);
    }
    if prefix.len() < 5 {
        return None;
    }
    let kind = prefix[4];
    match cap_of(kind) {
        None => Some(WireError::UnknownKind(kind)),
        Some(cap) if length as usize > cap => Some(WireError::FrameTooLarge { kind, length, cap }),
        Some(_) => None,
    }
}

/// The message in exactly one whole frame. The checks run in the order of `docs/WIRE_PROTOCOL.md` section 4.
pub fn decode_frame(data: &[u8]) -> Result<Message, WireError> {
    if data.len() < 4 {
        return Err(WireError::ShortFrame);
    }
    if let Some(e) = header_error(data) {
        return Err(e);
    }
    if data.len() < 5 {
        return Err(WireError::ShortFrame);
    }
    let length = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if data.len() < 4 + length {
        return Err(WireError::ShortFrame);
    }
    if data.len() > 4 + length {
        return Err(WireError::TrailingBytes);
    }
    decode_body(data[4], &data[5..4 + length])
}

fn decode_body(kind: u8, body: &[u8]) -> Result<Message, WireError> {
    let mut r = Reader::new(body);
    let msg = match kind {
        HELLO => Message::Hello(Hello {
            version: r.u32()?,
            chain_id: r.array()?,
            tip_height: r.u64()?,
            cumulative_work: r.array()?,
            tip_id: r.array()?,
            pruned_below: r.u64()?,
            nonce: r.u64()?,
        }),
        PING => Message::Ping(r.u64()?),
        PONG => Message::Pong(r.u64()?),
        GET_BLOCK_IDS => Message::GetBlockIds {
            locator: read_ids(&mut r, MAX_LOCATOR)?,
        },
        BLOCK_IDS => Message::BlockIds {
            first_height: r.u64()?,
            ids: read_ids(&mut r, MAX_IDS)?,
        },
        GET_BLOCKS => Message::GetBlocks {
            ids: read_ids(&mut r, MAX_BLOCKS)?,
        },
        BLOCKS => {
            let n = r.count(0, MAX_BLOCKS)?;
            let mut blocks = Vec::with_capacity(n);
            for _ in 0..n {
                blocks.push(Block::read(&mut r)?);
            }
            Message::Blocks { blocks }
        }
        NOT_FOUND => Message::NotFound {
            ids: read_ids(&mut r, MAX_NOT_FOUND)?,
        },
        NEW_BLOCK => Message::NewBlock {
            id: r.array()?,
            height: r.u64()?,
            cumulative_work: r.array()?,
        },
        NEW_TX => Message::NewTx {
            ids: read_ids(&mut r, MAX_TXS)?,
        },
        GET_TXS => Message::GetTxs {
            ids: read_ids(&mut r, MAX_TXS)?,
        },
        TXS => {
            let n = r.count(0, MAX_TXS)?;
            let mut txs = Vec::with_capacity(n);
            for _ in 0..n {
                txs.push(Transaction::read(&mut r)?);
            }
            Message::Txs { txs }
        }
        GET_ADDRS => Message::GetAddrs,
        ADDRS => {
            let n = r.count(0, MAX_ADDRS)?;
            let mut addrs = Vec::with_capacity(n);
            for _ in 0..n {
                addrs.push(PeerAddr {
                    ip: r.array()?,
                    port: r.u16()?,
                    last_seen: r.u64()?,
                });
            }
            Message::Addrs { addrs }
        }
        other => return Err(WireError::UnknownKind(other)),
    };
    r.finish()?;
    Ok(msg)
}

/// Turns a stream of bytes, arriving in chunks of any size, into messages.
///
/// Feed what the socket gave with [`FrameDecoder::push`], then call [`FrameDecoder::next_message`] until it
/// returns `Ok(None)`. The header is checked as soon as its 5 bytes exist, so the decoder never holds more than
/// one frame of the size its kind allows (plus what one `push` added). **After any error the connection must be
/// closed**: the decoder stays failed, because the stream cannot be resynchronised.
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    failed: Option<WireError>,
}

impl FrameDecoder {
    pub fn new() -> FrameDecoder {
        FrameDecoder::default()
    }

    /// Bytes waiting for the rest of their frame.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    pub fn push(&mut self, data: &[u8]) {
        if self.failed.is_none() {
            self.buf.extend_from_slice(data);
        }
    }

    pub fn next_message(&mut self) -> Result<Option<Message>, WireError> {
        if let Some(e) = &self.failed {
            return Err(e.clone());
        }
        if let Some(e) = header_error(&self.buf) {
            return Err(self.fail(e));
        }
        // (the header check above has waited for 5 bytes when it needed them; 4 are enough to read the length)
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let length =
            u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if self.buf.len() < 4 + length {
            return Ok(None);
        }
        let frame: Vec<u8> = self.buf.drain(..4 + length).collect();
        match decode_frame(&frame) {
            Ok(m) => Ok(Some(m)),
            Err(e) => Err(self.fail(e)),
        }
    }

    fn fail(&mut self, e: WireError) -> WireError {
        self.buf.clear();
        self.failed = Some(e.clone());
        e
    }
}
