//! The window drawn with no screen: egui runs a frame into shapes and these tests read the text out of them. They check
//! that every state draws without panicking and says what it should. They do NOT check how anything looks (that is by
//! hand, on the owner's machine).

use eframe::egui::{self, Shape};
use tenero_app::control::{BlockSummary, ChainStats, NodeInfo, NodeKind};
use tenero_core::u256::U256;
use tenero_explorer::core::{Snapshot, Source};
use tenero_explorer::ui::{App, POOL_ROWS};
use tenero_explorer::worker::{Update, Worker};
use tenero_node::PoolEntry;

const NOW: u64 = 1_800_000_000;

fn texts(shapes: &[egui::epaint::ClippedShape]) -> String {
    fn walk(s: &Shape, out: &mut String) {
        match s {
            Shape::Text(t) => {
                out.push_str(t.galley.text());
                out.push('\n');
            }
            Shape::Vec(v) => v.iter().for_each(|s| walk(s, out)),
            _ => {}
        }
    }
    let mut out = String::new();
    for c in shapes {
        walk(&c.shape, &mut out);
    }
    out
}

fn source() -> Source {
    Source {
        data_dir: "D:/node".into(),
        control: "127.0.0.1:38342".parse().unwrap(),
        found_by: "a test".into(),
    }
}

fn frame(ctx: &egui::Context, app: &mut App) -> String {
    let mut last = String::new();
    for _ in 0..2 {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(1150.0, 20_000.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| app.draw(ui));
        out.textures_delta.clear();
        last = texts(&out.shapes);
    }
    last
}

fn snapshot(pool: usize) -> Snapshot {
    // a block a minute, 2^24 attempts each, from height 812 down
    let blocks: Vec<BlockSummary> = (782..=812u64)
        .rev()
        .map(|h| BlockSummary {
            height: h,
            id: [(h % 251) as u8; 32],
            timestamp: NOW - 60 * (812 - h) - 30,
            target: {
                let mut t = [0u8; 32];
                t[2] = 1;
                t
            },
            cumulative_work: U256::from_u64(h << 24).to_be_bytes(),
            size: 400 + 1_500 * (h % 3),
            tx_count: (h % 3) as u32,
            coinbase_total: 2_000_000_000 + 1_000 * (h % 3),
        })
        .collect();
    let mut target = [0u8; 32];
    target[2] = 1;
    Snapshot {
        info: NodeInfo {
            height: 812,
            tip_id: blocks[0].id,
            peers: 2,
            inbound: 0,
            pruned_below: 0,
            mempool_txs: pool as u32,
            syncing: false,
            kind: NodeKind::Archive,
            network: "beta".into(),
            version: "0.2.0-beta.3".into(),
        },
        stats: ChainStats {
            height: 812,
            next_target: target,
            cumulative_work: blocks[0].cumulative_work,
            next_reward: 2_000_000_000,
            emitted: 812 * 2_000_000_000,
            max_supply: 2_000_000_000_000_000,
            tail_reward: 50_000_000,
            block_time: 60,
        },
        pool: (0..pool.min(4096))
            .map(|i| PoolEntry {
                id: [0x40 + (i % 100) as u8; 32],
                received: if i == 1 { 0 } else { NOW - 5 },
                fee: 300_000,
                size: 2_000,
            })
            .collect(),
        pool_total: pool as u32,
        blocks,
        taken_at: NOW - 2,
    }
}

#[test]
fn a_window_with_no_answer_yet_says_it_is_asking() {
    let ctx = egui::Context::default();
    let (w, _tx) = Worker::detached();
    let mut app = App::with_worker(w, Ok(source()), Box::new(|| NOW));
    let t = frame(&ctx, &mut app);
    assert!(t.contains("TEST NETWORK. NO VALUE. UNAUDITED."), "{t}");
    assert!(t.contains("Asking the node at 127.0.0.1:38342"), "{t}");
    assert!(t.contains("Nothing to show yet"), "{t}");
}

#[test]
fn a_window_that_cannot_find_the_node_says_why_and_how() {
    let ctx = egui::Context::default();
    let (w, _tx) = Worker::detached();
    let mut app = App::with_worker(w, Err("unknown argument `--x`".into()), Box::new(|| NOW));
    let t = frame(&ctx, &mut app);
    assert!(
        t.contains("Cannot find the node: unknown argument `--x`"),
        "{t}"
    );
    assert!(t.contains("--data FOLDER"), "the usage is shown: {t}");
}

