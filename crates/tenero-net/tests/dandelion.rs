//! Dandelion++ in the simulator (`docs/DANDELION_PLAN.md`): real engines, a spy connected to every node, and what the spy
//! can tell of where each transaction came from, with Dandelion++ off and on. Then the rules one by one: a node's own
//! transaction is never announced by it first, a lost stem is fluffed by the embargo, a stem transaction is not served to
//! `get_txs`, and an invalid one costs its sender.

use tenero_core::hash::sha256;
use tenero_core::u256::U256;
use tenero_core::v3::ids::tx_id;
use tenero_core::v3::rules;
use tenero_core::v3::*;
use tenero_net::sim::{Sim, SimConfig, SimRig};
use tenero_net::{DandelionConfig, EngineConfig, Hello, Message, PROTOCOL_VERSION};
use tenero_tree::hash_to_point;

const T0: u64 = 1_700_000_000;
const SEC: u64 = 1000;

fn cfg(enabled: bool, fluff_percent: u64, seed: u8) -> EngineConfig {
    EngineConfig {
        dandelion: DandelionConfig {
            enabled,
            fluff_percent,
            secret: [seed; 32],
            ..DandelionConfig::default()
        },
        ..EngineConfig::default()
    }
}

/// A transaction valid at `node`'s tip (its proof is junk: the simulator does not check proofs).
fn make_tx(sim: &Sim<'_>, node: usize, image: u64) -> Transaction {
    let next = sim.engines[node].node().next_block().unwrap();
    let pt = |tag: &[u8], n: u8| hash_to_point(sha256(&[tag, &image.to_le_bytes(), &[n]]));
    let out = |n: u8| Output {
        onetime_address: pt(b"addr", n),
        amount_commitment: pt(b"commit", n),
        amount_enc: [n; 8],
        view_tag: [n; 3],
        anchor_enc: [n; 16],
    };
    let mut outputs = vec![out(1), out(2)];
    outputs.sort_by_key(|o| o.onetime_address);
    let mut t = Transaction {
        prefix: TxPrefix {
            version: VERSION,
            inputs: vec![Input {
                key_image: pt(b"key image", 0),
            }],
            outputs,
            ephemeral_pubkeys: vec![pt(b"eph", 0)],
            fee: 0,
            encrypted_payment_id: [0; 8],
        },
        prunable: Prunable {
            reference_height: next.height - 1,
            proof_data: vec![7; 200],
        },
    };
    let size = t.to_bytes().unwrap().len() as u64;
    t.prefix.fee = rules::min_fee(size, next.reward, next.median).unwrap();
    t
}

/// Mines on node 0 until the curve tree holds outputs (block 61), so transactions can be made.
fn ready_for_spends(sim: &mut Sim<'_>) {
    while sim.tip(0).0 < 61 {
        sim.mine(0, None);
        assert!(sim.run_until(60 * SEC, |s| s.all_agree()));
        sim.run_for(61 * SEC);
    }
}

fn hello(rig: &SimRig) -> Message {
    Message::Hello(Hello {
        version: PROTOCOL_VERSION,
        chain_id: rig.store.chain_id(),
        tip_height: 0,
        cumulative_work: U256::from_be_bytes(&[0; 32]).to_be_bytes(),
        tip_id: [9; 32],
        pruned_below: 0,
        nonce: 0,
    })
}

/// Runs `ms` in half-second steps, the scripted peers answering pings so they stay connected.
fn run_spied(sim: &mut Sim<'_>, ms: u64, seen: &mut [usize]) {
    let mut t = 0;
    while t < ms {
        sim.run_for(500);
        t += 500;
        for (h, read) in seen.iter_mut().enumerate() {
            let new: Vec<Message> = sim.hostiles[h].inbox[*read..].to_vec();
            *read = sim.hostiles[h].inbox.len();
            for m in new {
                if let Message::Ping(n) = m {
                    sim.hostile_send(h, Message::Pong(n));
                }
            }
        }
    }
}

