//! The window. It draws a [`Snapshot`] and turns clicks into [`Cmd`]s; every decision is made in [`crate::core`] (which is
//! tested without a window). **This file is checked by hand on the owner's machine, not by an automated test.**
//!
//! Rules the screens keep (`docs/M10_M11_PLAN.md` M10.3): the banner "TEST NETWORK. NO VALUE. UNAUDITED." is on every
//! screen; nothing says or implies that coins are money or that payments are anonymous; a balance is never shown as
//! final while the wallet is catching up or no node is running; the 24 words are shown only on request, never put on the
//! clipboard, and asked for the password again.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};
use rand_core::RngCore;
use tenero_app::config::Network;
use tenero_app::ui::{format_rate, group_digits};
use tenero_wallet::{EntryKind, FeeLevel, KdfParams, SentStatus};
use zeroize::Zeroizing;

use crate::backend::Backend;
use crate::procs::tail_of;
use crate::settings::{MinerBackend, MiningMode, NodeKind, Settings};
use crate::text;
use crate::view::*;

const AMBER: Color32 = Color32::from_rgb(230, 160, 30);
const RED: Color32 = Color32::from_rgb(220, 70, 70);
const GREEN: Color32 = Color32::from_rgb(90, 190, 110);
const GREY: Color32 = Color32::from_rgb(150, 150, 150);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Wallet,
    Send,
    Receive,
    History,
    Prove,
    Node,
    Mining,
    Settings,
    About,
}

impl Tab {
    const ALL: [(Tab, &'static str); 9] = [
        (Tab::Wallet, "Wallet"),
        (Tab::Send, "Send"),
        (Tab::Receive, "Receive"),
        (Tab::History, "History"),
        (Tab::Prove, "Prove"),
        (Tab::Node, "Node"),
        (Tab::Mining, "Mining"),
        (Tab::Settings, "Settings"),
        (Tab::About, "About"),
    ];
}

struct Toast {
    text: String,
    error: bool,
    until: Instant,
}

#[derive(Default)]
struct Welcome {
    /// The new wallet's name (empty: the suggestion is used).
    name: String,
    restoring: bool,
    pw1: Zeroizing<String>,
    pw2: Zeroizing<String>,
    no_password: bool,
    phrase: Zeroizing<String>,
    birth: String,
}

enum PhraseStage {
    Show,
    Verify {
        positions: [usize; 3],
        answers: [String; 3],
    },
}

struct PhraseModal {
    words: Zeroizing<String>,
    new: bool,
    stage: PhraseStage,
}

#[derive(Default)]
struct SendForm {
    account: usize,
    /// The "Pieces to combine" field.
    combine_coins: String,
    to: String,
    amount: String,
    level: Option<FeeLevel>,
    estimate: Option<[u64; 3]>,
    /// What the estimate was asked for, so it is asked once per change.
    asked: String,
    /// Why the fees (or the payment) could not be worked out, if they could not: shown instead of "working".
    error: Option<String>,
    /// A request to the worker is out (the fees, building the payment, sending it) and when it went.
    working: Option<Instant>,
    /// The payment as last typed and when it last changed: fees are asked for once typing has paused, and never while an
    /// earlier question is still out (each one costs the node a number of round trips).
    seen: String,
    changed: Option<Instant>,
    /// The "paste a payment request or an address" field, the text it held when last read, and what it said.
    pasted: String,
    pasted_seen: String,
    paste_error: Option<String>,
    /// What the payment is for (the label of the request pasted), and the address that label was pasted with: if the address is
    /// changed by hand the label no longer belongs to it and is not sent.
    note: Option<String>,
    note_for: String,
    request_message: Option<String>,
}

#[derive(Default)]
struct RequestForm {
    amount: String,
    label: String,
    message: String,
    selected: Option<usize>,
    select_newest: bool,
}

#[derive(Default)]
struct ProveForm {
    sign_account: usize,
    sign_message: String,
    signature: Option<String>,
    verify_address: String,
    verify_message: String,
    verify_signature: String,
    verified: Option<Result<String, String>>,
    check_text: String,
    checked: Option<Result<CheckedView, String>>,
    key_text: String,
    key_address: String,
    key_from: String,
}

#[derive(Default)]
struct Prompt {
    /// `Some` while the "type your password" window for showing the words is open.
    reveal: Option<Zeroizing<String>>,
    change: Option<(Zeroizing<String>, Zeroizing<String>, bool)>,
}

pub struct App {
    backend: Backend,
    snap: Snapshot,
    app_dir: PathBuf,
    tab: Tab,
    toasts: Vec<Toast>,
    welcome: Welcome,
    unlock_pw: Zeroizing<String>,
    phrase: Option<PhraseModal>,
    send: SendForm,
    receive_account: usize,
    new_account: String,
    renaming: Option<(usize, String)>,
    prompt: Prompt,
    draft: Option<(Settings, String)>,
    /// The settings as they were when `draft` was made: what the user changed is `draft` against this (`Settings::with_changes`).
    draft_base: Option<Settings>,
    /// The folder typed in the "move the node's data" box.
    move_to: String,
    /// The wallet chooser is showing the form for another wallet (not the list).
    adding: bool,
    closing: bool,
    /// When `Quit` was sent: if the worker has not finished 75 s later (it is stuck), the window ends what it started itself.
    closing_since: Option<Instant>,
    node_tail: (Instant, String),
    prove: ProveForm,
    request: RequestForm,
    /// A payment proof just made (its text and what it shows), in a window until closed.
    proof_window: Option<(String, String)>,
    /// The secret of a sent payment, shown on request in a window until closed.
    tx_key_window: Option<Zeroizing<String>>,
    miner_tail: (Instant, String),
    /// The pool fields of the Mining tab as typed: (address, key, worker), and the settings they were read from.
    pool_form: Option<(String, String, String)>,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        app_dir: PathBuf,
        settings: Settings,
        kdf: KdfParams,
        notice: Option<String>,
    ) -> App {
        let ctx = cc.egui_ctx.clone();
        let backend = Backend::spawn(
            &app_dir,
            settings.clone(),
            kdf,
            Arc::new(move || ctx.request_repaint()),
        );
        cc.egui_ctx.all_styles_mut(|s| {
            for f in s.text_styles.values_mut() {
                f.size *= 1.12;
            }
        });
        App::with_backend(backend, app_dir, settings, notice)
    }

    /// The window over any backend (the real one, or a detached one in a test).
    pub fn with_backend(
        backend: Backend,
        app_dir: PathBuf,
        settings: Settings,
        notice: Option<String>,
    ) -> App {
        let now = Instant::now();
        let mut app = App {
            backend,
            snap: Snapshot {
                wallets: Vec::new(),
                settings,
                wallet: WalletView::NoWallet,
                node: NodeView::Stopped,
                miner: MinerView::Off,
                prepared: None,
                busy: Some("starting".into()),
                moving: None,
            },
            app_dir,
            tab: Tab::Wallet,
            toasts: Vec::new(),
            welcome: Welcome::default(),
            unlock_pw: Zeroizing::default(),
            phrase: None,
            send: SendForm::default(),
            receive_account: 0,
            new_account: String::new(),
            renaming: None,
            prompt: Prompt::default(),
            draft: None,
            draft_base: None,
            move_to: String::new(),
            adding: false,
            closing: false,
            closing_since: None,
            node_tail: (now, String::new()),
            prove: ProveForm::default(),
            request: RequestForm::default(),
            proof_window: None,
            tx_key_window: None,
            miner_tail: (now, String::new()),
            pool_form: None,
        };
        if let Some(n) = notice {
            app.toast(n, true);
        }
        app
    }

    /// Replaces what the window shows (a test's way of putting it in a state).
    pub fn set_snapshot(&mut self, s: Snapshot) {
        self.snap = s;
    }

    /// Opens a tab by its name on the tab bar (a test's way of visiting each screen).
    pub fn goto(&mut self, name: &str) {
        if let Some((t, _)) = Tab::ALL.iter().find(|(_, n)| *n == name) {
            self.tab = *t;
        }
    }

    /// Types into the "paste a payment request or an address" field (a test's way of pasting).
    pub fn set_send_paste(&mut self, text: &str) {
        self.send.pasted = text.to_string();
    }

    /// Types a payment into the send form (a test's way of filling it in).
    pub fn set_send_inputs(&mut self, to: &str, amount: &str) {
        self.send.to = to.to_string();
        self.send.amount = amount.to_string();
    }

