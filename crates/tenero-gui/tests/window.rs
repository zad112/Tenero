//! The window drawn with no screen: egui runs a frame into a list of shapes, and these tests read the text out of it.
//! This checks that no screen panics in any state and that the owner's rules for what the window says hold on every
//! screen. It does NOT check how anything looks (that is by hand, on the owner's machine).

use eframe::egui::{self, OutputCommand, Shape};
use tenero_app::control::{NodeInfo, NodeKind};
use tenero_app::miner_report::MinerReport;
use tenero_app::ui::NodeLink;
use tenero_gui::backend::Backend;
use tenero_gui::settings::Settings;
use tenero_gui::ui::App;
use tenero_gui::view::*;
use tenero_wallet::{Address, Balance, EntryKind, FeeLevel, Network, SentStatus, ViewTier};
use zeroize::Zeroizing;

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

struct Rig {
    ctx: egui::Context,
    app: App,
    events: std::sync::mpsc::Sender<Event>,
    commands: std::sync::mpsc::Receiver<Cmd>,
}

impl Rig {
    fn new() -> Rig {
        let dir = std::env::temp_dir().join("tenero-gui-window-test");
        let (backend, events, commands) = Backend::detached();
        let settings = Settings::defaults(&dir, tenero_app::config::Network::Test);
        Rig {
            ctx: egui::Context::default(),
            app: App::with_backend(backend, dir, settings, None),
            events,
            commands,
        }
    }

    /// Draws two frames (the second is the one after any layout settles) and returns the text and the clipboard writes.
    fn frame(&mut self) -> (String, Vec<String>) {
        let mut last = (String::new(), Vec::new());
        for _ in 0..2 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(1000.0, 4000.0),
                )),
                ..Default::default()
            };
            let app = &mut self.app;
            let mut out = self.ctx.run_ui(input, |ui| app.draw(ui));
            // nothing paints these frames, so the texture uploads are dropped on purpose (egui checks in debug builds)
            out.textures_delta.clear();
            let copies = out
                .platform_output
                .commands
                .iter()
                .filter_map(|c| match c {
                    OutputCommand::CopyText(t) => Some(t.clone()),
                    _ => None,
                })
                .collect();
            last = (texts(&out.shapes), copies);
        }
        last
    }
}

fn addr(n: u8) -> String {
    // a made-up address of the right shape (the window never checks it, the wallet does)
    format!("TENt{}", format!("{n:02x}").repeat(48))[..99].to_string()
}

fn info(syncing: bool, peers: u32) -> NodeInfo {
    NodeInfo {
        height: 1234,
        tip_id: [7; 32],
        peers,
        inbound: 0,
        pruned_below: 0,
        mempool_txs: 2,
        syncing,
        kind: NodeKind::Archive,
        network: "test".into(),
        version: "0.0.0".into(),
    }
}

fn bal(total: u64, spendable: u64) -> Balance {
    Balance {
        total,
        spendable,
        immature: total - spendable,
        reserved: 0,
    }
}

fn wallet(synced: bool, with_balances: bool) -> WalletView {
    let accounts = (0..2)
        .map(|i| AccountView {
            index: i,
            label: ["Main", "Savings"][i].into(),
            address: addr(i as u8 + 1),
            balance: with_balances.then(|| bal(1_500_000_000 * (i as u64 + 1), 1_000_000_000)),
        })
        .collect();
    let to = Address::parse(
        &{
            // a real address from a real key, so the history row can print it
            let w = tenero_wallet::Wallet::from_seed(&[3; 32], Network::Test, 0);
            w.address().to_text()
        },
        Network::Test,
    )
    .unwrap();
    let history = vec![
        HistoryRow {
            account: 0,
            account_label: "Main".into(),
            kind: EntryKind::Sent {
                to,
                fee: 400_000,
                status: SentStatus::Pending,
                time: 1_700_000_000,
            },
            amount: 250_000_000,
            height: 1200,
            id: Some([9; 32]),
            global_index: None,
            has_secret: true,
            note: Some("Rent".into()),
            payment_id: None,
        },
        HistoryRow {
            account: 0,
            account_label: "Main".into(),
            kind: EntryKind::Mined,
            amount: 1_000_000_000,
            height: 1100,
            id: None,
            global_index: Some(3),
            has_secret: false,
            note: None,
            payment_id: None,
        },
        HistoryRow {
            account: 1,
            account_label: "Savings".into(),
            kind: EntryKind::Received,
            amount: 5,
            height: 900,
            id: None,
            global_index: Some(5),
            has_secret: false,
            note: None,
            payment_id: Some([0xab; 8]),
        },
    ];
    WalletView::Unlocked(Box::new(WalletData {
        accounts,
        total: with_balances.then(|| bal(4_500_000_000, 2_000_000_000)),
        history: if with_balances { history } else { Vec::new() },
        requests: vec![RequestView {
            index: 0,
            account: 0,
            account_label: "Main".into(),
            amount: Some(150_000_000),
            label: Some("Rent".into()),
            message: Some("October rent".into()),
            time: 1_700_000_000,
            uri: format!("tenero:{}?amount=1.5&label=Rent", addr(1)),
        }],
        scanned: Some(if synced { 1234 } else { 600 }),
        tip: with_balances.then_some(1234),
        synced,
        has_password: true,
        tier: ViewTier::Full,
    }))
}