/// A network of `n` nodes (each dials the next and the fifth after it), a spy connected to every one (an inbound peer
/// that says hello and answers pings), and `txs` transactions made at nodes in turn. For each, the spy guesses that it
/// came from the node that announced it to the spy FIRST. Returns how many guesses were right, and checks the spy never
/// saw a stem.
fn spy_run(tag: &str, n: usize, txs: usize, engine: EngineConfig) -> usize {
    let rigs = SimRig::rigs(tag, n);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), engine);
    for i in 0..n {
        sim.connect(i, (i + 1) % n);
        sim.connect(i, (i + 5) % n);
    }
    sim.run_for(5 * SEC);
    ready_for_spends(&mut sim);
    for (i, rig) in rigs.iter().enumerate() {
        let h = sim.add_hostile(i, &format!("99.{}.0.1:8333", i + 1));
        sim.hostile_send(h, hello(rig));
    }
    let mut seen = vec![0; n];
    let mut right = 0;
    for k in 0..txs {
        let origin = (k * 7) % n;
        let t = make_tx(&sim, origin, 1000 + k as u64);
        let id = tx_id(&t).unwrap();
        sim.submit_tx(origin, t);
        // until every node holds it in its pool (the embargo is at most 4 minutes)
        let mut waited = 0;
        while !(0..n).all(|i| sim.engines[i].node().pool().contains(&id)) {
            assert!(
                waited < 300 * SEC,
                "transaction {k} did not reach every pool"
            );
            run_spied(&mut sim, SEC, &mut seen);
            waited += SEC;
        }
        run_spied(&mut sim, 2 * SEC, &mut seen);
        let first = (0..n)
            .filter_map(|h| {
                let s = &sim.hostiles[h];
                s.inbox
                    .iter()
                    .zip(&s.inbox_at)
                    .find(|(m, _)| matches!(m, Message::NewTx { ids } if ids.contains(&id)))
                    .map(|(_, at)| (*at, s.node))
            })
            .min()
            .expect("the spy saw it announced");
        right += usize::from(first.1 == origin);
    }
    for s in &sim.hostiles {
        assert!(
            !s.inbox.iter().any(|m| matches!(m, Message::StemTx(_))),
            "a stem reached a peer that dialled in"
        );
        assert!(!s.disconnected, "the spy was dropped");
    }
    right
}

/// The measurement this is for (threat D4): with every transaction announced at once, a spy on every node names the
/// origin of nearly every transaction from the first announcement; with Dandelion++ it mostly cannot.
#[test]
fn dandelion_hides_the_origin_from_a_spy_on_every_node() {
    let (n, txs) = (12, 24);
    let off = spy_run("spy-off", n, txs, cfg(false, 20, 1));
    let on = spy_run("spy-on", n, txs, cfg(true, 20, 1));
    println!(
        "the spy named the origin of {off} of {txs} without Dandelion++, {on} of {txs} with it"
    );
    assert!(off * 10 >= txs * 8, "without Dandelion++: {off} of {txs}");
    assert!(on * 3 <= txs, "with Dandelion++: {on} of {txs}");
}

/// A node's own transaction is passed to one outbound peer in its stem, never announced by it first; a peer that dialled
/// in never gets it as a stem; and every node ends up with it.
#[test]
fn a_nodes_own_transaction_goes_on_a_stem_first() {
    let rigs = SimRig::rigs("own", 6);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), cfg(true, 20, 2));
    for i in 0..6 {
        sim.connect(i, (i + 1) % 6);
        sim.connect(i, (i + 3) % 6);
    }
    sim.run_for(5 * SEC);
    ready_for_spends(&mut sim);
    let t = make_tx(&sim, 0, 77);
    let id = tx_id(&t).unwrap();
    let before = sim.engines[0]
        .stats
        .sent
        .get("new_tx")
        .copied()
        .unwrap_or(0);
    sim.submit_tx(0, t);
    assert!(sim.engines[0].holds_stem(&id), "held in the stem pool");
    assert!(
        !sim.engines[0].node().pool().contains(&id),
        "not in the public pool yet"
    );
    assert_eq!(sim.engines[0].stats.sent.get("stem_tx"), Some(&1));
    assert_eq!(
        sim.engines[0]
            .stats
            .sent
            .get("new_tx")
            .copied()
            .unwrap_or(0),
        before,
        "not announced"
    );
    assert!(sim.run_until(300 * SEC, |s| (0..6)
        .all(|i| s.engines[i].node().pool().contains(&id))));
    // and once it is public nobody still holds it in a stem
    assert!((0..6).all(|i| !sim.engines[i].holds_stem(&id)));
}

