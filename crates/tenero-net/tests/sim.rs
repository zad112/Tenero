//! The protocol engine on the simulated network. Real engines, real stores, really mined and validated blocks;
//! hostile peers are scripted endpoints that can send anything.

use tenero_core::fees;
use tenero_core::hash::sha256;
use tenero_core::u256::U256;
use tenero_core::v2::ids::{block_id, block_tx_root, PowKind};
use tenero_core::v2::*;
use tenero_net::sim::{Sim, SimConfig, SimRig};
use tenero_net::{EngineConfig, Hello, Message, PROTOCOL_VERSION};
use tenero_node::Payout;

const T0: u64 = 1_700_000_000;
const SEC: u64 = 1000;

fn new_sim(rigs: &[SimRig]) -> Sim<'_> {
    Sim::new(rigs, T0, SimConfig::default(), EngineConfig::default())
}

fn all(n: usize) -> Vec<usize> {
    (0..n).collect()
}

/// A valid handshake message from a hostile peer, claiming the given tip.
fn hello_for(rig: &SimRig, height: u64, work: U256) -> Message {
    Message::Hello(Hello {
        version: PROTOCOL_VERSION,
        chain_id: rig.store.chain_id(),
        tip_height: height,
        cumulative_work: work.to_be_bytes(),
        tip_id: [9; 32],
        pruned_below: 0,
        nonce: 0,
    })
}

fn zero_work() -> U256 {
    U256::from_be_bytes(&[0; 32])
}

fn huge_work() -> U256 {
    U256::pow2(200).unwrap()
}

/// A block on `node`'s tip that breaks exactly one rule (the coinbase pays a unit too much), correctly mined.
fn forged_block(sim: &mut Sim<'_>, node: usize) -> Block {
    let ts = sim.now_ms() / 1000;
    let eng = &sim.engines[node];
    let mut b = eng
        .node()
        .block_template(
            ts,
            1_000_000,
            Payout {
                onetime_address: [3; 32],
                view_tag: [4; 3],
                ephemeral_pubkey: [5; 32],
                anchor_enc: [6; 16],
            },
        )
        .unwrap();
    b.coinbase.outputs[0].amount += 1;
    b.header.tx_root = block_tx_root(&b.coinbase, &b.transactions).unwrap();
    let target = eng.node().next_block().unwrap().target;
    for nonce in 0.. {
        b.header.nonce = nonce;
        if U256::from_be_bytes(&block_id(&b.header, PowKind::Sha256)) < target {
            break;
        }
    }
    b
}

/// A transaction valid at `node`'s tip (ring of the first two outputs), paying the minimum plus `fee_delta`.
fn make_tx(sim: &Sim<'_>, node: usize, image: u64, fee_delta: i64) -> Transaction {
    let next = sim.engines[node].node().next_block().unwrap();
    let out = |n: u8| Output {
        onetime_address: sha256(&[b"addr", &image.to_le_bytes(), &[n]]),
        amount_commitment: sha256(&[b"commit", &image.to_le_bytes(), &[n]]),
        amount_enc: [n; 8],
        view_tag: [n; 3],
        ephemeral_pubkey: sha256(&[b"eph", &image.to_le_bytes(), &[n]]),
        anchor_enc: [n; 16],
    };
    let mut t = Transaction {
        prefix: TxPrefix {
            version: VERSION,
            inputs: vec![Input {
                key_image: sha256(&[b"key image", &image.to_le_bytes()]),
            }],
            outputs: vec![out(1), out(2)],
            fee: 0,
            extra: vec![],
        },
        prunable: Prunable {
            rings: vec![vec![0, 1]],
            proof_data: vec![7; 200],
        },
    };
    let size = t.to_bytes().unwrap().len() as u64;
    let min = fees::dynamic_min_fee(size, next.reward, next.median).unwrap();
    t.prefix.fee = u64::try_from(i64::try_from(min).unwrap() + fee_delta).unwrap();
    t
}

// ---- handshake and junk -----------------------------------------------------------------------

#[test]
fn two_nodes_shake_hands() {
    let rigs = SimRig::rigs("hs", 2);
    let mut sim = new_sim(&rigs);
    assert!(sim.connect(0, 1));
    sim.run_for(2 * SEC);
    assert_eq!(sim.engines[0].ready_peer_count(), 1);
    assert_eq!(sim.engines[1].ready_peer_count(), 1);
}

#[test]
fn a_peer_on_another_chain_is_dropped_and_banned_a_wrong_version_only_dropped() {
    let rigs = SimRig::rigs("chain", 1);
    let mut sim = new_sim(&rigs);
    let h = sim.add_hostile(0, "other-chain");
    let Message::Hello(mut hello) = hello_for(&rigs[0], 0, zero_work()) else {
        unreachable!()
    };
    hello.chain_id = [0xab; 32];
    sim.hostile_send(h, Message::Hello(hello));
    sim.run_for(2 * SEC);
    assert!(sim.hostiles[h].disconnected);
    let now = sim.now_ms();
    assert!(sim.engines[0].is_banned("other-chain", now));

    let h2 = sim.add_hostile(0, "old-version");
    let Message::Hello(mut hello) = hello_for(&rigs[0], 0, zero_work()) else {
        unreachable!()
    };
    hello.version = PROTOCOL_VERSION + 1;
    sim.hostile_send(h2, Message::Hello(hello));
    sim.run_for(2 * SEC);
    assert!(sim.hostiles[h2].disconnected);
    let now = sim.now_ms();
    assert!(!sim.engines[0].is_banned("old-version", now));
}

#[test]
fn messages_before_hello_and_a_second_hello_are_punished_until_the_peer_is_dropped() {
    let rigs = SimRig::rigs("junk1", 1);
    let mut sim = new_sim(&rigs);
    let h = sim.add_hostile(0, "junk-1");
    sim.hostile_send(h, Message::Ping(1));
    sim.hostile_send(h, Message::Ping(2));
    sim.run_for(2 * SEC);
    assert!(
        sim.hostiles[h].disconnected,
        "two messages before hello: 50 + 50"
    );
    let now = sim.now_ms();
    assert!(sim.engines[0].is_banned("junk-1", now));

    let h2 = sim.add_hostile(0, "junk-2");
    sim.hostile_send(h2, hello_for(&rigs[0], 0, zero_work()));
    sim.hostile_send(h2, hello_for(&rigs[0], 0, zero_work()));
    sim.run_for(SEC);
    assert!(
        !sim.hostiles[h2].disconnected,
        "one second hello is 50 points"
    );
    sim.hostile_send(h2, hello_for(&rigs[0], 0, zero_work()));
    sim.run_for(SEC);
    assert!(sim.hostiles[h2].disconnected);
}

