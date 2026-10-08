//! The window: the chain's numbers at the top, then the pool, then the latest blocks. Everything shown comes from the
//! latest [`Snapshot`]; the words and numbers are made in `core.rs`. A hash is shown cut short; hovering shows it whole and a
//! click copies it.

use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, Color32, RichText};
use tenero_app::control::BlockSummary;
use tenero_gui::view::BANNER;
use tenero_node::PoolEntry;

use crate::core::*;
use crate::worker::{unix_now, Update, Worker};

const AMBER: Color32 = Color32::from_rgb(230, 160, 30);
const RED: Color32 = Color32::from_rgb(220, 70, 70);
const GREEN: Color32 = Color32::from_rgb(90, 190, 110);
const GREY: Color32 = Color32::from_rgb(150, 150, 150);

/// The most pooled transactions listed on screen (the rest are counted).
pub const POOL_ROWS: usize = 200;

pub struct App {
    worker: Worker,
    /// Where the node is, in words, or why it could not be found.
    source: Result<Source, String>,
    snap: Option<Box<Snapshot>>,
    problem: Option<String>,
    /// The last hash copied, to say so.
    copied: Option<String>,
    /// This computer's clock, for "ago" (a test sets it).
    clock: Box<dyn Fn() -> u64>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, source: Result<Source, String>) -> App {
        let ctx = cc.egui_ctx.clone();
        let worker = match &source {
            Ok(s) => Worker::spawn(s.clone(), REFRESH, Arc::new(move || ctx.request_repaint())),
            // nothing to ask: the window says why
            Err(_) => Worker::detached().0,
        };
        App::with_worker(worker, source, Box::new(unix_now))
    }

    /// For tests: a window fed by hand.
    pub fn with_worker(
        worker: Worker,
        source: Result<Source, String>,
        clock: Box<dyn Fn() -> u64>,
    ) -> App {
        App {
            worker,
            source,
            snap: None,
            problem: None,
            copied: None,
            clock,
        }
    }

    fn drain(&mut self) {
        while let Some(u) = self.worker.try_recv() {
            match u {
                Update::Snapshot(s) => {
                    self.snap = Some(s);
                    self.problem = None;
                }
                Update::Problem(p) => self.problem = Some(p),
            }
        }
    }

    /// One frame of the whole window.
    pub fn draw(&mut self, ui: &mut egui::Ui) {
        self.drain();
        // the "ago" texts move on with the clock
        ui.ctx().request_repaint_after(Duration::from_secs(1));
        egui::Panel::top("banner").show(ui, |ui| {
            egui::Frame::new()
                .fill(Color32::from_rgb(70, 45, 0))
                .inner_margin(6.0)
                .show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new(BANNER).strong().color(AMBER).size(17.0));
                        ui.label(
                            RichText::new("A block explorer: what ONE node on this computer says.")
                                .small()
                                .color(AMBER),
                        );
                    });
                });
            ui.add_space(2.0);
            self.status_bar(ui);
            ui.add_space(2.0);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| self.body(ui));
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            match &self.source {
                Err(e) => {
                    ui.colored_label(RED, format!("Cannot find the node: {e}"));
                    ui.label(RichText::new(USAGE).small().color(GREY));
                    return;
                }
                Ok(s) => {
                    let now = (self.clock)();
                    let (txt, col) = match (&self.snap, &self.problem) {
                        (_, Some(p)) => (p.clone(), RED),
                        (None, None) => (format!("Asking the node at {}…", s.control), AMBER),
                        (Some(snap), None) => {
                            let i = &snap.info;
                            let state = if i.syncing {
                                "catching up"
                            } else {
                                "in step with its peers"
                            };
                            (
                                format!(
                                    "Node {} ({} network, version {}): {state}, {} peers. Updated {}.",
                                    s.control,
                                    i.network,
                                    i.version,
                                    i.peers,
                                    ago_text(now, snap.taken_at)
                                ),
                                if i.syncing { AMBER } else { GREEN },
                            )
                        }
                    };
                    ui.colored_label(col, txt);
                    if ui.small_button("Refresh now").clicked() {
                        self.worker.refresh_now();
                    }
                    ui.label(
                        RichText::new(format!("found through {}", s.found_by))
                            .small()
                            .color(GREY),
                    );
                }
            }
            if let Some(c) = &self.copied {
                ui.label(RichText::new(format!("copied {c}")).small().color(GREY));
            }
        });
    }

    fn body(&mut self, ui: &mut egui::Ui) {
        let Some(snap) = self.snap.clone() else {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Nothing to show yet.").color(GREY));
                ui.label(
                    RichText::new(
                        "The explorer needs a node running on this computer: start it from the wallet app's Node tab, \
                         or run tenerod.",
                    )
                    .color(GREY),
                );
            });
            return;
        };
        let now = (self.clock)();
        self.stats(ui, &snap);
        ui.add_space(12.0);
        self.pool(ui, &snap, now);
        ui.add_space(12.0);
        self.blocks(ui, &snap, now);
        ui.add_space(12.0);
        ui.label(
            RichText::new(
                "The difficulty is exact: the expected number of proof-of-work attempts for the next block. The hash rate \
                 is an ESTIMATE: the work of the last blocks divided by the time their timestamps say they took. Miners \
                 write those timestamps and luck matters over a few blocks, so read it as a rough figure. Emitted is the \
                 schedule's total (no penalty subtracted). Everything here is what one node says; it cannot prove it is \
                 on the best chain.",
            )
            .small()
            .color(GREY),
        );
    }

    fn stats(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let st = &snap.stats;
        let diff = difficulty(&st.next_target);
        let rate = estimate_hashrate(&snap.blocks, HASHRATE_WINDOW);
        let pool_bytes: u64 = snap.pool.iter().map(|t| t.size).sum();
        let mut cards: Vec<(&str, String, String)> = vec![
            (
                "Height",
                grouped(st.height),
                format!("tip {}", short_hex(&snap.info.tip_id)),
            ),
            (
                "Difficulty",
                diff.map_or("?".into(), |d| si(be_to_f64(&d.to_be_bytes()))),
                diff.map_or("?".into(), |d| {
                    format!("{} attempts a block", d.to_dec_string())
                }),
            ),
            (
                "Network hash rate (estimate)",
                rate.map_or("not enough blocks".into(), |r| hashrate_text(r.per_second)),
                rate.map_or(String::new(), |r| {
                    format!("over the last {} blocks", r.blocks)
                }),
            ),
            (
                "Block time",
                rate.map_or("?".into(), |r| format!("{:.0} s", r.mean_block_time())),
                format!("aim: {} s", st.block_time),
            ),
            (
                "Emitted",
                coins_text(st.emitted),
                format!(
                    "{} of the {} cap",
                    emitted_share_text(st.emitted, st.max_supply),
                    coins_text(st.max_supply)
                ),
            ),
            (
                "Block reward",
                coins_text(st.next_reward),
                format!("then a {} tail for ever", coins_text(st.tail_reward)),
            ),
        ];
        cards.push((
            "Transaction pool",
            format!("{} transactions", grouped(u64::from(snap.pool_total))),
            if snap.pool.len() as u64 == u64::from(snap.pool_total) {
                bytes_text(pool_bytes)
            } else {
                format!("{} in the best {}", bytes_text(pool_bytes), snap.pool.len())
            },
        ));
        ui.horizontal_wrapped(|ui| {
            for (title, value, note) in cards {
                egui::Frame::group(ui.style())
                    .inner_margin(8.0)
                    .show(ui, |ui| {
                        ui.set_min_width(170.0);
                        ui.vertical(|ui| {
                            ui.label(RichText::new(title).small().color(GREY));
                            ui.label(RichText::new(value).strong().size(19.0));
                            if !note.is_empty() {
                                ui.label(RichText::new(note).small().color(GREY));
                            }
                        });
                    });
            }
        });
    }

    /// A hash cut short; hovering shows it whole, a click copies it.
    fn hash_cell(&mut self, ui: &mut egui::Ui, id: &[u8; 32]) {
        let r = ui
            .add(
                egui::Label::new(RichText::new(short_hex(id)).monospace())
                    .sense(egui::Sense::click()),
            )
            .on_hover_text(format!("{}\n(click to copy)", hex(id)));
        if r.clicked() {
            ui.ctx().copy_text(hex(id));
            self.copied = Some(short_hex(id));
        }
    }

    fn pool(&mut self, ui: &mut egui::Ui, snap: &Snapshot, now: u64) {
        ui.heading(format!(
            "Transaction pool ({})",
            grouped(u64::from(snap.pool_total))
        ));
        if snap.pool.is_empty() {
            ui.label(
                RichText::new(
                    "The pool is empty: every transaction this node knows is in a block.",
                )
                .color(GREY),
            );
            return;
        }
        let shown: &[PoolEntry] = &snap.pool[..snap.pool.len().min(POOL_ROWS)];
        egui::Grid::new("pool")
            .striped(true)
            .num_columns(6)
            .spacing([18.0, 4.0])
            .show(ui, |ui| {
                for h in ["Hash", "Received", "", "Fee", "Fee rate", "Size"] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for t in shown {
                    self.hash_cell(ui, &t.id);
                    if t.received == 0 {
                        ui.label(RichText::new("unknown").color(GREY));
                        ui.label("");
                    } else {
                        ui.label(utc_text(t.received));
                        ui.label(RichText::new(ago_text(now, t.received)).color(GREY));
                    }
                    ui.label(coins_text(t.fee));
                    ui.label(fee_rate_text(t.fee, t.size));
                    ui.label(bytes_text(t.size));
                    ui.end_row();
                }
            });
        let unshown = u64::from(snap.pool_total).saturating_sub(shown.len() as u64);
        if unshown > 0 {
            ui.label(
                RichText::new(format!(
                    "and {} more with lower fee rates",
                    grouped(unshown)
                ))
                .color(GREY),
            );
        }
        ui.label(
            RichText::new("Best fee rate first: the order a block takes them in. \"Received\" is when this node got it.")
                .small()
                .color(GREY),
        );
    }

    fn blocks(&mut self, ui: &mut egui::Ui, snap: &Snapshot, now: u64) {
        ui.heading("Latest blocks");
        let shown: Vec<&BlockSummary> = snap.blocks.iter().take(LATEST_BLOCKS as usize).collect();
        egui::Grid::new("blocks")
            .striped(true)
            .num_columns(7)
            .spacing([18.0, 4.0])
            .show(ui, |ui| {
                for h in [
                    "Height",
                    "Time",
                    "",
                    "Size",
                    "Transactions",
                    "Coinbase paid",
                    "Hash",
                ] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for b in shown {
                    ui.label(RichText::new(grouped(b.height)).monospace());
                    if b.height == 0 {
                        // its timestamp is the network's, not a block found (0 on the test network)
                        ui.label(RichText::new("the genesis block").color(GREY));
                        ui.label("");
                    } else {
                        ui.label(utc_text(b.timestamp));
                        ui.label(RichText::new(ago_text(now, b.timestamp)).color(GREY));
                    }
                    ui.label(bytes_text(b.size));
                    ui.label(grouped(u64::from(b.tx_count)));
                    if b.height == 0 {
                        ui.label(RichText::new("genesis").color(GREY));
                    } else {
                        ui.label(coins_text(b.coinbase_total));
                    }
                    self.hash_cell(ui, &b.id);
                    ui.end_row();
                }
            });
        ui.label(
            RichText::new(
                "Time is the block's own timestamp, written by its miner. Coinbase paid is the reward and the fees.",
            )
            .small()
            .color(GREY),
        );
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }
}
