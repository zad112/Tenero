//! The control server: a listener on 127.0.0.1 and a [`Hooks`] that answers requests from inside the node's loop.
//!
//! * **Only this machine:** the listening address must be a loopback address (anything else is refused at start-up)
//!   and every accepted connection's peer address is checked again.
//! * **Only a program that can read the node's data directory:** the first message of a connection must be the
//!   contents of the cookie file (32 random bytes, made at start-up, readable by whoever can read the data
//!   directory). Anything else closes the connection. This keeps another user of the same computer, or a web page
//!   that tricks a browser into sending bytes to a local port, from reading the chain through the node or sending
//!   transactions.
//! * **The node's own thread does the work:** a connection's thread only reads and writes frames; every request
//!   goes through a bounded queue to [`ControlHook::poll`], which runs inside the node's loop and so needs no
//!   locks around the node.
//!
//! **What this does not do:** it is not encrypted (the bytes stay on this machine); it cannot tell two programs of
//! the same user apart; and anything holding the cookie may read the whole chain, submit transactions and ask the
//! node to stop. Experimental and unaudited.

use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tenero_core::v3::ids;
use tenero_net::transport::Hooks;
use tenero_net::{Engine, Event};
use tenero_wallet::ChainView;

use crate::control::{
    read_frame, scan_block_size, write_frame, BlockSummary, ChainStats, NodeInfo, NodeKind,
    Request, Response, Template, MAX_BLOCKS_BYTES, MAX_MEMPOOL_LIST,
};

/// Connections served at once; more are closed at once.
pub const MAX_CONNECTIONS: usize = 8;
/// Requests waiting for the node's loop; when full, the answer is "busy".
pub const QUEUE: usize = 64;
/// Requests the loop answers per poll (so a flood cannot starve the network code).
pub const PER_POLL: usize = 32;
/// The most transaction weight a block template carries, whatever a miner asks for: a quarter of the largest block
/// (`MAX_BLOCK_WEIGHT`), so that a template, whose bytes are at most four times its weight, and the mined block always
/// fit one control frame (16 MiB). The block limit stays below this until the chain's median passes 1.5 MiB; a miner in
/// the node's own process is not limited by it.
pub const MAX_TEMPLATE_WEIGHT: u64 = tenero_core::v3::rules::MAX_BLOCK_WEIGHT / 4;
/// A connection that has not authenticated in this long is closed.
pub const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
/// A connection silent for this long is closed.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(600);
/// How long a connection waits for the node's loop to answer.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(60);

/// One request waiting for the node's loop, and where its answer goes. (Shared with the miner service, which feeds the
/// same queue: `crate::miner_service`.)
pub(crate) struct Job {
    pub(crate) req: Request,
    pub(crate) reply: SyncSender<Response>,
}

/// What the answer to `Info` says that the engine does not know.
#[derive(Clone, Debug)]
pub struct Meta {
    pub kind: NodeKind,
    pub network: String,
    pub version: String,
}

/// Keeps the listener running; dropping it stops accepting (open connections end on their own).
pub struct ControlHandle {
    pub addr: SocketAddr,
    stop: Arc<AtomicBool>,
    active: Arc<AtomicUsize>,
    jobs: SyncSender<Job>,
}

impl ControlHandle {
    /// A way to put requests into the node's queue from another listener (the miner service).
    pub(crate) fn job_sender(&self) -> SyncSender<Job> {
        self.jobs.clone()
    }

    /// Connections being served now.
    pub fn connections(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }
}

impl Drop for ControlHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

pub struct ControlHook {
    rx: Receiver<Job>,
    shutdown: Arc<AtomicBool>,
    meta: Meta,
    blocks_bytes: usize,
    /// Transactions handed to the engine whose answer waits until the next poll shows whether the pool kept them.
    pending: Vec<([u8; 32], SyncSender<Response>)>,
    /// Blocks handed to the engine, waiting to be seen in the chain, on a side branch, or not at all.
    pending_blocks: Vec<([u8; 32], SyncSender<Response>)>,
    /// Requests answered, for the status line.
    pub answered: u64,
}