#[test]
fn oversized_lists_are_protocol_violations() {
    let rigs = SimRig::rigs("junk2", 1);
    let mut sim = new_sim(&rigs);
    let lim = EngineConfig::default().limits;
    let cases: Vec<(&str, Message)> = vec![
        (
            "locator",
            Message::GetBlockIds {
                locator: vec![[1; 32]; lim.max_locator + 1],
            },
        ),
        ("empty locator", Message::GetBlockIds { locator: vec![] }),
        (
            "get blocks",
            Message::GetBlocks {
                ids: vec![[1; 32]; lim.max_blocks + 1],
            },
        ),
        (
            "block ids",
            Message::BlockIds {
                first_height: 1,
                ids: vec![[1; 32]; lim.max_ids + 1],
            },
        ),
        (
            "announce",
            Message::NewTx {
                ids: vec![[1; 32]; lim.max_txs + 1],
            },
        ),
        (
            "get txs",
            Message::GetTxs {
                ids: vec![[1; 32]; lim.max_txs + 1],
            },
        ),
    ];
    for (i, (name, msg)) in cases.into_iter().enumerate() {
        let h = sim.add_hostile(0, &format!("big-{i}"));
        sim.hostile_send(h, hello_for(&rigs[0], 0, zero_work()));
        sim.hostile_send(h, msg.clone());
        sim.hostile_send(h, msg);
        sim.run_for(3 * SEC);
        assert!(
            sim.hostiles[h].disconnected,
            "{name}: two violations of 50 points each should drop the peer"
        );
    }
}

#[test]
fn a_flood_is_rate_limited() {
    let rigs = SimRig::rigs("flood", 1);
    let mut sim = new_sim(&rigs);
    let h = sim.add_hostile(0, "flooder");
    sim.hostile_send(h, hello_for(&rigs[0], 0, zero_work()));
    sim.run_for(SEC);
    for i in 0..600u64 {
        sim.hostile_send(h, Message::Ping(i));
    }
    sim.run_for(10 * SEC);
    assert!(sim.hostiles[h].disconnected);
    let now = sim.now_ms();
    assert!(sim.engines[0].is_banned("flooder", now));
}

#[test]
fn an_ordinary_burst_below_the_limit_is_fine() {
    let rigs = SimRig::rigs("burst", 1);
    let mut sim = new_sim(&rigs);
    let h = sim.add_hostile(0, "busy");
    sim.hostile_send(h, hello_for(&rigs[0], 0, zero_work()));
    for i in 0..150u64 {
        sim.hostile_send(h, Message::Ping(i));
    }
    sim.run_for(10 * SEC);
    assert!(!sim.hostiles[h].disconnected);
}

#[test]
fn silent_and_slow_peers_are_dropped() {
    let rigs = SimRig::rigs("silent", 1);
    let mut sim = new_sim(&rigs);
    // never says hello
    let a = sim.add_hostile(0, "mute");
    sim.run_for(9 * SEC);
    assert!(!sim.hostiles[a].disconnected);
    sim.run_for(3 * SEC);
    assert!(sim.hostiles[a].disconnected, "handshake timeout");
    // says hello, then nothing: pinged after a minute, dropped 30 s after an unanswered ping
    let b = sim.add_hostile(0, "quiet");
    sim.hostile_send(b, hello_for(&rigs[0], 0, zero_work()));
    sim.run_for(50 * SEC);
    assert!(!sim.hostiles[b].disconnected);
    sim.run_for(15 * SEC);
    assert!(
        sim.hostiles[b]
            .inbox
            .iter()
            .any(|m| matches!(m, Message::Ping(_))),
        "the node should have pinged"
    );
    // pinged again and again, never scored, and dropped only after five unanswered pings in a row
    sim.run_for(100 * SEC);
    assert!(
        !sim.hostiles[b].disconnected,
        "one lost ping must not cost the connection"
    );
    let pings = sim.hostiles[b]
        .inbox
        .iter()
        .filter(|m| matches!(m, Message::Ping(_)))
        .count();
    assert!(pings >= 3, "{pings} pings so far");
    sim.run_for(150 * SEC);
    assert!(sim.hostiles[b].disconnected, "ping timeout");
    let now = sim.now_ms();
    assert!(
        !sim.engines[0].is_banned("quiet", now),
        "slow is not hostile: dropped, not banned"
    );
}

#[test]
fn a_peer_that_answers_pings_is_kept() {
    let rigs = SimRig::rigs("alive", 2);
    let mut sim = new_sim(&rigs);
    sim.connect(0, 1);
    sim.run_for(600 * SEC);
    assert_eq!(sim.engines[0].ready_peer_count(), 1);
    assert_eq!(sim.engines[1].ready_peer_count(), 1);
}

#[test]
fn unsolicited_blocks_are_punished() {
    let rigs = SimRig::rigs("unsol", 2);
    let mut sim = new_sim(&rigs);
    let b = sim.mine(1, None);
    let h = sim.add_hostile(0, "spammer");
    sim.hostile_send(h, hello_for(&rigs[0], 0, zero_work()));
    for _ in 0..4 {
        sim.hostile_send(
            h,
            Message::Blocks {
                blocks: vec![b.clone()],
            },
        );
    }
    sim.run_for(3 * SEC);
    assert!(!sim.hostiles[h].disconnected, "4 x 20 points");
    sim.hostile_send(h, Message::Blocks { blocks: vec![b] });
    sim.run_for(3 * SEC);
    assert!(sim.hostiles[h].disconnected, "the fifth reaches 100");
    assert_eq!(sim.tip(0).0, 0, "an unrequested block is never applied");
}

#[test]
fn an_invalid_block_bans_the_peer_at_once_and_the_ban_holds() {
    let rigs = SimRig::rigs("badblock", 3);
    let mut sim = new_sim(&rigs);
    sim.connect(0, 1);
    sim.connect(0, 2);
    let bad = forged_block(&mut sim, 1);
    let bad_id = block_id(&bad.header, PowKind::Sha256);
    let h = sim.add_hostile(0, "evil");
    sim.hostile_send(h, hello_for(&rigs[0], 1, huge_work()));
    sim.hostile_send(
        h,
        Message::NewBlock {
            id: bad_id,
            height: 1,
            cumulative_work: huge_work().to_be_bytes(),
        },
    );
    sim.run_for(2 * SEC);
    // the node asked the announcer for the block, and only it
    assert!(sim.hostiles[h]
        .inbox
        .iter()
        .any(|m| matches!(m, Message::GetBlocks { ids } if ids == &vec![bad_id])));
    sim.hostile_send(h, Message::Blocks { blocks: vec![bad] });
    sim.run_for(2 * SEC);
    assert!(sim.hostiles[h].disconnected);
    let now = sim.now_ms();
    assert!(sim.engines[0].is_banned("evil", now));
    assert_eq!(sim.engines[0].stats.bans, 1);
    assert_eq!(sim.tip(0).0, 0, "the invalid block was not applied");
    // the honest peers are untouched, and the banned address is refused at once
    assert_eq!(sim.engines[0].ready_peer_count(), 2);
    sim.hostile_connect(h);
    sim.run_for(SEC);
    assert!(
        sim.hostiles[h].disconnected,
        "a banned address cannot reconnect"
    );
}

