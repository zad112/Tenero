//! A handler that takes a long time must not make the NEXT request look old. (Found on a slow CI runner: the engine stamped a
//! request with the time its handler started; applying a batch of blocks took longer than the request timeout; the next
//! tick gave up on a request that had only just been sent, forgot it, and then punished the honest peer for the answer.
//! On a slow machine, real sync would have done the same to honest peers.) No real clock here: the test says when each event
//! happens, which is what a transport does after a slow handler.

use std::sync::OnceLock;

use tenero_chain::{ProofsNotChecked, Sha256Pow};
use tenero_core::u256::U256;
use tenero_core::v2::Block;
use tenero_net::sim::{Sim, SimConfig, SimRig};
use tenero_net::{Action, Engine, EngineConfig, Event, Hello, Message, PROTOCOL_VERSION};
use tenero_node::{Node, NodeConfig};

const T0: u64 = 1_700_000_000;
const CHAIN: usize = 60;

struct Chain {
    ids: Vec<[u8; 32]>,
    blocks: Vec<Block>,
    work: [u8; 32],
    start_ms: u64,
}

fn chain() -> &'static Chain {
    static C: OnceLock<Chain> = OnceLock::new();
    C.get_or_init(|| {
        let rigs = SimRig::rigs("slow-fixture", 1);
        let mut sim = Sim::new(&rigs, T0, SimConfig::default(), EngineConfig::default());
        sim.mine_chain(0, CHAIN);
        let store = &rigs[0].store;
        let mut ids = vec![store.block_index(0).unwrap().unwrap().block_id];
        let mut blocks = vec![];
        let mut work = [0u8; 32];
        for h in 1..=CHAIN as u64 {
            let i = store.block_index(h).unwrap().unwrap();
            ids.push(i.block_id);
            work = i.cumulative_work;
            blocks.push(store.get_block(h).unwrap().unwrap().into_full().unwrap());
        }
        Chain {
            ids,
            blocks,
            work,
            start_ms: (T0 + CHAIN as u64 * 60 + 600) * 1000,
        }
    })
}

fn engine_on(rig: &SimRig, cfg: EngineConfig) -> Engine<'_> {
    let node = Node::with_proof_check(
        &rig.store,
        &rig.params,
        &Sha256Pow,
        &ProofsNotChecked,
        NodeConfig {
            allow_unchecked_proofs_for_tests: true,
            ..NodeConfig::default()
        },
    )
    .expect("a test node");
    Engine::new(node, cfg)
}

fn sent(actions: &[Action]) -> Vec<Message> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Send { msg, .. } => Some(msg.clone()),
            _ => None,
        })
        .collect()
}

/// The reply an honest peer with the whole chain gives to `req`.
fn answer(req: &Message) -> Option<Message> {
    let c = chain();
    let height = |id: &[u8; 32]| c.ids.iter().position(|x| x == id);
    match req {
        Message::GetBlockIds { locator } => {
            let from = locator.iter().find_map(height).unwrap_or(0);
            Some(Message::BlockIds {
                first_height: from as u64 + 1,
                ids: c.ids[from + 1..].to_vec(),
            })
        }
        Message::GetBlocks { ids } => Some(Message::Blocks {
            blocks: ids
                .iter()
                .filter_map(height)
                .filter(|h| *h > 0)
                .map(|h| c.blocks[h - 1].clone())
                .collect(),
        }),
        _ => None,
    }
}

