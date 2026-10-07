//! The POOL protocol's messages: how a miner and a mining pool turn what they say into bytes. **A DRAFT, and only the
//! messages**: no pool and no pool miner exists yet. `docs/POOL_PROTOCOL.md` is the description,
//! `tests/vectors/pool.json` the golden vectors made by an independent Python reference
//! (`reference/tools/make_vectors_pool.py`).
//!
//! A frame is `length u32 little-endian | body`, the body `kind u8 | payload`, the length at most [`MAX_FRAME`]. A miner sends
//! kinds 1 to 5; a pool sends 0x80 and up; an error is 0xFF. Decoding is strict, as in the control protocol: an unknown kind (or a
//! kind of the other direction), a short payload, a trailing byte, a count out of range, a flag that is not 0 or 1, text that is not
//! UTF-8 or a value the protocol forbids (a share target of zero or all ones, a prefix that does not fit its bits, an
//! accepted share with a reason, a header that already carries a nonce) is an error, and one message has one encoding.
//! **Experimental and unaudited.**

use tenero_core::v2::codec::{DecodeError, EncodeError, Reader, Wire, Writer};
use tenero_core::v2::{BlockHeader, Coinbase, Transaction};
use tenero_node::Payout;

/// The biggest body a frame may carry.
pub const MAX_FRAME: usize = 4 * 1024 * 1024;
pub const MAX_NETWORK: usize = 32;
pub const MAX_ADDRESS: usize = 256;
pub const MAX_WORKER: usize = 32;
pub const MAX_AGENT: usize = 64;
pub const MAX_POOL_NAME: usize = 64;
pub const MAX_TEXT: usize = 128;
/// The most transaction ids in a declaration, and in a "missing" answer.
pub const MAX_TX_IDS: usize = 8192;
/// The most transactions one `provide_txs` carries.
pub const MAX_PROVIDED_TXS: usize = 64;
/// The longest a job may live, in seconds.
pub const MAX_TTL: u32 = 3600;
/// The most bits of the nonce a pool may fix for a miner.
pub const MAX_PREFIX_BITS: u8 = 32;
/// The length of a block header.
pub const HEADER_LEN: usize = 146;

pub const K_HELLO: u8 = 1;
pub const K_SUBMIT_SHARE: u8 = 2;
pub const K_PING: u8 = 3;
pub const K_DECLARE_JOB: u8 = 4;
pub const K_PROVIDE_TXS: u8 = 5;
pub const K_HELLO_OK: u8 = 0x81;
pub const K_SHARE_RESULT: u8 = 0x82;
pub const K_PONG: u8 = 0x83;
pub const K_DECLARE_RESULT: u8 = 0x84;
pub const K_JOB: u8 = 0x90;
pub const K_SET_SHARE_TARGET: u8 = 0x91;
pub const K_SET_PAYOUT: u8 = 0x92;
pub const K_ERROR: u8 = 0xFF;

/// The capability bit of job declaration.
pub const CAP_JOB_DECLARATION: u32 = 1;

/// Reasons a share is not accepted (0 is accepted).
pub const SHARE_REASONS: u8 = 6;
/// Reasons a declaration is refused (1 to this).
pub const DECLARE_REFUSED_REASONS: u8 = 7;

#[derive(Debug, PartialEq, Eq)]
pub enum PoolError {
    /// The frame's length is 0 or over [`MAX_FRAME`].
    BadLength(usize),
    UnknownKind(u8),
    Decode(DecodeError),
    /// A byte after the end of the message.
    Trailing,
    Encode(String),
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PoolError::BadLength(n) => write!(f, "a frame of {n} bytes is not allowed"),
            PoolError::UnknownKind(k) => write!(f, "unknown message kind {k}"),
            PoolError::Decode(e) => write!(f, "malformed message: {}", e.as_str()),
            PoolError::Trailing => write!(f, "bytes after the end of the message"),
            PoolError::Encode(e) => write!(f, "cannot encode: {e}"),
        }
    }
}

impl std::error::Error for PoolError {}

impl From<DecodeError> for PoolError {
    fn from(e: DecodeError) -> Self {
        PoolError::Decode(e)
    }
}