/// [`wallet`] (synced, with balances) as a view-only wallet of `tier`.
fn view_only(tier: ViewTier) -> WalletView {
    match wallet(true, true) {
        WalletView::Unlocked(mut d) => {
            d.tier = tier;
            WalletView::Unlocked(d)
        }
        other => other,
    }
}

fn report(stale: bool) -> MinerView {
    MinerView::Running {
        report: Box::new(MinerReport {
            written_at: 1,
            backend: "gpu: NVIDIA GeForce RTX 5070 Ti".into(),
            link: NodeLink::Connected,
            node_height: 1234,
            searching: true,
            s10: Some(34_700.0),
            s60: Some(34_000.0),
            m15: None,
            average: Some(33_900.0),
            found: 2,
            accepted: 1,
            lost_race: 1,
            refused: 0,
            uptime_secs: 3700,
            expected_blocks: 1.5,
            gpu_name: Some("NVIDIA GeForce RTX 5070 Ti".into()),
            gpu_temp_c: Some(61),
            gpu_power_w: Some(231.0),
            gpu_fan_pct: Some(48),
            gpu_core_mhz: Some(2700),
            gpu_mem_mhz: Some(14000),
            gpu_busy_pct: Some(100),
            gpu_limited_by: Some("power cap".into()),
        }),
        stale,
    }
}

fn snap(
    rig: &Rig,
    wallet: WalletView,
    node: NodeView,
    miner: MinerView,
    prepared: Option<Quote>,
) -> Snapshot {
    Snapshot {
        settings: rig.app_settings(),
        wallets: Vec::new(),
        wallet,
        node,
        miner,
        prepared,
        busy: None,
        moving: None,
    }
}

impl Rig {
    fn app_settings(&self) -> Settings {
        Settings::defaults(
            &std::env::temp_dir().join("tenero-gui-window-test"),
            tenero_app::config::Network::Test,
        )
    }
}

fn quote() -> Quote {
    Quote {
        account: 0,
        to: addr(9),
        amount: 250_000_000,
        fee: 602_669,
        change: 749_397_331,
        level: FeeLevel::Normal,
        note: Some("Rent".into()),
        transactions: 1,
        coins: 1,
        unsent_payments: 0,
        unsent_total: 0,
        own: None,
    }
}

fn combine_quote() -> Quote {
    Quote {
        to: String::new(),
        amount: 1_999_000_000,
        change: 0,
        note: None,
        transactions: 1,
        coins: 12,
        own: Some("Combined 12 pieces".into()),
        ..quote()
    }
}

/// Every state the window can be in, with a name for the failure message.
fn states(rig: &Rig) -> Vec<(&'static str, Snapshot)> {
    let run = |syncing| NodeView::Running {
        info: info(syncing, 3),
        ours: true,
    };
    vec![
        (
            "no wallet",
            snap(
                rig,
                WalletView::NoWallet,
                NodeView::Stopped,
                MinerView::Off,
                None,
            ),
        ),
        (
            "locked",
            snap(
                rig,
                WalletView::Locked,
                NodeView::Stopped,
                MinerView::Off,
                None,
            ),
        ),
        (
            "open, no node",
            snap(
                rig,
                wallet(false, false),
                NodeView::Stopped,
                MinerView::Off,
                None,
            ),
        ),
        (
            "open, node starting",
            snap(
                rig,
                wallet(false, false),
                NodeView::Starting,
                MinerView::Off,
                None,
            ),
        ),
        (
            "open, node stopping",
            snap(
                rig,
                wallet(false, true),
                NodeView::Stopping,
                MinerView::Off,
                None,
            ),
        ),
        (
            "open, node failed",
            snap(
                rig,
                wallet(false, false),
                NodeView::Failed {
                    why: "the node stopped with code 2".into(),
                    output: "error: cannot listen".into(),
                },
                MinerView::Off,
                None,
            ),
        ),
        (
            "open, catching up",
            snap(rig, wallet(false, true), run(true), MinerView::Off, None),
        ),
        (
            "open, synced",
            snap(rig, wallet(true, true), run(false), MinerView::Off, None),
        ),
        (
            "open, no peers",
            snap(
                rig,
                wallet(true, true),
                NodeView::Running {
                    info: info(false, 0),
                    ours: false,
                },
                MinerView::Off,
                None,
            ),
        ),
        (
            "mining",
            snap(rig, wallet(true, true), run(false), report(false), None),
        ),
        (
            "mining, stale",
            snap(rig, wallet(true, true), run(false), report(true), None),
        ),
        (
            "miner starting",
            snap(
                rig,
                wallet(true, true),
                run(false),
                MinerView::Starting,
                None,
            ),
        ),
        (
            "miner failed",
            snap(
                rig,
                wallet(true, true),
                run(false),
                MinerView::Failed {
                    why: "the miner stopped with code 1".into(),
                    output: "no GPU".into(),
                },
                None,
            ),
        ),
        (
            "payment waiting for a yes",
            snap(
                rig,
                wallet(true, true),
                run(false),
                MinerView::Off,
                Some(quote()),
            ),
        ),
        (
            "payment made of several transactions, part of it waiting",
            snap(
                rig,
                wallet(true, true),
                run(false),
                MinerView::Off,
                Some(Quote {
                    transactions: 3,
                    coins: 190,
                    unsent_payments: 1,
                    unsent_total: 400_000_000,
                    ..quote()
                }),
            ),
        ),
        (
            "combine waiting for a yes",
            snap(
                rig,
                wallet(true, true),
                run(false),
                MinerView::Off,
                Some(combine_quote()),
            ),
        ),
    ]
}