/// A stem lost on the way (here: every message dropped for a moment) is not lost: the embargo fluffs it.
#[test]
fn a_lost_stem_is_fluffed_by_the_embargo() {
    let rigs = SimRig::rigs("embargo", 4);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), cfg(true, 0, 3));
    for i in 0..4 {
        sim.connect(i, (i + 1) % 4);
    }
    sim.run_for(5 * SEC);
    ready_for_spends(&mut sim);
    let t = make_tx(&sim, 0, 78);
    let id = tx_id(&t).unwrap();
    sim.set_drop_permille(1000);
    sim.submit_tx(0, t);
    sim.run_for(SEC);
    sim.set_drop_permille(0);
    assert!(sim.engines[0].holds_stem(&id));
    assert!(sim.run_until(300 * SEC, |s| (0..4)
        .all(|i| s.engines[i].node().pool().contains(&id))));
    assert!(
        sim.engines[0]
            .stats
            .sent
            .get("new_tx")
            .copied()
            .unwrap_or(0)
            >= 1
    );
}

/// A transaction in its stem is not served to `get_txs` (or a spy could ask every node for it and find who holds it),
/// and an invalid stem transaction costs the peer that sent it.
#[test]
fn a_stem_transaction_is_not_served_and_an_invalid_one_costs_its_sender() {
    let rigs = SimRig::rigs("probe", 3);
    // relayers only (no diffusers), so node 1 keeps what it is given in its stem
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), cfg(true, 0, 4));
    sim.connect(0, 1);
    sim.connect(1, 2);
    sim.connect(2, 0);
    sim.run_for(5 * SEC);
    ready_for_spends(&mut sim);
    let spy = sim.add_hostile(1, "99.9.0.1:8333");
    sim.hostile_send(spy, hello(&rigs[1]));
    sim.run_for(SEC);
    let t = make_tx(&sim, 1, 79);
    let id = tx_id(&t).unwrap();
    sim.hostile_send(spy, Message::StemTx(Box::new(t)));
    sim.run_for(200);
    assert!(
        sim.engines[1].holds_stem(&id),
        "a relayer keeps it in its stem"
    );
    let n = sim.hostiles[spy].inbox.len();
    sim.hostile_send(spy, Message::GetTxs { ids: vec![id] });
    sim.run_for(SEC);
    let answers = &sim.hostiles[spy].inbox[n..];
    assert!(
        !answers
            .iter()
            .any(|m| matches!(m, Message::Txs { txs } if !txs.is_empty())),
        "a stem transaction was served: {answers:?}"
    );
    let peer = sim.hostiles[spy].peer_at_node().unwrap();
    let score = |s: &Sim<'_>| s.engines[1].peer_score(peer);
    // a fee under the minimum is refused but costs nothing: an honest peer can hold one the minimum has risen past
    // (review of 2026-10-16)
    let mut low = make_tx(&sim, 1, 81);
    low.prefix.fee = 1;
    let before = score(&sim);
    sim.hostile_send(spy, Message::StemTx(Box::new(low)));
    sim.run_for(200);
    assert_eq!(score(&sim), before);
    // a forged one (outputs out of order: never valid anywhere) costs the spy, as an invalid transaction does
    let mut bad = make_tx(&sim, 1, 80);
    bad.prefix.outputs.reverse();
    sim.hostile_send(spy, Message::StemTx(Box::new(bad)));
    sim.run_for(200);
    assert!(score(&sim) > before, "{:?} then {:?}", before, score(&sim));
}

/// Peers that fill the stem pool cannot make a node announce its OWN transaction itself (that would name it as the
/// origin): the node's own have room of their own, so it still goes on a stem. A peer's next one is fluffed, as before.
/// (Found by a tester in 0.4.0-gamma.1, where a full pool of 1,024 made the node announce its own.)
#[test]
fn a_stem_pool_the_peers_filled_still_takes_the_nodes_own_transaction() {
    let rigs = SimRig::rigs("own-room", 3);
    let mut engine = cfg(true, 0, 9);
    engine.dandelion.max_stem_txs = 2;
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), engine);
    sim.connect(1, 2);
    sim.connect(0, 1);
    sim.run_for(5 * SEC);
    ready_for_spends(&mut sim);
    // a peer fills node 1's stem pool with two stems of its own
    let spy = sim.add_hostile(1, "99.9.0.1:8333");
    sim.hostile_send(spy, hello(&rigs[1]));
    sim.run_for(SEC);
    for k in 0..2 {
        let t = make_tx(&sim, 1, 500 + k);
        sim.hostile_send(spy, Message::StemTx(Box::new(t)));
    }
    sim.run_for(SEC);
    assert_eq!(sim.engines[1].stem_count(), 2, "the peers' share is full");
    // node 1's own: held in its stem, not announced
    let t = make_tx(&sim, 1, 600);
    let id = tx_id(&t).unwrap();
    let announced = |s: &Sim<'_>| s.engines[1].stats.sent.get("new_tx").copied().unwrap_or(0);
    let before = announced(&sim);
    sim.submit_tx(1, t);
    assert!(
        sim.engines[1].holds_stem(&id),
        "its own still goes on a stem"
    );
    assert!(!sim.engines[1].node().pool().contains(&id));
    assert_eq!(announced(&sim), before, "and is not announced by it");
}

