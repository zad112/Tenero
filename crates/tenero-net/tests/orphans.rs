//! Blocks that arrive before their ancestors: held, not fetched again, and applied when the ancestors come. A
//! scripted peer serves real blocks in a chosen order.

use tenero_core::u256::U256;
use tenero_core::v3::Block;
use tenero_net::sim::{Sim, SimConfig, SimRig};
use tenero_net::{EngineConfig, Hello, Message, PROTOCOL_VERSION};

const T0: u64 = 1_700_000_000;
const SEC: u64 = 1000;

/// Mines `n` blocks on node 0 of `rigs` (they stay in its store) and returns them, with the simulated time after.
fn mined(rigs: &[SimRig], n: usize) -> (Vec<Block>, Vec<[u8; 32]>, Vec<U256>, u64) {
    let mut sim = Sim::new(rigs, T0, SimConfig::default(), EngineConfig::default());
    sim.mine_chain(0, n);
    let store = &rigs[0].store;
    let mut blocks = Vec::new();
    let mut ids = Vec::new();
    let mut work = Vec::new();
    for h in 1..=n as u64 {
        let index = store.block_index(h).unwrap().unwrap();
        ids.push(index.block_id);
        work.push(U256::from_be_bytes(&index.cumulative_work));
        blocks.push(store.get_block(h).unwrap().unwrap().into_full().unwrap());
    }
    (blocks, ids, work, T0 + (n as u64 + 10) * 60)
}

fn hello(rig: &SimRig, work: U256) -> Message {
    Message::Hello(Hello {
        version: PROTOCOL_VERSION,
        chain_id: rig.store.chain_id(),
        tip_height: 500,
        cumulative_work: work.to_be_bytes(),
        tip_id: [9; 32],
        pruned_below: 0,
        nonce: 0,
    })
}

fn requests_for(sim: &Sim<'_>, h: usize, id: &[u8; 32]) -> usize {
    sim.hostiles[h]
        .inbox
        .iter()
        .filter(|m| match m {
            Message::GetBlocks { ids } => ids.contains(id),
            // a new block is asked for in compact form
            Message::GetCompact { id: x } => x == id,
            _ => false,
        })
        .count()
}

/// A block's compact form (what answers `GetCompact`).
fn compact(b: &Block) -> Message {
    Message::Compact(Box::new(tenero_net::CompactBlock {
        header: b.header.clone(),
        coinbase: b.coinbase.clone(),
        tx_ids: b
            .transactions
            .iter()
            .map(|t| tenero_core::v3::ids::tx_id(t).unwrap())
            .collect(),
    }))
}

/// Node 1 has no connection but a scripted peer; the peer announces block 3 and serves it first, then the ids,
/// then blocks 1 and 2.
#[test]
fn a_block_that_arrives_before_its_ancestors_is_held_not_fetched_again_and_applied_when_they_come()
{
    let rigs = SimRig::rigs("orphan-flow", 2);
    let (blocks, ids, work, start) = mined(&rigs, 3);
    let mut sim = Sim::new(&rigs, start, SimConfig::default(), EngineConfig::default());
    let h = sim.add_hostile(1, "scripted");
    sim.hostile_send(h, hello(&rigs[1], work[2]));
    sim.run_for(SEC);
    sim.hostile_send(
        h,
        Message::NewBlock {
            id: ids[2],
            height: 3,
            cumulative_work: work[2].to_be_bytes(),
        },
    );
    sim.run_for(2 * SEC);
    assert_eq!(requests_for(&sim, h, &ids[2]), 1, "block 3 is asked for");
    // served first, before anything it builds on
    sim.hostile_send(h, compact(&blocks[2]));
    sim.run_for(2 * SEC);
    assert_eq!(sim.tip(1).0, 0, "it cannot be applied yet");
    assert_eq!(sim.engines[1].node().chain().orphan_count(), 1);
    assert!(sim.engines[1].node().chain().is_orphan(&ids[2]));
    // the peer is asked what it has; it lists all three
    assert!(sim.hostiles[h]
        .inbox
        .iter()
        .any(|m| matches!(m, Message::GetBlockIds { .. })));
    sim.hostile_send(
        h,
        Message::BlockIds {
            first_height: 1,
            ids: ids.clone(),
        },
    );
    sim.run_for(2 * SEC);
    // only the two missing blocks are asked for: block 3 is already held
    assert_eq!(requests_for(&sim, h, &ids[0]), 1);
    assert_eq!(requests_for(&sim, h, &ids[1]), 1);
    assert_eq!(
        requests_for(&sim, h, &ids[2]),
        1,
        "not asked for a second time"
    );
    sim.hostile_send(
        h,
        Message::Blocks {
            blocks: vec![blocks[0].clone(), blocks[1].clone()],
        },
    );
    sim.run_for(2 * SEC);
    assert_eq!(
        sim.tip(1).0,
        3,
        "the held block went in behind its ancestors"
    );
    assert_eq!(sim.tip(1).1, ids[2]);
    assert_eq!(sim.engines[1].stats.blocks_applied, 3);
    assert_eq!(sim.engines[1].node().chain().orphan_count(), 0);
}