const FORBIDDEN: &[&str] = &[
    "anonymous",
    "untraceable",
    "invest",
    "profit",
    "get rich",
    "guaranteed",
    "money",
];

#[test]
fn every_screen_in_every_state_draws_and_keeps_the_owners_rules() {
    let mut rig = Rig::new();
    for (state, s) in states(&rig) {
        rig.app.set_snapshot(s);
        for tab in App::tab_names() {
            rig.app.goto(tab);
            let (text, copies) = rig.frame();
            // the banner is on every screen
            assert!(
                text.contains(BANNER),
                "[{state} / {tab}] the banner is missing"
            );
            assert!(
                text.contains("UNAUDITED. Do not rely on their privacy"),
                "[{state} / {tab}] the scheme note is missing"
            );
            // nothing that sells it, and nothing that calls it money (the one allowed use says it is NOT money)
            let lower = text.to_lowercase();
            for w in FORBIDDEN {
                let found = lower.match_indices(w).any(|(i, _)| {
                    let before = &lower[i.saturating_sub(24)..i];
                    !(before.contains("not ")
                        || before.contains("no ")
                        || before.contains("treat these coins as"))
                });
                assert!(!found, "[{state} / {tab}] the word `{w}` appears: {text}");
            }
            // drawing never writes to the clipboard on its own
            assert!(copies.is_empty(), "[{state} / {tab}] copied {copies:?}");
        }
    }
}

#[test]
fn a_balance_is_never_final_while_catching_up_or_without_a_node() {
    let mut rig = Rig::new();
    rig.app.goto("Wallet");

    let s = snap(
        &rig,
        wallet(false, true),
        NodeView::Running {
            info: info(true, 3),
            ours: true,
        },
        MinerView::Off,
        None,
    );
    rig.app.set_snapshot(s);
    let (t, _) = rig.frame();
    assert!(t.contains("NOT FINAL"), "{t}");
    assert!(!t.contains("up to date"), "{t}");

    let s = snap(
        &rig,
        wallet(true, true),
        NodeView::Running {
            info: info(false, 3),
            ours: true,
        },
        MinerView::Off,
        None,
    );
    rig.app.set_snapshot(s);
    let (t, _) = rig.frame();
    assert!(!t.contains("NOT FINAL"), "{t}");
    assert!(t.contains("Wallet: up to date"), "{t}");

    // with no node there is no number at all, only the way to get one
    let s = snap(
        &rig,
        wallet(false, false),
        NodeView::Stopped,
        MinerView::Off,
        None,
    );
    rig.app.set_snapshot(s);
    let (t, _) = rig.frame();
    assert!(
        t.contains("Balance unknown") && t.contains("Start it on the Node tab"),
        "{t}"
    );
    assert!(!t.contains("TNR\n") || !t.contains("Balance\n"), "{t}");
    rig.app.goto("Send");
    let (t, _) = rig.frame();
    assert!(t.contains("not running, so the wallet cannot send"), "{t}");
    rig.app.goto("History");
    let (t, _) = rig.frame();
    assert!(t.contains("The node is not running"), "{t}");
}

#[test]
fn the_confirmation_screen_shows_everything_before_anything_is_sent() {
    let mut rig = Rig::new();
    rig.app.goto("Send");
    let s = snap(
        &rig,
        wallet(true, true),
        NodeView::Running {
            info: info(false, 3),
            ours: true,
        },
        MinerView::Off,
        Some(quote()),
    );
    rig.app.set_snapshot(s);
    let (t, _) = rig.frame();
    for needle in [
        "Nothing has been sent yet",
        "Fee (Normal)",
        "0.00602669 TNR",
        "2.5 TNR",
        "Change back",
        &addr(9),
    ] {
        assert!(
            t.contains(needle),
            "`{needle}` missing from the confirmation screen:\n{t}"
        );
    }
    assert!(t.contains("Send") && t.contains("Back"));
}