/// A node that stops cleanly announces its OWN transactions still in their stem first (they would be lost with the stem
/// pool, which is not saved: seen on `gamma`, 2026-10-13); a peer's stem transaction it holds is left to its embargo.
#[test]
fn a_node_that_stops_announces_its_own_stem_transactions_first() {
    use tenero_net::Action;
    let rigs = SimRig::rigs("own-on-stop", 3);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), cfg(true, 0, 11));
    sim.connect(0, 1);
    sim.connect(1, 2);
    sim.run_for(5 * SEC);
    ready_for_spends(&mut sim);
    // node 1 holds one of its own and one a peer passed it, both in their stem
    let own = make_tx(&sim, 1, 700);
    let own_id = tx_id(&own).unwrap();
    sim.submit_tx(1, own);
    let spy = sim.add_hostile(1, "99.9.0.2:8333");
    sim.hostile_send(spy, hello(&rigs[1]));
    sim.run_for(SEC);
    let theirs = make_tx(&sim, 1, 701);
    let their_id = tx_id(&theirs).unwrap();
    sim.hostile_send(spy, Message::StemTx(Box::new(theirs)));
    sim.run_for(200);
    assert!(sim.engines[1].holds_stem(&own_id) && sim.engines[1].holds_stem(&their_id));
    // the stop
    let now = (T0 + 10_000) * 1000;
    let actions = sim.engines[1].fluff_own_stems(now);
    assert!(
        sim.engines[1].node().pool().contains(&own_id),
        "its own: into the pool"
    );
    assert!(!sim.engines[1].holds_stem(&own_id));
    assert!(
        actions.iter().any(|a| matches!(a, Action::Send { msg: Message::NewTx { ids }, .. } if ids.contains(&own_id))),
        "and announced"
    );
    // the peer's: still in its stem, not announced by this node
    assert!(sim.engines[1].holds_stem(&their_id));
    assert!(!sim.engines[1].node().pool().contains(&their_id));
}

/// A node that comes back after the others moved on (behind by blocks, its pool empty) gets the transactions waiting in
/// its peers' pools: they announce their pools when it connects (`push_pool`), it remembers what it hears while it syncs
/// and fetches it once in step (`fetch_deferred_txs`). Up to 0.4.1 it heard only of transactions announced after it
/// connected (seen on `gamma`, 2026-10-13: a restarted node's miner could not take in the transactions waiting elsewhere).
#[test]
fn a_node_that_comes_back_behind_gets_the_transactions_waiting_in_its_peers_pools() {
    let rigs = SimRig::rigs("pool-push", 3);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), cfg(true, 100, 12));
    sim.connect(0, 1);
    sim.run_for(5 * SEC);
    // (node 2 is away: only nodes 0 and 1 need to agree)
    while sim.tip(0).0 < 61 {
        sim.mine(0, None);
        assert!(sim.run_until(60 * SEC, |s| s.tip(1) == s.tip(0)));
        sim.run_for(61 * SEC);
    }
    // node 2 has been away: the others mine on, and transactions gather in their pools (fluffed: diffusers only)
    for _ in 0..20 {
        sim.mine(0, None);
        sim.run_for(61 * SEC);
    }
    let ids: Vec<[u8; 32]> = (0..5)
        .map(|k| {
            let t = make_tx(&sim, 0, 800 + k);
            let id = tx_id(&t).unwrap();
            sim.submit_tx(0, t);
            id
        })
        .collect();
    assert!(sim.run_until(60 * SEC, |s| ids
        .iter()
        .all(|id| s.engines[1].node().pool().contains(id))));
    assert!(sim.tip(2).0 < sim.tip(0).0, "node 2 is behind");
    assert!(sim.engines[2].node().pool().is_empty());
    // it comes back: it syncs the blocks, then has every transaction that was waiting
    sim.connect(2, 1);
    assert!(
        sim.run_until(120 * SEC, |s| s.all_agree()
            && ids.iter().all(|id| s.engines[2].node().pool().contains(id))),
        "node 2 has {} of {} waiting transactions",
        ids.iter()
            .filter(|id| sim.engines[2].node().pool().contains(id))
            .count(),
        ids.len()
    );
}