/// Runs a sync of the whole chain against an honest peer. Every event after the first batch of blocks arrives `lag_ms` later
/// than it would have, as if applying that batch had taken that long. Returns the engine's tip height and the peer's score.
fn sync(lag_ms: u64, rig_tag: &str) -> (u64, Option<u32>, u64, Option<bool>) {
    let c = chain();
    let rig = SimRig::new(rig_tag, 0);
    let cfg = EngineConfig {
        nonce: 5,
        request_timeout_ms: 200,
        ..EngineConfig::default()
    };
    let mut e = engine_on(&rig, cfg);
    let mut now = c.start_ms;
    e.handle(
        now,
        Event::PeerConnected {
            peer: 1,
            addr: "20.1.1.1:8333".into(),
            inbound: false,
        },
    );
    now += 5;
    let hello = Message::Hello(Hello {
        version: PROTOCOL_VERSION,
        chain_id: rig.store.chain_id(),
        tip_height: CHAIN as u64,
        cumulative_work: c.work,
        tip_id: c.ids[CHAIN],
        pruned_below: 0,
        nonce: 77,
    });
    let mut pending: Vec<Message> = sent(&e.handle(
        now,
        Event::Message {
            peer: 1,
            msg: hello,
        },
    ));
    let mut slow_batches = 0;
    let mut syncing_after_lag = None;
    for _ in 0..40 {
        let Some(req) = pending.iter().find(|m| answer(m).is_some()).cloned() else {
            // nothing to answer: a tick, as a quiet moment
            now += 50;
            pending = sent(&e.handle(now, Event::Tick));
            if pending.iter().all(|m| answer(m).is_none())
                && e.node().store().tip().unwrap().0 as usize == CHAIN
            {
                break;
            }
            continue;
        };
        pending.retain(|m| *m != req);
        now += 10;
        let reply = answer(&req).unwrap();
        let is_blocks = matches!(reply, Message::Blocks { .. });
        let out = e.handle(
            now,
            Event::Message {
                peer: 1,
                msg: reply,
            },
        );
        pending.extend(sent(&out));
        if is_blocks && slow_batches == 0 {
            // the handler for the first batch of blocks took `lag_ms`: the next event comes that much later
            slow_batches += 1;
            now += lag_ms;
            pending.extend(sent(&e.handle(now, Event::Tick)));
            syncing_after_lag = Some(e.is_syncing());
        }
    }
    let tip = e.node().store().tip().unwrap().0;
    (
        tip,
        e.peer_score(1),
        e.stats.late_replies_forgiven,
        syncing_after_lag,
    )
}

#[test]
fn a_handler_that_takes_longer_than_the_request_timeout_does_not_make_the_next_request_time_out() {
    // the first batch of blocks takes 1,500 ms to apply (more than four request timeouts, after which a late block is no longer welcome)
    let (tip, score, _, syncing) = sync(1500, "slow-a");
    assert_eq!(
        syncing,
        Some(true),
        "the sync was given up on, for a request that had only just been sent"
    );
    assert_eq!(
        tip, CHAIN as u64,
        "the sync did not finish (the honest peer's answer was treated as unasked-for)"
    );
    assert_eq!(score, Some(0), "the honest peer was punished");
}

#[test]
fn with_no_delay_the_same_sync_finishes_as_before() {
    let (tip, score, forgiven, syncing) = sync(0, "slow-b");
    assert_eq!((tip, score, forgiven), (CHAIN as u64, Some(0), 0));
    assert_eq!(syncing, Some(true));
}

#[test]
fn a_request_that_is_never_answered_still_times_out_however_many_events_pass() {
    // the restart of a request's clock happens once, not on every event: a silent peer is still given up on
    let c = chain();
    let rig = SimRig::new("slow-c", 0);
    let mut e = engine_on(
        &rig,
        EngineConfig {
            nonce: 5,
            request_timeout_ms: 200,
            max_timeouts: 1,
            ..EngineConfig::default()
        },
    );
    let mut now = c.start_ms;
    e.handle(
        now,
        Event::PeerConnected {
            peer: 1,
            addr: "20.1.1.1:8333".into(),
            inbound: false,
        },
    );
    let hello = Message::Hello(Hello {
        version: PROTOCOL_VERSION,
        chain_id: rig.store.chain_id(),
        tip_height: CHAIN as u64,
        cumulative_work: U256::from_be_bytes(&c.work).to_be_bytes(),
        tip_id: c.ids[CHAIN],
        pruned_below: 0,
        nonce: 77,
    });
    now += 5;
    let asked = sent(&e.handle(
        now,
        Event::Message {
            peer: 1,
            msg: hello,
        },
    ));
    assert!(
        asked
            .iter()
            .any(|m| matches!(m, Message::GetBlockIds { .. })),
        "it asked for the block ids"
    );
    // the peer never answers: ticks every 50 ms for two seconds; it must be given up on well before that
    let mut dropped = false;
    for _ in 0..40 {
        now += 50;
        let out = e.handle(now, Event::Tick);
        if out
            .iter()
            .any(|a| matches!(a, Action::Disconnect { peer: 1, .. }))
            || !e.has_peer(1)
        {
            dropped = true;
            break;
        }
    }
    assert!(
        dropped,
        "a request that is never answered was waited for ever (the clock keeps being restarted)"
    );
}