    pub fn tab_names() -> Vec<&'static str> {
        Tab::ALL.iter().map(|(_, n)| *n).collect()
    }

    fn toast(&mut self, text: impl Into<String>, error: bool) {
        let secs = if error { 12 } else { 6 };
        self.toasts.push(Toast {
            text: text.into(),
            error,
            until: Instant::now() + Duration::from_secs(secs),
        });
    }

    fn drain(&mut self) {
        while let Some(ev) = self.backend.try_recv() {
            match ev {
                Event::Snapshot(s) => {
                    self.snap = *s;
                    if self.snap.prepared.is_some() {
                        self.send.working = None;
                    }
                }
                Event::Phrase { words, new } => {
                    self.phrase = Some(PhraseModal {
                        words,
                        new,
                        stage: PhraseStage::Show,
                    });
                }
                Event::Estimate { fees } => {
                    self.send.estimate = Some(fees);
                    self.send.error = None;
                    self.send.working = None;
                }
                Event::EstimateFailed(m) => {
                    self.send.estimate = None;
                    self.send.error = Some(m);
                    self.send.working = None;
                }
                Event::Sent {
                    fee, transactions, ..
                } => {
                    let what = if transactions > 1 {
                        format!("{transactions} transactions sent")
                    } else {
                        "Payment sent".to_string()
                    };
                    self.toast(
                        format!(
                            "{what} (fee {}). It counts once a block takes it in.",
                            text::coins(fee)
                        ),
                        false,
                    );
                    self.send = SendForm::default();
                }
                Event::Signed { signature } => self.prove.signature = Some(signature),
                Event::Proof { text, note } => self.proof_window = Some((text, note)),
                Event::TxKey { key, .. } => self.tx_key_window = Some(key),
                Event::ProofChecked(r) => self.prove.checked = Some(r),
                Event::Notice(m) => self.toast(m, false),
                Event::Error(m) => {
                    self.send.working = None;
                    self.toast(m, true);
                }
                Event::Quit => {
                    self.closing = false;
                    self.backend.join();
                    std::process::exit(0);
                }
            }
        }
        let now = Instant::now();
        self.toasts.retain(|t| t.until > now);
    }

    fn wallet(&self) -> Option<&WalletData> {
        match &self.snap.wallet {
            WalletView::Unlocked(d) => Some(d),
            _ => None,
        }
    }

    // ------------------------------------------------------------------------------------------------------------
    // the frame
    // ------------------------------------------------------------------------------------------------------------

    fn banner(&self, ui: &mut egui::Ui) {
        egui::Frame::new()
            .fill(Color32::from_rgb(70, 45, 0))
            .inner_margin(6.0)
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(BANNER).strong().color(AMBER).size(17.0));
                    ui.label(RichText::new(SCHEME_NOTE).small().color(AMBER));
                });
            });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let (txt, col) = match &self.snap.node {
                NodeView::Stopped => ("Node: stopped".to_string(), GREY),
                NodeView::Starting => ("Node: starting…".to_string(), AMBER),
                NodeView::Stopping => ("Node: stopping…".to_string(), AMBER),
                NodeView::Failed { why, .. } => (format!("Node: {why}"), RED),
                NodeView::Running { info, .. } if info.syncing => (
                    format!("Node: catching up (height {})", group_digits(info.height)),
                    AMBER,
                ),
                NodeView::Running { info, .. } => (
                    format!(
                        "Node: running, height {}, {} peers",
                        group_digits(info.height),
                        info.peers
                    ),
                    GREEN,
                ),
            };
            ui.colored_label(col, txt);
            ui.separator();
            match &self.snap.miner {
                MinerView::Off => ui.colored_label(GREY, "Mining: off"),
                MinerView::Starting => ui.colored_label(AMBER, "Mining: starting…"),
                MinerView::Failed { why, .. } => ui.colored_label(RED, format!("Mining: {why}")),
                MinerView::Running { report, stale } => {
                    if *stale {
                        ui.colored_label(RED, "Mining: not reporting")
                    } else {
                        let rate = report
                            .s10
                            .or(report.average)
                            .map_or("measuring".to_string(), format_rate);
                        ui.colored_label(GREEN, format!("Mining: {rate} attempts/s"))
                    }
                }
            };
            if self.wallet().is_some() {
                ui.separator();
                let name = self
                    .snap
                    .settings
                    .wallet_file
                    .file_stem()
                    .map_or(String::new(), |s| s.to_string_lossy().into_owned());
                ui.label(format!("Wallet {name}"));
            }
            if let Some(d) = self.wallet() {
                ui.separator();
                if d.synced {
                    ui.colored_label(GREEN, "Wallet: up to date");
                } else if d.tip.is_some() {
                    ui.colored_label(
                        AMBER,
                        format!(
                            "Wallet: reading the chain ({} of {})",
                            d.scanned.map_or("0".into(), group_digits),
                            d.tip.map_or("?".into(), group_digits)
                        ),
                    );
                } else {
                    ui.colored_label(GREY, "Wallet: no node, balances unknown");
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.wallet().is_some() && ui.button("Lock / switch wallet").clicked() {
                    self.backend.send(Cmd::Lock);
                }
            });
        });
    }

    fn toasts_ui(&mut self, ui: &mut egui::Ui) {
        for t in &self.toasts {
            let col = if t.error { RED } else { GREEN };
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(col, if t.error { "✖" } else { "✔" });
                ui.label(&t.text);
            });
        }
    }

    // ------------------------------------------------------------------------------------------------------------
    // before the wallet is open
    // ------------------------------------------------------------------------------------------------------------

    /// Draws what the person needs before the wallet tabs mean anything. `true` = the wallet is open.
    fn gate(&mut self, ui: &mut egui::Ui) -> bool {
        match self.snap.wallet.clone() {
            WalletView::Unlocked(_) => true,
            WalletView::Locked if self.adding => {
                if ui.button("Back to my wallets").clicked() {
                    self.adding = false;
                }
                self.welcome_ui(ui);
                false
            }
            WalletView::Locked => {
                ui.add_space(20.0);
                ui.heading("Your wallets");
                let current = self.snap.settings.wallet_file.clone();
                for w in self.snap.wallets.clone() {
                    let selected = w.path == current;
                    ui.horizontal(|ui| {
                        if ui
                            .radio(selected, RichText::new(&w.name).strong())
                            .clicked()
                            && !selected
                        {
                            self.unlock_pw = Zeroizing::default();
                            self.backend.send(Cmd::SelectWallet {
                                path: w.path.clone(),
                            });
                        }
                        ui.label(
                            RichText::new(w.path.display().to_string())
                                .small()
                                .color(GREY),
                        );
                    });
                }
                ui.add_space(8.0);
                let name = current
                    .file_stem()
                    .map_or(String::new(), |s| s.to_string_lossy().into_owned());
                ui.label(format!("Unlock \"{name}\":"));
                let mut go = false;
                ui.horizontal(|ui| {
                    ui.label("Password");
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut *self.unlock_pw)
                            .password(true)
                            .desired_width(260.0),
                    );
                    go |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    go |= ui.button("Unlock").clicked();
                });
                if go {
                    let password = std::mem::take(&mut self.unlock_pw);
                    self.backend.send(Cmd::Unlock { password });
                }
                ui.add_space(6.0);
                ui.label(
                    RichText::new("No password was set? Leave it empty and press Unlock.")
                        .small()
                        .color(GREY),
                );
                ui.add_space(14.0);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Create another wallet").clicked() {
                        self.welcome.restoring = false;
                        self.adding = true;
                    }
                    if ui.button("Restore another wallet from 24 words").clicked() {
                        self.welcome.restoring = true;
                        self.adding = true;
                    }
                });
                false
            }
            WalletView::NoWallet => {
                self.welcome_ui(ui);
                false
            }
        }
    }

    fn welcome_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(12.0);
        ui.heading(if self.snap.wallets.is_empty() {
            "Welcome"
        } else {
            "Another wallet"
        });
        if self.welcome.name.is_empty() {
            self.welcome.name = crate::wallets::suggest_name(&self.snap.settings);
        }
        ui.horizontal(|ui| {
            ui.label("Wallet name");
            ui.add(egui::TextEdit::singleline(&mut self.welcome.name).desired_width(240.0));
        });
        let name_problem = crate::wallets::new_path(&self.snap.settings, &self.welcome.name).err();
        if let Some(p) = &name_problem {
            ui.colored_label(AMBER, p);
        }
        ui.label("This wallet keeps coins of an experimental test network. They have no value, and nothing here is audited.");
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.welcome.restoring, false, "Create a new wallet");
            ui.selectable_value(
                &mut self.welcome.restoring,
                true,
                "Restore from my 24 words",
            );
        });
        ui.separator();
        if self.welcome.restoring {
            ui.label("Type your 24 words, separated by spaces:");
            ui.add(
                egui::TextEdit::multiline(&mut *self.welcome.phrase)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY),
            );
            ui.label(
                RichText::new(
                    "A restored wallet finds what it received. It cannot know whom you paid: that is kept only in the wallet file, not in the words.",
                )
                .small()
                .color(GREY),
            );
            ui.horizontal(|ui| {
                ui.label(
                    "First block that could hold your coins (leave empty if you do not know):",
                );
                ui.add(egui::TextEdit::singleline(&mut self.welcome.birth).desired_width(90.0));
            });
        }
        ui.add_space(6.0);
        ui.heading("A password for the wallet file on this computer");
        ui.label("It only locks the file. Your 24 words restore the wallet anywhere, with or without it.");
        ui.checkbox(
            &mut self.welcome.no_password,
            "No password (anyone who can read the wallet file can spend what is in it)",
        );
        if !self.welcome.no_password {
            ui.horizontal(|ui| {
                ui.label("Password (8 or more characters)");
                ui.add(
                    egui::TextEdit::singleline(&mut *self.welcome.pw1)
                        .password(true)
                        .desired_width(240.0),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Again");
                ui.add(
                    egui::TextEdit::singleline(&mut *self.welcome.pw2)
                        .password(true)
                        .desired_width(240.0),
                );
            });
        }
        let pw_problem = if self.welcome.no_password {
            None
        } else if self.welcome.pw1.chars().count() < MIN_PASSWORD {
            Some(format!(
                "The password needs at least {MIN_PASSWORD} characters."
            ))
        } else if *self.welcome.pw1 != *self.welcome.pw2 {
            Some("The two passwords differ.".to_string())
        } else {
            None
        };
        if let Some(p) = &pw_problem {
            ui.colored_label(AMBER, p);
        }
        let birth_ok = self.welcome.birth.trim().is_empty()
            || self.welcome.birth.trim().parse::<u64>().is_ok();
        if !birth_ok {
            ui.colored_label(AMBER, "The block number must be digits only.");
        }
        let phrase_ok = !self.welcome.restoring || !self.welcome.phrase.trim().is_empty();
        let label = if self.welcome.restoring {
            "Restore wallet"
        } else {
            "Create wallet"
        };
        if ui
            .add_enabled(
                pw_problem.is_none() && birth_ok && phrase_ok && name_problem.is_none(),
                egui::Button::new(label),
            )
            .clicked()
        {
            let password = if self.welcome.no_password {
                Password::None
            } else {
                Password::Set(std::mem::take(&mut self.welcome.pw1))
            };
            if self.welcome.restoring {
                let phrase = std::mem::take(&mut self.welcome.phrase);
                let birth = self.welcome.birth.trim().parse().ok();
                self.backend.send(Cmd::RestoreWallet {
                    phrase,
                    password,
                    birth,
                    name: Some(self.welcome.name.clone()),
                });
            } else {
                self.backend.send(Cmd::CreateWallet {
                    password,
                    name: Some(self.welcome.name.clone()),
                });
            }
            self.welcome = Welcome::default();
            self.adding = false;
        }
    }

    fn phrase_window(&mut self, ctx: &egui::Context) {
        let Some(modal) = self.phrase.as_mut() else {
            return;
        };
        let mut close = false;
        let mut verified = false;
        egui::Window::new("Your 24 words")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| match &mut modal.stage {
                PhraseStage::Show => {
                    ui.colored_label(
                        AMBER,
                        "Write these words on paper, in order. They ARE the wallet: anyone who sees them can spend everything, and without them (and the wallet file) the coins are gone. They are not copied anywhere.",
                    );
                    ui.add_space(6.0);
                    let words: Vec<&str> = modal.words.split(' ').collect();
                    // six lines of four, numbered, in a fixed-width font so the columns line up
                    for (row, chunk) in words.chunks(4).enumerate() {
                        let line: String = chunk
                            .iter()
                            .enumerate()
                            .map(|(j, w)| format!("{:>2}. {:<10}", row * 4 + j + 1, w))
                            .collect();
                        ui.label(RichText::new(line.trim_end()).monospace().size(17.0));
                    }
                    ui.add_space(8.0);
                    if modal.new {
                        if ui.button("I have written them down").clicked() {
                            // three places to type back, so a wrong copy is found now and not in a year
                            let mut positions = [0usize; 3];
                            let mut rng = rand_core::OsRng;
                            let mut k = 0;
                            while k < 3 {
                                let p = (rng.next_u32() % 24) as usize;
                                if !positions[..k].contains(&p) {
                                    positions[k] = p;
                                    k += 1;
                                }
                            }
                            positions.sort_unstable();
                            modal.stage = PhraseStage::Verify {
                                positions,
                                answers: Default::default(),
                            };
                        }
                    } else if ui.button("Hide them").clicked() {
                        close = true;
                    }
                }
                PhraseStage::Verify { positions, answers } => {
                    ui.label("To check your copy, type these words from your paper:");
                    let words: Vec<String> = modal.words.split(' ').map(str::to_string).collect();
                    let mut all = true;
                    for (i, p) in positions.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label(format!("Word {}", p + 1));
                            ui.add(egui::TextEdit::singleline(&mut answers[i]).desired_width(160.0));
                        });
                        all &= answers[i].trim().eq_ignore_ascii_case(&words[*p]);
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Show the words again").clicked() {
                            modal.stage = PhraseStage::Show;
                        } else if ui.add_enabled(all, egui::Button::new("They match: done")).clicked() {
                            verified = true;
                        }
                    });
                    if !all {
                        ui.label(RichText::new("Not matching yet.").small().color(GREY));
                    }
                }
            });
        if close || verified {
            self.phrase = None;
            if verified {
                self.toast("Wallet ready. Keep your paper safe.", false);
            }
        }
    }

    // ------------------------------------------------------------------------------------------------------------
    // the tabs
    // ------------------------------------------------------------------------------------------------------------

    fn wallet_tab(&mut self, ui: &mut egui::Ui) {
        let Some(d) = self.wallet().cloned() else {
            return;
        };
        ui.add_space(6.0);
        match d.total {
            Some(t) => {
                let title = if d.synced {
                    "Balance"
                } else {
                    "Balance (NOT FINAL: the wallet is still catching up)"
                };
                ui.label(RichText::new(title).color(if d.synced { GREY } else { AMBER }));
                ui.label(RichText::new(text::coins(t.total)).size(30.0).strong());
                ui.label(format!(
                    "Spendable {}   ·   waiting to mature {}   ·   tied up in a payment waiting for a block {}",
                    text::coins(t.spendable),
                    text::coins(t.immature),
                    text::coins(t.reserved)
                ));
                let (n, amount, fee) = pending_out(&d, None);
                if n > 0 {
                    ui.colored_label(
                        AMBER,
                        format!(
                            "{n} payment(s) waiting for a block: {} going out (fee {}). Whole coins are tied up until a block takes the payment in; the change comes back then, and a payment to another of your accounts shows there then. Nothing moves while no block is mined (start mining on the Mining tab, or wait for another miner).",
                            text::coins(amount),
                            text::coins(fee)
                        ),
                    );
                }
            }
            None => {
                ui.label(RichText::new("Balance unknown").size(26.0).strong());
                ui.label("A balance needs the node, which says what has been spent. Start it on the Node tab.");
                if ui.button("Go to the Node tab").clicked() {
                    self.tab = Tab::Node;
                }
            }
        }
        ui.add_space(10.0);
        ui.heading("Accounts");
        for a in &d.accounts {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    if let Some((i, buf)) = self.renaming.as_mut().filter(|(i, _)| *i == a.index) {
                        let _ = i;
                        ui.add(egui::TextEdit::singleline(buf).desired_width(180.0));
                        if ui.button("Save").clicked() {
                            let label = buf.clone();
                            self.backend.send(Cmd::RenameAccount {
                                index: a.index,
                                label,
                            });
                            self.renaming = None;
                        }
                        if ui.button("Cancel").clicked() {
                            self.renaming = None;
                        }
                    } else {
                        ui.label(RichText::new(&a.label).strong().size(17.0));
                        if ui.small_button("Rename").clicked() {
                            self.renaming = Some((a.index, a.label.clone()));
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        match a.balance {
                            Some(b) => {
                                ui.label(RichText::new(text::coins(b.total)).strong().size(17.0))
                            }
                            None => ui.label(RichText::new("—").color(GREY)),
                        };
                    });
                });
                ui.label(RichText::new(&a.address).monospace().small());
                if let Some(b) = a.balance {
                    ui.label(
                        RichText::new(format!(
                            "spendable {} · maturing {} · tied up in a waiting payment {}",
                            text::coins(b.spendable),
                            text::coins(b.immature),
                            text::coins(b.reserved)
                        ))
                        .small()
                        .color(GREY),
                    );
                }
                ui.horizontal(|ui| {
                    if ui.small_button("Send from here").clicked() {
                        self.send.account = a.index;
                        self.tab = Tab::Send;
                    }
                    if ui.small_button("Receive here").clicked() {
                        self.receive_account = a.index;
                        self.tab = Tab::Receive;
                    }
                });
            });
        }
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.new_account)
                    .hint_text("name of a new account")
                    .desired_width(200.0),
            );
            if ui
                .add_enabled(
                    !self.new_account.trim().is_empty(),
                    egui::Button::new("Add account"),
                )
                .clicked()
            {
                let label = std::mem::take(&mut self.new_account);
                self.backend.send(Cmd::AddAccount { label });
            }
        });
        ui.label(
            RichText::new(
                "Each account has its own address and balance, and a payment comes from one account only. All of them come back from the same 24 words (the names do not).",
            )
            .small()
            .color(GREY),
        );
        ui.add_space(12.0);
        ui.separator();
        ui.heading("Security");
        ui.horizontal(|ui| {
            if ui.button("Show my 24 words…").clicked() {
                self.prompt.reveal = Some(Zeroizing::default());
            }
            if ui.button("Change the password…").clicked() {
                self.prompt.change = Some((Zeroizing::default(), Zeroizing::default(), false));
            }
        });
        ui.label(if d.has_password {
            "The wallet file has a password."
        } else {
            "The wallet file has NO password: anyone who can read it can spend what is in it."
        });
    }

    fn prompts(&mut self, ctx: &egui::Context) {
        if self.prompt.reveal.is_some() {
            let mut send = None;
            let mut cancel = false;
            egui::Window::new("Show my 24 words")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label("Type your password again (leave it empty if you set none).");
                    if let Some(pw) = self.prompt.reveal.as_mut() {
                        let r = ui.add(egui::TextEdit::singleline(&mut **pw).password(true));
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        ui.horizontal(|ui| {
                            if ui.button("Show").clicked() || enter {
                                send = Some(std::mem::take(pw));
                            }
                            if ui.button("Cancel").clicked() {
                                cancel = true;
                            }
                        });
                    }
                });
            if let Some(password) = send {
                self.backend.send(Cmd::RevealPhrase { password });
                self.prompt.reveal = None;
            } else if cancel {
                self.prompt.reveal = None;
            }
        }
        if self.prompt.change.is_some() {
            let mut action: Option<Cmd> = None;
            let mut cancel = false;
            egui::Window::new("Change the password")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    if let Some((old, new, none)) = self.prompt.change.as_mut() {
                        ui.horizontal(|ui| {
                            ui.label("Current password");
                            ui.add(egui::TextEdit::singleline(&mut **old).password(true));
                        });
                        ui.checkbox(none, "No password from now on");
                        if !*none {
                            ui.horizontal(|ui| {
                                ui.label(format!("New password ({MIN_PASSWORD}+ characters)"));
                                ui.add(egui::TextEdit::singleline(&mut **new).password(true));
                            });
                        }
                        ui.horizontal(|ui| {
                            let ok = *none || new.chars().count() >= MIN_PASSWORD;
                            if ui.add_enabled(ok, egui::Button::new("Change")).clicked() {
                                action = Some(Cmd::ChangePassword {
                                    old: std::mem::take(old),
                                    new: if *none {
                                        Password::None
                                    } else {
                                        Password::Set(std::mem::take(new))
                                    },
                                });
                            }
                            if ui.button("Cancel").clicked() {
                                cancel = true;
                            }
                        });
                    }
                });
            if let Some(cmd) = action {
                self.backend.send(cmd);
                self.prompt.change = None;
            } else if cancel {
                self.prompt.change = None;
            }
        }
    }

    fn send_tab(&mut self, ui: &mut egui::Ui) {
        let Some(d) = self.wallet().cloned() else {
            return;
        };
        ui.add_space(6.0);
        if let Some(q) = self.snap.prepared.clone() {
            self.review(ui, &d, &q);
            return;
        }
        ui.heading("Send");
        if d.total.is_none() {
            ui.label(
                "The node is not running, so the wallet cannot send. Start it on the Node tab.",
            );
            return;
        }
        if !d.synced {
            ui.colored_label(AMBER, "The wallet is still catching up with the chain. You can fill in the payment; sending waits until it is up to date.");
        }
        if self.send.account >= d.accounts.len() {
            self.send.account = 0;
        }
        ui.label("Paste a payment request or an address (optional)");
        ui.add(
            egui::TextEdit::singleline(&mut self.send.pasted)
                .hint_text("tenero:… or tni1…")
                .desired_width(f32::INFINITY)
                .font(egui::TextStyle::Monospace),
        );
        if self.send.pasted != self.send.pasted_seen {
            self.send.pasted_seen = self.send.pasted.clone();
            self.send.paste_error = None;
            self.send.note = None;
            self.send.request_message = None;
            if !self.send.pasted.trim().is_empty() {
                match tenero_wallet::request::parse_pay_text(
                    &self.send.pasted,
                    self.snap.settings.network.wallet_network(),
                ) {
                    Ok(r) => {
                        self.send.to = r.address.to_text();
                        if let Some(a) = r.amount {
                            self.send.amount = tenero_wallet::amount::format_coins(a);
                        }
                        self.send.note_for = self.send.to.clone();
                        self.send.note = r.label;
                        self.send.request_message = r.message;
                    }
                    Err(e) => self.send.paste_error = Some(e.to_string()),
                }
            }
        }
        if let Some(e) = &self.send.paste_error {
            ui.colored_label(RED, e);
        }
        if self.send.note.is_some() || self.send.request_message.is_some() {
            let what = [self.send.note.clone(), self.send.request_message.clone()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" — ");
            ui.colored_label(GREEN, format!("Payment request: {what}"));
        }
        egui::ComboBox::from_label("From account")
            .selected_text(account_text(&d, self.send.account))
            .show_ui(ui, |ui| {
                for a in &d.accounts {
                    ui.selectable_value(&mut self.send.account, a.index, account_text(&d, a.index));
                }
            });
        ui.label("To (an address starting tni1…)");
        ui.add(
            egui::TextEdit::singleline(&mut self.send.to)
                .desired_width(f32::INFINITY)
                .font(egui::TextStyle::Monospace),
        );
        ui.horizontal(|ui| {
            ui.label("Amount");
            ui.add(
                egui::TextEdit::singleline(&mut self.send.amount)
                    .desired_width(160.0)
                    .hint_text("0.0"),
            );
            ui.label(tenero_app::ui::TICKER);
            if let Some(a) = d.accounts.get(self.send.account).and_then(|a| a.balance) {
                ui.label(
                    RichText::new(format!("(spendable {})", text::coins(a.spendable)))
                        .small()
                        .color(GREY),
                );
            }
        });
        // the fee: three levels, each with its price once the payment is filled in
        let key = format!(
            "{}|{}|{}",
            self.send.account,
            self.send.to.trim(),
            self.send.amount.trim()
        );
        let filled = !self.send.to.trim().is_empty() && !self.send.amount.trim().is_empty();
        if self.send.seen != key {
            self.send.seen = key.clone();
            self.send.changed = Some(Instant::now());
        }
        let paused = self
            .send
            .changed
            .is_none_or(|t| t.elapsed() > Duration::from_millis(600));
        if filled && d.synced && self.send.asked != key && paused && self.send.working.is_none() {
            self.send.asked = key.clone();
            self.send.estimate = None;
            self.send.error = None;
            self.send.working = Some(Instant::now());
            self.backend.send(Cmd::EstimateFees {
                account: self.send.account,
                to: self.send.to.clone(),
                amount: self.send.amount.clone(),
            });
        }
        if !filled {
            self.send.estimate = None;
            self.send.error = None;
            self.send.asked.clear();
        }
        ui.add_space(6.0);
        ui.label(RichText::new("Fee").strong());
        let current = self.send.level.unwrap_or(FeeLevel::Low);
        for (i, level) in FeeLevel::ALL.into_iter().enumerate() {
            let price = match self.send.estimate {
                Some(f) => text::coins(f[i]),
                None if self.send.error.is_some() => "no price".to_string(),
                None if filled && !d.synced => {
                    "waiting for the wallet to finish reading the chain".to_string()
                }
                None if filled => match self.send.working {
                    Some(t) => format!("working it out… ({} s)", t.elapsed().as_secs()),
                    None => "working it out…".to_string(),
                },
                None => "fill in the payment to see the price".to_string(),
            };
            let blurb = match level {
                FeeLevel::Low => {
                    "the least this chain accepts, plus a margin: fine unless blocks are full"
                }
                FeeLevel::Normal => "twice the minimum: goes ahead of Low when the pool is full",
                FeeLevel::High => "five times the minimum: first in line when the pool is full",
            };
            ui.horizontal(|ui| {
                if ui
                    .radio(current == level, format!("{} — {price}", level.name()))
                    .clicked()
                {
                    self.send.level = Some(level);
                }
                ui.label(RichText::new(blurb).small().color(GREY));
            });
        }
        if let Some(e) = &self.send.error {
            ui.colored_label(RED, format!("This payment cannot be made yet: {e}"));
        } else if self
            .send
            .working
            .is_some_and(|t| t.elapsed() > Duration::from_secs(15))
        {
            ui.label(
                RichText::new("Still working. The node answers slowly while it is busy (a miner using the GPU slows it down).")
                    .small()
                    .color(AMBER),
            );
        }
        ui.label(
            RichText::new("The fee goes to whoever mines the block. A higher fee only buys a better place in a full pool; it never makes a block come sooner.")
                .small()
                .color(GREY),
        );
        ui.add_space(8.0);
        if ui
            .add_enabled(
                filled && d.synced && self.send.error.is_none() && self.send.working.is_none(),
                egui::Button::new("Review payment…"),
            )
            .clicked()
        {
            self.send.working = Some(Instant::now());
            self.backend.send(Cmd::PreparePayment {
                account: self.send.account,
                to: self.send.to.clone(),
                amount: self.send.amount.clone(),
                level: current,
                note: self
                    .send
                    .note
                    .clone()
                    .filter(|_| self.send.to.trim() == self.send.note_for),
            });
        }
        ui.add_space(18.0);
        ui.separator();
        ui.heading("Combine pieces");
        ui.label(
            RichText::new("Your balance is made of separate pieces: one for each payment you received, and each block reward is one piece. One payment can only use about 95 pieces, so if you hold many small ones, combine them into fewer, larger pieces first. It costs a fee, and the new piece can be spent after about 10 blocks. Nobody is paid: you see a summary first.")
                .small()
                .color(GREY),
        );
        ui.horizontal(|ui| {
            ui.label("Pieces to combine");
            ui.add(egui::TextEdit::singleline(&mut self.send.combine_coins).desired_width(60.0));
            let count = self.send.combine_coins.trim().parse::<usize>().ok();
            let idle = d.synced && self.send.working.is_none();
            if ui
                .add_enabled(
                    idle && count.is_some_and(|c| c >= 2),
                    egui::Button::new("Combine these…"),
                )
                .clicked()
            {
                self.send.working = Some(Instant::now());
                self.backend.send(Cmd::PrepareCombine {
                    account: self.send.account,
                    coins: count,
                    level: current,
                });
            }
            if ui
                .add_enabled(idle, egui::Button::new("Combine all…"))
                .on_hover_text("every piece worth more than the fee it adds, in as many transactions as it takes")
                .clicked()
            {
                self.send.working = Some(Instant::now());
                self.backend.send(Cmd::PrepareCombine {
                    account: self.send.account,
                    coins: None,
                    level: current,
                });
            }
        });
    }

    fn review(&mut self, ui: &mut egui::Ui, d: &WalletData, q: &Quote) {
        if let Some(what) = &q.own {
            ui.heading("Check the combine");
            ui.label("Nothing has been sent yet. This moves your own pieces into fewer, larger ones: nobody is paid.");
            ui.add_space(6.0);
            egui::Grid::new("review_own")
                .num_columns(2)
                .spacing([20.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Account");
                    ui.label(account_text(d, q.account));
                    ui.end_row();
                    ui.label("What");
                    ui.label(what);
                    ui.end_row();
                    ui.label("Transactions");
                    ui.label(q.transactions.to_string());
                    ui.end_row();
                    ui.label(format!("Fee ({})", q.level.name()));
                    ui.label(text::coins(q.fee));
                    ui.end_row();
                    ui.label("The account keeps");
                    ui.label(RichText::new(text::coins(q.amount)).strong());
                    ui.end_row();
                });
            ui.add_space(4.0);
            ui.colored_label(
                AMBER,
                "The new pieces can be spent after about 10 blocks. Until then the balance still counts them, but they are not spendable.",
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(self.send.working.is_none(), egui::Button::new("Combine"))
                    .clicked()
                {
                    self.send.working = Some(Instant::now());
                    self.backend.send(Cmd::SendPrepared);
                }
                if ui.button("Back").clicked() {
                    self.backend.send(Cmd::CancelPrepared);
                }
            });
            return;
        }
        ui.heading("Check the payment");
        ui.label("Nothing has been sent yet.");
        ui.add_space(6.0);
        egui::Grid::new("review")
            .num_columns(2)
            .spacing([20.0, 6.0])
            .show(ui, |ui| {
                ui.label("From");
                ui.label(account_text(d, q.account));
                ui.end_row();
                ui.label("To");
                ui.label(RichText::new(&q.to).monospace());
                ui.end_row();
                ui.label("Amount");
                ui.label(RichText::new(text::coins(q.amount)).strong());
                ui.end_row();
                ui.label(format!("Fee ({})", q.level.name()));
                ui.label(text::coins(q.fee));
                ui.end_row();
                if let Some(n) = &q.note {
                    ui.label("For");
                    ui.label(n);
                    ui.end_row();
                }
                ui.label("Taken from the account");
                ui.label(RichText::new(text::coins(q.amount + q.fee)).strong());
                ui.end_row();
                ui.label("Change back to the account");
                ui.label(text::coins(q.change));
                ui.end_row();
            });
        if q.transactions > 1 {
            ui.add_space(4.0);
            ui.label(format!(
                "This payment needs more pieces than one transaction can carry, so it is made as {} transactions that spend different pieces ({} pieces in all). Each pays a fee; the fee shown is the total. The person paid receives several amounts that add up to the payment.",
                q.transactions, q.coins
            ));
        }
        if q.unsent_payments > 0 {
            ui.add_space(4.0);
            ui.colored_label(
                AMBER,
                format!(
                    "Only part of this payment can be sent now: {} of it has to wait, because the pieces that are free ran out. The change of these transactions can be spent after about 10 blocks; send the rest then.",
                    text::coins(q.unsent_total)
                ),
            );
        }
        ui.add_space(4.0);
        ui.colored_label(AMBER, "Payments cannot be taken back. Check the address: a wrong address loses the coins (they have no value, but the habit matters).");
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.send.working.is_none(), egui::Button::new("Send"))
                .clicked()
            {
                self.send.working = Some(Instant::now());
                self.backend.send(Cmd::SendPrepared);
            }
            if ui.button("Back").clicked() {
                self.backend.send(Cmd::CancelPrepared);
            }
        });
    }

    fn receive_tab(&mut self, ui: &mut egui::Ui) {
        let Some(d) = self.wallet().cloned() else {
            return;
        };
        ui.add_space(6.0);
        ui.heading("Receive");
        if self.receive_account >= d.accounts.len() {
            self.receive_account = 0;
        }
        egui::ComboBox::from_label("Into account")
            .selected_text(account_text(&d, self.receive_account))
            .show_ui(ui, |ui| {
                for a in &d.accounts {
                    ui.selectable_value(
                        &mut self.receive_account,
                        a.index,
                        account_text(&d, a.index),
                    );
                }
            });
        let Some(a) = d.accounts.get(self.receive_account) else {
            return;
        };
        ui.add_space(6.0);
        ui.label("Give this address to the person paying you:");
        ui.add(
            egui::Label::new(RichText::new(&a.address).monospace())
                .selectable(true)
                .wrap(),
        );
        if ui.button("Copy the address").clicked() {
            ui.ctx().copy_text(a.address.clone());
            self.toast("Address copied.", false);
        }
        ui.add_space(8.0);
        match crate::qr::modules(&a.address) {
            Some((w, squares)) => {
                let quiet = 4.0;
                let cell = (260.0 / (w as f32 + 2.0 * quiet)).floor().max(2.0);
                let side = cell * (w as f32 + 2.0 * quiet);
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::hover());
                let p = ui.painter_at(rect);
                p.rect_filled(rect, 0.0, Color32::WHITE);
                for y in 0..w {
                    for x in 0..w {
                        if squares[y * w + x] {
                            let min = rect.min
                                + egui::vec2((x as f32 + quiet) * cell, (y as f32 + quiet) * cell);
                            p.rect_filled(
                                egui::Rect::from_min_size(min, egui::vec2(cell, cell)),
                                0.0,
                                Color32::BLACK,
                            );
                        }
                    }
                }
            }
            None => {
                ui.label("(this address does not fit a QR code)");
            }
        }
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "This address works on one test network only (its first letters say which: TENg gamma, TENd development, TENt the SHA-256 test network; none has value). It is a Carrot address: a payment to it cannot be linked to it by someone reading the chain, but Carrot and FCMP++ here are unaudited.",
            )
            .small()
            .color(GREY),
        );
        self.requests_section(ui, &d);
    }

    fn requests_section(&mut self, ui: &mut egui::Ui, d: &WalletData) {
        ui.add_space(14.0);
        ui.separator();
        ui.heading("Request a payment");
        ui.label("Makes a link and a QR code that asks to be paid to the account chosen above. Whoever opens it (Send tab, \"Paste a payment request\") gets the address, the amount and what it is for filled in. A request is not an invoice and is not marked as paid: the chain cannot tell which payment answered it.");
        let amount_ok = self.request.amount.trim().is_empty()
            || tenero_wallet::amount::parse_coins(self.request.amount.trim())
                .is_some_and(|a| a > 0);
        let label_ok = tenero_wallet::request::check_text(
            self.request.label.trim(),
            tenero_wallet::request::MAX_LABEL,
            "label",
        )
        .is_ok();
        let message_ok = tenero_wallet::request::check_text(
            self.request.message.trim(),
            tenero_wallet::request::MAX_MESSAGE,
            "message",
        )
        .is_ok();
        ui.horizontal(|ui| {
            ui.label("Amount (empty: the payer chooses)");
            ui.add(egui::TextEdit::singleline(&mut self.request.amount).desired_width(130.0));
            ui.label(tenero_app::ui::TICKER);
        });
        ui.horizontal(|ui| {
            ui.label("Label (what it is for)");
            ui.add(egui::TextEdit::singleline(&mut self.request.label).desired_width(300.0));
        });
        ui.horizontal(|ui| {
            ui.label("Message (optional)");
            ui.add(egui::TextEdit::singleline(&mut self.request.message).desired_width(420.0));
        });
        if !amount_ok {
            ui.colored_label(
                AMBER,
                "The amount must be digits with up to 8 decimals, and not zero.",
            );
        }
        if !label_ok {
            ui.colored_label(
                AMBER,
                "The label is too long (64 bytes at most) or has a line break.",
            );
        }
        if !message_ok {
            ui.colored_label(
                AMBER,
                "The message is too long (200 bytes at most) or has a line break.",
            );
        }
        if ui
            .add_enabled(
                amount_ok && label_ok && message_ok,
                egui::Button::new("Make request"),
            )
            .clicked()
        {
            self.backend.send(Cmd::AddRequest {
                account: self.receive_account,
                amount: std::mem::take(&mut self.request.amount),
                label: std::mem::take(&mut self.request.label),
                message: std::mem::take(&mut self.request.message),
            });
            self.request.select_newest = true;
        }
        if self.request.select_newest && !d.requests.is_empty() {
            self.request.selected = Some(d.requests.len() - 1);
            self.request.select_newest = false;
        }
        if d.requests.is_empty() {
            return;
        }
        ui.add_space(8.0);
        ui.label(RichText::new("Your requests").strong());
        let mut delete = None;
        for q in &d.requests {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(q.label.as_deref().unwrap_or("(no label)")).strong());
                ui.label(match q.amount {
                    Some(a) => text::coins(a),
                    None => "any amount".to_string(),
                });
                ui.label(
                    RichText::new(format!(
                        "· into {} · {}",
                        q.account_label,
                        text::when(q.time)
                    ))
                    .color(GREY),
                );
                if ui.small_button("Show").clicked() {
                    self.request.selected = Some(q.index);
                }
                if ui.small_button("Delete").clicked() {
                    delete = Some(q.index);
                }
            });
        }
        if let Some(index) = delete {
            self.backend.send(Cmd::DeleteRequest { index });
            self.request.selected = None;
        }
        let Some(q) = self.request.selected.and_then(|i| d.requests.get(i)) else {
            return;
        };
        ui.add_space(8.0);
        ui.label(
            RichText::new(format!(
                "Request: {}",
                q.label.as_deref().unwrap_or("(no label)")
            ))
            .strong(),
        );
        if let Some(m) = &q.message {
            ui.label(m);
        }
        ui.add(
            egui::Label::new(RichText::new(&q.uri).monospace().small())
                .selectable(true)
                .wrap(),
        );
        if ui.button("Copy the link").clicked() {
            ui.ctx().copy_text(q.uri.clone());
            self.toast("Link copied.", false);
        }
        draw_qr(ui, &q.uri);
        ui.label(
            RichText::new("The link shows the address and the amount to whoever gets it, and to anyone they pass it on to.")
                .small()
                .color(GREY),
        );
    }

    fn history_tab(&mut self, ui: &mut egui::Ui) {
        let Some(d) = self.wallet().cloned() else {
            return;
        };
        ui.add_space(6.0);
        ui.heading("History");
        if d.total.is_none() {
            ui.label("The node is not running: the history needs it to say which payments were taken in. Start it on the Node tab.");
            return;
        }
        if d.history.is_empty() {
            ui.label("Nothing yet.");
            return;
        }
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // two short lines per entry that wrap with the window (a wide table pushed the buttons off the right edge)
                for row in &d.history {
                    let (kind, col, sign) = match &row.kind {
                        EntryKind::Received => ("Received", GREEN, "+"),
                        EntryKind::Mined => ("Mined", GREEN, "+"),
                        EntryKind::Sent { .. } => ("Sent", RED, "-"),
                    };
                    ui.horizontal_wrapped(|ui| {
                        ui.colored_label(col, RichText::new(kind).strong());
                        ui.colored_label(
                            col,
                            RichText::new(format!("{sign}{}", text::coins(row.amount))).strong(),
                        );
                        ui.label(format!("· {}", row.account_label));
                        match &row.kind {
                            EntryKind::Sent { time, .. } => ui.label(format!(
                                "· {} (block {})",
                                text::when(*time),
                                group_digits(row.height)
                            )),
                            _ => ui.label(format!("· block {}", group_digits(row.height))),
                        };
                    });
                    ui.horizontal_wrapped(|ui| {
                        match &row.kind {
                            EntryKind::Sent {
                                to, fee, status, ..
                            } => {
                                let (st, c) = match status {
                                    SentStatus::Pending => ("waiting for a block", AMBER),
                                    SentStatus::Confirmed => ("taken in", GREEN),
                                    SentStatus::NotConfirmed => ("not taken in: dropped", RED),
                                };
                                ui.colored_label(c, st);
                                let own = d
                                    .accounts
                                    .iter()
                                    .find(|a| a.address == to.to_text())
                                    .map(|a| format!("your account {}", a.label));
                                ui.label(format!(
                                    "to {} · fee {}",
                                    own.unwrap_or_else(|| text::short_address(&to.to_text())),
                                    text::coins(*fee)
                                ));
                                if let Some(n) = &row.note {
                                    ui.label(RichText::new(format!("for {n}")).color(GREY));
                                }
                            }
                            EntryKind::Mined => {
                                ui.label(RichText::new("block reward").color(GREY));
                            }
                            EntryKind::Received => {
                                ui.label(
                                    RichText::new(
                                        "sender unknown (the format hides it from the receiver)",
                                    )
                                    .color(GREY),
                                );
                            }
                        }
                        if let Some(id) = row.id {
                            if ui.small_button("Copy id").clicked() {
                                ui.ctx().copy_text(text::hex(&id));
                            }
                            if row.has_secret {
                                if ui.small_button("Prove payment").clicked() {
                                    self.backend.send(Cmd::MakeProof(ProofRequest::Sent {
                                        id,
                                        key: false,
                                    }));
                                }
                                if ui.small_button("Show transaction key").clicked() {
                                    self.backend.send(Cmd::RevealTxKey { id });
                                }
                            } else {
                                ui.label(RichText::new("no key kept").small().color(GREY));
                            }
                        }
                        if let Some(global_index) = row.global_index {
                            if ui.small_button("Prove receipt").clicked() {
                                self.backend.send(Cmd::MakeProof(ProofRequest::Received {
                                    account: row.account,
                                    global_index,
                                }));
                            }
                        }
                    });
                    ui.separator();
                }
            });
        ui.label(
            RichText::new("The history of payments you SENT is kept in the wallet file. A wallet restored from the 24 words shows what it received but not whom it paid.")
                .small()
                .color(GREY),
        );
    }

    fn tail(slot: &mut (Instant, String), path: PathBuf) -> String {
        if slot.0.elapsed() > Duration::from_secs(1) || slot.1.is_empty() {
            slot.1 = tail_of(&path, 30);
            slot.0 = Instant::now();
        }
        slot.1.clone()
    }

    fn node_tab(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.heading("Node");
        ui.label("The node is the program that keeps a copy of the chain and talks to other nodes. The wallet needs it to see balances and to send.");
        ui.add_space(6.0);
        let node = self.snap.node.clone();
        match &node {
            NodeView::Stopped => {
                ui.label("The node is stopped.");
            }
            NodeView::Starting => {
                ui.colored_label(AMBER, "The node is starting…");
            }
            NodeView::Stopping => {
                ui.colored_label(
                    AMBER,
                    "The node is stopping (it finishes writing its files first; up to a minute)…",
                );
            }
            NodeView::Failed { why, output } => {
                ui.colored_label(RED, why);
                if !output.is_empty() {
                    ui.label("What it printed last:");
                    ui.add(
                        egui::Label::new(RichText::new(output).monospace().small())
                            .selectable(true),
                    );
                }
            }
            NodeView::Running { info, ours } => {
                egui::Grid::new("nodeinfo")
                    .num_columns(2)
                    .spacing([20.0, 4.0])
                    .show(ui, |ui| {
                        let mut row = |k: &str, v: String| {
                            ui.label(RichText::new(k).color(GREY));
                            ui.label(v);
                            ui.end_row();
                        };
                        row("Network", info.network.clone());
                        row("Height", group_digits(info.height));
                        row("Tip", tenero_app::daemon::short_id(&info.tip_id));
                        row(
                            "Peers",
                            format!("{} ({} inbound)", info.peers, info.inbound),
                        );
                        row("Waiting transactions", info.mempool_txs.to_string());
                        row(
                            "Keeps",
                            match info.kind {
                                tenero_app::control::NodeKind::Archive => {
                                    "every block in full".to_string()
                                }
                                tenero_app::control::NodeKind::Pruned => format!(
                                    "recent proofs only (from block {})",
                                    group_digits(info.pruned_below)
                                ),
                            },
                        );
                        row(
                            "State",
                            if info.syncing {
                                "catching up".into()
                            } else {
                                "up to date".into()
                            },
                        );
                        row(
                            "Started by",
                            if *ours {
                                "this window".into()
                            } else {
                                "someone else (found already running)".into()
                            },
                        );
                        row("Version", info.version.clone());
                    });
                if info.peers == 0 {
                    ui.add_space(4.0);
                    ui.colored_label(AMBER, "No peers: this node is alone, so it can only build its own chain. Add seeds in Settings to join others.");
                }
            }
        }
        ui.add_space(8.0);
        let external = self.snap.settings.external_node;
        ui.horizontal(|ui| {
            let can_start =
                matches!(node, NodeView::Stopped | NodeView::Failed { .. }) && !external;
            if ui
                .add_enabled(can_start, egui::Button::new("Start the node"))
                .clicked()
            {
                self.backend.send(Cmd::StartNode);
            }
            let can_stop = matches!(node, NodeView::Running { .. });
            if ui
                .add_enabled(can_stop, egui::Button::new("Stop the node"))
                .clicked()
            {
                self.backend.send(Cmd::StopNode);
            }
        });
        if external {
            ui.label(RichText::new("Settings say to use a node that is already running: this window will not start one.").small().color(GREY));
        }
        ui.label(
            RichText::new("Closing this window stops a node it started, and any miner. A node it only found running is left running.")
                .small()
                .color(GREY),
        );
        ui.add_space(8.0);
        let out = Self::tail(&mut self.node_tail, self.app_dir.join("node-output.txt"));
        ui.collapsing("What the node printed (last lines)", |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(if out.is_empty() {
                        "(nothing yet)"
                    } else {
                        &out
                    })
                    .monospace()
                    .small(),
                )
                .selectable(true),
            );
        });
    }

    fn mining_tab(&mut self, ui: &mut egui::Ui) {
        let Some(d) = self.wallet().cloned() else {
            return;
        };
        ui.add_space(6.0);
        ui.heading("Mining");
        ui.label("Mining searches for blocks. It is off until you press Start, and it stops when you lock the wallet or close this window.");
        ui.add_space(6.0);
        let s = self.snap.settings.clone();
        let miner_now = self.snap.miner.clone();
        let busy = !matches!(miner_now, MinerView::Off | MinerView::Failed { .. });
        // where to mine: for a pool, or alone on this computer's own node
        ui.label(RichText::new("Where to mine").strong());
        let mut mode = s.mining_mode;
        ui.add_enabled_ui(!busy, |ui| {
            ui.radio_value(&mut mode, MiningMode::Pool, "On a pool: you need no node to mine, and the pool pays you for your share of its work");
            ui.radio_value(&mut mode, MiningMode::Solo, "Alone, on my own node: a block I find pays me directly, but I may wait a long time for one");
        });
        if mode != s.mining_mode {
            let mut n = s.clone();
            n.mining_mode = mode;
            self.backend.send(Cmd::SetSettings(Box::new(n)));
        }
        if s.mining_mode == MiningMode::Pool {
            let (default_pool, key_ok) = (
                tenero_app::pool_miner::default_pool(s.network).is_some(),
                crate::settings::is_key_hex(&s.pool_key),
            );
            let form = self
                .pool_form
                .get_or_insert_with(|| (s.pool.clone(), s.pool_key.clone(), s.pool_worker.clone()));
            let mut committed = false;
            ui.add_enabled_ui(!busy, |ui| {
                egui::Grid::new("poolform")
                    .num_columns(2)
                    .spacing([12.0, 4.0])
                    .show(ui, |ui| {
                        ui.label("Pool address");
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut form.0)
                                .hint_text(if default_pool {
                                    "empty: this program's own pool"
                                } else {
                                    "HOST:PORT"
                                })
                                .desired_width(300.0),
                        );
                        committed |= r.lost_focus();
                        ui.end_row();
                        ui.label("Pool key");
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut form.1)
                                .hint_text(if form.0.trim().is_empty() && default_pool {
                                    "not needed for the built-in pool"
                                } else {
                                    "64 hexadecimal digits, from the pool's operator"
                                })
                                .desired_width(300.0),
                        );
                        committed |= r.lost_focus();
                        ui.end_row();
                        ui.label("Name of this computer");
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut form.2)
                                .hint_text("optional")
                                .desired_width(300.0),
                        );
                        committed |= r.lost_focus();
                        ui.end_row();
                    });
            });
            if committed {
                let (a, k, w) = (
                    form.0.trim().to_string(),
                    form.1.trim().to_ascii_lowercase(),
                    form.2.trim().to_string(),
                );
                if (a.clone(), k.clone(), w.clone())
                    != (s.pool.clone(), s.pool_key.clone(), s.pool_worker.clone())
                {
                    let mut n = s.clone();
                    (n.pool, n.pool_key, n.pool_worker) = (a, k, w);
                    self.backend.send(Cmd::SetSettings(Box::new(n)));
                }
            }
            if s.pool.is_empty() && !default_pool {
                ui.colored_label(AMBER, "No pool is built into this program for this network yet: type the address and key of a pool, or mine alone.");
            } else if !s.pool.is_empty() && !key_ok {
                ui.colored_label(AMBER, "A pool you type in needs its key (64 hexadecimal digits). The miner refuses a pool that proves another key, which stops someone between you and the pool.");
            }
            ui.colored_label(
                AMBER,
                "On a pool, the block rewards go to the POOL, which someone else runs. It pays you by its own rules, and nothing makes any pool pay. The pool also sees your address and your internet address. Nothing on this network has any value.",
            );
        }
        ui.add_space(6.0);
        if s.mining_mode == MiningMode::Solo {
            ui.label("A block found pays its reward to one of your accounts.");
        } else {
            ui.label("The pool will pay the account chosen below.");
        }
        let notice = match s.miner_backend {
            MinerBackend::Gpu => "This uses your GPU at full load: the card gets hot and loud, and anything else using the GPU slows down.".to_string(),
            MinerBackend::Cpu => format!("This uses {} CPU core(s) at full load.", s.miner_cores),
            MinerBackend::Sha256 => "Test-network mining: a light load on one CPU core.".to_string(),
        };
        ui.colored_label(AMBER, notice);
        ui.label(format!(
            "Backend: {} (change it in Settings)   ·   rewards go to: {}",
            s.miner_backend.name(),
            account_text(&d, s.miner_account)
        ));
        let miner = self.snap.miner.clone();
        let running = !matches!(miner, MinerView::Off | MinerView::Failed { .. });
        ui.add_enabled_ui(!running, |ui| {
            let mut acct = s.miner_account;
            egui::ComboBox::from_label("Pay rewards to")
                .selected_text(account_text(&d, acct))
                .show_ui(ui, |ui| {
                    for a in &d.accounts {
                        ui.selectable_value(&mut acct, a.index, account_text(&d, a.index));
                    }
                });
            if acct != s.miner_account {
                let mut n = s.clone();
                n.miner_account = acct;
                self.backend.send(Cmd::SetSettings(Box::new(n)));
            }
        });
        // mining for a pool needs no node; mining alone does
        let node_up =
            matches!(self.snap.node, NodeView::Running { .. }) || s.mining_mode == MiningMode::Pool;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!running && node_up, egui::Button::new("Start mining"))
                .clicked()
            {
                self.backend.send(Cmd::StartMiner);
            }
            if ui
                .add_enabled(running, egui::Button::new("Stop mining"))
                .clicked()
            {
                self.backend.send(Cmd::StopMiner);
            }
            if !node_up {
                ui.label(
                    RichText::new("start the node first (Node tab)")
                        .small()
                        .color(GREY),
                );
            }
        });
        ui.add_space(8.0);
        match &miner {
            MinerView::Off => {}
            MinerView::Starting => {
                ui.colored_label(AMBER, "The miner is starting…");
            }
            MinerView::Failed { why, output } => {
                ui.colored_label(RED, why);
                ui.add(
                    egui::Label::new(RichText::new(output).monospace().small()).selectable(true),
                );
            }
            MinerView::Running { report, stale } => {
                if *stale {
                    ui.colored_label(
                        RED,
                        "The miner has not reported for a few seconds: what follows is old.",
                    );
                }
                let r = report;
                ui.label(RichText::new(tenero_app::ui::short_backend(&r.backend)).strong());
                let rate = |v: Option<f64>| {
                    v.map_or("-".to_string(), |v| {
                        format!("{} attempts/s", format_rate(v))
                    })
                };
                egui::Grid::new("minerinfo")
                    .num_columns(2)
                    .spacing([20.0, 4.0])
                    .show(ui, |ui| {
                        let mut row = |k: &str, v: String| {
                            ui.label(RichText::new(k).color(GREY));
                            ui.label(v);
                            ui.end_row();
                        };
                        row(
                            "Now (10 s)",
                            if r.searching {
                                rate(r.s10)
                            } else {
                                "paused: the node is not ready".into()
                            },
                        );
                        row("Last minute", rate(r.s60));
                        row("Last 15 minutes", rate(r.m15));
                        row("Since start", rate(r.average));
                        if s.mining_mode == MiningMode::Pool {
                            row(
                                "Shares handed in",
                                format!(
                                    "{} (accepted {}, too late {}, refused {})",
                                    r.found, r.accepted, r.lost_race, r.refused
                                ),
                            );
                        } else {
                            row(
                                "Blocks found",
                                format!(
                                    "{} (in the chain {}, lost a race {}, refused {})",
                                    r.found, r.accepted, r.lost_race, r.refused
                                ),
                            );
                            row("Expected by luck", format!("{:.2}", r.expected_blocks));
                        }
                        row("Running for", text::duration(r.uptime_secs));
                        if let Some(t) = r.gpu_temp_c {
                            row("GPU temperature", format!("{t} °C"));
                        }
                        if let Some(w) = r.gpu_power_w {
                            row("GPU power", format!("{w:.0} W"));
                        }
                        if let Some(f) = r.gpu_fan_pct {
                            row("GPU fan", format!("{f} %"));
                        }
                        if let (Some(c), Some(m)) = (r.gpu_core_mhz, r.gpu_mem_mhz) {
                            row("GPU clocks", format!("core {c} MHz, memory {m} MHz"));
                        }
                        if let Some(b) = r.gpu_busy_pct {
                            row("GPU busy", format!("{b} %"));
                        }
                        if let Some(l) = &r.gpu_limited_by {
                            row("Held back by", l.clone());
                        }
                    });
                ui.label(
                    RichText::new("An attempt is one evaluation of this chain's proof of work. The number is not comparable with another coin's hashes.")
                        .small()
                        .color(GREY),
                );
            }
        }
        ui.add_space(8.0);
        let out = Self::tail(&mut self.miner_tail, self.app_dir.join("miner-output.txt"));
        ui.collapsing("What the miner printed (last lines)", |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(if out.is_empty() {
                        "(nothing yet)"
                    } else {
                        &out
                    })
                    .monospace()
                    .small(),
                )
                .selectable(true),
            );
        });
    }

    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.heading("Settings");
        if self.draft.is_none() {
            let s = self.snap.settings.clone();
            let seeds = s.seeds.join("\n");
            self.draft_base = Some(s.clone());
            self.draft = Some((s, seeds));
        }
        let node_busy = !matches!(self.snap.node, NodeView::Stopped | NodeView::Failed { .. });
        let miner_busy = !matches!(self.snap.miner, MinerView::Off | MinerView::Failed { .. });
        let unlocked = self.wallet().is_some();
        let app_dir = self.app_dir.clone();
        // the wallet that is selected NOW (the draft may be older than the wallet that was made or opened since)
        let wallet_file_now = self.snap.settings.wallet_file.clone();
        let data_dir_now = self.snap.settings.data_dir.clone();
        let moving = self.snap.moving.clone();
        // the node's data was moved since the draft was made (and the person did not edit that box): show the new place
        if let (Some((d, _)), Some(b)) = (self.draft.as_mut(), self.draft_base.as_mut()) {
            if b.data_dir != data_dir_now {
                if d.data_dir == b.data_dir {
                    d.data_dir = data_dir_now.clone();
                }
                b.data_dir = data_dir_now.clone();
            }
        }
        let mut start_move: Option<PathBuf> = None;
        let mut cancel_move = false;
        let (draft, seeds) = self.draft.as_mut().expect("just set");
        let mut apply = false;
        let mut reset = false;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.label(RichText::new("Node").strong());
                ui.add_enabled_ui(!node_busy, |ui| {
                    let before = draft.network;
                    egui::ComboBox::from_label("Network")
                        .selected_text(crate::procs::network_words(draft.network))
                        .show_ui(ui, |ui| {
                            for n in Network::ALL {
                                ui.selectable_value(
                                    &mut draft.network,
                                    n,
                                    crate::procs::network_words(n),
                                );
                            }
                        });
                    if draft.network != before {
                        // a network brings its own folders, wallet file, control port and backend
                        let keep = draft.program_dir.clone();
                        *draft = Settings::defaults(&app_dir, draft.network);
                        draft.program_dir = keep;
                        seeds.clear();
                    }
                    let mut pruned = matches!(draft.node_kind, NodeKind::Pruned { .. });
                    ui.checkbox(
                        &mut pruned,
                        "Pruned node (throws away old proofs to save disk)",
                    );
                    draft.node_kind = if pruned {
                        let keep = match draft.node_kind {
                            NodeKind::Pruned { keep } => keep,
                            NodeKind::Archive => 10_000,
                        };
                        let mut k = keep;
                        ui.horizontal(|ui| {
                            ui.label("Keep proofs of the last");
                            ui.add(
                                egui::DragValue::new(&mut k)
                                    .range(1_000..=10_000_000)
                                    .speed(100),
                            );
                            ui.label("blocks");
                        });
                        NodeKind::Pruned { keep: k }
                    } else {
                        NodeKind::Archive
                    };
                    ui.label("Seeds (other nodes to start from, one host:port per line)");
                    let built_in = draft.network.builtin_seeds();
                    ui.label(
                        RichText::new(if built_in.is_empty() {
                            "None are built in for this network: add one below, or run a node alone.".to_string()
                        } else {
                            format!(
                                "Already built in for this network (you need not add it): {}. Seeds you add below are used as well.",
                                built_in.join(", ")
                            )
                        })
                        .small()
                        .color(GREY),
                    );
                    ui.add(
                        egui::TextEdit::multiline(seeds)
                            .desired_rows(3)
                            .desired_width(f32::INFINITY),
                    );
                    let mut inbound = draft.inbound_port.is_some();
                    ui.checkbox(&mut inbound, "Let other nodes connect to me");
                    if inbound {
                        let mut port = draft
                            .inbound_port
                            .unwrap_or_else(|| crate::settings::default_inbound_port(draft.network));
                        ui.horizontal(|ui| {
                            ui.label("TCP port");
                            ui.add(egui::DragValue::new(&mut port).range(1..=65535));
                        });
                        draft.inbound_port = Some(port);
                        ui.label(
                            RichText::new(
                                "Others can then fetch blocks from you, which takes load off the seeds. This app cannot open your router: forward this TCP port to this \
                                 computer on your router, and allow the program in Windows Firewall. Your internet address may change: nothing to do, the node tells each \
                                 peer the address it sees you at. Strangers will be able to connect to this computer, and this software is unaudited. Behind a provider \
                                 that shares one address between customers (CGNAT) nobody can reach you, and this does nothing.",
                            )
                            .small()
                            .color(GREY),
                        );
                    } else {
                        draft.inbound_port = None;
                    }
                    if let Some(l) = draft.listen.as_ref().filter(|_| draft.inbound_port.is_none()) {
                        ui.label(
                            RichText::new(format!(
                                "The settings file also has `listen = {l}` (it listens but does not tell anyone where to find it)."
                            ))
                            .small()
                            .color(GREY),
                        );
                    }
                    ui.checkbox(
                        &mut draft.external_node,
                        "Use a node that is already running; do not start one",
                    );
                    ui.label(format!(
                        "Control address (this computer only): {}",
                        draft.control
                    ));
                    path_row(ui, "Node data folder", &mut draft.data_dir);
                    let mut pd = draft
                        .program_dir
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    ui.horizontal(|ui| {
                        ui.label(
                            "Folder with tenerod and tenero-miner (empty = next to this program)",
                        );
                        ui.add(egui::TextEdit::singleline(&mut pd).desired_width(240.0));
                    });
                    draft.program_dir = (!pd.trim().is_empty()).then(|| PathBuf::from(pd.trim()));
                });
                if node_busy {
                    ui.label(
                        RichText::new("Stop the node to change these.")
                            .small()
                            .color(GREY),
                    );
                }
                ui.add_space(8.0);
                ui.label(RichText::new("Move the node's data (the blockchain) to another folder or drive").strong());
                ui.label(format!("The node keeps its data in: {}", data_dir_now.display()));
                match &moving {
                    Some(m) => {
                        let what = match m.phase {
                            crate::movedata::Phase::Measuring => "Measuring what there is to copy",
                            crate::movedata::Phase::Copying => "Copying",
                            crate::movedata::Phase::Checking => "Checking the copy against the original",
                        };
                        ui.label(format!("{what}: {} to {}", m.from.display(), m.to.display()));
                        let frac = if m.total == 0 {
                            0.0
                        } else {
                            (m.done as f32 / m.total as f32).clamp(0.0, 1.0)
                        };
                        ui.add(
                            egui::ProgressBar::new(frac)
                                .show_percentage()
                                .text(format!("{} of {}", bytes_text(m.done), bytes_text(m.total))),
                        );
                        if ui.button("Cancel the move").clicked() {
                            cancel_move = true;
                        }
                    }
                    None => {
                        ui.add_enabled_ui(!node_busy && !miner_busy, |ui| {
                            ui.horizontal(|ui| {
                                ui.label("Move the data to");
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.move_to)
                                        .hint_text("D:\\TeneroData")
                                        .desired_width(360.0),
                                );
                            });
                            let ready = !self.move_to.trim().is_empty();
                            if ui
                                .add_enabled(ready, egui::Button::new("Copy the data there, check it, and use it"))
                                .clicked()
                            {
                                start_move = Some(PathBuf::from(self.move_to.trim()));
                            }
                        });
                        if node_busy || miner_busy {
                            ui.label(
                                RichText::new("Stop the node and the miner first.")
                                    .small()
                                    .color(GREY),
                            );
                        }
                        ui.label(
                            RichText::new("Use a new or empty folder (a full path, on any drive). The data is copied, then every file is read back and compared with the original, and only then is the new place used. The old folder is left exactly as it is: delete it yourself once the node has run from the new place. Do not use a network drive, or a USB stick you may unplug. The box above only points the node at another folder (an empty one starts from nothing); it does not move anything.")
                                .small()
                                .color(GREY),
                        );
                    }
                }
                ui.add_space(8.0);
                ui.label(RichText::new("Wallet").strong());
                ui.label(format!("Selected wallet file: {}", wallet_file_now.display()));
                ui.add_enabled_ui(!unlocked, |ui| {
                    path_row(ui, "Wallets folder", &mut draft.wallets_dir)
                });
                if unlocked {
                    ui.label(
                        RichText::new("Lock the wallet (\"Lock / switch wallet\") to choose or add another, or to change the folder.")
                            .small()
                            .color(GREY),
                    );
                }
                ui.add_space(8.0);
                ui.label(RichText::new("Mining").strong());
                ui.add_enabled_ui(!miner_busy, |ui| {
                    egui::ComboBox::from_label("Backend")
                        .selected_text(draft.miner_backend.name())
                        .show_ui(ui, |ui| {
                            for b in MinerBackend::for_network(draft.network) {
                                ui.selectable_value(&mut draft.miner_backend, *b, b.name());
                            }
                        });
                    ui.horizontal(|ui| {
                        ui.label("CPU threads (cpu backend, 1 to 6)");
                        ui.add(egui::DragValue::new(&mut draft.miner_cores).range(1..=6));
                    });
                    ui.horizontal(|ui| {
                        ui.label("GPU number (gpu backend)");
                        ui.add(egui::DragValue::new(&mut draft.miner_gpu_device).range(0..=15));
                    });
                    ui.checkbox(
                        &mut draft.miner_gpu_auto_batch,
                        "Measure the best GPU batch size at start (takes a few seconds)",
                    );
                    ui.horizontal(|ui| {
                        ui.label("Seconds to wait after a block is found");
                        ui.add(egui::DragValue::new(&mut draft.miner_pace_secs).range(0..=3600));
                    });
                });
                if miner_busy {
                    ui.label(
                        RichText::new("Stop the miner to change these.")
                            .small()
                            .color(GREY),
                    );
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    apply = ui
                        .add_enabled(moving.is_none(), egui::Button::new("Apply"))
                        .clicked();
                    reset = ui.button("Discard changes").clicked();
                });
                ui.label(
                    RichText::new(format!(
                        "Settings are kept in {}. They hold no secrets.",
                        app_dir.join("settings.conf").display()
                    ))
                    .small()
                    .color(GREY),
                );
            });
        if apply {
            let mut s = draft.clone();
            s.seeds = seeds
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect();
            // only what the user changed, over the settings as they are now (the draft may be old: see `Settings::with_changes`)
            let now = self.snap.settings.clone();
            let merged = match &self.draft_base {
                Some(base) => now.with_changes(base, &s),
                None => s,
            };
            self.backend.send(Cmd::SetSettings(Box::new(merged)));
            self.draft = None;
            self.draft_base = None;
            self.toast("Settings sent to be applied.", false);
        } else if reset {
            self.draft = None;
            self.draft_base = None;
        }
        if let Some(to) = start_move {
            self.backend.send(Cmd::MoveNodeData { to });
        }
        if cancel_move {
            self.backend.send(Cmd::CancelMove);
        }
    }

    fn about_tab(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.heading("About");
        ui.label(format!(
            "Tenero wallet app, version {} (commit {})",
            env!("CARGO_PKG_VERSION"),
            tenero_app::daemon::COMMIT
        ));
        ui.add_space(6.0);
        for line in [
            "Tenero is an experimental proof-of-work coin, a learning project: unaudited, one developer, not for real value.",
            "Nothing on the gamma, development or test network has any value. Do not treat these coins as money.",
            "Payments use Carrot addresses and FCMP++ proofs, Monero's designs written for Tenero (the FCMP++ crates are monero-oxide's). None of it is audited as used here: do not rely on its privacy.",
            "Nothing cryptographic here has been audited as used.",
        ] {
            ui.label(line);
        }
        ui.add_space(8.0);
        ui.label(format!("App folder: {}", self.app_dir.display()));
        ui.label(format!(
            "Wallet file: {}",
            self.snap.settings.wallet_file.display()
        ));
        ui.label(format!(
            "Node data: {}",
            self.snap.settings.data_dir.display()
        ));
        ui.add_space(8.0);
        ui.label(
            RichText::new("Window library: egui (MIT or Apache-2.0), with its bundled fonts (SIL Open Font License; Ubuntu Font Licence). QR codes: the qrcode crate.")
                .small()
                .color(GREY),
        );
    }
}