#[test]
fn a_peer_claiming_work_it_cannot_show_stops_being_believed() {
    let rigs = SimRig::rigs("liar", 1);
    let mut sim = new_sim(&rigs);
    let h = sim.add_hostile(0, "liar");
    sim.hostile_send(h, hello_for(&rigs[0], 500, huge_work()));
    sim.run_for(SEC);
    // it is asked for block ids; it answers with none
    assert!(sim.hostiles[h]
        .inbox
        .iter()
        .any(|m| matches!(m, Message::GetBlockIds { .. })));
    sim.hostile_send(
        h,
        Message::BlockIds {
            first_height: 1,
            ids: vec![],
        },
    );
    sim.run_for(120 * SEC);
    assert!(!sim.engines[0].is_syncing());
    assert_eq!(
        score(&sim, h),
        Some(10),
        "10 points for claiming work it cannot show"
    );
    let asks = sim.hostiles[h]
        .inbox
        .iter()
        .filter(|m| matches!(m, Message::GetBlockIds { .. }))
        .count();
    assert_eq!(asks, 1, "it must not be asked again after showing nothing");
}

#[test]
fn a_peer_offering_blocks_it_cannot_serve_is_left_alone_for_a_while() {
    let rigs = SimRig::rigs("notfound", 1);
    let mut sim = new_sim(&rigs);
    let h = sim.add_hostile(0, "pruned");
    sim.hostile_send(h, hello_for(&rigs[0], 5, huge_work()));
    sim.run_for(SEC);
    let ids: Vec<[u8; 32]> = (1..=3u8).map(|i| [i; 32]).collect();
    sim.hostile_send(
        h,
        Message::BlockIds {
            first_height: 1,
            ids: ids.clone(),
        },
    );
    sim.run_for(SEC);
    sim.hostile_send(h, Message::NotFound { ids });
    sim.run_for(SEC);
    assert!(!sim.engines[0].is_syncing());
    assert_eq!(
        score(&sim, h),
        Some(20),
        "20 points for offering blocks it cannot serve"
    );
    let asks = sim.hostiles[h]
        .inbox
        .iter()
        .filter(|m| matches!(m, Message::GetBlockIds { .. }))
        .count();
    assert_eq!(
        asks, 1,
        "the cooldown keeps the node from asking it again at once"
    );
}

#[test]
fn a_peer_that_has_pruned_the_blocks_we_need_is_not_asked_to_serve_them() {
    let rigs = SimRig::rigs("prunedpeer", 1);
    let mut sim = new_sim(&rigs);
    let asks = |sim: &Sim<'_>, h: usize| {
        sim.hostiles[h]
            .inbox
            .iter()
            .filter(|m| matches!(m, Message::GetBlockIds { .. }))
            .count()
    };
    let with_pruned = |rig: &SimRig, pruned_below: u64| {
        let Message::Hello(mut h) = hello_for(rig, 500, huge_work()) else {
            unreachable!()
        };
        h.pruned_below = pruned_below;
        Message::Hello(h)
    };
    // we are at height 0 and need block 1; a peer whose blocks start at 2 cannot give it to us
    let far = sim.add_hostile(0, "pruned-far");
    sim.hostile_send(far, with_pruned(&rigs[0], 2));
    sim.run_for(2 * SEC);
    assert_eq!(asks(&sim, far), 0);
    assert!(!sim.engines[0].is_syncing());
    // one whose blocks start at 1 can: the boundary is inclusive
    let edge = sim.add_hostile(0, "pruned-edge");
    sim.hostile_send(edge, with_pruned(&rigs[0], 1));
    sim.run_for(2 * SEC);
    assert_eq!(asks(&sim, edge), 1);
    // and it is not punished for having pruned
    assert_eq!(score(&sim, far), Some(0));
}

#[test]
fn an_invalid_transaction_is_punished_and_not_fetched_again() {
    let rigs = SimRig::rigs("badtx", 1);
    let mut sim = new_sim(&rigs);
    sim.mine_chain(0, 4);
    let bad = make_tx(&sim, 0, 1, -1); // one unit under the minimum fee
    let id = tx_id_of(&bad);
    let h = sim.add_hostile(0, "spammer");
    sim.hostile_send(h, hello_for(&rigs[0], 4, zero_work()));
    sim.hostile_send(h, Message::NewTx { ids: vec![id] });
    sim.run_for(SEC);
    assert!(sim.hostiles[h]
        .inbox
        .iter()
        .any(|m| matches!(m, Message::GetTxs { ids } if ids == &vec![id])));
    sim.hostile_send(h, Message::Txs { txs: vec![bad] });
    sim.run_for(SEC);
    assert_eq!(
        score(&sim, h),
        Some(20),
        "20 points for an invalid transaction"
    );
    sim.hostile_send(h, Message::NewTx { ids: vec![id] });
    sim.run_for(SEC);
    let asks = sim.hostiles[h]
        .inbox
        .iter()
        .filter(|m| matches!(m, Message::GetTxs { .. }))
        .count();
    assert_eq!(asks, 1, "a rejected transaction is not requested again");
    assert!(sim.engines[0].node().pool().is_empty());
}

fn tx_id_of(t: &Transaction) -> [u8; 32] {
    tenero_core::v2::ids::tx_id(t).unwrap()
}

// ---- relay ---------------------------------------------------------------------------------------

#[test]
fn a_block_reaches_every_node_and_each_downloads_it_once() {
    let rigs = SimRig::rigs("relay", 8);
    let mut sim = new_sim(&rigs);
    sim.connect_all();
    sim.run_for(3 * SEC);
    let b = sim.mine(3, None);
    assert!(
        sim.run_until(60 * SEC, |s| s.all_agree()),
        "the network did not converge"
    );
    assert_eq!(sim.tip(0).1, block_id(&b.header, PowKind::Sha256));
    assert_eq!(
        sim.sent_by_kind["blocks"], 7,
        "the block body is sent once per node, not once per peer"
    );
    assert_eq!(sim.sent_by_kind["get_blocks"], 7);
    assert!(sim.sent_by_kind["new_block"] >= 7);
}

