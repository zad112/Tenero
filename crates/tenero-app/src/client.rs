//! The other end of the control protocol: [`RemoteNode`] is a node in another process, reached on this machine, and
//! stands in for an in-process node wherever the wallet wants a [`ChainView`] and a [`Submitter`].

use std::io;
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use rand_core::{CryptoRng, RngCore};
use tenero_core::hash::hex_lower;
use tenero_core::v3::Transaction;
use tenero_wallet::{ChainView, Rules, ScanBlock, SpendPaths, Submitter};

use crate::control::{read_frame, write_frame, NodeInfo, Request, Response};

/// The file in a node's data directory that holds the cookie, as 64 lower-case hexadecimal digits.
pub const COOKIE_FILE: &str = "control.cookie";

/// Makes a fresh cookie, writes it to `path` (replacing any there), and returns it. The file gets the default
/// permissions of the data directory: **protecting that directory is the node operator's job.**
pub fn create_cookie(path: &Path, rng: &mut (impl RngCore + CryptoRng)) -> io::Result<[u8; 32]> {
    let mut cookie = [0u8; 32];
    rng.fill_bytes(&mut cookie);
    tenero_net::transport::write_atomic(path, hex_lower(&cookie).as_bytes())?;
    Ok(cookie)
}

pub fn read_cookie(path: &Path) -> Result<[u8; 32], String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let text = text.trim();
    if text.len() != 64 || !text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(format!("{} does not hold a cookie", path.display()));
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// What a node made of a block it was given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockVerdict {
    /// It is part of the node's chain.
    InChain([u8; 32]),
    /// It is valid and kept on a side branch: another block took its place first.
    LostRace([u8; 32]),
    /// The node would not take it (invalid), with its reason.
    Refused(String),
}

/// A connection to a node: a plain socket (the control interface, on this machine) or an encrypted one (the miner
/// service, on another machine).
trait Duplex: io::Read + io::Write + Send {}
impl<T: io::Read + io::Write + Send> Duplex for T {}

pub struct RemoteNode {
    stream: Mutex<Box<dyn Duplex>>,
}