#[test]
fn the_confirmation_screen_of_a_combine_says_nobody_is_paid_and_a_split_payment_says_how_many_transactions(
) {
    let mut rig = Rig::new();
    rig.app.goto("Send");
    let running = || NodeView::Running {
        info: info(false, 3),
        ours: true,
    };
    let s = snap(
        &rig,
        wallet(true, true),
        running(),
        MinerView::Off,
        Some(combine_quote()),
    );
    rig.app.set_snapshot(s);
    let (t, _) = rig.frame();
    for needle in [
        "Check the combine",
        "nobody is paid",
        "Combined 12 pieces",
        "Fee (Normal)",
        "The account keeps",
        "Combine",
        "Back",
    ] {
        assert!(t.contains(needle), "`{needle}` missing:\n{t}");
    }
    assert!(
        !t.contains("Check the payment"),
        "a combine is not a payment:\n{t}"
    );
    let s = snap(
        &rig,
        wallet(true, true),
        running(),
        MinerView::Off,
        Some(Quote {
            transactions: 3,
            coins: 190,
            unsent_payments: 1,
            unsent_total: 400_000_000,
            ..quote()
        }),
    );
    rig.app.set_snapshot(s);
    let (t, _) = rig.frame();
    for needle in [
        "made as 3 transactions",
        "190 pieces in all",
        "Only part of this payment can be sent now",
        "4 TNR",
    ] {
        assert!(t.contains(needle), "`{needle}` missing:\n{t}");
    }
}

#[test]
fn the_send_screen_has_a_combine_coins_section() {
    let mut rig = Rig::new();
    rig.app.goto("Send");
    let s = snap(
        &rig,
        wallet(true, true),
        NodeView::Running {
            info: info(false, 3),
            ours: true,
        },
        MinerView::Off,
        None,
    );
    rig.app.set_snapshot(s);
    let (t, _) = rig.frame();
    for needle in ["Combine pieces", "Pieces to combine", "Combine all"] {
        assert!(t.contains(needle), "`{needle}` missing:\n{t}");
    }
}

#[test]
fn the_send_screen_offers_three_fee_levels() {
    let mut rig = Rig::new();
    rig.app.goto("Send");
    let s = snap(
        &rig,
        wallet(true, true),
        NodeView::Running {
            info: info(false, 3),
            ours: true,
        },
        MinerView::Off,
        None,
    );
    rig.app.set_snapshot(s);
    let (t, _) = rig.frame();
    for level in ["Low", "Normal", "High"] {
        assert!(t.contains(&format!("{level} —")), "{level} missing:\n{t}");
    }
    assert!(t.contains("never makes a block come sooner"), "{t}");
}

#[test]
fn the_words_are_shown_on_request_and_never_copied_and_the_mining_notice_is_always_there() {
    let mut rig = Rig::new();
    let s = snap(
        &rig,
        wallet(true, true),
        NodeView::Running {
            info: info(false, 3),
            ours: true,
        },
        MinerView::Off,
        None,
    );
    rig.app.set_snapshot(s);
    let words = "abandon ".repeat(23) + "art";
    rig.events
        .send(Event::Phrase {
            words: Zeroizing::new(words.clone()),
            new: true,
        })
        .unwrap();
    let (t, copies) = rig.frame();
    assert!(
        t.contains("Your 24 words") && t.contains("23. abandon") && t.contains("24. art"),
        "{t}"
    );
    assert!(t.contains("They are not copied anywhere"), "{t}");
    assert!(t.contains("I have written them down"));
    assert!(copies.is_empty());

    // mining says what it uses before it is started
    rig.app.goto("Mining");
    let (t, _) = rig.frame();
    assert!(
        t.contains("Test-network mining") || t.contains("full load"),
        "{t}"
    );
    assert!(t.contains("Start mining"));
}

#[test]
fn the_mining_screen_offers_a_pool_or_mining_alone_and_a_pool_needs_no_node() {
    let mut rig = Rig::new();
    rig.app.goto("Mining");
    // no node running, mining alone: it cannot start
    let mut s = snap(
        &rig,
        wallet(true, true),
        NodeView::Stopped,
        MinerView::Off,
        None,
    );
    rig.app.set_snapshot(s.clone());
    let (t, _) = rig.frame();
    for needle in [
        "Where to mine",
        "On a pool",
        "Alone, on my own node",
        "start the node first",
    ] {
        assert!(
            t.contains(needle),
            "`{needle}` missing:
{t}"
        );
    }
    assert!(
        !t.contains("Pool address"),
        "the pool fields are for pool mode:
{t}"
    );
    // for a pool: the fields, the plain statement of where the reward goes, and no word about starting a node
    s.settings.mining_mode = tenero_gui::settings::MiningMode::Pool;
    rig.app.set_snapshot(s.clone());
    let (t, _) = rig.frame();
    for needle in [
        "Pool address",
        "Pool key",
        "Name of this computer",
        "the block rewards go to the POOL",
        "nothing makes any pool pay",
        "Start mining",
    ] {
        assert!(
            t.contains(needle),
            "`{needle}` missing:
{t}"
        );
    }
    assert!(
        !t.contains("start the node first"),
        "a pool miner needs no node:
{t}"
    );
    // a pool typed in without its key is said to need it
    s.settings.pool = "pool.example:38335".into();
    rig.app.set_snapshot(s);
    let (t, _) = rig.frame();
    assert!(t.contains("needs its key"), "{t}");
}