#[test]
fn a_transaction_spreads_and_is_dropped_from_every_pool_when_mined() {
    let rigs = SimRig::rigs("tx", 6);
    let mut sim = new_sim(&rigs);
    sim.connect_all();
    sim.run_for(3 * SEC);
    for _ in 0..4 {
        sim.mine(0, None);
        assert!(sim.run_until(60 * SEC, |s| s.all_agree()));
        sim.run_for(70 * SEC); // a minute passes between blocks
    }
    let t = make_tx(&sim, 0, 1, 0);
    let id = tx_id_of(&t);
    sim.submit_tx(2, t);
    assert!(
        sim.run_until(60 * SEC, |s| (0..6)
            .all(|n| s.engines[n].node().pool().contains(&id))),
        "the transaction did not reach every pool"
    );
    // mined at a node that got it from the network
    sim.run_for(70 * SEC);
    let b = sim.mine(5, None);
    assert_eq!(b.transactions.len(), 1);
    assert!(sim.run_until(60 * SEC, |s| s.all_agree()));
    sim.run_for(5 * SEC);
    for n in 0..6 {
        assert!(
            sim.engines[n].node().pool().is_empty(),
            "node {n} still holds it"
        );
    }
    // the transaction crossed each link at most once as an id, and its body was fetched once per node
    assert_eq!(sim.sent_by_kind["txs"], 5);
}

// ---- sync ---------------------------------------------------------------------------------------

#[test]
fn a_new_node_syncs_a_long_chain_in_batches() {
    let rigs = SimRig::rigs("sync", 2);
    let mut sim = new_sim(&rigs);
    sim.mine_chain(0, 1100);
    assert_eq!(sim.tip(0).0, 1100);
    assert!(sim.connect(1, 0));
    assert!(
        sim.run_until(600 * SEC, |s| s.all_agree()),
        "node 1 reached only height {}",
        sim.tip(1).0
    );
    assert_eq!(sim.tip(1).0, 1100);
    // ids come at most 500 to a message, blocks at most 32
    assert!(sim.sent_by_kind["block_ids"] >= 3);
    assert!(sim.sent_by_kind["blocks"] >= 1100 / 32);
    assert_eq!(sim.engines[1].stats.blocks_applied, 1100);
    assert!(
        !sim.engines[1].stats.sent.contains_key("new_block"),
        "a node that synced from a peer does not announce those blocks back to it"
    );
}

/// Mines `blocks` blocks on `node`, a minute apart; from the eleventh on each carries one transaction, so that
/// pruning has something to delete (a block with only a coinbase has no proofs to prune).
fn mine_with_txs(sim: &mut Sim<'_>, node: usize, blocks: u64) {
    for _ in 0..blocks {
        let h = sim.tip(node).0;
        if h >= 10 {
            let t = make_tx(sim, node, h, 0);
            sim.submit_tx(node, t);
        }
        sim.mine(node, None);
        sim.run_for(70 * SEC);
    }
}

/// The plan's "done when" for sync: a fresh node takes a 10,000-block chain from an archive node.
#[test]
fn a_fresh_node_syncs_ten_thousand_blocks_from_an_archive_node() {
    let rigs = SimRig::rigs("sync10k", 2);
    let mut sim = new_sim(&rigs);
    sim.mine_chain(0, 10_000);
    assert_eq!(sim.tip(0).0, 10_000);
    assert!(sim.connect(1, 0));
    let started = std::time::Instant::now();
    assert!(
        sim.run_until(3_600 * SEC, |s| s.tip(1).0 == 10_000),
        "node 1 reached only height {}",
        sim.tip(1).0
    );
    eprintln!(
        "10,000 blocks synced in {:?} of real time",
        started.elapsed()
    );
    assert_eq!(sim.tip(1).1, sim.tip(0).1);
    assert_eq!(sim.engines[1].stats.blocks_applied, 10_000);
}

#[test]
fn a_fresh_node_cannot_sync_from_a_pruned_peer_and_does_not_blame_it() {
    let rigs = SimRig::rigs("syncpruned-fresh", 3);
    let mut sim = new_sim(&rigs);
    // node 2 stays away until later, or it would find the archive node on its own
    sim.set_online(2, false);
    mine_with_txs(&mut sim, 0, 40);
    assert_eq!(sim.tip(0).0, 40);
    assert!(sim.connect(1, 0));
    assert!(sim.run_until(600 * SEC, |s| s.tip(1).0 == 40));
    // node 1 keeps only its last 10 blocks
    sim.engines[1].node().store().prune_keeping(10).unwrap();
    assert_eq!(sim.engines[1].node().store().pruned_below().unwrap(), 31);

    sim.partition(&[vec![0], vec![1, 2]]);
    sim.set_online(2, true);
    assert!(sim.connect(2, 1));
    sim.run_for(120 * SEC);
    assert_eq!(
        sim.tip(2).0,
        0,
        "a pruned peer has nothing a fresh node can use"
    );
    assert_eq!(sim.engines[2].stats.blocks_applied, 0);
    assert!(!sim.engines[2].is_syncing());
    assert_eq!(
        sim.engines[2].peer_count(),
        1,
        "the pruned peer was neither banned nor dropped"
    );
    // once the archive node is reachable, the same fresh node completes from it
    sim.heal();
    assert!(sim.connect(2, 0));
    assert!(sim.run_until(600 * SEC, |s| s.tip(2).0 == 40));
    assert_eq!(sim.tip(2).1, sim.tip(0).1);
}

#[test]
fn a_node_that_is_nearly_up_to_date_can_sync_from_a_pruned_peer() {
    let rigs = SimRig::rigs("syncpruned", 2);
    let mut sim = new_sim(&rigs);
    mine_with_txs(&mut sim, 0, 40);
    assert!(sim.connect(1, 0));
    assert!(sim.run_until(600 * SEC, |s| s.tip(1).0 == 40));
    // node 0 prunes all but its last 10 blocks, then goes on alone for 3 more while node 1 is cut off
    sim.engines[0].node().store().prune_keeping(10).unwrap();
    assert_eq!(sim.engines[0].node().store().pruned_below().unwrap(), 31);
    sim.partition(&[vec![0], vec![1]]);
    mine_with_txs(&mut sim, 0, 3);
    // node 1 is behind by 3 blocks, all inside node 0's kept tail
    sim.heal();
    assert!(sim.run_until(300 * SEC, |s| s.tip(1).0 == 43));
    assert_eq!(sim.tip(1).1, sim.tip(0).1);
}

#[test]
fn a_new_node_takes_the_chain_of_the_peer_with_the_most_work() {
    let rigs = SimRig::rigs("bestpeer", 3);
    let mut sim = new_sim(&rigs);
    sim.mine_chain(1, 10);
    sim.mine_chain(2, 30);
    sim.connect(0, 1);
    sim.connect(0, 2);
    assert!(sim.run_until(300 * SEC, |s| s.tip(0).0 == 30));
    assert_eq!(sim.tip(0).1, sim.tip(2).1);
    // and it never took node 1's blocks
    assert_eq!(sim.engines[0].stats.blocks_applied, 30);
}