impl RemoteNode {
    /// Connects and authenticates. Refuses an address that is not loopback: the cookie must never leave this
    /// machine.
    pub fn connect(addr: SocketAddr, cookie: &[u8; 32]) -> Result<RemoteNode, String> {
        if !addr.ip().is_loopback() {
            return Err(
                "the control interface is only reachable on this machine (127.0.0.1)".into(),
            );
        }
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
            .map_err(|e| format!("cannot reach the node at {addr}: {e} (is it running?)"))?;
        stream.set_nodelay(true).map_err(|e| e.to_string())?;
        stream
            .set_read_timeout(Some(Duration::from_secs(120)))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(Duration::from_secs(30)))
            .map_err(|e| e.to_string())?;
        let node = RemoteNode {
            stream: Mutex::new(Box::new(stream)),
        };
        match node.request(&Request::Auth { cookie: *cookie })? {
            Response::Authed => Ok(node),
            other => Err(format!("the node did not accept the cookie: {other:?}")),
        }
    }

    /// Connects to a node's **miner service** (`docs/REMOTE_MINING_PLAN.md`), which may be on another machine: an
    /// encrypted connection, with the pre-shared `key` if the operator set one. There is no cookie, and the service
    /// answers only `info`, `block_template` and `submit_block`.
    pub fn connect_miner_service(
        addr: SocketAddr,
        key: Option<&[u8; 32]>,
    ) -> Result<RemoteNode, String> {
        let stream = crate::miner_service::SecureStream::connect(addr, key)?;
        Ok(RemoteNode {
            stream: Mutex::new(Box::new(stream)),
        })
    }

    /// One request, one answer. An error answer from the node is an `Err` with its message.
    pub fn request(&self, req: &Request) -> Result<Response, String> {
        match self.request_raw(req)? {
            Response::Error(m) => Err(m),
            r => Ok(r),
        }
    }

    /// One request, one answer, with an error answer from the node returned as a `Response::Error` and `Err` only
    /// for trouble with the connection itself (so a caller can tell "the node said no" from "the node is gone").
    pub fn request_raw(&self, req: &Request) -> Result<Response, String> {
        let mut s = self.stream.lock().map_err(|_| "poisoned".to_string())?;
        let body = req.to_body().map_err(|e| e.to_string())?;
        write_frame(&mut *s, &body).map_err(|e| format!("lost the node: {e}"))?;
        let answer = read_frame(&mut *s).map_err(|e| format!("lost the node: {e}"))?;
        Response::from_body(&answer).map_err(|e| e.to_string())
    }

    /// Asks the node to run the full proof-of-work check of a header (see `Request::CheckPow`): `Ok(true)` if the mix is right.
    pub fn check_pow(
        &self,
        height: u64,
        header: &tenero_core::v3::BlockHeader,
    ) -> Result<bool, String> {
        match self.request(&Request::CheckPow {
            height,
            header: header.clone(),
        })? {
            Response::PowChecked(b) => Ok(b),
            other => Err(format!("unexpected answer: {other:?}")),
        }
    }

    pub fn info(&self) -> Result<NodeInfo, String> {
        match self.request(&Request::Info)? {
            Response::Info(i) => Ok(i),
            other => Err(format!("unexpected answer: {other:?}")),
        }
    }

    /// A block to mine, its reward paying the main address `to` (the node makes the output: see
    /// `Request::BlockTemplate`), with at most `max_weight` of transactions.
    pub fn block_template(
        &self,
        to: &tenero_wallet::Address,
        max_weight: u64,
    ) -> Result<crate::control::Template, String> {
        if to.kind != tenero_wallet::Kind::Main {
            return Err("a block reward is paid to a main address only (not a subaddress or an integrated address)".into());
        }
        match self.request(&Request::BlockTemplate {
            spend_pubkey: to.spend_pubkey,
            view_pubkey: to.view_pubkey,
            max_weight,
        })? {
            Response::Template(t) => Ok(t),
            other => Err(format!("unexpected answer: {other:?}")),
        }
    }

    /// Hands the node a mined block. `Err` means the connection failed; what the node made of the block is the
    /// [`BlockVerdict`].
    pub fn submit_block(&self, block: tenero_core::v3::Block) -> Result<BlockVerdict, String> {
        match self.request_raw(&Request::SubmitBlock(block))? {
            Response::BlockSubmitted { id, in_chain: true } => Ok(BlockVerdict::InChain(id)),
            Response::BlockSubmitted {
                id,
                in_chain: false,
            } => Ok(BlockVerdict::LostRace(id)),
            Response::Error(why) => Ok(BlockVerdict::Refused(why)),
            other => Err(format!("unexpected answer: {other:?}")),
        }
    }

    /// Up to `count` block summaries (1 to 64) from height `from` on: fewer at the tip.
    pub fn headers(
        &self,
        from: u64,
        count: u16,
    ) -> Result<Vec<crate::control::BlockSummary>, String> {
        match self.request(&Request::Headers { from, count })? {
            Response::Headers(v) => Ok(v),
            other => Err(format!("unexpected answer: {other:?}")),
        }
    }

    /// How many transactions the node's pool holds, and the best of them by fee rate.
    pub fn mempool(&self) -> Result<(u32, Vec<tenero_node::PoolEntry>), String> {
        match self.request(&Request::Mempool)? {
            Response::Mempool { total, txs } => Ok((total, txs)),
            other => Err(format!("unexpected answer: {other:?}")),
        }
    }

    /// The chain's numbers: the next target, the work, the reward and the emission.
    pub fn chain_stats(&self) -> Result<crate::control::ChainStats, String> {
        match self.request(&Request::ChainStats)? {
            Response::ChainStats(c) => Ok(c),
            other => Err(format!("unexpected answer: {other:?}")),
        }
    }

    /// Asks the node to shut down cleanly.
    pub fn stop(&self) -> Result<(), String> {
        match self.request(&Request::Stop)? {
            Response::Stopping => Ok(()),
            other => Err(format!("unexpected answer: {other:?}")),
        }
    }
}