/// A chain of orphans: the newest two first, then the block they hang from.
#[test]
fn a_run_of_orphans_is_applied_in_order_when_the_first_ancestor_arrives_and_the_tip_is_announced() {
    let rigs = SimRig::rigs("orphan-run", 3);
    let (blocks, ids, work, start) = mined(&rigs[..1], 4);
    let mut sim = Sim::new(&rigs, start, SimConfig::default(), EngineConfig::default());
    // node 1 is the one that receives out of order; node 2 is a peer that should hear of the new tip afterwards
    assert!(sim.connect(2, 1));
    sim.run_for(3 * SEC);
    let h = sim.add_hostile(1, "scripted");
    sim.hostile_send(h, hello(&rigs[1], work[3]));
    sim.run_for(SEC);
    for i in [3usize, 2] {
        sim.hostile_send(
            h,
            Message::NewBlock {
                id: ids[i],
                height: i as u64 + 1,
                cumulative_work: work[i].to_be_bytes(),
            },
        );
        sim.run_for(SEC);
        sim.hostile_send(
            h,
            Message::Blocks {
                blocks: vec![blocks[i].clone()],
            },
        );
        sim.run_for(SEC);
    }
    assert_eq!(sim.engines[1].node().chain().orphan_count(), 2);
    assert_eq!(sim.tip(1).0, 0);
    // blocks 1 and 2... the peer sends block 1, then block 2: block 2 connects, and the two orphans after it follow
    sim.hostile_send(
        h,
        Message::BlockIds {
            first_height: 1,
            ids: ids[..2].to_vec(),
        },
    );
    sim.run_for(SEC);
    sim.hostile_send(
        h,
        Message::Blocks {
            blocks: vec![blocks[0].clone(), blocks[1].clone()],
        },
    );
    sim.run_for(3 * SEC);
    assert_eq!(sim.tip(1).0, 4);
    assert_eq!(sim.tip(1).1, ids[3]);
    assert_eq!(sim.engines[1].stats.blocks_applied, 4);
    // and the new tip was announced on: node 2 has it
    assert!(sim.run_until(30 * SEC, |s| s.tip(2).0 == 4));
}

/// An orphan that hangs from a block kept aside (not on the chain) still connects, and when that makes the branch
/// heavier the reorganisation is announced to peers at once (not left to the periodic repeat).
#[test]
fn an_orphan_behind_a_block_kept_aside_connects_and_the_reorganisation_it_causes_is_announced() {
    let rigs = SimRig::rigs("orphan-side", 4);
    // chain A: 3 blocks, mined on node 0; chain B: 4 blocks (more work), mined on node 1; both from the genesis block
    let mut pre = Sim::new(&rigs, T0, SimConfig::default(), EngineConfig::default());
    pre.mine_chain(0, 3);
    pre.mine_chain(1, 4);
    drop(pre);
    let b: Vec<(Block, [u8; 32], U256)> = (1..=4u64)
        .map(|h| {
            let index = rigs[1].store.block_index(h).unwrap().unwrap();
            (
                rigs[1]
                    .store
                    .get_block(h)
                    .unwrap()
                    .unwrap()
                    .into_full()
                    .unwrap(),
                index.block_id,
                U256::from_be_bytes(&index.cumulative_work),
            )
        })
        .collect();
    let start = T0 + 30 * 60;
    let mut sim = Sim::new(&rigs, start, SimConfig::default(), EngineConfig::default());
    // X (node 2) takes chain A from node 0 and Y (node 3) follows X; then node 0 goes away
    assert!(sim.connect(2, 0));
    assert!(sim.connect(3, 2));
    assert!(sim.run_until(120 * SEC, |s| s.tip(2).0 == 3 && s.tip(3).0 == 3));
    sim.set_online(0, false);
    let a_tip = sim.tip(2).1;
    assert_ne!(a_tip, b[2].1, "two different chains");
    // the scripted peer announces chain B's tip and serves it first, then the rest, as in the flow above
    let h = sim.add_hostile(2, "scripted");
    sim.hostile_send(h, hello(&rigs[2], b[3].2));
    sim.run_for(SEC);
    sim.hostile_send(
        h,
        Message::NewBlock {
            id: b[3].1,
            height: 4,
            cumulative_work: b[3].2.to_be_bytes(),
        },
    );
    sim.run_for(2 * SEC);
    sim.hostile_send(
        h,
        Message::Blocks {
            blocks: vec![b[3].0.clone()],
        },
    );
    sim.run_for(2 * SEC);
    assert!(sim.engines[2].node().chain().is_orphan(&b[3].1));
    sim.hostile_send(
        h,
        Message::BlockIds {
            first_height: 1,
            ids: b.iter().map(|x| x.1).collect(),
        },
    );
    sim.run_for(2 * SEC);
    // blocks 1 to 3 of chain B are kept aside (they tie chain A's work); block 4, waiting behind block 3, then
    // makes chain B heavier, and the chain moves to it
    let announced = |sim: &Sim<'_>| {
        sim.engines[2]
            .stats
            .sent
            .get("new_block")
            .copied()
            .unwrap_or(0)
    };
    let before = announced(&sim);
    sim.hostile_send(
        h,
        Message::Blocks {
            blocks: b[..3].iter().map(|x| x.0.clone()).collect(),
        },
    );
    sim.run_for(SEC);
    assert_eq!(
        sim.tip(2).1,
        b[3].1,
        "the held block went in and the chain moved"
    );
    // and the new tip was announced at once (within that second, not by the periodic repeat)
    assert!(
        announced(&sim) > before,
        "no announcement of the reorganisation"
    );
    assert!(sim.run_until(8 * SEC, |s| s.tip(3).1 == b[3].1));
}