/// Payments sent and not yet taken in by a block: how many, the amount going out and the fees (all accounts, or one).
fn pending_out(d: &WalletData, account: Option<usize>) -> (usize, u64, u64) {
    let mut out = (0, 0u64, 0u64);
    for h in &d.history {
        if let EntryKind::Sent {
            fee,
            status: SentStatus::Pending,
            ..
        } = &h.kind
        {
            if account.is_none_or(|a| a == h.account) {
                out.0 += 1;
                out.1 += h.amount;
                out.2 += fee;
            }
        }
    }
    out
}

impl App {
    fn proof_windows(&mut self, ctx: &egui::Context) {
        let mut close_proof = false;
        if let Some((text, note)) = &self.proof_window {
            let mut shown = text.clone();
            egui::Window::new("Payment proof")
                .collapsible(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .default_width(560.0)
                .show(ctx, |ui| {
                    ui.label(note);
                    ui.add_space(4.0);
                    ui.add(
                        egui::TextEdit::multiline(&mut shown)
                            .desired_rows(5)
                            .desired_width(f32::INFINITY)
                            .font(egui::TextStyle::Monospace),
                    );
                    ui.add_space(4.0);
                    ui.colored_label(
                        AMBER,
                        "Whoever you give this to learns the amount and that this output went to that address. It does not show who sent it, and it is not a legal or financial proof (it is unaudited).",
                    );
                    ui.horizontal(|ui| {
                        if ui.button("Copy proof").clicked() {
                            ui.ctx().copy_text(text.clone());
                        }
                        if ui.button("Close").clicked() {
                            close_proof = true;
                        }
                    });
                });
        }
        if close_proof {
            self.proof_window = None;
        }
        let mut close_key = false;
        if let Some(key) = &self.tx_key_window {
            egui::Window::new("Transaction key (secret)")
                .collapsible(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.colored_label(
                        AMBER,
                        "Anyone who has this key can prove this one payment (its amount and that it went to that address). It cannot spend anything. Keep it to yourself unless you mean to prove the payment.",
                    );
                    ui.add_space(4.0);
                    ui.add(egui::Label::new(RichText::new(key.as_str()).monospace()).selectable(true).wrap());
                    ui.horizontal(|ui| {
                        if ui.button("Copy key").clicked() {
                            ui.ctx().copy_text(key.to_string());
                        }
                        if ui.button("Hide").clicked() {
                            close_key = true;
                        }
                    });
                });
        }
        if close_key {
            self.tx_key_window = None;
        }
    }