fn same(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The server's limits (the defaults are the constants above; tests use shorter ones).
#[derive(Clone, Copy, Debug)]
pub struct ControlConfig {
    pub max_connections: usize,
    pub auth_timeout: Duration,
    pub idle_timeout: Duration,
    /// The most bytes of blocks one `Blocks` answer carries (at least one block is always sent).
    pub blocks_bytes: usize,
}

impl Default for ControlConfig {
    fn default() -> ControlConfig {
        ControlConfig {
            max_connections: MAX_CONNECTIONS,
            auth_timeout: AUTH_TIMEOUT,
            idle_timeout: IDLE_TIMEOUT,
            blocks_bytes: MAX_BLOCKS_BYTES,
        }
    }
}

/// Starts listening with the default limits. `shutdown` is set when a client asks the node to stop.
pub fn start(
    listen: SocketAddr,
    cookie: [u8; 32],
    shutdown: Arc<AtomicBool>,
    meta: Meta,
) -> io::Result<(ControlHandle, ControlHook)> {
    start_with(listen, cookie, shutdown, meta, ControlConfig::default())
}

pub fn start_with(
    listen: SocketAddr,
    cookie: [u8; 32],
    shutdown: Arc<AtomicBool>,
    meta: Meta,
    cfg: ControlConfig,
) -> io::Result<(ControlHandle, ControlHook)> {
    if !listen.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the control interface only listens on a loopback address (127.0.0.1)",
        ));
    }
    let listener = TcpListener::bind(listen)?;
    listener.set_nonblocking(true)?;
    let addr = listener.local_addr()?;
    let (tx, rx) = sync_channel::<Job>(QUEUE);
    let stop = Arc::new(AtomicBool::new(false));
    let active = Arc::new(AtomicUsize::new(0));
    let jobs = tx.clone();
    {
        let (stop, active) = (Arc::clone(&stop), Arc::clone(&active));
        thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, peer)) => {
                        // belt and braces: the socket is bound to loopback, so this cannot fail
                        if !peer.ip().is_loopback() {
                            continue;
                        }
                        if active.fetch_add(1, Ordering::SeqCst) >= cfg.max_connections {
                            active.fetch_sub(1, Ordering::SeqCst);
                            continue;
                        }
                        let (tx, active) = (tx.clone(), Arc::clone(&active));
                        thread::spawn(move || {
                            let _ = serve(stream, cookie, &tx, cfg);
                            active.fetch_sub(1, Ordering::SeqCst);
                        });
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(100)),
                }
            }
        });
    }
    Ok((
        ControlHandle {
            addr,
            stop,
            active,
            jobs,
        },
        ControlHook {
            rx,
            shutdown,
            meta,
            blocks_bytes: cfg.blocks_bytes,
            pending: Vec::new(),
            pending_blocks: Vec::new(),
            answered: 0,
        },
    ))
}

fn serve(
    mut stream: TcpStream,
    cookie: [u8; 32],
    jobs: &SyncSender<Job>,
    cfg: ControlConfig,
) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(cfg.auth_timeout))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let mut authed = false;
    loop {
        let body = read_frame(&mut stream)?;
        let Ok(req) = Request::from_body(&body) else {
            // not a message: say so once and hang up
            let _ = write_frame(
                &mut stream,
                &Response::Error("malformed request".into())
                    .to_body()
                    .unwrap_or_default(),
            );
            return Ok(());
        };
        if !authed {
            match req {
                Request::Auth { cookie: given } if same(&given, &cookie) => {
                    authed = true;
                    stream.set_read_timeout(Some(cfg.idle_timeout))?;
                    write_frame(&mut stream, &Response::Authed.to_body().expect("encodes"))?;
                    continue;
                }
                // wrong cookie or no cookie: no explanation
                _ => return Ok(()),
            }
        }
        let response = match req {
            Request::Auth { .. } => Response::Error("already authenticated".into()),
            req => {
                let (reply, wait) = sync_channel(1);
                match jobs.try_send(Job { req, reply }) {
                    Ok(()) => match wait.recv_timeout(ANSWER_TIMEOUT) {
                        Ok(r) => r,
                        Err(RecvTimeoutError::Timeout) => {
                            Response::Error("the node did not answer in time".into())
                        }
                        Err(RecvTimeoutError::Disconnected) => return Ok(()),
                    },
                    Err(TrySendError::Full(_)) => Response::Error("the node is busy".into()),
                    Err(TrySendError::Disconnected(_)) => return Ok(()),
                }
            }
        };
        let body = match response.to_body() {
            Ok(b) => b,
            Err(e) => Response::Error(e.to_string()).to_body().expect("encodes"),
        };
        write_frame(&mut stream, &body)?;
    }
}

fn err(m: impl Into<String>) -> Response {
    Response::Error(m.into())
}

/// What an explorer shows of the block at `height`, or `None` above the tip. Reads the index record and the coinbase only,
/// never the transactions.
fn summary(store: &tenero_store::Store, height: u64) -> Result<Option<BlockSummary>, String> {
    let Some(index) = store.block_index(height).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let coinbase = store.coinbase(height).map_err(|e| e.to_string())?;
    let coinbase_total = coinbase.as_ref().map_or(0, |cb| {
        cb.outputs
            .iter()
            .fold(0u64, |acc, o| acc.saturating_add(o.amount))
    });
    Ok(Some(BlockSummary {
        height,
        id: index.block_id,
        timestamp: index.header.timestamp,
        target: index.target,
        cumulative_work: index.cumulative_work,
        weight: index.body_weight,
        tx_count: index.tx_count,
        coinbase_total,
    }))
}