fn unexpected<T>(r: Response) -> Result<T, String> {
    Err(format!("unexpected answer: {r:?}"))
}

impl ChainView for RemoteNode {
    fn tip(&self) -> Result<(u64, [u8; 32]), String> {
        match self.request(&Request::Tip)? {
            Response::Tip { height, id } => Ok((height, id)),
            r => unexpected(r),
        }
    }

    fn block(&self, height: u64) -> Result<Option<ScanBlock>, String> {
        match self.request(&Request::Block { height })? {
            Response::Block(b) => Ok(b),
            r => unexpected(r),
        }
    }

    fn blocks(&self, from: u64, max: u64) -> Result<Vec<ScanBlock>, String> {
        let count = max.clamp(1, u64::from(crate::control::MAX_BLOCKS_PER_REQUEST)) as u16;
        match self.request(&Request::Blocks { from, count })? {
            Response::Blocks(b) => Ok(b),
            r => unexpected(r),
        }
    }

    /// In pieces of [`MAX_SPEND_PATHS`](crate::control::MAX_SPEND_PATHS); every piece must come from the tree of one
    /// reference block, so when a block arrives between two pieces the whole request is made again (a few times at most).
    fn spend_paths(&self, global_indexes: &[u64]) -> Result<SpendPaths, String> {
        if global_indexes.is_empty() {
            return Err("no outputs to find paths for".into());
        }
        'again: for _ in 0..5 {
            let mut all: Option<SpendPaths> = None;
            for chunk in global_indexes.chunks(crate::control::MAX_SPEND_PATHS) {
                let piece = match self.request(&Request::SpendPaths {
                    indexes: chunk.to_vec(),
                })? {
                    Response::SpendPaths(p) if p.paths.len() == chunk.len() => p,
                    r => return unexpected(r),
                };
                match &mut all {
                    None => all = Some(piece),
                    Some(a)
                        if a.reference_height == piece.reference_height && a.tree == piece.tree =>
                    {
                        a.paths.extend(piece.paths)
                    }
                    Some(_) => continue 'again,
                }
            }
            return all.ok_or_else(|| "no answer".to_string());
        }
        Err("the node's tip kept moving while the paths were asked for: try again".into())
    }

    fn key_image_spent(&self, key_image: &[u8; 32]) -> Result<bool, String> {
        match self.request(&Request::KeyImageSpent {
            key_image: *key_image,
        })? {
            Response::Spent(b) => Ok(b),
            r => unexpected(r),
        }
    }

    fn key_images_spent(&self, key_images: &[[u8; 32]]) -> Result<Vec<bool>, String> {
        let mut out = Vec::with_capacity(key_images.len());
        for chunk in key_images.chunks(crate::control::MAX_KEY_IMAGES) {
            match self.request(&Request::KeyImagesSpent {
                key_images: chunk.to_vec(),
            })? {
                Response::SpentMany(flags) if flags.len() == chunk.len() => out.extend(flags),
                r => return unexpected(r),
            }
        }
        Ok(out)
    }
    fn rules(&self) -> Result<Rules, String> {
        match self.request(&Request::Rules)? {
            Response::Rules(r) => Ok(r),
            r => unexpected(r),
        }
    }
}

impl Submitter for RemoteNode {
    fn submit(&mut self, tx: Transaction) -> Result<(), String> {
        match self.request(&Request::SubmitTx(tx))? {
            Response::TxAccepted { .. } => Ok(()),
            r => unexpected(r),
        }
    }
}