/// Transactions announced to a node WHILE it syncs are not fetched then (they would be judged against a chain far behind),
/// but remembered, and asked for from whoever announced them once the sync is over (`fetch_deferred_txs`).
#[test]
fn transactions_announced_during_a_sync_are_asked_for_once_it_is_over() {
    let rigs = SimRig::rigs("deferred", 3);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), cfg(true, 100, 13));
    sim.connect(0, 1);
    sim.run_for(5 * SEC);
    while sim.tip(0).0 < 80 {
        sim.mine(0, None);
        assert!(sim.run_until(60 * SEC, |s| s.tip(1) == s.tip(0)));
        sim.run_for(61 * SEC);
    }
    let t = make_tx(&sim, 0, 900);
    let id = tx_id(&t).unwrap();
    // node 2 comes back far behind and starts to sync from node 1; a peer tells it of a transaction meanwhile
    let spy = sim.add_hostile(2, "99.9.0.3:8333");
    sim.hostile_send(spy, hello(&rigs[2]));
    sim.connect(2, 1);
    assert!(sim.run_until(10 * SEC, |s| s.engines[2].is_syncing()));
    sim.hostile_send(spy, Message::NewTx { ids: vec![id] });
    sim.run_for(50);
    let asked = |s: &Sim<'_>| {
        s.hostiles[spy]
            .inbox
            .iter()
            .any(|m| matches!(m, Message::GetTxs { ids } if ids.contains(&id)))
    };
    if sim.engines[2].is_syncing() {
        assert!(!asked(&sim), "not fetched during the sync");
    }
    // once in step it asks the peer that told it
    assert!(sim.run_until(120 * SEC, |s| !s.engines[2].is_syncing()
        && s.tip(2) == s.tip(0)));
    assert!(
        sim.run_until(5 * SEC, |s| asked(s)),
        "asked for it after the sync"
    );
}

/// A peer that becomes ready is told what waits in our pool in announcements of at most 64 ids: a 0.4.x node asks for a
/// whole announcement in one `get_txs`, and a request for more than 64 is refused (and costs the asker), so a bigger chunk
/// would make every old peer misbehave.
#[test]
fn the_pool_is_announced_to_a_new_peer_in_chunks_an_old_node_can_ask_for() {
    let rigs = SimRig::rigs("push-chunks", 2);
    let mut sim = Sim::new(&rigs, T0, SimConfig::default(), cfg(true, 100, 14));
    sim.connect(0, 1);
    sim.run_for(5 * SEC);
    ready_for_spends(&mut sim);
    let ids: Vec<[u8; 32]> = (0..70)
        .map(|k| {
            let t = make_tx(&sim, 0, 1000 + k);
            let id = tx_id(&t).unwrap();
            sim.submit_tx(0, t);
            id
        })
        .collect();
    assert!(sim.run_until(300 * SEC, |s| ids
        .iter()
        .all(|id| s.engines[0].node().pool().contains(id))));
    let spy = sim.add_hostile(0, "99.9.0.4:8333");
    sim.hostile_send(spy, hello(&rigs[0]));
    sim.run_for(SEC);
    let pushed: Vec<Vec<[u8; 32]>> = sim.hostiles[spy]
        .inbox
        .iter()
        .filter_map(|m| match m {
            Message::NewTx { ids } => Some(ids.clone()),
            _ => None,
        })
        .collect();
    assert!(pushed.len() >= 2, "{} announcements", pushed.len());
    assert!(pushed
        .iter()
        .all(|c| c.len() <= tenero_net::message::MAX_TXS));
    let all: std::collections::HashSet<[u8; 32]> = pushed.into_iter().flatten().collect();
    assert!(
        ids.iter().all(|id| all.contains(id)),
        "every waiting transaction"
    );
    // and asking for one of those chunks, as a 0.4.x node does, is answered in full
    let chunk: Vec<[u8; 32]> = ids[..tenero_net::message::MAX_TXS].to_vec();
    sim.hostile_send(spy, Message::GetTxs { ids: chunk });
    sim.run_for(SEC);
    let got: usize = sim.hostiles[spy]
        .inbox
        .iter()
        .filter_map(|m| match m {
            Message::Txs { txs } => Some(txs.len()),
            _ => None,
        })
        .sum();
    assert_eq!(got, tenero_net::message::MAX_TXS);
    assert!(!sim.hostiles[spy].disconnected);
}