#[test]
fn partitions_heal_onto_the_heavier_chain() {
    let rigs = SimRig::rigs("partition", 10);
    let mut sim = new_sim(&rigs);
    sim.connect_all();
    sim.run_for(3 * SEC);
    sim.mine_chain(0, 5);
    assert!(sim.run_until(120 * SEC, |s| s.all_agree()));
    let common = sim.tip(0).1;
    sim.partition(&[all(5), (5..10).collect()]);
    sim.mine_chain(0, 3); // group A: 3 more
    sim.mine_chain(5, 5); // group B: 5 more
    sim.run_for(120 * SEC);
    assert!(sim.agree(&all(5)) && sim.agree(&(5..10).collect::<Vec<_>>()));
    assert_eq!(sim.distinct_tips(), 2);
    assert_ne!(sim.tip(0).1, common);
    let b_tip = sim.tip(5).1;
    sim.heal();
    assert!(
        sim.run_until(600 * SEC, |s| s.all_agree()),
        "the network did not converge after healing"
    );
    assert_eq!(sim.tip(0).1, b_tip, "the chain with more work wins");
    assert_eq!(sim.tip(0).0, 10);
    assert_eq!(sim.distinct_tips(), 1);
}

#[test]
fn a_lossy_network_still_converges() {
    let rigs = SimRig::rigs("lossy", 12);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), EngineConfig::default());
    sim.connect_all();
    sim.run_for(30 * SEC);
    sim.set_drop_permille(100);
    for i in 0..8 {
        sim.mine(i % 12, None);
        sim.run_for(65 * SEC);
    }
    assert!(
        sim.run_until(600 * SEC, |s| s.all_agree()),
        "{} distinct tips remain",
        sim.distinct_tips()
    );
    assert!(
        sim.dropped > 100,
        "the network should really have lost messages"
    );
    assert!(sim.tip(0).0 >= 4);
    for n in 0..12 {
        assert!(
            sim.engines[n].peer_count() >= 10,
            "node {n} kept only {} of its 11 peers",
            sim.engines[n].peer_count()
        );
    }
}

// ---- scale --------------------------------------------------------------------------------------

#[test]
fn sixty_nodes_with_fifty_nine_peers_each_converge_and_download_each_block_once() {
    let n = 60;
    let rigs = SimRig::rigs("mesh", n);
    let mut sim = new_sim(&rigs);
    sim.connect_all();
    sim.run_for(10 * SEC);
    for i in 0..n {
        assert_eq!(sim.engines[i].peer_count(), 59, "node {i}");
        assert_eq!(sim.engines[i].ready_peer_count(), 59, "node {i}");
    }
    let blocks = 6;
    for k in 0..blocks {
        sim.mine((k * 7) % n, None);
        assert!(
            sim.run_until(120 * SEC, |s| s.all_agree()),
            "block {k} did not reach everyone"
        );
        sim.run_for(65 * SEC);
    }
    assert_eq!(sim.tip(0).0, blocks as u64);
    assert_eq!(
        sim.sent_by_kind["blocks"],
        (blocks * (n - 1)) as u64,
        "every node downloads every block exactly once, whatever its peer count"
    );
    for i in 0..n {
        assert_eq!(
            sim.engines[i].stats.bans, 0,
            "node {i} banned an honest peer"
        );
        assert_eq!(sim.engines[i].peer_count(), 59, "node {i} lost a peer");
    }
}

// ---- behaviours that need their own tests -----------------------------------------------------------

#[test]
fn connection_limits_are_enforced() {
    // total cap
    let rigs = SimRig::rigs("cap1", 1);
    let cfg = EngineConfig {
        max_peers: 3,
        max_inbound: 64,
        max_addr_only: 0,
        ..EngineConfig::default()
    };
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), cfg);
    let hs: Vec<usize> = (0..4)
        .map(|i| sim.add_hostile(0, &format!("h{i}")))
        .collect();
    sim.run_for(SEC);
    assert_eq!(
        hs.iter().filter(|&&h| sim.hostiles[h].disconnected).count(),
        1
    );
    assert_eq!(sim.engines[0].peer_count(), 3);
    // inbound cap, with room in the total
    let rigs = SimRig::rigs("cap2", 1);
    let cfg = EngineConfig {
        max_peers: 64,
        max_inbound: 2,
        max_addr_only: 0,
        ..EngineConfig::default()
    };
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), cfg);
    let hs: Vec<usize> = (0..4)
        .map(|i| sim.add_hostile(0, &format!("h{i}")))
        .collect();
    sim.run_for(SEC);
    assert_eq!(
        hs.iter().filter(|&&h| sim.hostiles[h].disconnected).count(),
        2
    );
    assert_eq!(sim.engines[0].peer_count(), 2);
}

#[test]
fn a_tip_is_announced_once_and_not_echoed_back() {
    let rigs = SimRig::rigs("announce", 2);
    let mut sim = new_sim(&rigs);
    sim.connect(0, 1);
    sim.run_for(3 * SEC);
    sim.mine(0, None);
    assert!(sim.run_until(30 * SEC, |s| s.all_agree()));
    sim.run_for(300 * SEC);
    assert_eq!(
        sim.engines[0].stats.sent["new_block"], 1,
        "announced once, and not again to a peer that has it"
    );
    assert!(
        !sim.engines[1].stats.sent.contains_key("new_block"),
        "a block is never announced back to the peer it came from"
    );
}

#[test]
fn a_lost_announcement_is_repeated_to_a_peer_that_is_behind() {
    let rigs = SimRig::rigs("reannounce", 1);
    let mut sim = new_sim(&rigs);
    let h = sim.add_hostile(0, "behind");
    sim.hostile_send(h, hello_for(&rigs[0], 0, zero_work()));
    sim.run_for(2 * SEC);
    sim.mine(0, None);
    sim.run_for(40 * SEC);
    let announced = sim.hostiles[h]
        .inbox
        .iter()
        .filter(|m| matches!(m, Message::NewBlock { .. }))
        .count();
    assert!(
        announced >= 2,
        "the tip should be repeated to a peer that never asked for it ({announced})"
    );
}

/// A valid block on `node`'s tip that the test keeps to itself (nobody is told).
fn unshared_block(sim: &mut Sim<'_>, node: usize) -> Block {
    let ts = sim.now_ms() / 1000;
    let eng = &sim.engines[node];
    let mut b = eng
        .node()
        .block_template(
            ts,
            1_000_000,
            Payout {
                onetime_address: [7; 32],
                view_tag: [7; 3],
                ephemeral_pubkey: [7; 32],
                anchor_enc: [7; 16],
            },
        )
        .unwrap();
    let target = eng.node().next_block().unwrap().target;
    for nonce in 0.. {
        b.header.nonce = nonce;
        if U256::from_be_bytes(&block_id(&b.header, PowKind::Sha256)) < target {
            break;
        }
    }
    b
}

fn count_get_blocks(sim: &Sim<'_>, h: usize, id: [u8; 32]) -> usize {
    sim.hostiles[h]
        .inbox
        .iter()
        .filter(|m| matches!(m, Message::GetBlocks { ids } if ids == &vec![id]))
        .count()
}