    fn prove_tab(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.heading("Sign, verify and prove");
        ui.colored_label(
            AMBER,
            "UNAUDITED. Signatures and proofs are being rebuilt on Carrot for 0.3.0; they are not a legal or financial proof of anything.",
        );
        ui.add_space(8.0);

        // ---- sign
        ui.label(RichText::new("Sign a message").strong());
        ui.label("Shows that whoever holds one of your accounts' keys wrote exactly this text. It says nothing about when or where.");
        let wallet = self.wallet().cloned();
        match &wallet {
            None => {
                ui.label(RichText::new("Unlock the wallet (Wallet tab) to sign.").color(GREY));
            }
            Some(d) => {
                if self.prove.sign_account >= d.accounts.len() {
                    self.prove.sign_account = 0;
                }
                egui::ComboBox::from_label("Sign as")
                    .selected_text(
                        d.accounts
                            .get(self.prove.sign_account)
                            .map_or(String::new(), |a| a.label.clone()),
                    )
                    .show_ui(ui, |ui| {
                        for a in &d.accounts {
                            ui.selectable_value(&mut self.prove.sign_account, a.index, &a.label);
                        }
                    });
                ui.add(
                    egui::TextEdit::multiline(&mut self.prove.sign_message)
                        .hint_text("the message")
                        .desired_rows(3)
                        .desired_width(f32::INFINITY),
                );
                if ui
                    .add_enabled(
                        !self.prove.sign_message.is_empty(),
                        egui::Button::new("Sign"),
                    )
                    .clicked()
                {
                    self.prove.signature = None;
                    self.backend.send(Cmd::SignMessage {
                        account: self.prove.sign_account,
                        message: self.prove.sign_message.clone(),
                    });
                }
                if let Some(sig) = self.prove.signature.clone() {
                    if let Some(a) = d.accounts.get(self.prove.sign_account) {
                        ui.label(
                            RichText::new(format!("Signed by {}", a.address))
                                .small()
                                .color(GREY),
                        );
                    }
                    ui.add(
                        egui::Label::new(RichText::new(&sig).monospace().small())
                            .selectable(true)
                            .wrap(),
                    );
                    if ui.button("Copy signature").clicked() {
                        ui.ctx().copy_text(sig);
                    }
                }
            }
        }
        ui.add_space(10.0);
        ui.separator();

        // ---- verify
        ui.label(RichText::new("Verify a signed message").strong());
        ui.label("Needs only the address, the message and the signature: no wallet and no node.");
        ui.horizontal(|ui| {
            ui.label("Address");
            ui.add(
                egui::TextEdit::singleline(&mut self.prove.verify_address)
                    .desired_width(f32::INFINITY)
                    .font(egui::TextStyle::Monospace),
            );
        });
        ui.add(
            egui::TextEdit::multiline(&mut self.prove.verify_message)
                .hint_text("the message, exactly as it was signed")
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        );
        ui.horizontal(|ui| {
            ui.label("Signature");
            ui.add(
                egui::TextEdit::singleline(&mut self.prove.verify_signature)
                    .desired_width(f32::INFINITY)
                    .font(egui::TextStyle::Monospace),
            );
        });
        if ui.button("Verify").clicked() {
            self.prove.verified = Some(verify_text(
                &self.prove.verify_address,
                &self.prove.verify_message,
                &self.prove.verify_signature,
            ));
        }
        match &self.prove.verified {
            Some(Ok(m)) => {
                ui.colored_label(GREEN, m);
            }
            Some(Err(e)) => {
                ui.colored_label(RED, e);
            }
            None => {}
        }
        ui.add_space(10.0);
        ui.separator();

        // ---- check a payment proof
        ui.label(RichText::new("Check a payment proof").strong());
        ui.label("Paste a proof (it starts with tnpay1). The node is asked for the output it names, so the node must be running; no wallet is needed.");
        ui.add(
            egui::TextEdit::multiline(&mut self.prove.check_text)
                .hint_text("tnpay1…")
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .font(egui::TextStyle::Monospace),
        );
        let node_up = matches!(self.snap.node, NodeView::Running { .. });
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    node_up && !self.prove.check_text.trim().is_empty(),
                    egui::Button::new("Check against the node"),
                )
                .clicked()
            {
                self.prove.checked = None;
                self.backend.send(Cmd::CheckProof {
                    text: self.prove.check_text.clone(),
                });
            }
            if !node_up {
                ui.label(
                    RichText::new("start the node first (Node tab)")
                        .small()
                        .color(GREY),
                );
            }
        });
        ui.add_space(10.0);
        ui.separator();

        // ---- check a transaction key
        ui.label(RichText::new("Check a transaction key").strong());
        ui.label("A transaction key (shown in History, \"Show transaction key\") and the address it paid. The node's chain is read from the block you give until the output that key made is found, so give a block at or before the payment: a vague start is a slower check.");
        ui.horizontal(|ui| {
            ui.label("Key");
            ui.add(
                egui::TextEdit::singleline(&mut self.prove.key_text)
                    .desired_width(f32::INFINITY)
                    .font(egui::TextStyle::Monospace),
            );
        });
        ui.horizontal(|ui| {
            ui.label("Address");
            ui.add(
                egui::TextEdit::singleline(&mut self.prove.key_address)
                    .desired_width(f32::INFINITY)
                    .font(egui::TextStyle::Monospace),
            );
        });
        ui.horizontal(|ui| {
            ui.label("Search from block");
            ui.add(
                egui::TextEdit::singleline(&mut self.prove.key_from)
                    .hint_text("0")
                    .desired_width(100.0),
            );
        });
        let from_ok = self.prove.key_from.trim().is_empty()
            || self.prove.key_from.trim().parse::<u64>().is_ok();
        if !from_ok {
            ui.colored_label(AMBER, "The block number must be digits only.");
        }
        ui.horizontal(|ui| {
            let ready = node_up
                && from_ok
                && !self.prove.key_text.trim().is_empty()
                && !self.prove.key_address.trim().is_empty();
            if ui
                .add_enabled(ready, egui::Button::new("Check the key against the node"))
                .clicked()
            {
                self.prove.checked = None;
                self.backend.send(Cmd::CheckKey {
                    key: self.prove.key_text.clone(),
                    address: self.prove.key_address.clone(),
                    from_height: self.prove.key_from.trim().parse().ok(),
                });
            }
            if !node_up {
                ui.label(
                    RichText::new("start the node first (Node tab)")
                        .small()
                        .color(GREY),
                );
            }
        });
        ui.add_space(8.0);
        ui.label(RichText::new("Result").strong());
        match &self.prove.checked {
            Some(Ok(c)) => {
                ui.colored_label(GREEN, "VALID");
                egui::Grid::new("checked")
                    .num_columns(2)
                    .spacing([20.0, 4.0])
                    .show(ui, |ui| {
                        let mut row = |k: &str, v: String| {
                            ui.label(RichText::new(k).color(GREY));
                            ui.label(v);
                            ui.end_row();
                        };
                        row("Kind", c.kind.to_string());
                        row(
                            "Amount",
                            text::coins(c.amount)
                                + if c.block_reward {
                                    " (a block reward)"
                                } else {
                                    ""
                                },
                        );
                        row("Paid to", text::short_address(&c.address));
                        row(
                            "In block",
                            format!(
                                "{} (output {})",
                                group_digits(c.height),
                                group_digits(c.global_index)
                            ),
                        );
                        row("Blocks on top", group_digits(c.confirmations));
                    });
                ui.label(
                    RichText::new("This shows the output is in the node's chain and is addressed to that address with that amount. It does not show who sent it.")
                        .small()
                        .color(GREY),
                );
            }
            Some(Err(e)) => {
                ui.colored_label(RED, format!("NOT valid: {e}"));
            }
            None => {}
        }
    }
}