/// What the loop did with a request. (One answer in flight at a time, so the size of the largest, a block template, is
/// of no consequence.)
#[allow(clippy::large_enum_variant)]
enum Outcome {
    Reply(Response),
    /// A transaction given to the engine: the answer waits for the next poll.
    HandedOver([u8; 32]),
    /// A block given to the engine: the answer waits for the next poll.
    BlockHandedOver([u8; 32]),
}

impl ControlHook {
    fn answer(
        &mut self,
        engine: &Engine<'_>,
        req: Request,
        now_ms: u64,
        events: &mut Vec<Event>,
    ) -> Outcome {
        let node = engine.node();
        Outcome::Reply(match req {
            Request::Auth { .. } => err("already authenticated"),
            Request::Tip => match ChainView::tip(node) {
                Ok((height, id)) => Response::Tip { height, id },
                Err(e) => err(e),
            },
            Request::Block { height } => match ChainView::block(node, height) {
                Ok(b) => Response::Block(b),
                Err(e) => err(e),
            },
            Request::Blocks { from, count } => {
                let mut out = Vec::new();
                let mut bytes = 0usize;
                for h in from..from.saturating_add(u64::from(count)) {
                    match ChainView::block(node, h) {
                        Ok(Some(b)) => {
                            bytes += scan_block_size(&b);
                            // always at least one block, so a client always makes progress
                            if !out.is_empty() && bytes > self.blocks_bytes {
                                break;
                            }
                            out.push(b);
                        }
                        Ok(None) => break,
                        Err(e) => return Outcome::Reply(err(e)),
                    }
                }
                Response::Blocks(out)
            }
            Request::SpendPaths { indexes } => match ChainView::spend_paths(node, &indexes) {
                Ok(p) => Response::SpendPaths(p),
                Err(e) => err(e),
            },
            Request::KeyImagesSpent { key_images } => {
                let mut flags = Vec::with_capacity(key_images.len());
                for k in &key_images {
                    match node.key_image_spent(k) {
                        Ok(b) => flags.push(b),
                        Err(e) => return Outcome::Reply(err(e)),
                    }
                }
                Response::SpentMany(flags)
            }
            Request::KeyImageSpent { key_image } => match node.key_image_spent(&key_image) {
                Ok(b) => Response::Spent(b),
                Err(e) => err(e),
            },
            Request::CheckPow { height, header } => match node.check_proof_of_work(&header, height)
            {
                Ok(b) => Response::PowChecked(b),
                Err(e) => err(e),
            },
            Request::Headers { from, count } => {
                let mut out = Vec::new();
                for h in from..from.saturating_add(u64::from(count)) {
                    match summary(node.store(), h) {
                        Ok(Some(b)) => out.push(b),
                        Ok(None) => break,
                        Err(e) => return Outcome::Reply(err(e)),
                    }
                }
                Response::Headers(out)
            }
            Request::Mempool => {
                let pool = node.pool();
                Response::Mempool {
                    total: u32::try_from(pool.len()).unwrap_or(u32::MAX),
                    txs: pool.listing(MAX_MEMPOOL_LIST),
                }
            }
            Request::ChainStats => {
                let next = match node.next_block() {
                    Ok(n) => n,
                    Err(e) => return Outcome::Reply(err(e.to_string())),
                };
                let p = node.params();
                let height = next.height.saturating_sub(1);
                Response::ChainStats(ChainStats {
                    height,
                    next_target: next.target.to_be_bytes(),
                    cumulative_work: next.cumulative_work.to_be_bytes(),
                    next_reward: next.reward,
                    // only past the end of a u64 of coins, which no schedule here comes near
                    emitted: p.emission.paid_through(height).unwrap_or(u64::MAX),
                    max_supply: p.emission.max_supply,
                    tail_reward: p.emission.tail_reward,
                    block_time: p.difficulty.block_time,
                })
            }
            Request::Rules => match ChainView::rules(node) {
                Ok(r) => Response::Rules(r),
                Err(e) => err(e),
            },
            Request::SubmitTx(tx) => {
                if let Err(e) = node.check_tx(&tx) {
                    return Outcome::Reply(err(e));
                }
                let id = match ids::tx_id(&tx) {
                    Ok(id) => id,
                    Err(e) => return Outcome::Reply(err(e.to_string())),
                };
                // checked against the tip: hand it to the engine (which keeps it and tells the peers); the answer
                // waits for the next poll, which shows whether the pool kept it
                events.push(Event::LocalTx(tx));
                return Outcome::HandedOver(id);
            }
            Request::Info => {
                let store = node.store();
                let (height, tip) = match store.tip() {
                    Ok(t) => t,
                    Err(e) => return Outcome::Reply(err(e.to_string())),
                };
                Response::Info(NodeInfo {
                    height,
                    tip_id: tip.block_id,
                    peers: engine.peer_count() as u32,
                    inbound: engine.inbound_count() as u32,
                    pruned_below: store.pruned_below().unwrap_or(0),
                    mempool_txs: node.pool().len() as u32,
                    syncing: engine.is_syncing(),
                    kind: self.meta.kind,
                    network: self.meta.network.clone(),
                    version: self.meta.version.clone(),
                })
            }
            Request::Stop => {
                self.shutdown.store(true, Ordering::SeqCst);
                Response::Stopping
            }
            Request::BlockTemplate {
                spend_pubkey,
                view_pubkey,
                max_weight,
            } => {
                // a node that is catching up has no tip worth building on
                if engine.is_syncing() {
                    return Outcome::Reply(err(
                        "the node is syncing: it has no block to build on yet",
                    ));
                }
                let next = match node.next_block() {
                    Ok(n) => n,
                    Err(e) => return Outcome::Reply(err(e.to_string())),
                };
                // the output is made once the node knows the amount, with a fresh anchor that goes back with the template
                // so that the miner can check the output is its own; keys that make no output (not points) leave a
                // placeholder, and the template is refused rather than handed to a miner to waste its work on
                let anchor = tenero_carrot::output::random_anchor(&mut rand_core::OsRng);
                let bad_keys = std::cell::Cell::new(false);
                let payout = |amount| {
                    tenero_wallet::coinbase_payout_to_keys(
                        &spend_pubkey,
                        &view_pubkey,
                        next.height,
                        amount,
                        &anchor,
                    )
                    .unwrap_or_else(|| {
                        bad_keys.set(true);
                        tenero_node::Payout {
                            onetime_address: [0; 32],
                            view_tag: [0; 3],
                            ephemeral_pubkey: [0; 32],
                            anchor_enc: [0; 16],
                        }
                    })
                };
                let made = node.block_template(
                    now_ms / 1000,
                    max_weight.min(MAX_TEMPLATE_WEIGHT),
                    &payout,
                );
                if bad_keys.get() {
                    return Outcome::Reply(err("the address's keys are not valid points"));
                }
                match made {
                    Ok(block) => Response::Template(Template {
                        block,
                        anchor,
                        height: next.height,
                        target: next.target.to_be_bytes(),
                    }),
                    Err(e) => err(e.to_string()),
                }
            }
            Request::SubmitBlock(block) => {
                // handed to the engine as a local block (it validates it fully and tells the peers); the answer
                // waits for the next look, which shows what became of it
                let id = ids::block_id(&block.header, node.store().pow());
                events.push(Event::LocalBlock(block));
                return Outcome::BlockHandedOver(id);
            }
        })
    }
}