#[test]
fn a_block_request_that_times_out_goes_to_the_next_announcer() {
    let rigs = SimRig::rigs("retry", 2);
    let mut sim = new_sim(&rigs);
    let b = unshared_block(&mut sim, 1);
    let id = block_id(&b.header, PowKind::Sha256);
    let a = sim.add_hostile(0, "first");
    let c = sim.add_hostile(0, "second");
    for h in [a, c] {
        sim.hostile_send(h, hello_for(&rigs[0], 1, huge_work()));
    }
    sim.run_for(SEC);
    let ann = Message::NewBlock {
        id,
        height: 1,
        cumulative_work: huge_work().to_be_bytes(),
    };
    sim.hostile_send(a, ann.clone());
    sim.run_for(SEC);
    sim.hostile_send(c, ann);
    sim.run_for(SEC);
    assert_eq!(
        (count_get_blocks(&sim, a, id), count_get_blocks(&sim, c, id)),
        (1, 0),
        "only the first announcer is asked"
    );
    // it never answers: after the timeout the next announcer is asked
    sim.run_for(35 * SEC);
    assert_eq!(
        count_get_blocks(&sim, c, id),
        1,
        "the request should move to the second announcer"
    );
}

#[test]
fn a_block_request_moves_on_at_once_when_the_peer_holding_it_leaves() {
    let rigs = SimRig::rigs("retry2", 2);
    let mut sim = new_sim(&rigs);
    let b = unshared_block(&mut sim, 1);
    let id = block_id(&b.header, PowKind::Sha256);
    let a = sim.add_hostile(0, "first");
    let c = sim.add_hostile(0, "second");
    for h in [a, c] {
        sim.hostile_send(h, hello_for(&rigs[0], 1, huge_work()));
    }
    sim.run_for(SEC);
    let ann = Message::NewBlock {
        id,
        height: 1,
        cumulative_work: huge_work().to_be_bytes(),
    };
    sim.hostile_send(a, ann.clone());
    sim.run_for(SEC);
    sim.hostile_send(c, ann);
    sim.run_for(SEC);
    // the first announcer misbehaves until it is banned and disconnected
    for _ in 0..2 {
        sim.hostile_send(a, Message::GetBlocks { ids: vec![] });
    }
    sim.run_for(3 * SEC);
    assert!(sim.hostiles[a].disconnected);
    assert_eq!(count_get_blocks(&sim, c, id), 1);
}

#[test]
fn a_silent_sync_peer_is_abandoned_after_the_timeout() {
    let rigs = SimRig::rigs("silentsync", 1);
    let mut sim = new_sim(&rigs);
    let h = sim.add_hostile(0, "slowpoke");
    sim.hostile_send(h, hello_for(&rigs[0], 50, huge_work()));
    sim.run_for(2 * SEC);
    assert!(sim.engines[0].is_syncing());
    sim.run_for(20 * SEC);
    assert!(
        sim.engines[0].is_syncing(),
        "still within the request timeout"
    );
    sim.run_for(15 * SEC);
    assert!(
        !sim.engines[0].is_syncing(),
        "abandoned after 30 s of silence"
    );
    assert!(
        !sim.hostiles[h].disconnected,
        "one timeout is not a reason to disconnect"
    );
}

#[test]
fn a_block_ahead_of_the_clock_is_held_and_applied_when_its_time_comes() {
    let rigs = SimRig::rigs("notyet", 2);
    let mut sim = new_sim(&rigs);
    sim.connect(0, 1);
    sim.run_for(3 * SEC);
    // 300 s ahead: more than the 120 s future limit, so not acceptable yet
    let ts = sim.now_ms() / 1000 + 300;
    sim.mine(0, Some(ts));
    sim.run_for(60 * SEC);
    assert_eq!(sim.tip(0).0, 0, "held, not applied");
    assert_eq!(sim.tip(1).0, 0);
    // some time later the block is within the limit
    assert!(
        sim.run_until(200 * SEC, |s| s.all_agree() && s.tip(0).0 == 1),
        "the held block should be applied and relayed once its time is near"
    );
}

#[test]
fn a_peer_that_answers_most_pings_is_not_dropped_because_answers_reset_the_count() {
    let rigs = SimRig::rigs("gappy", 1);
    let mut sim = new_sim(&rigs);
    let h = sim.add_hostile(0, "gappy");
    sim.hostile_send(h, hello_for(&rigs[0], 0, zero_work()));
    let mut seen = 0;
    for _ in 0..1500 {
        sim.run_for(SEC);
        let pings: Vec<u64> = sim.hostiles[h]
            .inbox
            .iter()
            .filter_map(|m| match m {
                Message::Ping(n) => Some(*n),
                _ => None,
            })
            .collect();
        while seen < pings.len() {
            // ignore every third ping: three in a row would be needed to lose it (the limit is five)
            if seen % 3 != 2 {
                sim.hostile_send(h, Message::Pong(pings[seen]));
            }
            seen += 1;
        }
        assert!(!sim.hostiles[h].disconnected, "dropped after {seen} pings");
    }
    assert!(
        seen >= 12,
        "the test should have exercised many pings ({seen})"
    );
}

// ---- every scoring and limit rule gets its own test ---------------------------------------------------

#[test]
fn oversized_blocks_and_transaction_lists_are_violations() {
    let rigs = SimRig::rigs("bigblocks", 1);
    let mut sim = new_sim(&rigs);
    let lim = EngineConfig::default().limits;
    let b = unshared_block(&mut sim, 0);
    let t = make_tx(&sim, 0, 1, 0);
    let cases: Vec<(&str, Message)> = vec![
        (
            "blocks",
            Message::Blocks {
                blocks: vec![b; lim.max_blocks + 1],
            },
        ),
        (
            "txs",
            Message::Txs {
                txs: vec![t; lim.max_txs + 1],
            },
        ),
    ];
    for (i, (name, msg)) in cases.into_iter().enumerate() {
        let h = sim.add_hostile(0, &format!("huge-{i}"));
        sim.hostile_send(h, hello_for(&rigs[0], 0, zero_work()));
        sim.hostile_send(h, msg.clone());
        sim.hostile_send(h, msg);
        sim.run_for(3 * SEC);
        assert!(
            sim.hostiles[h].disconnected,
            "{name}: two violations should drop the peer"
        );
    }
}

/// How many points the node has scored against a hostile peer.
fn score(sim: &Sim<'_>, h: usize) -> Option<u32> {
    let peer = sim.hostiles[h].peer_at_node()?;
    sim.engines[sim.hostiles[h].node].peer_score(peer)
}