/// Verifies a pasted signature (needs no wallet and no node): rebuilt on Carrot in milestone G5.
fn verify_text(_address: &str, _message: &str, _signature: &str) -> Result<String, String> {
    Err(crate::core::PROOFS_IN_G5.into())
}

/// A QR code of `text`, drawn as squares.
fn draw_qr(ui: &mut egui::Ui, text: &str) {
    match crate::qr::modules(text) {
        Some((w, squares)) => {
            let quiet = 4.0;
            let cell = (260.0 / (w as f32 + 2.0 * quiet)).floor().max(2.0);
            let side = cell * (w as f32 + 2.0 * quiet);
            let (rect, _) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::hover());
            let p = ui.painter_at(rect);
            p.rect_filled(rect, 0.0, Color32::WHITE);
            for y in 0..w {
                for x in 0..w {
                    if squares[y * w + x] {
                        let min = rect.min
                            + egui::vec2((x as f32 + quiet) * cell, (y as f32 + quiet) * cell);
                        p.rect_filled(
                            egui::Rect::from_min_size(min, egui::vec2(cell, cell)),
                            0.0,
                            Color32::BLACK,
                        );
                    }
                }
            }
        }
        None => {
            ui.label("(this does not fit a QR code)");
        }
    }
}

fn account_text(d: &WalletData, index: usize) -> String {
    match d.accounts.get(index) {
        Some(a) => match a.balance {
            Some(b) => format!("{} ({})", a.label, text::coins(b.total)),
            None => a.label.clone(),
        },
        None => format!("account {index}"),
    }
}

