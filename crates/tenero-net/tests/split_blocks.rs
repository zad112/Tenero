//! A request for blocks whose size together passes the wire's frame ceiling (16 MiB) used to fail to encode, and the transport
//! only logged it: the requester never got an answer. Found by reading the engine while fuzzing it (M9). The reply is now
//! split into as many `blocks` messages as it takes; and since version 2 of the protocol a block bigger than a whole reply goes
//! as a compact block, its transactions in pieces (`split_txs`).

use tenero_core::v3::{Block, Transaction, Wire};
use tenero_core::vectors::{hex, load};
use tenero_net::sim::{Sim, SimConfig, SimRig};
use tenero_net::{
    encode, split_blocks, split_txs, CompactBlock, EngineConfig, Message, BLOCKS_REPLY_BYTES,
    MAX_FRAME,
};

const T0: u64 = 1_700_000_000;

/// A block of about `txs` * 75 KB: the vector block with copies of the longest transaction the format allows (MAX_TX_SIZE bytes) added.
fn big_block(txs: usize, salt: u8) -> Block {
    let v = load("v3_serialization").unwrap();
    let cases = v["valid"].as_array().unwrap();
    let find = |kind: &str, note: &str| {
        cases
            .iter()
            .find(|c| c["kind"] == kind && c["note"].as_str().unwrap().contains(note))
            .map(|c| hex(c["hex"].as_str().unwrap()).unwrap())
            .unwrap()
    };
    let mut b = Block::from_bytes(&find("block", "")).unwrap();
    let tx = Transaction::from_bytes(&find("transaction", "exactly MAX_TX_SIZE")).unwrap();
    b.transactions = vec![tx; txs];
    b.header.nonce = u64::from(salt);
    b
}

fn size_of(b: &Block) -> usize {
    b.to_bytes().unwrap().len()
}

#[test]
fn three_blocks_of_ten_megabytes_cannot_be_one_message_but_can_be_three() {
    let blocks: Vec<Block> = (0..3).map(|i| big_block(140, i)).collect();
    assert!(size_of(&blocks[0]) > 9_000_000 && size_of(&blocks[0]) < 11_000_000);
    // the old behaviour: one reply with all of them does not fit a frame
    assert!(
        encode(&Message::Blocks {
            blocks: blocks.clone()
        })
        .is_err(),
        "thirty megabytes in one frame"
    );
    let (groups, too_big) = split_blocks(blocks.clone(), BLOCKS_REPLY_BYTES);
    assert!(too_big.is_empty());
    assert_eq!(
        groups.len(),
        3,
        "each is over half the budget, so each goes alone"
    );
    for g in &groups {
        let frame =
            encode(&Message::Blocks { blocks: g.clone() }).expect("each reply fits a frame");
        assert!(frame.len() <= MAX_FRAME + 4);
    }
    // nothing lost, nothing repeated, the order kept
    let flat: Vec<Block> = groups.into_iter().flatten().collect();
    assert_eq!(flat, blocks);
}

/// The ceiling of the rules (48 MiB of transactions, `rules::MAX_BLOCK_BYTES`) leaves no block that cannot be sent: one far over
/// a frame goes as a compact block (its ids) and its transactions in replies that each fit a frame.
#[test]
fn a_block_as_large_as_the_rules_allow_is_sent_compact_and_in_pieces() {
    use tenero_core::v3::rules::MAX_BLOCK_BYTES;
    // 671 transactions of the longest the format allows (75,000 bytes each): just under the ceiling
    let b = big_block(671, 1);
    let body: usize = b
        .transactions
        .iter()
        .map(|t| t.to_bytes().unwrap().len())
        .sum();
    assert!(
        body as u64 <= MAX_BLOCK_BYTES && body as u64 > MAX_BLOCK_BYTES - 80_000,
        "{body}"
    );
    assert!(encode(&Message::Blocks {
        blocks: vec![b.clone()]
    })
    .is_err());
    let compact = Message::Compact(Box::new(CompactBlock {
        header: b.header.clone(),
        coinbase: b.coinbase.clone(),
        tx_ids: b
            .transactions
            .iter()
            .map(|t| tenero_core::v3::ids::tx_id(t).unwrap())
            .collect(),
    }));
    assert!(encode(&compact).unwrap().len() < 64 * 1024);
    let groups = split_txs(b.transactions.clone(), BLOCKS_REPLY_BYTES);
    assert!(groups.len() >= 6, "{} replies", groups.len());
    for g in &groups {
        let frame = encode(&Message::BlockTxs {
            block_id: [1; 32],
            txs: g.clone(),
        })
        .expect("each reply fits a frame");
        assert!(frame.len() <= MAX_FRAME + 4);
    }
    assert_eq!(
        groups.into_iter().flatten().collect::<Vec<_>>(),
        b.transactions
    );
}