#[test]
fn the_mining_screen_shows_the_card_and_marks_old_readings() {
    let mut rig = Rig::new();
    rig.app.goto("Mining");
    let s = snap(
        &rig,
        wallet(true, true),
        NodeView::Running {
            info: info(false, 3),
            ours: true,
        },
        report(false),
        None,
    );
    rig.app.set_snapshot(s);
    let (t, _) = rig.frame();
    for needle in [
        "34.7k attempts/s",
        "61 °C",
        "231 W",
        "power cap",
        "not comparable with another coin",
    ] {
        assert!(t.contains(needle), "`{needle}` missing:\n{t}");
    }
    assert!(!t.contains("has not reported"), "{t}");
    let s = snap(
        &rig,
        wallet(true, true),
        NodeView::Running {
            info: info(false, 3),
            ours: true,
        },
        report(true),
        None,
    );
    rig.app.set_snapshot(s);
    let (t, _) = rig.frame();
    assert!(t.contains("has not reported for a few seconds"), "{t}");
}

#[test]
fn the_history_shows_what_came_in_what_went_out_and_where_a_payment_stands() {
    let mut rig = Rig::new();
    rig.app.goto("History");
    let s = snap(
        &rig,
        wallet(true, true),
        NodeView::Running {
            info: info(false, 3),
            ours: true,
        },
        MinerView::Off,
        None,
    );
    rig.app.set_snapshot(s);
    let (t, _) = rig.frame();
    for needle in [
        "Received",
        "Mined",
        "Sent",
        "+10 TNR",
        "-2.5 TNR",
        "+0.00000005 TNR",
        "block reward",
        "waiting for a block",
        "fee 0.004 TNR",
        "2023-11-14 22:13 UTC",
        "Copy id",
        "sender unknown",
        "payment ID abababababababab",
    ] {
        assert!(
            t.contains(needle),
            "`{needle}` missing from the history:\n{t}"
        );
    }
}

#[test]
fn the_receive_screen_shows_the_address_and_a_code_and_the_settings_screen_its_fields() {
    let mut rig = Rig::new();
    let s = snap(
        &rig,
        wallet(true, true),
        NodeView::Stopped,
        MinerView::Off,
        None,
    );
    rig.app.set_snapshot(s);
    rig.app.goto("Receive");
    let (t, _) = rig.frame();
    assert!(
        t.contains(&addr(1)) && t.contains("Copy the address") && t.contains("Carrot address"),
        "{t}"
    );
    rig.app.goto("Settings");
    let (t, _) = rig.frame();
    for needle in [
        "Network",
        "Seeds",
        "Wallet file",
        "Backend",
        "Apply",
        "no secrets",
    ] {
        assert!(
            t.to_lowercase().contains(&needle.to_lowercase()),
            "`{needle}` missing from settings:\n{t}"
        );
    }
}

#[test]
fn the_settings_screen_moves_the_nodes_data_and_shows_how_far_it_has_got() {
    let mut rig = Rig::new();
    // idle, node stopped: the box and the button, and what the move does and does not do
    let s = snap(
        &rig,
        wallet(true, true),
        NodeView::Stopped,
        MinerView::Off,
        None,
    );
    rig.app.set_snapshot(s);
    rig.app.goto("Settings");
    let (t, _) = rig.frame();
    for needle in [
        "Move the node's data",
        "The node keeps its data in",
        "Copy the data there, check it, and use it",
        "compared with the original",
        "delete it yourself",
    ] {
        assert!(t.contains(needle), "`{needle}` missing:\n{t}");
    }
    assert!(!t.contains("Cancel the move"), "{t}");
    // the node running: it must be stopped first, and the screen says so
    let s = snap(
        &rig,
        wallet(true, true),
        NodeView::Starting,
        MinerView::Off,
        None,
    );
    rig.app.set_snapshot(s);
    let (t, _) = rig.frame();
    assert!(t.contains("Stop the node and the miner first."), "{t}");
    // a move in progress: where from and to, how far, and a way to stop it
    let mut s = snap(
        &rig,
        wallet(true, true),
        NodeView::Stopped,
        MinerView::Off,
        None,
    );
    s.moving = Some(MoveView {
        from: "C:\\old".into(),
        to: "D:\\new".into(),
        phase: tenero_gui::movedata::Phase::Copying,
        done: 3 * 1024 * 1024 * 1024,
        total: 6 * 1024 * 1024 * 1024,
    });
    rig.app.set_snapshot(s);
    let (t, _) = rig.frame();
    for needle in [
        "Copying: C:\\old to D:\\new",
        "3.00 GiB of 6.00 GiB",
        "Cancel the move",
    ] {
        assert!(t.contains(needle), "`{needle}` missing:\n{t}");
    }
    assert!(
        !t.contains("Copy the data there"),
        "no second move can be started: {t}"
    );
}