#[test]
fn unsolicited_pongs_block_ids_and_transactions_are_punished() {
    let rigs = SimRig::rigs("unsolicited2", 1);
    let mut sim = new_sim(&rigs);
    let t = make_tx(&sim, 0, 1, 0);
    let pong = sim.add_hostile(0, "pong");
    let ids = sim.add_hostile(0, "ids");
    let txs = sim.add_hostile(0, "txs");
    for h in [pong, ids, txs] {
        sim.hostile_send(h, hello_for(&rigs[0], 0, zero_work()));
    }
    sim.run_for(SEC);
    for _ in 0..9 {
        sim.hostile_send(pong, Message::Pong(12345));
    }
    for _ in 0..4 {
        sim.hostile_send(
            ids,
            Message::BlockIds {
                first_height: 1,
                ids: vec![[1; 32]],
            },
        );
        sim.hostile_send(
            txs,
            Message::Txs {
                txs: vec![t.clone()],
            },
        );
    }
    sim.run_for(3 * SEC);
    assert_eq!(score(&sim, pong), Some(90), "10 points an unsolicited pong");
    assert_eq!(
        score(&sim, ids),
        Some(80),
        "20 points unsolicited block ids"
    );
    assert_eq!(
        score(&sim, txs),
        Some(80),
        "20 points an unrequested transaction"
    );
    sim.hostile_send(pong, Message::Pong(1));
    sim.hostile_send(
        ids,
        Message::BlockIds {
            first_height: 1,
            ids: vec![[1; 32]],
        },
    );
    sim.hostile_send(txs, Message::Txs { txs: vec![t] });
    sim.run_for(3 * SEC);
    for h in [pong, ids, txs] {
        assert!(sim.hostiles[h].disconnected);
    }
}

#[test]
fn block_ids_starting_at_the_genesis_block_are_refused() {
    let rigs = SimRig::rigs("firstheight", 1);
    let mut sim = new_sim(&rigs);
    let h = sim.add_hostile(0, "zero");
    sim.hostile_send(h, hello_for(&rigs[0], 50, huge_work()));
    sim.run_for(2 * SEC);
    assert!(sim.engines[0].is_syncing());
    let msg = Message::BlockIds {
        first_height: 0,
        ids: vec![[1; 32]],
    };
    sim.hostile_send(h, msg.clone());
    sim.run_for(SEC);
    assert_eq!(score(&sim, h), Some(50));
    sim.hostile_send(h, msg);
    sim.run_for(SEC);
    assert!(sim.hostiles[h].disconnected);
}

#[test]
fn a_block_known_to_be_invalid_is_not_welcome_again() {
    let rigs = SimRig::rigs("knowninvalid", 1);
    let mut sim = new_sim(&rigs);
    let bad = forged_block(&mut sim, 0);
    let bad_id = block_id(&bad.header, PowKind::Sha256);
    // the node learns that it is invalid from a first peer
    let evil = sim.add_hostile(0, "evil");
    sim.hostile_send(evil, hello_for(&rigs[0], 1, zero_work()));
    sim.hostile_send(
        evil,
        Message::NewBlock {
            id: bad_id,
            height: 1,
            cumulative_work: huge_work().to_be_bytes(),
        },
    );
    sim.run_for(2 * SEC);
    sim.hostile_send(evil, Message::Blocks { blocks: vec![bad] });
    sim.run_for(2 * SEC);
    assert!(sim.hostiles[evil].disconnected);
    // announcing it again is punished
    let friend = sim.add_hostile(0, "friend");
    sim.hostile_send(friend, hello_for(&rigs[0], 0, zero_work()));
    let announce = Message::NewBlock {
        id: bad_id,
        height: 1,
        cumulative_work: huge_work().to_be_bytes(),
    };
    sim.hostile_send(friend, announce.clone());
    sim.run_for(SEC);
    assert_eq!(score(&sim, friend), Some(50));
    sim.hostile_send(friend, announce);
    sim.run_for(SEC);
    assert!(sim.hostiles[friend].disconnected);
    // and so is offering it as part of a sync
    let offer = sim.add_hostile(0, "offer");
    sim.hostile_send(offer, hello_for(&rigs[0], 50, huge_work()));
    sim.run_for(2 * SEC);
    assert!(sim.engines[0].is_syncing());
    let ids = Message::BlockIds {
        first_height: 1,
        ids: vec![bad_id],
    };
    sim.hostile_send(offer, ids.clone());
    sim.run_for(SEC);
    assert_eq!(score(&sim, offer), Some(50));
    sim.hostile_send(offer, ids);
    sim.run_for(SEC);
    assert!(sim.hostiles[offer].disconnected);
}

#[test]
fn a_transaction_is_announced_once_and_not_echoed() {
    let rigs = SimRig::rigs("txecho", 2);
    let mut sim = new_sim(&rigs);
    sim.connect(0, 1);
    sim.run_for(3 * SEC);
    for _ in 0..4 {
        sim.mine(0, None);
        assert!(sim.run_until(60 * SEC, |s| s.all_agree()));
        sim.run_for(70 * SEC);
    }
    let t = make_tx(&sim, 0, 1, 0);
    let id = tx_id_of(&t);
    sim.submit_tx(0, t);
    sim.run_for(120 * SEC);
    assert!(sim.engines[1].node().pool().contains(&id));
    assert_eq!(sim.engines[0].stats.sent["new_tx"], 1);
    assert!(
        !sim.engines[1].stats.sent.contains_key("new_tx"),
        "not echoed to the peer it came from"
    );
    assert!(
        !sim.engines[0].stats.sent.contains_key("get_block_ids")
            && !sim.engines[1].stats.sent.contains_key("get_block_ids"),
        "equal chains never start a sync"
    );
}

#[test]
fn an_orphan_block_starts_a_sync_with_the_peer_that_sent_it() {
    let rigs = SimRig::rigs("orphan", 2);
    let mut sim = new_sim(&rigs);
    sim.mine_chain(1, 2);
    let (_, _) = sim.tip(1);
    let two = sim.engines[1]
        .node()
        .store()
        .get_block(2)
        .unwrap()
        .unwrap()
        .into_full()
        .unwrap();
    let two_id = block_id(&two.header, PowKind::Sha256);
    let h = sim.add_hostile(0, "gossip");
    sim.hostile_send(h, hello_for(&rigs[0], 0, zero_work()));
    sim.run_for(SEC);
    sim.hostile_send(
        h,
        Message::NewBlock {
            id: two_id,
            height: 2,
            cumulative_work: huge_work().to_be_bytes(),
        },
    );
    sim.run_for(SEC);
    assert_eq!(count_get_blocks(&sim, h, two_id), 1);
    assert!(!sim.engines[0].is_syncing());
    // its parent is unknown here: an orphan, and the node asks that peer for the chain
    sim.hostile_send(h, Message::Blocks { blocks: vec![two] });
    sim.run_for(SEC);
    assert!(sim.engines[0].is_syncing());
    assert!(sim.hostiles[h]
        .inbox
        .iter()
        .any(|m| matches!(m, Message::GetBlockIds { .. })));
    assert_eq!(sim.tip(0).0, 0, "an orphan is not applied");
}