impl Hooks for ControlHook {
    fn poll(&mut self, engine: &mut Engine<'_>, now_ms: u64) -> Vec<Event> {
        // the blocks handed over at the last poll: what became of each?
        for (id, reply) in std::mem::take(&mut self.pending_blocks) {
            let r = if matches!(engine.node().store().height_of(&id), Ok(Some(_))) {
                Response::BlockSubmitted { id, in_chain: true }
            } else if engine.node().chain().holds_block(&id) {
                Response::BlockSubmitted {
                    id,
                    in_chain: false,
                }
            } else {
                err("the node refused the block (it is not valid)")
            };
            let _ = reply.try_send(r);
            self.answered += 1;
        }
        // the transactions handed over at the last poll: did the pool keep them?
        for (id, reply) in std::mem::take(&mut self.pending) {
            let r = if engine.node().pool().contains(&id) {
                Response::TxAccepted { id }
            } else {
                err("the node did not keep the transaction (its pool refused it)")
            };
            let _ = reply.try_send(r);
            self.answered += 1;
        }
        let mut events = Vec::new();
        for _ in 0..PER_POLL {
            let Ok(job) = self.rx.try_recv() else { break };
            match self.answer(engine, job.req, now_ms, &mut events) {
                Outcome::Reply(r) => {
                    let _ = job.reply.try_send(r);
                    self.answered += 1;
                }
                Outcome::HandedOver(id) => self.pending.push((id, job.reply)),
                Outcome::BlockHandedOver(id) => self.pending_blocks.push((id, job.reply)),
            }
        }
        events
    }
}