#[test]
fn blocks_are_packed_up_to_the_budget_and_no_further() {
    let blocks: Vec<Block> = (0..4).map(|i| big_block(10, i)).collect();
    let one = size_of(&blocks[0]);
    assert!(blocks.iter().all(|b| size_of(b) == one));
    // room for exactly two
    let (g, _) = split_blocks(blocks.clone(), 2 * one);
    assert_eq!(g.iter().map(Vec::len).collect::<Vec<_>>(), vec![2, 2]);
    // one byte short of two: one each
    let (g, _) = split_blocks(blocks.clone(), 2 * one - 1);
    assert_eq!(g.iter().map(Vec::len).collect::<Vec<_>>(), vec![1, 1, 1, 1]);
    // room for all four
    let (g, _) = split_blocks(blocks.clone(), 4 * one);
    assert_eq!(g.iter().map(Vec::len).collect::<Vec<_>>(), vec![4]);
    // a budget smaller than a block still sends every block, alone
    let (g, t) = split_blocks(blocks, 1);
    assert_eq!(g.len(), 4);
    assert!(t.is_empty());
}

#[test]
fn a_block_too_big_for_any_frame_is_set_apart_and_the_rest_still_go() {
    let huge = big_block(520, 9); // about 17 MB
    assert!(size_of(&huge) > MAX_FRAME);
    let ok = big_block(10, 1);
    let (groups, too_big) = split_blocks(
        vec![ok.clone(), huge.clone(), ok.clone()],
        BLOCKS_REPLY_BYTES,
    );
    assert_eq!(too_big, vec![huge]);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0], vec![ok.clone(), ok]);
}

#[test]
fn nothing_to_send_is_no_message() {
    let (groups, too_big) = split_blocks(vec![], BLOCKS_REPLY_BYTES);
    assert!(groups.is_empty() && too_big.is_empty());
}

/// The engine, end to end: a node whose replies are smaller than any block sends every block in compact form, and is still
/// synced from.
#[test]
fn a_node_that_answers_in_many_small_replies_is_still_synced_from() {
    let rigs = SimRig::rigs("split-sync", 2);
    let small = EngineConfig {
        // every block bigger than a reply: each goes compact
        blocks_reply_bytes: 1,
        ..EngineConfig::default()
    };
    let mut sim = Sim::with_configs(
        &rigs,
        T0,
        SimConfig::default(),
        vec![small, EngineConfig::default()],
    );
    sim.mine_chain(0, 40);
    assert!(sim.connect(0, 1));
    assert!(
        sim.run_until(120_000, |s| s.tip(1).0 == 40),
        "node 1 reached height {}",
        sim.tip(1).0
    );
    assert!(sim.agree(&[0, 1]));
    let sent = sim.engines[0]
        .stats
        .sent
        .get("compact")
        .copied()
        .unwrap_or(0);
    assert!(sent >= 40, "{sent} compact blocks for 40 blocks");
    assert!(!sim.engines[0].stats.sent.contains_key("blocks"));
    assert_eq!(sim.engines[1].stats.bans, 0);
}