#[test]
fn serving_block_ids_blocks_and_transactions() {
    let rigs = SimRig::rigs("serving", 1);
    let engine = EngineConfig {
        limits: tenero_net::Limits {
            max_ids: 5,
            ..tenero_net::Limits::default()
        },
        ..EngineConfig::default()
    };
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), engine);
    sim.mine_chain(0, 12);
    let id_at = |sim: &Sim<'_>, h: u64| {
        sim.engines[0]
            .node()
            .store()
            .block_index(h)
            .unwrap()
            .unwrap()
            .block_id
    };
    let (genesis, b3) = (id_at(&sim, 0), id_at(&sim, 3));
    let h = sim.add_hostile(0, "asker");
    sim.hostile_send(h, hello_for(&rigs[0], 0, zero_work()));
    sim.run_for(SEC);
    // from the genesis block: the first five ids (the limit), oldest first
    sim.hostile_send(
        h,
        Message::GetBlockIds {
            locator: vec![genesis],
        },
    );
    // from block 3: the ids after it
    sim.hostile_send(
        h,
        Message::GetBlockIds {
            locator: vec![b3, genesis],
        },
    );
    // an unknown locator shares nothing, however long it is: an empty answer, and a punishment
    sim.hostile_send(
        h,
        Message::GetBlockIds {
            locator: vec![[9; 32]],
        },
    );
    sim.run_for(2 * SEC);
    let replies: Vec<&Message> = sim.hostiles[h]
        .inbox
        .iter()
        .filter(|m| matches!(m, Message::BlockIds { .. }))
        .collect();
    assert_eq!(replies.len(), 3);
    assert_eq!(
        replies[0],
        &Message::BlockIds {
            first_height: 1,
            ids: (1..=5).map(|k| id_at(&sim, k)).collect()
        }
    );
    assert_eq!(
        replies[1],
        &Message::BlockIds {
            first_height: 4,
            ids: (4..=8).map(|k| id_at(&sim, k)).collect()
        }
    );
    assert_eq!(
        replies[2],
        &Message::BlockIds {
            first_height: 1,
            ids: vec![]
        }
    );
    assert_eq!(score(&sim, h), Some(20));
    // blocks: the one we have is sent, the unknown one is "not found"
    sim.hostile_send(
        h,
        Message::GetBlocks {
            ids: vec![b3, [8; 32]],
        },
    );
    sim.hostile_send(h, Message::GetTxs { ids: vec![[7; 32]] });
    sim.run_for(2 * SEC);
    let block3 = sim.engines[0]
        .node()
        .store()
        .get_block(3)
        .unwrap()
        .unwrap()
        .into_full()
        .unwrap();
    assert!(sim.hostiles[h].inbox.iter().any(|m| m
        == &Message::Blocks {
            blocks: vec![block3.clone()]
        }));
    assert!(sim.hostiles[h]
        .inbox
        .iter()
        .any(|m| m == &Message::NotFound { ids: vec![[8; 32]] }));
    assert!(sim.hostiles[h]
        .inbox
        .iter()
        .any(|m| m == &Message::NotFound { ids: vec![[7; 32]] }));
}

#[test]
fn a_ban_expires() {
    let rigs = SimRig::rigs("banexpiry", 1);
    let engine = EngineConfig {
        ban_ms: 10_000,
        ..EngineConfig::default()
    };
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), engine);
    let h = sim.add_hostile(0, "reformed");
    let Message::Hello(mut hello) = hello_for(&rigs[0], 0, zero_work()) else {
        unreachable!()
    };
    hello.chain_id = [1; 32];
    sim.hostile_send(h, Message::Hello(hello));
    sim.run_for(2 * SEC);
    assert!(sim.hostiles[h].disconnected);
    sim.hostile_connect(h);
    sim.run_for(SEC);
    assert!(sim.hostiles[h].disconnected, "still banned");
    sim.run_for(12 * SEC);
    sim.hostile_connect(h);
    sim.run_for(SEC);
    assert!(!sim.hostiles[h].disconnected, "the ban has run out");
}

#[test]
fn a_block_a_peer_already_announced_is_not_announced_back_to_it() {
    // A hostile peer announces block X to node 0 and never delivers it; node 0 gets X from node 1 instead.
    // It must not then announce X to the peer that told it about X.
    let rigs = SimRig::rigs("known", 2);
    let mut sim = new_sim(&rigs);
    let x = sim.mine(1, None);
    let x_id = block_id(&x.header, PowKind::Sha256);
    let (_, tip1) = sim.engines[1].node().store().tip().unwrap();
    let h = sim.add_hostile(0, "announcer");
    sim.hostile_send(h, hello_for(&rigs[0], 0, zero_work()));
    sim.run_for(SEC);
    sim.hostile_send(
        h,
        Message::NewBlock {
            id: x_id,
            height: 1,
            cumulative_work: tip1.cumulative_work,
        },
    );
    sim.run_for(SEC);
    sim.connect(0, 1);
    assert!(
        sim.run_until(400 * SEC, |s| s.tip(0).1 == x_id),
        "node 0 should get the block from node 1 in the end"
    );
    sim.run_for(60 * SEC);
    let announced = sim.hostiles[h]
        .inbox
        .iter()
        .filter(|m| matches!(m, Message::NewBlock { .. }))
        .count();
    assert_eq!(
        announced, 0,
        "the peer that announced it must not be told about it"
    );
}

#[test]
fn a_transaction_is_not_announced_to_a_peer_that_already_announced_it() {
    let rigs = SimRig::rigs("txknown", 1);
    let mut sim = new_sim(&rigs);
    sim.mine_chain(0, 4);
    let t = make_tx(&sim, 0, 1, 0);
    let id = tx_id_of(&t);
    let a = sim.add_hostile(0, "first");
    let b = sim.add_hostile(0, "second");
    for h in [a, b] {
        sim.hostile_send(h, hello_for(&rigs[0], 4, zero_work()));
    }
    sim.run_for(SEC);
    sim.hostile_send(a, Message::NewTx { ids: vec![id] });
    sim.run_for(SEC);
    sim.hostile_send(b, Message::NewTx { ids: vec![id] });
    sim.run_for(SEC);
    // only the first announcer was asked; it delivers
    sim.hostile_send(a, Message::Txs { txs: vec![t] });
    sim.run_for(3 * SEC);
    assert!(sim.engines[0].node().pool().contains(&id));
    for h in [a, b] {
        assert!(
            !sim.hostiles[h]
                .inbox
                .iter()
                .any(|m| matches!(m, Message::NewTx { .. })),
            "a peer that announced a transaction is not told about it"
        );
    }
}