#[test]
fn a_fee_that_cannot_be_worked_out_says_why_instead_of_working_for_ever() {
    let mut rig = Rig::new();
    rig.app.goto("Send");
    let s = snap(
        &rig,
        wallet(true, true),
        NodeView::Running {
            info: info(false, 3),
            ours: true,
        },
        MinerView::Off,
        None,
    );
    rig.app.set_snapshot(s);
    rig.app.set_send_inputs(&addr(9), "1");
    // fees are asked for once typing has paused, not on every key
    rig.frame();
    assert!(
        rig.commands.try_recv().is_err(),
        "asked for the fees while still typing"
    );
    std::thread::sleep(std::time::Duration::from_millis(700));
    let (t, _) = rig.frame();
    assert!(t.contains("working it out"), "{t}");
    assert!(matches!(
        rig.commands.try_recv(),
        Ok(Cmd::EstimateFees { .. })
    ));
    // and only once, however many frames are drawn while the answer is awaited
    rig.frame();
    rig.frame();
    assert!(rig.commands.try_recv().is_err());
    rig.events
        .send(Event::EstimateFailed(
            "the chain does not have enough matured outputs yet".into(),
        ))
        .unwrap();
    let (t, _) = rig.frame();
    assert!(
        t.contains(
            "This payment cannot be made yet: the chain does not have enough matured outputs yet"
        ),
        "{t}"
    );
    assert!(
        t.contains("no price") && !t.contains("working it out"),
        "{t}"
    );
}

#[test]
fn the_history_offers_proofs_and_the_transaction_key_is_not_shown_until_asked() {
    let mut rig = Rig::new();
    rig.app.goto("History");
    let s = snap(
        &rig,
        wallet(true, true),
        NodeView::Running {
            info: info(false, 3),
            ours: true,
        },
        MinerView::Off,
        None,
    );
    rig.app.set_snapshot(s);
    let (t, _) = rig.frame();
    for needle in ["Prove payment", "Show payment key", "Prove receipt"] {
        assert!(
            t.contains(needle),
            "`{needle}` missing from the history:
{t}"
        );
    }
    // nothing secret is on screen yet
    let key = "ab".repeat(16);
    assert!(!t.contains(&key));
    // after the click the worker answers; only then is the key drawn, in a window that says what it is, with a copy button of its own
    rig.events
        .send(Event::TxKey {
            id: [9; 32],
            key: Zeroizing::new(key.clone()),
        })
        .unwrap();
    let (t, copies) = rig.frame();
    assert!(
        t.contains("Payment key (secret)") && t.contains(&key),
        "{t}"
    );
    assert!(t.contains("It cannot spend anything"), "{t}");
    assert!(
        copies.is_empty(),
        "the key was put on the clipboard without a click"
    );
    // and a proof just made is shown with its warning, also not copied by itself
    rig.events
        .send(Event::Proof {
            text: "tnpay1deadbeef".into(),
            note: "Proves this payment without giving away its secret key.".into(),
        })
        .unwrap();
    let (t, copies) = rig.frame();
    assert!(
        t.contains("Payment proof") && t.contains("tnpay1deadbeef"),
        "{t}"
    );
    assert!(t.contains("does not show who sent it"), "{t}");
    assert!(copies.is_empty());
}

#[test]
fn the_prove_screen_signs_verifies_and_checks_and_says_what_it_does_not_show() {
    let mut rig = Rig::new();
    rig.app.goto("Prove");
    // with the wallet locked and no node: verifying still works (no wallet, no node), signing and checking say what they need
    let s = snap(
        &rig,
        WalletView::Locked,
        NodeView::Stopped,
        MinerView::Off,
        None,
    );
    rig.app.set_snapshot(s);
    let (t, _) = rig.frame();
    for needle in [
        "UNAUDITED",
        "Sign a message",
        "Unlock the wallet",
        "Verify a signed message",
        "Check a payment proof",
        "Check a payment key",
        "Search from block",
        "start the node first",
    ] {
        assert!(
            t.contains(needle),
            "`{needle}` missing from the Prove screen:
{t}"
        );
    }
    // unlocked, with the node up: signing is offered
    let s = snap(
        &rig,
        wallet(true, true),
        NodeView::Running {
            info: info(false, 3),
            ours: true,
        },
        MinerView::Off,
        None,
    );
    rig.app.set_snapshot(s);
    let (t, _) = rig.frame();
    assert!(
        t.contains("Sign as") && t.contains("Check against the node"),
        "{t}"
    );
    // the answers
    rig.events
        .send(Event::Signed {
            signature: format!("tnsig1{}", "0".repeat(128)),
        })
        .unwrap();
    let (t, _) = rig.frame();
    assert!(
        t.contains("Signed by") && t.contains("Copy signature"),
        "{t}"
    );
    rig.events
        .send(Event::ProofChecked(Ok(CheckedView {
            kind: "sent",
            address: addr(9),
            amount: 250_000_000,
            height: 1200,
            global_index: 77,
            confirmations: 3,
            block_reward: false,
        })))
        .unwrap();
    let (t, _) = rig.frame();
    for needle in [
        "VALID",
        "2.5 TNR",
        "Blocks on top",
        "does not show who sent it",
    ] {
        assert!(
            t.contains(needle),
            "`{needle}` missing:
{t}"
        );
    }
    rig.events
        .send(Event::ProofChecked(Err("the proof is NOT valid".into())))
        .unwrap();
    let (t, _) = rig.frame();
    assert!(
        t.contains("NOT valid: the proof is NOT valid") && !t.contains("Blocks on top"),
        "{t}"
    );
}