/// A size for the screen: "3.2 GiB", "480 MiB", "12 KiB", "0 bytes".
fn bytes_text(n: u64) -> String {
    const KIB: f64 = 1024.0;
    let x = n as f64;
    if x >= KIB * KIB * KIB {
        format!("{:.2} GiB", x / (KIB * KIB * KIB))
    } else if x >= KIB * KIB {
        format!("{:.1} MiB", x / (KIB * KIB))
    } else if x >= KIB {
        format!("{:.0} KiB", x / KIB)
    } else {
        format!("{n} bytes")
    }
}

fn path_row(ui: &mut egui::Ui, label: &str, path: &mut PathBuf) {
    let mut s = path.display().to_string();
    ui.horizontal(|ui| {
        ui.label(label);
        ui.add(egui::TextEdit::singleline(&mut s).desired_width(420.0));
    });
    *path = PathBuf::from(s);
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }
}

impl App {
    /// One frame of the whole window.
    pub fn draw(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.drain();
        // closing: ask the worker to stop what this window started, and close when it says it has
        if ctx.input(|i| i.viewport().close_requested()) && !self.closing {
            self.closing = true;
            self.closing_since = Some(Instant::now());
            self.backend.send(Cmd::Quit);
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        if self
            .closing_since
            .is_some_and(|t| t.elapsed() > Duration::from_secs(75))
        {
            self.backend.emergency_stop(&self.snap.settings);
            std::process::exit(0);
        }
        // a request that has had no answer for two minutes does not keep the buttons off for ever
        if self
            .send
            .working
            .is_some_and(|t| t.elapsed() > Duration::from_secs(120))
        {
            self.send.working = None;
            self.send.error =
                Some("the node did not answer in two minutes: look at the Node tab".into());
        }
        ctx.request_repaint_after(Duration::from_millis(500));

        egui::Panel::top("banner").show(ui, |ui| {
            self.banner(ui);
            ui.add_space(2.0);
            self.status_bar(ui);
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                for (tab, name) in Tab::ALL {
                    ui.selectable_value(&mut self.tab, tab, name);
                }
            });
        });
        egui::Panel::bottom("toasts").show(ui, |ui| {
            self.toasts_ui(ui);
            if self.closing {
                ui.colored_label(AMBER, "Closing: stopping what this window started…");
            }
        });
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match self.tab {
                    Tab::Node => self.node_tab(ui),
                    Tab::Settings => self.settings_tab(ui),
                    Tab::About => self.about_tab(ui),
                    Tab::Prove => self.prove_tab(ui),
                    tab => {
                        if self.gate(ui) {
                            match tab {
                                Tab::Wallet => self.wallet_tab(ui),
                                Tab::Send => self.send_tab(ui),
                                Tab::Receive => self.receive_tab(ui),
                                Tab::History => self.history_tab(ui),
                                Tab::Mining => self.mining_tab(ui),
                                _ => {}
                            }
                        }
                    }
                });
        });
        self.phrase_window(&ctx);
        self.prompts(&ctx);
        self.proof_windows(&ctx);
    }
}