impl From<EncodeError> for PoolError {
    fn from(e: EncodeError) -> Self {
        PoolError::Encode(e.to_string())
    }
}

/// The first message of a connection, from the miner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub min_version: u16,
    pub max_version: u16,
    pub capabilities: u32,
    pub network: String,
    /// The miner's own wallet address: where the pool is to pay it.
    pub address: String,
    pub worker: String,
    pub agent: String,
}

/// A job the miner built on its own node (job declaration).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclareJob {
    pub decl_id: u64,
    pub height: u64,
    pub prev_id: [u8; 32],
    pub timestamp: u64,
    pub coinbase: Coinbase,
    pub tx_ids: Vec<[u8; 32]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MinerMessage {
    Hello(Hello),
    SubmitShare {
        job_id: u64,
        nonce: u64,
        mix: [u8; 64],
    },
    Ping {
        token: u64,
    },
    DeclareJob(DeclareJob),
    ProvideTxs {
        decl_id: u64,
        txs: Vec<Transaction>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelloOk {
    pub version: u16,
    pub capabilities: u32,
    pub session: u64,
    /// How many of the top bits of the 64-bit nonce are the pool's, for this miner.
    pub prefix_bits: u8,
    pub prefix: u64,
    pub share_target: [u8; 32],
    pub pool_name: String,
    /// Always true: the reward goes to the pool.
    pub pays_pool: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    pub job_id: u64,
    pub height: u64,
    /// Every earlier job is dead.
    pub clean: bool,
    /// The nonce and the mix are empty: they are the miner's to fill.
    pub header: BlockHeader,
    pub block_target: [u8; 32],
    pub ttl: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeclareOutcome {
    Accepted {
        job_id: u64,
        header: BlockHeader,
    },
    Refused {
        reason: u8,
        text: String,
    },
    /// The pool lacks these transactions: the miner sends them in `provide_txs`.
    Missing(Vec<[u8; 32]>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PoolMessage {
    HelloOk(HelloOk),
    ShareResult {
        job_id: u64,
        accepted: bool,
        reason: u8,
        text: String,
    },
    Pong {
        token: u64,
    },
    DeclareResult {
        decl_id: u64,
        outcome: DeclareOutcome,
    },
    Job(Job),
    SetShareTarget {
        share_target: [u8; 32],
    },
    /// Where the reward of a block at `height` goes: the pool's (a declared job must pay exactly this).
    SetPayout {
        height: u64,
        payout: Payout,
    },
    Error(String),
}

// ---- small pieces ----------------------------------------------------------------------------------------------

fn put_text(w: &mut Writer, s: &str, max: usize) -> Result<(), EncodeError> {
    w.var(s.as_bytes(), max)
}

fn read_text(r: &mut Reader<'_>, max: usize) -> Result<String, DecodeError> {
    String::from_utf8(r.var(max)?).map_err(|_| DecodeError::CountOutOfRange)
}

fn read_flag(r: &mut Reader<'_>) -> Result<bool, DecodeError> {
    match r.take(1)?[0] {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(DecodeError::CountOutOfRange),
    }
}

fn put_flag(w: &mut Writer, b: bool) {
    w.raw(&[u8::from(b)]);
}

/// A share target of zero can never be met and one of all ones makes every attempt a share: the protocol forbids both.
fn target_ok(t: &[u8; 32]) -> bool {
    t.iter().any(|b| *b != 0) && t.iter().any(|b| *b != 0xFF)
}

fn bad<T>() -> Result<T, DecodeError> {
    Err(DecodeError::CountOutOfRange)
}

fn malformed() -> PoolError {
    PoolError::Decode(DecodeError::CountOutOfRange)
}

fn put_header_empty(w: &mut Writer, h: &BlockHeader) -> Result<(), PoolError> {
    if h.nonce != 0 || h.mix != [0u8; 64] {
        return Err(PoolError::Encode(
            "a header handed out has a nonce or a mix".into(),
        ));
    }
    h.write(w)?;
    Ok(())
}

fn read_header_empty(r: &mut Reader<'_>) -> Result<BlockHeader, DecodeError> {
    let h = BlockHeader::read(r)?;
    if h.nonce != 0 || h.mix != [0u8; 64] {
        return bad();
    }
    Ok(h)
}

fn read_ids(r: &mut Reader<'_>, min: usize) -> Result<Vec<[u8; 32]>, DecodeError> {
    let n = r.count(min, MAX_TX_IDS)?;
    (0..n).map(|_| r.array()).collect()
}

fn finish(r: Reader<'_>) -> Result<(), PoolError> {
    r.finish().map_err(|_| PoolError::Trailing)
}

impl MinerMessage {
    pub fn kind(&self) -> u8 {
        match self {
            MinerMessage::Hello(_) => K_HELLO,
            MinerMessage::SubmitShare { .. } => K_SUBMIT_SHARE,
            MinerMessage::Ping { .. } => K_PING,
            MinerMessage::DeclareJob(_) => K_DECLARE_JOB,
            MinerMessage::ProvideTxs { .. } => K_PROVIDE_TXS,
        }
    }

    pub fn to_body(&self) -> Result<Vec<u8>, PoolError> {
        let mut w = Writer::new();
        w.raw(&[self.kind()]);
        match self {
            MinerMessage::Hello(h) => {
                if h.min_version == 0 || h.min_version > h.max_version {
                    return Err(PoolError::Encode(
                        "the versions are not 1 <= min <= max".into(),
                    ));
                }
                w.u16(h.min_version);
                w.u16(h.max_version);
                w.u32(h.capabilities);
                put_text(&mut w, &h.network, MAX_NETWORK)?;
                put_text(&mut w, &h.address, MAX_ADDRESS)?;
                put_text(&mut w, &h.worker, MAX_WORKER)?;
                put_text(&mut w, &h.agent, MAX_AGENT)?;
            }
            MinerMessage::SubmitShare { job_id, nonce, mix } => {
                w.u64(*job_id);
                w.u64(*nonce);
                w.raw(mix);
            }
            MinerMessage::Ping { token } => w.u64(*token),
            MinerMessage::DeclareJob(d) => {
                w.u64(d.decl_id);
                w.u64(d.height);
                w.raw(&d.prev_id);
                w.u64(d.timestamp);
                d.coinbase.write(&mut w)?;
                w.count(d.tx_ids.len(), 0, MAX_TX_IDS)?;
                for id in &d.tx_ids {
                    w.raw(id);
                }
            }
            MinerMessage::ProvideTxs { decl_id, txs } => {
                w.u64(*decl_id);
                w.count(txs.len(), 1, MAX_PROVIDED_TXS)?;
                for t in txs {
                    t.write(&mut w)?;
                }
            }
        }
        Ok(w.into_bytes())
    }

    pub fn from_body(body: &[u8]) -> Result<MinerMessage, PoolError> {
        let mut r = Reader::new(body);
        let kind = r.take(1).map_err(|_| PoolError::BadLength(0))?[0];
        let m = match kind {
            K_HELLO => {
                let (min_version, max_version, capabilities) = (r.u16()?, r.u16()?, r.u32()?);
                if min_version == 0 || min_version > max_version {
                    return Err(malformed());
                }
                MinerMessage::Hello(Hello {
                    min_version,
                    max_version,
                    capabilities,
                    network: read_text(&mut r, MAX_NETWORK)?,
                    address: read_text(&mut r, MAX_ADDRESS)?,
                    worker: read_text(&mut r, MAX_WORKER)?,
                    agent: read_text(&mut r, MAX_AGENT)?,
                })
            }
            K_SUBMIT_SHARE => MinerMessage::SubmitShare {
                job_id: r.u64()?,
                nonce: r.u64()?,
                mix: r.array()?,
            },
            K_PING => MinerMessage::Ping { token: r.u64()? },
            K_DECLARE_JOB => MinerMessage::DeclareJob(DeclareJob {
                decl_id: r.u64()?,
                height: r.u64()?,
                prev_id: r.array()?,
                timestamp: r.u64()?,
                coinbase: Coinbase::read(&mut r)?,
                tx_ids: read_ids(&mut r, 0)?,
            }),
            K_PROVIDE_TXS => {
                let decl_id = r.u64()?;
                let n = r.count(1, MAX_PROVIDED_TXS)?;
                let txs = (0..n)
                    .map(|_| Transaction::read(&mut r))
                    .collect::<Result<Vec<_>, _>>()?;
                MinerMessage::ProvideTxs { decl_id, txs }
            }
            other => return Err(PoolError::UnknownKind(other)),
        };
        finish(r)?;
        Ok(m)
    }
}

impl PoolMessage {
    pub fn kind(&self) -> u8 {
        match self {
            PoolMessage::HelloOk(_) => K_HELLO_OK,
            PoolMessage::ShareResult { .. } => K_SHARE_RESULT,
            PoolMessage::Pong { .. } => K_PONG,
            PoolMessage::DeclareResult { .. } => K_DECLARE_RESULT,
            PoolMessage::Job(_) => K_JOB,
            PoolMessage::SetShareTarget { .. } => K_SET_SHARE_TARGET,
            PoolMessage::SetPayout { .. } => K_SET_PAYOUT,
            PoolMessage::Error(_) => K_ERROR,
        }
    }

    pub fn to_body(&self) -> Result<Vec<u8>, PoolError> {
        let mut w = Writer::new();
        w.raw(&[self.kind()]);
        match self {
            PoolMessage::HelloOk(h) => {
                if h.version == 0
                    || h.prefix_bits > MAX_PREFIX_BITS
                    || h.prefix >> h.prefix_bits != 0
                    || !target_ok(&h.share_target)
                {
                    return Err(PoolError::Encode(
                        "hello_ok has a value out of range".into(),
                    ));
                }
                w.u16(h.version);
                w.u32(h.capabilities);
                w.u64(h.session);
                w.raw(&[h.prefix_bits]);
                w.u64(h.prefix);
                w.raw(&h.share_target);
                put_text(&mut w, &h.pool_name, MAX_POOL_NAME)?;
                put_flag(&mut w, h.pays_pool);
            }
            PoolMessage::ShareResult {
                job_id,
                accepted,
                reason,
                text,
            } => {
                if *accepted != (*reason == 0) || *reason > SHARE_REASONS {
                    return Err(PoolError::Encode(
                        "an accepted share has reason 0 and nothing else does".into(),
                    ));
                }
                w.u64(*job_id);
                put_flag(&mut w, *accepted);
                w.raw(&[*reason]);
                put_text(&mut w, text, MAX_TEXT)?;
            }
            PoolMessage::Pong { token } => w.u64(*token),
            PoolMessage::DeclareResult { decl_id, outcome } => {
                w.u64(*decl_id);
                match outcome {
                    DeclareOutcome::Accepted { job_id, header } => {
                        w.raw(&[0]);
                        w.u64(*job_id);
                        put_header_empty(&mut w, header)?;
                    }
                    DeclareOutcome::Refused { reason, text } => {
                        if *reason == 0 || *reason > DECLARE_REFUSED_REASONS {
                            return Err(PoolError::Encode("a refusal has a reason 1 to 7".into()));
                        }
                        w.raw(&[1, *reason]);
                        put_text(&mut w, text, MAX_TEXT)?;
                    }
                    DeclareOutcome::Missing(ids) => {
                        w.raw(&[2]);
                        w.count(ids.len(), 1, MAX_TX_IDS)?;
                        for id in ids {
                            w.raw(id);
                        }
                    }
                }
            }
            PoolMessage::Job(j) => {
                if j.ttl == 0 || j.ttl > MAX_TTL {
                    return Err(PoolError::Encode("a job lives 1 to 3600 seconds".into()));
                }
                w.u64(j.job_id);
                w.u64(j.height);
                put_flag(&mut w, j.clean);
                put_header_empty(&mut w, &j.header)?;
                w.raw(&j.block_target);
                w.u32(j.ttl);
            }
            PoolMessage::SetShareTarget { share_target } => {
                if !target_ok(share_target) {
                    return Err(PoolError::Encode(
                        "a share target of zero or all ones".into(),
                    ));
                }
                w.raw(share_target);
            }
            PoolMessage::SetPayout { height, payout } => {
                w.u64(*height);
                w.raw(&payout.onetime_address);
                w.raw(&payout.view_tag);
                w.raw(&payout.ephemeral_pubkey);
                w.raw(&payout.anchor_enc);
            }
            PoolMessage::Error(text) => put_text(&mut w, text, MAX_TEXT)?,
        }
        Ok(w.into_bytes())
    }

    pub fn from_body(body: &[u8]) -> Result<PoolMessage, PoolError> {
        let mut r = Reader::new(body);
        let kind = r.take(1).map_err(|_| PoolError::BadLength(0))?[0];
        let m = match kind {
            K_HELLO_OK => {
                let (version, capabilities, session) = (r.u16()?, r.u32()?, r.u64()?);
                let prefix_bits = r.take(1)?[0];
                let prefix = r.u64()?;
                let share_target: [u8; 32] = r.array()?;
                if version == 0
                    || prefix_bits > MAX_PREFIX_BITS
                    || prefix >> prefix_bits != 0
                    || !target_ok(&share_target)
                {
                    return Err(malformed());
                }
                PoolMessage::HelloOk(HelloOk {
                    version,
                    capabilities,
                    session,
                    prefix_bits,
                    prefix,
                    share_target,
                    pool_name: read_text(&mut r, MAX_POOL_NAME)?,
                    pays_pool: read_flag(&mut r)?,
                })
            }
            K_SHARE_RESULT => {
                let job_id = r.u64()?;
                let accepted = read_flag(&mut r)?;
                let reason = r.take(1)?[0];
                if reason > SHARE_REASONS || accepted != (reason == 0) {
                    return Err(malformed());
                }
                PoolMessage::ShareResult {
                    job_id,
                    accepted,
                    reason,
                    text: read_text(&mut r, MAX_TEXT)?,
                }
            }
            K_PONG => PoolMessage::Pong { token: r.u64()? },
            K_DECLARE_RESULT => {
                let decl_id = r.u64()?;
                let outcome = match r.take(1)?[0] {
                    0 => DeclareOutcome::Accepted {
                        job_id: r.u64()?,
                        header: read_header_empty(&mut r)?,
                    },
                    1 => {
                        let reason = r.take(1)?[0];
                        if reason == 0 || reason > DECLARE_REFUSED_REASONS {
                            return Err(malformed());
                        }
                        DeclareOutcome::Refused {
                            reason,
                            text: read_text(&mut r, MAX_TEXT)?,
                        }
                    }
                    2 => DeclareOutcome::Missing(read_ids(&mut r, 1)?),
                    _ => return Err(malformed()),
                };
                PoolMessage::DeclareResult { decl_id, outcome }
            }
            K_JOB => {
                let (job_id, height) = (r.u64()?, r.u64()?);
                let clean = read_flag(&mut r)?;
                let header = read_header_empty(&mut r)?;
                let block_target = r.array()?;
                let ttl = r.u32()?;
                if ttl == 0 || ttl > MAX_TTL {
                    return Err(malformed());
                }
                PoolMessage::Job(Job {
                    job_id,
                    height,
                    clean,
                    header,
                    block_target,
                    ttl,
                })
            }
            K_SET_SHARE_TARGET => {
                let share_target: [u8; 32] = r.array()?;
                if !target_ok(&share_target) {
                    return Err(malformed());
                }
                PoolMessage::SetShareTarget { share_target }
            }
            K_SET_PAYOUT => PoolMessage::SetPayout {
                height: r.u64()?,
                payout: Payout {
                    onetime_address: r.array()?,
                    view_tag: r.array()?,
                    ephemeral_pubkey: r.array()?,
                    anchor_enc: r.array()?,
                },
            },
            K_ERROR => PoolMessage::Error(read_text(&mut r, MAX_TEXT)?),
            other => return Err(PoolError::UnknownKind(other)),
        };
        finish(r)?;
        Ok(m)
    }
}

/// A frame: `length u32 little-endian | body`.
pub fn frame(body: &[u8]) -> Result<Vec<u8>, PoolError> {
    if body.is_empty() || body.len() > MAX_FRAME {
        return Err(PoolError::BadLength(body.len()));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    Ok(out)
}

/// The length a frame header announces, checked.
pub fn frame_len(header: [u8; 4]) -> Result<usize, PoolError> {
    let n = u32::from_le_bytes(header) as usize;
    if n == 0 || n > MAX_FRAME {
        return Err(PoolError::BadLength(n));
    }
    Ok(n)
}