#[test]
fn a_snapshot_shows_the_numbers_the_pool_and_the_blocks() {
    let ctx = egui::Context::default();
    let (w, tx) = Worker::detached();
    let mut app = App::with_worker(w, Ok(source()), Box::new(|| NOW));
    tx.send(Update::Snapshot(Box::new(snapshot(3)))).unwrap();
    let t = frame(&ctx, &mut app);
    // the node and the numbers
    assert!(t.contains("beta network, version 0.2.0-beta.3"), "{t}");
    assert!(
        t.contains("in step with its peers, 2 peers. Updated 2 s ago."),
        "{t}"
    );
    assert!(t.contains("812"), "{t}");
    // 2^24 attempts a block: the difficulty, exact and short
    assert!(
        t.contains("16777216 attempts a block") && t.contains("16.8 M"),
        "{t}"
    );
    // 2^24 attempts a minute, estimated over the 30-block window
    assert!(t.contains("Network hash rate (estimate)"), "{t}");
    assert!(
        t.contains("280 kH/s") && t.contains("over the last 30 blocks"),
        "{t}"
    );
    assert!(t.contains("60 s") && t.contains("aim: 60 s"), "{t}");
    assert!(t.contains("16,240 TNR") && t.contains("0.081 %"), "{t}");
    assert!(t.contains("20 TNR") && t.contains("0.5 TNR tail"), "{t}");
    // the pool: hash, time (or unknown), fee, rate, size
    assert!(t.contains("Transaction pool (3)"), "{t}");
    assert!(t.contains("4040404040…404040"), "{t}");
    assert!(t.contains("5 s ago") && t.contains("unknown"), "{t}");
    assert!(
        t.contains("0.003 TNR") && t.contains("0.0015 TNR /kB") && t.contains("2.00 kB"),
        "{t}"
    );
    // the blocks: height, time, size, transactions, paid, hash
    assert!(t.contains("Latest blocks"), "{t}");
    assert!(t.contains("30 s ago"), "{t}");
    assert!(t.contains("3.40 kB") && t.contains("20.00002 TNR"), "{t}");
    assert!(
        t.contains(&tenero_explorer::core::short_hex(&[(812 % 251) as u8; 32])),
        "{t}"
    );
    // 30 rows of blocks, not the 31 fetched for the hash rate
    assert!(t.contains("\n783\n") && !t.contains("\n782\n"), "{t}");
    // and what it cannot know is said
    assert!(t.contains("The hash rate is an ESTIMATE"), "{t}");
}

#[test]
fn an_empty_pool_and_a_long_one_are_both_said_plainly() {
    let ctx = egui::Context::default();
    let (w, tx) = Worker::detached();
    let mut app = App::with_worker(w, Ok(source()), Box::new(|| NOW));
    tx.send(Update::Snapshot(Box::new(snapshot(0)))).unwrap();
    assert!(frame(&ctx, &mut app).contains("The pool is empty"));
    // more than the node lists, and more than the window shows
    let mut big = snapshot(500);
    big.pool_total = 9_000;
    tx.send(Update::Snapshot(Box::new(big))).unwrap();
    let t = frame(&ctx, &mut app);
    assert!(t.contains("Transaction pool (9,000)"), "{t}");
    assert_eq!(POOL_ROWS, 200);
    assert!(t.contains("and 8,800 more with lower fee rates"), "{t}");
}

#[test]
fn the_genesis_block_shows_no_time_of_its_own() {
    let ctx = egui::Context::default();
    let (w, tx) = Worker::detached();
    let mut app = App::with_worker(w, Ok(source()), Box::new(|| NOW));
    let mut young = snapshot(0);
    young.blocks.retain(|b| b.height >= 810);
    young.blocks.push(BlockSummary {
        height: 0,
        id: [0xcd; 32],
        timestamp: 0,
        target: [0; 32],
        cumulative_work: [0; 32],
        size: 146,
        tx_count: 0,
        coinbase_total: 0,
    });
    tx.send(Update::Snapshot(Box::new(young))).unwrap();
    let t = frame(&ctx, &mut app);
    assert!(
        t.contains("the genesis block") && t.contains("genesis"),
        "{t}"
    );
    assert!(!t.contains("1970"), "{t}");
}

#[test]
fn a_lost_node_is_said_and_the_last_numbers_stay() {
    let ctx = egui::Context::default();
    let (w, tx) = Worker::detached();
    let mut app = App::with_worker(w, Ok(source()), Box::new(|| NOW));
    tx.send(Update::Snapshot(Box::new(snapshot(1)))).unwrap();
    tx.send(Update::Problem(
        "the node did not answer: lost the node".into(),
    ))
    .unwrap();
    let t = frame(&ctx, &mut app);
    assert!(t.contains("the node did not answer: lost the node"), "{t}");
    assert!(t.contains("Latest blocks"), "{t}");
}