#[test]
fn the_locked_screen_lists_the_wallets_and_offers_another_and_an_open_wallet_can_be_switched() {
    use tenero_gui::wallets::WalletEntry;
    let mut rig = Rig::new();
    let dir = std::env::temp_dir().join("tenero-gui-window-test");
    let mut s = snap(
        &rig,
        WalletView::Locked,
        NodeView::Stopped,
        MinerView::Off,
        None,
    );
    s.wallets = vec![
        WalletEntry {
            name: "Main wallet".into(),
            path: dir.join("Main wallet.twl"),
        },
        WalletEntry {
            name: "Savings stash".into(),
            path: dir.join("Savings stash.twl"),
        },
    ];
    s.settings.wallet_file = dir.join("Savings stash.twl");
    rig.app.set_snapshot(s);
    rig.app.goto("Wallet");
    let (t, _) = rig.frame();
    for needle in [
        "Your wallets",
        "Main wallet",
        "Savings stash",
        "Unlock \"Savings stash\"",
        "Create another wallet",
        "Restore another wallet from 24 words",
    ] {
        assert!(
            t.contains(needle),
            "`{needle}` missing from the chooser:\n{t}"
        );
    }
    // open: the way back to the list is on every screen, and says which wallet is open
    let mut s = snap(
        &rig,
        wallet(true, true),
        NodeView::Stopped,
        MinerView::Off,
        None,
    );
    s.settings.wallet_file = dir.join("Savings stash.twl");
    rig.app.set_snapshot(s);
    let (t, _) = rig.frame();
    assert!(
        t.contains("Lock / switch wallet") && t.contains("Wallet Savings stash"),
        "{t}"
    );
}

#[test]
fn the_receive_screen_makes_requests_and_the_send_screen_reads_them() {
    let mut rig = Rig::new();
    let s = snap(
        &rig,
        wallet(true, true),
        NodeView::Running {
            info: info(false, 3),
            ours: true,
        },
        MinerView::Off,
        None,
    );
    rig.app.set_snapshot(s);
    rig.app.goto("Receive");
    let (t, _) = rig.frame();
    for needle in [
        "Integrated address",
        "Make an integrated address",
        "Request a payment",
        "is not marked as paid",
        "Make request",
        "Your requests",
        "Rent",
        "1.5 TNR",
        "Show",
        "Delete",
    ] {
        assert!(
            t.contains(needle),
            "`{needle}` missing from the receive screen:\n{t}"
        );
    }

    // the send screen: a request is read into the form; a bad one says what is wrong; a bare address fills only the address
    rig.app.goto("Send");
    let to = tenero_wallet::Wallet::from_seed(&[1; 32], Network::Test, 0).address();
    let uri = format!(
        "tenero:{}?amount=2.5&label=Rent&message=October%20rent",
        to.to_text()
    );
    rig.app.set_send_paste(&uri);
    let (t, _) = rig.frame();
    assert!(t.contains("Paste a payment request or an address"), "{t}");
    assert!(t.contains("Payment request: Rent — October rent"), "{t}");
    assert!(
        t.contains(&to.to_text()) && t.contains("2.5"),
        "the form was not filled in:\n{t}"
    );
    rig.app.set_send_paste("tenero:nonsense");
    let (t, _) = rig.frame();
    assert!(
        t.contains("not a payment request") || t.contains("the address in the request"),
        "{t}"
    );
    assert!(
        !t.contains("Payment request: Rent"),
        "the old request is not kept after a bad paste"
    );
    rig.app.set_send_paste(&to.to_text());
    let (t, _) = rig.frame();
    assert!(
        !t.contains("Payment request:"),
        "a bare address is not a request:\n{t}"
    );
}

#[test]
fn the_confirmation_and_the_history_say_what_a_payment_was_for() {
    let mut rig = Rig::new();
    rig.app.goto("Send");
    let s = snap(
        &rig,
        wallet(true, true),
        NodeView::Running {
            info: info(false, 3),
            ours: true,
        },
        MinerView::Off,
        Some(quote()),
    );
    rig.app.set_snapshot(s);
    let (t, _) = rig.frame();
    assert!(t.contains("For") && t.contains("Rent"), "{t}");
    rig.app.goto("History");
    let (t, _) = rig.frame();
    assert!(t.contains("for Rent"), "{t}");
}

#[test]
fn a_node_with_no_peers_is_not_called_up_to_date() {
    let mut rig = Rig::new();
    rig.app.goto("Node");
    for peers in [0, 3] {
        let s = snap(
            &rig,
            wallet(true, true),
            NodeView::Running {
                info: info(false, peers),
                ours: true,
            },
            MinerView::Off,
            None,
        );
        rig.app.set_snapshot(s);
        let (t, _) = rig.frame();
        // the Node tab's State row ("Wallet: up to date" in the top bar is about the wallet, not the node)
        assert_eq!(
            t.contains("no peers: cannot tell if it is up to date"),
            peers == 0,
            "{peers} peers:\n{t}"
        );
    }
}

#[test]
fn a_view_only_wallet_says_so_and_offers_nothing_that_needs_the_spend_key() {
    let mut rig = Rig::new();
    let running = || NodeView::Running {
        info: info(false, 3),
        ours: true,
    };
    for tier in [ViewTier::ViewAll, ViewTier::ViewReceived] {
        let s = snap(&rig, view_only(tier), running(), MinerView::Off, None);
        rig.app.set_snapshot(s.clone());
        rig.app.goto("Wallet");
        let (t, _) = rig.frame();
        assert!(t.contains("VIEW-ONLY"), "{t}");
        for absent in ["Show my 24 words", "Add account", "Send from here"] {
            assert!(
                !t.contains(absent),
                "{tier:?}: `{absent}` is offered:
{t}"
            );
        }
        assert!(
            t.contains("Show a view key"),
            "a view-all wallet can give a view key"
        );
        if tier == ViewTier::ViewReceived {
            assert!(t.contains("Received in all (not a balance)"), "{t}");
            assert!(!t.contains("Spendable"), "{t}");
        } else {
            assert!(t.contains("Balance") && t.contains("Spendable"), "{t}");
        }
        rig.app.set_snapshot(s.clone());
        rig.app.goto("Send");
        let (t, _) = rig.frame();
        assert!(t.contains("VIEW-ONLY wallet: it cannot send"), "{t}");
        assert!(!t.contains("Paste a payment request"), "{t}");
        rig.app.set_snapshot(s);
        rig.app.goto("Prove");
        let (t, _) = rig.frame();
        assert!(t.contains("cannot sign"), "{t}");
        assert!(!t.contains("Sign as"), "{t}");
    }
    // a full wallet is not called view-only
    let s = snap(&rig, wallet(true, true), running(), MinerView::Off, None);
    rig.app.set_snapshot(s);
    rig.app.goto("Wallet");
    let (t, _) = rig.frame();
    assert!(
        !t.contains("VIEW-ONLY") && t.contains("Show my 24 words"),
        "{t}"
    );
}

#[test]
fn a_view_only_wallet_can_be_added_from_the_welcome_and_the_locked_screens() {
    let mut rig = Rig::new();
    let s = snap(
        &rig,
        WalletView::NoWallet,
        NodeView::Stopped,
        MinerView::Off,
        None,
    );
    rig.app.set_snapshot(s);
    let (t, _) = rig.frame();
    assert!(t.contains("View-only, from a view key"), "{t}");
    let s = snap(
        &rig,
        WalletView::Locked,
        NodeView::Stopped,
        MinerView::Off,
        None,
    );
    rig.app.set_snapshot(s);
    let (t, _) = rig.frame();
    assert!(t.contains("Add a view-only wallet"), "{t}");
}

#[test]
fn a_payment_of_many_pieces_says_before_review_how_long_its_proofs_take() {
    let mut rig = Rig::new();
    rig.app.goto("Send");
    let s = snap(
        &rig,
        wallet(true, true),
        NodeView::Running {
            info: info(false, 3),
            ours: true,
        },
        MinerView::Off,
        None,
    );
    rig.app.set_snapshot(s);
    rig.app.set_send_inputs(&addr(9), "1");
    rig.frame();
    std::thread::sleep(std::time::Duration::from_millis(700));
    rig.frame();
    // a payment of 50 block rewards: said before Review is pressed
    rig.events
        .send(Event::Estimate {
            fees: [100, 200, 500],
            coins: 50,
        })
        .unwrap();
    let (t, _) = rig.frame();
    assert!(t.contains("This payment spends 50 pieces"), "{t}");
    assert!(t.contains("about a second for each"), "{t}");
    // and a payment of a few pieces says nothing about it
    rig.events
        .send(Event::Estimate {
            fees: [100, 200, 500],
            coins: 2,
        })
        .unwrap();
    let (t, _) = rig.frame();
    assert!(!t.contains("pieces. Making its proofs"), "{t}");
}
