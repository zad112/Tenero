//! The wallet app's logic, without a window: it owns the wallet (unlocked in memory only while the person has it open),
//! the node and miner processes this program started, and the one payment that is waiting for a yes. The window drives it
//! with [`Cmd`]s and draws [`Snapshot`]s; `backend.rs` runs it on a thread. Everything here is tested without a window.
//!
//! What it keeps in memory while a wallet is open, said plainly: the seed (inside the purse) and the password (so the
//! wallet file can be rewritten as it scans and sends; it is zeroed when the wallet is locked or the program ends). It
//! writes neither to a log or to a screen except when the person asks to see the words.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand_core::OsRng;
use tenero_app::client::{RemoteNode, COOKIE_FILE};
use tenero_app::control::NodeInfo;
use tenero_app::miner_report::MinerReport;
use tenero_wallet::amount::parse_coins;
use tenero_wallet::{
    Address, Balance, Built, ChainView, FeeLevel, FileError, KdfParams, Purse, PurseError, Rules,
    ScanBlock,
};
use zeroize::Zeroizing;

use crate::movedata;
use crate::procs::{self, Proc};
use crate::settings::{MinerBackend, Settings};
use crate::view::*;
use crate::wallets::{self, WalletEntry};

/// How many blocks one pass of scanning reads (so the window stays responsive during a long first scan).
const SCAN_SLICE: u64 = 400;
/// How often to ask the node about itself.
const POLL_EVERY: Duration = Duration::from_secs(2);
/// A miner report older than this is shown as stale.
const MINER_STALE_SECS: u64 = 5;
/// How long a node that was just started may take to answer before it is called failed.
const NODE_START_PATIENCE: Duration = Duration::from_secs(90);

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A chain view that stops at `cap`, so one pass of scanning is a bounded amount of work.
struct Capped<'a> {
    node: &'a RemoteNode,
    cap: u64,
}

impl ChainView for Capped<'_> {
    fn tip(&self) -> Result<(u64, [u8; 32]), String> {
        let (h, id) = self.node.tip()?;
        if h <= self.cap {
            return Ok((h, id));
        }
        let b = self
            .node
            .block(self.cap)?
            .ok_or_else(|| "the node has no block at the height it reported".to_string())?;
        Ok((self.cap, b.id))
    }
    fn block(&self, height: u64) -> Result<Option<ScanBlock>, String> {
        self.node.block(height)
    }
    fn blocks(&self, from: u64, max: u64) -> Result<Vec<ScanBlock>, String> {
        if from > self.cap {
            return Ok(Vec::new());
        }
        self.node.blocks(from, max.min(self.cap - from + 1))
    }
    fn output(&self, i: u64) -> Result<Option<tenero_store::StoredOutput>, String> {
        self.node.output(i)
    }
    fn outputs(&self, is: &[u64]) -> Result<Vec<Option<tenero_store::StoredOutput>>, String> {
        self.node.outputs(is)
    }
    fn output_count(&self) -> Result<u64, String> {
        self.node.output_count()
    }
    fn key_image_spent(&self, k: &[u8; 32]) -> Result<bool, String> {
        self.node.key_image_spent(k)
    }
    fn key_images_spent(&self, ks: &[[u8; 32]]) -> Result<Vec<bool>, String> {
        self.node.key_images_spent(ks)
    }
    fn rules(&self) -> Result<Rules, String> {
        self.node.rules()
    }
}

/// What a prepared set of transactions is.
enum PreparedKind {
    /// A payment to `to`.
    Pay { to_text: String },
    /// A combining of the account's own coins; `what` is the note it is recorded with.
    Own { what: String },
}

struct Prepared {
    account: usize,
    kind: PreparedKind,
    /// One transaction, or several that spend different coins.
    txs: Vec<Built>,
    /// The payments that could not be made now.
    unsent: Vec<(Address, u64)>,
    level: FeeLevel,
    note: Option<String>,
}

/// The programs this window started (`true` = the node, `false` = the miner), kept where the window can reach them even if
/// the worker thread is stuck, so that closing the window can always end what it started.
pub type Registry = std::sync::Arc<std::sync::Mutex<Vec<(bool, Proc)>>>;

pub struct Core {
    registry: Registry,
    wallets: Vec<WalletEntry>,
    app_dir: PathBuf,
    settings: Settings,
    kdf: KdfParams,
    purse: Option<Purse>,
    pass: Option<Zeroizing<Vec<u8>>>,
    has_password: bool,
    data: Option<WalletData>,
    node: Option<RemoteNode>,
    node_proc: Option<Proc>,
    node_started: Option<Instant>,
    node_view: NodeView,
    pending_stop: bool,
    last_poll: Option<Instant>,
    miner_proc: Option<Proc>,
    miner_started: Option<Instant>,
    miner_view: MinerView,
    prepared: Option<Prepared>,
    needs_discovery: bool,
    dirty: bool,
    last_save: Instant,
    refresh_due: bool,
    last_refresh: Option<Instant>,
    last_tip: Option<u64>,
    /// The node's data being copied to another folder, if it is.
    move_job: Option<MoveJob>,
}

/// A move of the node's data running on a thread of its own (a chain can be many gigabytes: the window must stay alive), polled by `tick`.
struct MoveJob {
    from: PathBuf,
    to: PathBuf,
    progress: Arc<movedata::Progress>,
    thread: Option<std::thread::JoinHandle<Result<(), String>>>,
}

fn pass_ok(p: &Password) -> Result<Zeroizing<Vec<u8>>, String> {
    match p {
        Password::Set(s) => {
            if s.chars().count() < MIN_PASSWORD {
                return Err(format!(
                    "the password must be at least {MIN_PASSWORD} characters (or choose no password)"
                ));
            }
            Ok(Zeroizing::new(s.as_bytes().to_vec()))
        }
        Password::None => Ok(Zeroizing::new(Vec::new())),
    }
}

fn file_err(e: FileError) -> String {
    match e {
        FileError::WrongPassphraseOrCorrupt => {
            "wrong password, or the wallet file has been changed".to_string()
        }
        other => other.to_string(),
    }
}

fn purse_err(e: PurseError) -> String {
    e.to_string()
}

impl Core {
    pub fn new(app_dir: &Path, settings: Settings, kdf: KdfParams) -> Core {
        Core::with_registry(app_dir, settings, kdf, Registry::default())
    }

    pub fn with_registry(
        app_dir: &Path,
        settings: Settings,
        kdf: KdfParams,
        registry: Registry,
    ) -> Core {
        let mut c = Core {
            registry,
            wallets: Vec::new(),
            app_dir: app_dir.to_path_buf(),
            settings,
            kdf,
            purse: None,
            pass: None,
            has_password: true,
            data: None,
            node: None,
            node_proc: None,
            node_started: None,
            node_view: NodeView::Stopped,
            pending_stop: false,
            last_poll: None,
            miner_proc: None,
            miner_started: None,
            miner_view: MinerView::Off,
            prepared: None,
            needs_discovery: false,
            dirty: false,
            last_save: Instant::now(),
            refresh_due: false,
            last_refresh: None,
            last_tip: None,
            move_job: None,
        };
        c.refresh_wallets();
        // a node an earlier run (or the owner) left going is picked up, not started a second time
        c.poll_node(true);
        c
    }

    fn register(&self, is_node: bool, p: &Proc) {
        if let Ok(mut r) = self.registry.lock() {
            r.retain(|(_, q)| q.exited_quietly().is_none());
            r.push((is_node, p.clone()));
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Reads the list of wallets again, and if the selected file is not there but others are, selects the first of them.
    fn refresh_wallets(&mut self) {
        self.wallets = wallets::list(&self.settings);
        if !self.settings.wallet_file.is_file() {
            if let Some(first) = self.wallets.first() {
                self.settings.wallet_file = first.path.clone();
            }
        }
    }

    fn wallet_exists(&self) -> bool {
        self.settings.wallet_file.exists()
    }

    fn miner_status_file(&self) -> PathBuf {
        self.app_dir.join("miner-status.txt")
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            settings: self.settings.clone(),
            wallets: self.wallets.clone(),
            wallet: match (&self.purse, &self.data) {
                (Some(p), Some(d)) => {
                    let mut d = d.clone();
                    d.requests = self.request_views(p);
                    WalletView::Unlocked(Box::new(d))
                }
                (Some(p), None) => {
                    let mut d = self.bare_data(p);
                    d.requests = self.request_views(p);
                    WalletView::Unlocked(Box::new(d))
                }
                (None, _) if self.wallet_exists() => WalletView::Locked,
                (None, _) => WalletView::NoWallet,
            },
            node: self.node_view.clone(),
            miner: self.miner_view.clone(),
            prepared: self.prepared.as_ref().map(|p| Quote {
                account: p.account,
                to: match &p.kind {
                    PreparedKind::Pay { to_text } => to_text.clone(),
                    PreparedKind::Own { .. } => String::new(),
                },
                amount: p.txs.iter().map(|t| t.amount).sum(),
                fee: p.txs.iter().map(|t| t.fee).sum(),
                change: p.txs.iter().map(|t| t.change).sum(),
                level: p.level,
                note: p.note.clone(),
                transactions: p.txs.len(),
                coins: p.txs.iter().map(|t| t.tx.prefix.inputs.len()).sum(),
                unsent_payments: p.unsent.len(),
                unsent_total: p.unsent.iter().map(|(_, v)| *v).sum(),
                own: match &p.kind {
                    PreparedKind::Own { what } => Some(what.clone()),
                    PreparedKind::Pay { .. } => None,
                },
            }),
            busy: None,
            moving: self.move_job.as_ref().map(|j| MoveView {
                from: j.from.clone(),
                to: j.to.clone(),
                phase: j.progress.phase(),
                done: j.progress.done(),
                total: j.progress.total(),
            }),
        }
    }

    /// The payment requests made, as the window shows them (they need no node).
    fn request_views(&self, p: &Purse) -> Vec<RequestView> {
        p.requests()
            .iter()
            .enumerate()
            .filter_map(|(i, q)| {
                let uri = p.request_of(i).ok()?.to_uri();
                Some(RequestView {
                    index: i,
                    account: q.account as usize,
                    account_label: p
                        .accounts()
                        .get(q.account as usize)
                        .map_or(String::new(), |a| a.label().to_string()),
                    amount: q.amount,
                    label: q.label.clone(),
                    message: q.message.clone(),
                    time: q.time,
                    uri,
                })
            })
            .collect()
    }

    /// The wallet as far as it can be shown with no node: the accounts and addresses, no balances.
    fn bare_data(&self, p: &Purse) -> WalletData {
        WalletData {
            accounts: p
                .accounts()
                .iter()
                .enumerate()
                .map(|(i, a)| AccountView {
                    index: i,
                    label: a.label().to_string(),
                    address: a.address().to_text(),
                    balance: None,
                })
                .collect(),
            total: None,
            history: Vec::new(),
            requests: Vec::new(),
            scanned: p
                .accounts()
                .iter()
                .filter_map(|a| a.wallet().scanned_height())
                .min(),
            tip: None,
            synced: false,
            has_password: self.has_password,
        }
    }

    fn with_snapshot(&self, mut events: Vec<Event>) -> Vec<Event> {
        events.push(Event::Snapshot(Box::new(self.snapshot())));
        events
    }

    // ----------------------------------------------------------------------------------------------------------
    // commands
    // ----------------------------------------------------------------------------------------------------------

    pub fn handle(&mut self, cmd: Cmd) -> Vec<Event> {
        let mut events = Vec::new();
        let result = match cmd {
            Cmd::CreateWallet { password, name } => self.create_wallet(password, name, &mut events),
            Cmd::SelectWallet { path } => self.select_wallet(&path),
            Cmd::RestoreWallet {
                phrase,
                password,
                birth,
                name,
            } => self.restore_wallet(&phrase, password, birth, name),
            Cmd::Unlock { password } => self.unlock(&password),
            Cmd::Lock => self.lock(),
            Cmd::RevealPhrase { password } => self.reveal(&password, &mut events),
            Cmd::ChangePassword { old, new } => self.change_password(&old, new),
            Cmd::AddAccount { label } => self.add_account(&label),
            Cmd::RenameAccount { index, label } => self.rename_account(index, &label),
            Cmd::EstimateFees {
                account,
                to,
                amount,
            } => match self.estimate(account, &to, &amount, &mut events) {
                Ok(()) => Ok(()),
                Err(e) => {
                    events.push(Event::EstimateFailed(e));
                    Ok(())
                }
            },
            Cmd::PreparePayment {
                account,
                to,
                amount,
                level,
                note,
            } => self.prepare(account, &to, &amount, level, note),
            Cmd::AddRequest {
                account,
                amount,
                label,
                message,
            } => self.add_request(account, &amount, &label, &message),
            Cmd::DeleteRequest { index } => self.delete_request(index),
            Cmd::PrepareCombine {
                account,
                coins,
                level,
            } => self.prepare_combine(account, coins, level),
            Cmd::SendPrepared => self.send_prepared(&mut events),
            Cmd::SignMessage { account, message } => {
                self.sign_message(account, &message, &mut events)
            }
            Cmd::MakeProof(req) => self.make_proof(req, &mut events),
            Cmd::RevealTxKey { id } => self.reveal_tx_key(&id, &mut events),
            Cmd::CheckKey {
                key,
                address,
                from_height,
            } => {
                let r = self.check_key(&key, &address, from_height.unwrap_or(0));
                events.push(Event::ProofChecked(r));
                Ok(())
            }
            Cmd::CheckProof { text } => {
                let r = self.check_proof(&text);
                events.push(Event::ProofChecked(r));
                Ok(())
            }
            Cmd::CancelPrepared => {
                self.prepared = None;
                Ok(())
            }
            Cmd::StartNode => self.start_node(&mut events),
            Cmd::StopNode => self.request_stop_node(),
            Cmd::StartMiner => self.start_miner(),
            Cmd::StopMiner => {
                self.stop_miner();
                Ok(())
            }
            Cmd::SetSettings(s) => self.set_settings(*s),
            Cmd::MoveNodeData { to } => self.start_move(to),
            Cmd::CancelMove => {
                if let Some(j) = &self.move_job {
                    j.progress.cancel();
                }
                Ok(())
            }
            Cmd::Quit => {
                self.quit();
                events.push(Event::Snapshot(Box::new(self.snapshot())));
                events.push(Event::Quit);
                return events;
            }
        };
        if let Err(e) = result {
            events.push(Event::Error(e));
        }
        self.with_snapshot(events)
    }

    fn need_purse(&mut self) -> Result<&mut Purse, String> {
        self.purse
            .as_mut()
            .ok_or_else(|| "the wallet is locked".to_string())
    }

    fn save_wallet(&mut self) -> Result<(), String> {
        let (Some(p), Some(pw)) = (&self.purse, &self.pass) else {
            return Ok(());
        };
        if let Some(dir) = self.settings.wallet_file.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        p.save(&self.settings.wallet_file, pw, self.kdf, &mut OsRng)
            .map_err(file_err)?;
        self.dirty = false;
        self.last_save = Instant::now();
        Ok(())
    }

    /// The height a wallet made now should start scanning from: the node's tip if one answers (nothing sent to an
    /// address that did not exist yet can be older), else the start of the chain (safe, and cheap on a young chain).
    fn birth_now(&self) -> u64 {
        self.node
            .as_ref()
            .and_then(|n| n.tip().ok())
            .map_or(0, |(h, _)| h)
    }

    fn open(&mut self, purse: Purse, pw: Zeroizing<Vec<u8>>) {
        self.has_password = !pw.is_empty();
        self.purse = Some(purse);
        self.pass = Some(pw);
        self.data = None;
        self.prepared = None;
        self.refresh_due = true;
    }

    /// Where a new wallet goes: a named one in the wallets folder (which becomes the selected one), or, with no name, the
    /// selected file's own place (a wallet from before names).
    fn target_for_new(&mut self, name: Option<String>) -> Result<(), String> {
        if self.purse.is_some() {
            return Err("lock the open wallet first (\"Lock / switch wallet\"), then make or restore another".into());
        }
        match name {
            Some(n) => {
                let (_, path) = wallets::new_path(&self.settings, &n)?;
                std::fs::create_dir_all(&self.settings.wallets_dir).map_err(|e| {
                    format!("cannot create {}: {e}", self.settings.wallets_dir.display())
                })?;
                self.settings.wallet_file = path;
            }
            None => {
                if self.wallet_exists() {
                    return Err("a wallet file already exists here; unlock it, or give the new wallet a name".into());
                }
            }
        }
        Ok(())
    }

    /// After a wallet was written: remember it as the selected one and list it.
    fn remember_new_wallet(&mut self) {
        let _ = self.settings.save(&self.app_dir);
        self.wallets = wallets::list(&self.settings);
    }

    fn create_wallet(
        &mut self,
        password: Password,
        name: Option<String>,
        events: &mut Vec<Event>,
    ) -> Result<(), String> {
        let pw = pass_ok(&password)?;
        let before = self.settings.wallet_file.clone();
        self.target_for_new(name)?;
        let purse = Purse::create(&mut OsRng, self.birth_now());
        let words = purse.phrase();
        self.open(purse, pw);
        if let Err(e) = self.save_wallet() {
            // nothing was written: go back to what was selected
            self.purse = None;
            self.pass = None;
            self.settings.wallet_file = before;
            return Err(e);
        }
        self.remember_new_wallet();
        events.push(Event::Phrase { words, new: true });
        Ok(())
    }

    fn restore_wallet(
        &mut self,
        phrase: &str,
        password: Password,
        birth: Option<u64>,
        name: Option<String>,
    ) -> Result<(), String> {
        let pw = pass_ok(&password)?;
        let seed = tenero_wallet::seed_of(phrase).map_err(|e| e.to_string())?;
        let before = self.settings.wallet_file.clone();
        self.target_for_new(name)?;
        let purse = Purse::from_seed(&seed, birth.unwrap_or(0));
        self.open(purse, pw);
        // the number of accounts is not in the words: look for them once the node can be read
        self.needs_discovery = true;
        if let Err(e) = self.save_wallet() {
            self.purse = None;
            self.pass = None;
            self.needs_discovery = false;
            self.settings.wallet_file = before;
            return Err(e);
        }
        self.remember_new_wallet();
        Ok(())
    }

    fn select_wallet(&mut self, path: &std::path::Path) -> Result<(), String> {
        if self.purse.is_some() {
            return Err("lock the open wallet first".into());
        }
        if !self.wallets.iter().any(|w| w.path == path) {
            return Err("that wallet is not in the list".into());
        }
        self.settings.wallet_file = path.to_path_buf();
        self.settings.save(&self.app_dir)?;
        self.data = None;
        Ok(())
    }

    fn unlock(&mut self, password: &str) -> Result<(), String> {
        if self.purse.is_some() {
            return Ok(());
        }
        if !self.wallet_exists() {
            return Err("there is no wallet file here".into());
        }
        let purse =
            Purse::load(&self.settings.wallet_file, password.as_bytes()).map_err(file_err)?;
        self.open(purse, Zeroizing::new(password.as_bytes().to_vec()));
        Ok(())
    }

    fn lock(&mut self) -> Result<(), String> {
        self.save_wallet()?;
        self.purse = None;
        self.pass = None;
        self.data = None;
        self.prepared = None;
        // what was learned about one wallet's scanning is not true of the next
        self.needs_discovery = false;
        self.last_tip = None;
        self.dirty = false;
        self.last_refresh = None;
        self.wallets = wallets::list(&self.settings);
        // mining pays a wallet address: with the wallet locked the miner is stopped, so the screen never shows a
        // miner working for a wallet that is not open
        self.stop_miner();
        Ok(())
    }

    fn reveal(&mut self, password: &str, events: &mut Vec<Event>) -> Result<(), String> {
        let words = self.purse.as_ref().ok_or("the wallet is locked")?.phrase();
        // asked again on purpose: someone at an open window must not be able to read the words by a click
        if self.pass.as_ref().map(|p| p.as_slice()) != Some(password.as_bytes()) {
            return Err("wrong password".into());
        }
        events.push(Event::Phrase { words, new: false });
        Ok(())
    }

    fn change_password(&mut self, old: &str, new: Password) -> Result<(), String> {
        if self.pass.as_ref().map(|p| p.as_slice()) != Some(old.as_bytes()) {
            return Err("wrong password".into());
        }
        let pw = pass_ok(&new)?;
        let previous = (self.pass.take(), self.has_password);
        self.has_password = !pw.is_empty();
        self.pass = Some(pw);
        if let Err(e) = self.save_wallet() {
            self.pass = previous.0;
            self.has_password = previous.1;
            return Err(e);
        }
        Ok(())
    }

    fn add_account(&mut self, label: &str) -> Result<(), String> {
        let birth = if self.node.is_some() {
            self.birth_now()
        } else {
            self.purse.as_ref().map_or(0, Purse::birth_height)
        };
        self.need_purse()?
            .add_account(label, birth)
            .map_err(purse_err)?;
        self.save_wallet()?;
        self.refresh_due = true;
        Ok(())
    }

    fn rename_account(&mut self, index: usize, label: &str) -> Result<(), String> {
        self.need_purse()?.rename(index, label).map_err(purse_err)?;
        self.save_wallet()?;
        self.refresh_due = true;
        Ok(())
    }

    // ---- payment requests -------------------------------------------------------------------------------

    fn add_request(
        &mut self,
        account: usize,
        amount: &str,
        label: &str,
        message: &str,
    ) -> Result<(), String> {
        let amount = match amount.trim() {
            "" => None,
            a => Some(parse_coins(a).ok_or_else(|| {
                format!("amount: `{a}` is not an amount (digits with up to 8 decimals), or leave it empty")
            })?),
        };
        let opt = |t: &str| (!t.trim().is_empty()).then(|| t.trim().to_string());
        self.need_purse()?
            .add_request(account, amount, opt(label), opt(message), now_unix())
            .map_err(purse_err)?;
        // a request is kept in the wallet file: write it now, not at the next scan
        self.save_wallet()
    }

    fn delete_request(&mut self, index: usize) -> Result<(), String> {
        self.need_purse()?
            .remove_request(index)
            .map_err(purse_err)?;
        self.save_wallet()
    }

    // ---- paying ------------------------------------------------------------------------------------------

    fn ready_to_pay(&self) -> Result<&RemoteNode, String> {
        let node = self
            .node
            .as_ref()
            .ok_or("the node is not running: start it first (Node tab)")?;
        let d = self
            .data
            .as_ref()
            .ok_or("the wallet has not finished loading")?;
        if !d.synced {
            return Err("the wallet is still catching up with the chain: wait until it says it is up to date".into());
        }
        Ok(node)
    }

    fn parse_payment(&self, to: &str, amount: &str) -> Result<(Address, u64), String> {
        let addr = Address::from_text(to.trim()).map_err(|e| format!("address: {e}"))?;
        let units = parse_coins(amount.trim()).ok_or_else(|| {
            format!(
                "amount: `{}` is not an amount (digits with up to 8 decimals)",
                amount.trim()
            )
        })?;
        if units == 0 {
            return Err("amount: cannot pay nothing".into());
        }
        Ok((addr, units))
    }

    fn estimate(
        &mut self,
        account: usize,
        to: &str,
        amount: &str,
        events: &mut Vec<Event>,
    ) -> Result<(), String> {
        let (addr, units) = self.parse_payment(to, amount)?;
        self.ready_to_pay()?;
        let node = self.node.as_ref().expect("checked");
        let purse = self.purse.as_mut().ok_or("the wallet is locked")?;
        let plan = purse
            .build_batch(account, node, &mut OsRng, &[(addr, units)], FeeLevel::Low)
            .map_err(purse_err)?;
        let rules = node.rules()?;
        // a payment that needs several transactions pays the fee of each
        let mut fees = [0u64; 3];
        for built in &plan.txs {
            let size = tenero_core::v2::Wire::to_bytes(&built.tx)
                .map_err(|e| e.to_string())?
                .len() as u64;
            let min = tenero_core::fees::dynamic_min_fee(size, rules.reward, rules.median)?;
            for (f, level) in fees.iter_mut().zip(FeeLevel::ALL) {
                *f += min.saturating_mul(level.percent_of_minimum()) / 100 + 1;
            }
        }
        events.push(Event::Estimate { fees });
        Ok(())
    }

    fn prepare(
        &mut self,
        account: usize,
        to: &str,
        amount: &str,
        level: FeeLevel,
        note: Option<String>,
    ) -> Result<(), String> {
        self.prepared = None;
        let (addr, units) = self.parse_payment(to, amount)?;
        self.ready_to_pay()?;
        let node = self.node.as_ref().expect("checked");
        let purse = self.purse.as_mut().ok_or("the wallet is locked")?;
        let plan = purse
            .build_batch(account, node, &mut OsRng, &[(addr, units)], level)
            .map_err(purse_err)?;
        self.prepared = Some(Prepared {
            account,
            kind: PreparedKind::Pay {
                to_text: addr.to_text(),
            },
            txs: plan.txs,
            unsent: plan.unsent,
            level,
            note: note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()),
        });
        Ok(())
    }

    /// Prepares combining an account's own coins (see `Cmd::PrepareCombine`).
    fn prepare_combine(
        &mut self,
        account: usize,
        coins: Option<usize>,
        level: FeeLevel,
    ) -> Result<(), String> {
        self.prepared = None;
        self.ready_to_pay()?;
        let node = self.node.as_ref().expect("checked");
        let purse = self.purse.as_mut().ok_or("the wallet is locked")?;
        let txs = match coins {
            Some(n) => vec![purse
                .build_combine(account, node, &mut OsRng, n, level)
                .map_err(purse_err)?],
            None => purse
                .build_sweep(account, node, &mut OsRng, None, level)
                .map_err(purse_err)?,
        };
        let n: usize = txs.iter().map(|t| t.tx.prefix.inputs.len()).sum();
        self.prepared = Some(Prepared {
            account,
            kind: PreparedKind::Own {
                what: format!("Combined {n} pieces"),
            },
            txs,
            unsent: Vec::new(),
            level,
            note: None,
        });
        Ok(())
    }

    fn send_prepared(&mut self, events: &mut Vec<Event>) -> Result<(), String> {
        let p = self
            .prepared
            .take()
            .ok_or("there is no payment waiting to be sent")?;
        // the screen said "not final" while out of date; a payment built a while ago is rebuilt rather than sent stale
        self.ready_to_pay()?;
        let mut node = self.node.take().ok_or("the node is not running")?;
        let result = {
            let purse = self.purse.as_mut().ok_or("the wallet is locked")?;
            match &p.kind {
                PreparedKind::Pay { .. } => {
                    purse.send_batch(p.account, &mut node, &p.txs, p.level, now_unix())
                }
                PreparedKind::Own { what } => {
                    purse.send_own(p.account, &mut node, &p.txs, p.level, what, now_unix())
                }
            }
            .map_err(purse_err)
        };
        self.node = Some(node);
        let sent = result?;
        if sent.sent == 0 {
            if let Some(e) = sent.failed {
                return Err(purse_err(tenero_wallet::PurseError::Wallet(e)));
            }
        }
        if sent.sent > 0 {
            if let (Some(note), Some(purse), PreparedKind::Pay { .. }) =
                (&p.note, self.purse.as_mut(), &p.kind)
            {
                // a note that cannot be kept (too long) must not undo a payment that has been sent
                let _ = purse.annotate_sent(&p.txs[0].id, note);
            }
            // the reservation and the record must reach the file, or a restart would pick the same coins
            self.save_wallet()?;
            self.refresh_due = true;
            events.push(Event::Sent {
                id: p.txs[0].id,
                fee: p.txs[..sent.sent].iter().map(|t| t.fee).sum(),
                transactions: sent.sent,
            });
        }
        if let Some(e) = sent.failed {
            return Err(format!(
                "{} of {} transactions were sent; the node refused the next one: {e}",
                sent.sent,
                p.txs.len()
            ));
        }
        Ok(())
    }

    // ---- signatures and proofs --------------------------------------------------------------------------

    fn sign_message(
        &mut self,
        account: usize,
        message: &str,
        events: &mut Vec<Event>,
    ) -> Result<(), String> {
        if message.is_empty() {
            return Err("type the message to sign".into());
        }
        let purse = self.purse.as_ref().ok_or("the wallet is locked")?;
        let sig = purse
            .sign_message(account, &mut OsRng, message.as_bytes())
            .map_err(purse_err)?;
        events.push(Event::Signed {
            signature: sig.to_text(),
        });
        Ok(())
    }

    fn make_proof(&mut self, req: ProofRequest, events: &mut Vec<Event>) -> Result<(), String> {
        let node = self
            .node
            .as_ref()
            .ok_or("the node is not running: a proof is made against the chain (start the node on the Node tab)")?;
        let purse = self.purse.as_ref().ok_or("the wallet is locked")?;
        let (proof, what) = match req {
            ProofRequest::Received {
                account,
                global_index,
            } => (
                purse
                    .prove_received(account, global_index, node, &mut OsRng)
                    .map_err(purse_err)?,
                "Proves that this account received this output.",
            ),
            ProofRequest::Sent { id, key } => (
                purse
                    .prove_sent(
                        &id,
                        if key {
                            tenero_wallet::proofs::ProofKind::Key
                        } else {
                            tenero_wallet::proofs::ProofKind::Sent
                        },
                        node,
                        &mut OsRng,
                    )
                    .map_err(purse_err)?,
                if key {
                    "Contains the payment's secret key: anyone holding it can check this one output."
                } else {
                    "Proves this payment without giving away its secret key."
                },
            ),
        };
        events.push(Event::Proof {
            text: proof.to_text(),
            note: what.to_string(),
        });
        Ok(())
    }

    fn reveal_tx_key(&mut self, id: &[u8; 32], events: &mut Vec<Event>) -> Result<(), String> {
        let purse = self.purse.as_ref().ok_or("the wallet is locked")?;
        let secret = purse.tx_secret(id).map_err(purse_err)?;
        let hex = tenero_core::hash::hex_lower(secret.expose());
        events.push(Event::TxKey {
            id: *id,
            key: Zeroizing::new(hex),
        });
        Ok(())
    }

    /// Checks a transaction key and an address against the node's chain. Needs a node, not a wallet.
    fn check_key(&self, key: &str, address: &str, from_height: u64) -> Result<CheckedView, String> {
        let k = key.trim();
        if k.len() != 64 || !k.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err("a transaction key is 64 lower-case hexadecimal digits".into());
        }
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = u8::from_str_radix(&k[2 * i..2 * i + 2], 16).map_err(|e| e.to_string())?;
        }
        let address = tenero_wallet::Address::from_text(address.trim())
            .map_err(|e| format!("address: {e}"))?;
        let node = self
            .node
            .as_ref()
            .ok_or("the node is not running: a key is checked against the chain (start the node on the Node tab)")?;
        let (c, confirmations) =
            tenero_wallet::proofs::check_key(node, &bytes, &address, from_height)
                .map_err(|e| e.to_string())?;
        Ok(CheckedView {
            kind: c.kind.name(),
            address: c.address.to_text(),
            amount: c.amount,
            height: c.height,
            global_index: c.global_index,
            confirmations,
            block_reward: c.block_reward,
        })
    }

    /// Checks a proof against the node's chain. Needs a node, not a wallet.
    fn check_proof(&self, text: &str) -> Result<CheckedView, String> {
        let proof =
            tenero_wallet::proofs::PaymentProof::from_text(text).map_err(|e| e.to_string())?;
        let node = self
            .node
            .as_ref()
            .ok_or("the node is not running: a proof is checked against the chain (start the node on the Node tab)")?;
        let (c, confirmations) =
            tenero_wallet::proofs::check_on_chain(node, &proof).map_err(|e| e.to_string())?;
        Ok(CheckedView {
            kind: c.kind.name(),
            address: c.address.to_text(),
            amount: c.amount,
            height: c.height,
            global_index: c.global_index,
            confirmations,
            block_reward: c.block_reward,
        })
    }

    // ---- the node ---------------------------------------------------------------------------------------

    fn start_node(&mut self, events: &mut Vec<Event>) -> Result<(), String> {
        if self.move_job.is_some() {
            return Err("wait for the move of the node's data to finish (or cancel it)".into());
        }
        if self.settings.external_node {
            return Err("the settings say to use a node that is already running; this window does not start one".into());
        }
        if self.node.is_some() || self.node_proc.is_some() {
            events.push(Event::Notice("the node is already running".into()));
            return Ok(());
        }
        let exe = procs::program_path(&self.settings, "tenerod");
        if !exe.exists() {
            return Err(format!(
                "cannot find the node program at {} (put tenerod next to this program, or set the program folder in the settings)",
                exe.display()
            ));
        }
        // A folder this window creates is made private to its owner, as the node requires of its data folder (it holds the
        // control cookie). One that already exists is left as it is: the node checks it and says what is wrong.
        let existed = self.settings.data_dir.exists();
        std::fs::create_dir_all(&self.settings.data_dir)
            .map_err(|e| format!("cannot create {}: {e}", self.settings.data_dir.display()))?;
        if !existed {
            tenero_app::private_dir::make_private(&self.settings.data_dir)?;
        }
        let log = self.app_dir.join("node-output.txt");
        let proc = Proc::spawn(&exe, &procs::node_args(&self.settings), &log)?;
        self.register(true, &proc);
        self.node_proc = Some(proc);
        self.node_started = Some(Instant::now());
        self.node_view = NodeView::Starting;
        Ok(())
    }

    fn request_stop_node(&mut self) -> Result<(), String> {
        if self.node.is_none() && self.node_proc.is_none() {
            return Err("the node is not running".into());
        }
        // the miner works for the node: it goes first
        self.stop_miner();
        self.pending_stop = true;
        self.node_view = NodeView::Stopping;
        Ok(())
    }

    /// Does the (slow) stop the last command asked for. Called by the loop after it has shown "stopping".
    fn do_stop_node(&mut self) -> Vec<Event> {
        self.pending_stop = false;
        let mut events = Vec::new();
        let node = self
            .node
            .take()
            .or_else(|| procs::reach_node(&self.settings));
        match node {
            Some(n) => match procs::stop_node(&n, self.node_proc.as_mut()) {
                Ok(how) => events.push(Event::Notice(format!("the node {how}"))),
                Err(e) => events.push(Event::Error(format!("could not stop the node: {e}"))),
            },
            None => {
                // not answering: if it is ours and stuck at start-up, end our own handle
                if let Some(p) = self.node_proc.as_mut() {
                    p.kill();
                }
            }
        }
        // a node asked to stop by us and not ours (attached): give it a moment to close its control port
        let end = Instant::now() + Duration::from_secs(10);
        while procs::reach_node(&self.settings).is_some() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(200));
        }
        self.node_proc = None;
        self.node_started = None;
        self.node_view = NodeView::Stopped;
        self.refresh_due = true;
        events
    }

    fn poll_node(&mut self, force: bool) {
        if self.pending_stop {
            return;
        }
        if !force {
            if let Some(t) = self.last_poll {
                if t.elapsed() < POLL_EVERY {
                    return;
                }
            }
        }
        self.last_poll = Some(Instant::now());
        // our own child may have ended by itself
        if let Some(p) = self.node_proc.as_mut() {
            if let Some(how) = p.exited() {
                let output = p.tail(12);
                self.node_proc = None;
                self.node = None;
                self.node_started = None;
                self.stop_miner();
                self.node_view = NodeView::Failed {
                    why: format!("the node {how}"),
                    output,
                };
                return;
            }
        }
        if self.node.is_none() {
            // the cookie appears when the node is up; until then a connect would only fail
            if self.settings.data_dir.join(COOKIE_FILE).exists() {
                self.node = procs::reach_node(&self.settings);
            }
        }
        match self.node.as_ref().map(|n| n.info()) {
            Some(Ok(info)) => {
                self.node_view = NodeView::Running {
                    info,
                    ours: self.node_proc.is_some(),
                };
            }
            Some(Err(_)) => {
                // the node went away (or hung): forget the connection and look again next time
                self.node = None;
                self.stop_miner();
                self.node_view = if self.node_proc.is_some() {
                    NodeView::Starting
                } else {
                    NodeView::Stopped
                };
            }
            None => {
                if self.node_proc.is_some() {
                    let waited = self.node_started.map_or(Duration::ZERO, |t| t.elapsed());
                    if waited > NODE_START_PATIENCE {
                        let output = self
                            .node_proc
                            .as_ref()
                            .map(|p| p.tail(12))
                            .unwrap_or_default();
                        if let Some(p) = self.node_proc.as_mut() {
                            p.kill();
                        }
                        self.node_proc = None;
                        self.node_started = None;
                        self.node_view = NodeView::Failed {
                            why: "the node did not come up in 90 seconds".into(),
                            output,
                        };
                    } else {
                        self.node_view = NodeView::Starting;
                    }
                } else if !matches!(self.node_view, NodeView::Failed { .. }) {
                    self.node_view = NodeView::Stopped;
                }
            }
        }
    }

    fn node_info(&self) -> Option<&NodeInfo> {
        match &self.node_view {
            NodeView::Running { info, .. } => Some(info),
            _ => None,
        }
    }

    // ---- the miner --------------------------------------------------------------------------------------

    fn start_miner(&mut self) -> Result<(), String> {
        if self.move_job.is_some() {
            return Err("wait for the move of the node's data to finish (or cancel it)".into());
        }
        if self.miner_proc.is_some() {
            return Err("the miner is already running".into());
        }
        if self.node_info().is_none() {
            return Err("the node is not running: start it first".into());
        }
        let purse = self
            .purse
            .as_ref()
            .ok_or("unlock the wallet first: the miner pays its rewards to one of your accounts")?;
        let account = purse.account(self.settings.miner_account).map_err(|_| {
            format!(
                "account {} does not exist: choose another in the settings",
                self.settings.miner_account
            )
        })?;
        let address = account.address().to_text();
        if !MinerBackend::for_network(self.settings.network).contains(&self.settings.miner_backend)
        {
            return Err(format!(
                "the {} backend cannot mine the {} network",
                self.settings.miner_backend.name(),
                self.settings.network.name()
            ));
        }
        let exe = procs::program_path(&self.settings, "tenero-miner");
        if !exe.exists() {
            return Err(format!(
                "cannot find the miner program at {}",
                exe.display()
            ));
        }
        let status = self.miner_status_file();
        let _ = std::fs::remove_file(&status);
        let log = self.app_dir.join("miner-output.txt");
        let proc = Proc::spawn(
            &exe,
            &procs::miner_args(&self.settings, &address, &status),
            &log,
        )?;
        self.register(false, &proc);
        self.miner_proc = Some(proc);
        self.miner_started = Some(Instant::now());
        self.miner_view = MinerView::Starting;
        Ok(())
    }

    fn stop_miner(&mut self) {
        if let Some(p) = self.miner_proc.as_mut() {
            p.kill();
        }
        self.miner_proc = None;
        self.miner_started = None;
        let _ = std::fs::remove_file(self.miner_status_file());
        self.miner_view = MinerView::Off;
    }

    fn poll_miner(&mut self) {
        let Some(p) = self.miner_proc.as_mut() else {
            return;
        };
        if let Some(how) = p.exited() {
            let output = p.tail(12);
            self.miner_proc = None;
            self.miner_started = None;
            self.miner_view = MinerView::Failed {
                why: format!("the miner {how}"),
                output,
            };
            return;
        }
        let report = std::fs::read_to_string(self.miner_status_file())
            .ok()
            .and_then(|t| MinerReport::parse(&t));
        match report {
            Some(r) => {
                let stale = r.is_stale(now_unix(), MINER_STALE_SECS);
                self.miner_view = MinerView::Running {
                    report: Box::new(r),
                    stale,
                };
            }
            None => self.miner_view = MinerView::Starting,
        }
    }

    // ---- settings ---------------------------------------------------------------------------------------

    /// Starts copying the node's data to `to` (see `movedata`), on a thread of its own. Nothing is changed until the copy has been checked.
    fn start_move(&mut self, to: PathBuf) -> Result<(), String> {
        if self.move_job.is_some() {
            return Err("a move is already running".into());
        }
        if self.node.is_some() || self.node_proc.is_some() {
            return Err("stop the node before moving its data".into());
        }
        if self.miner_proc.is_some() {
            return Err("stop the miner before moving the node's data".into());
        }
        let from = self.settings.data_dir.clone();
        movedata::check_destination(&from, &to)?;
        let has_data = from.is_dir()
            && std::fs::read_dir(&from)
                .map(|mut d| d.next().is_some())
                .unwrap_or(false);
        if !has_data {
            // nothing there to move (a node never ran here): just use the new place, the node makes the folder itself
            let mut s = self.settings.clone();
            s.data_dir = to;
            return self.set_settings(s);
        }
        let progress = Arc::new(movedata::Progress::default());
        let (p, f, t) = (progress.clone(), from.clone(), to.clone());
        let thread = std::thread::Builder::new()
            .name("move-node-data".into())
            .spawn(move || movedata::move_data(&f, &t, &p))
            .map_err(|e| format!("cannot start the copy: {e}"))?;
        self.move_job = Some(MoveJob {
            from,
            to,
            progress,
            thread: Some(thread),
        });
        Ok(())
    }

    /// If a move has ended: uses the new folder (it was checked), or says why it did not happen. The old folder is never touched.
    fn poll_move(&mut self) -> Vec<Event> {
        let finished = self
            .move_job
            .as_ref()
            .and_then(|j| j.thread.as_ref())
            .is_some_and(|t| t.is_finished());
        if !finished {
            return Vec::new();
        }
        let mut job = self.move_job.take().expect("a finished job");
        let result = job
            .thread
            .take()
            .expect("a finished thread")
            .join()
            .unwrap_or_else(|_| {
                Err(
                    "the copy stopped unexpectedly; the old folder is unchanged and still in use"
                        .into(),
                )
            });
        match result {
            Ok(()) => {
                let mut s = self.settings.clone();
                s.data_dir = job.to.clone();
                match self.set_settings(s) {
                    Ok(()) => vec![Event::Notice(format!(
                        "The node's data was moved to {} and the copy was checked. The old folder, {}, is still there and untouched: start the node, make sure it runs from the new place, and then delete the old folder yourself.",
                        job.to.display(),
                        job.from.display()
                    ))],
                    Err(e) => vec![Event::Error(format!(
                        "the data was copied to {} but the new place could not be saved ({e}); the old folder is still in use",
                        job.to.display()
                    ))],
                }
            }
            Err(e) if e == "cancelled" => vec![Event::Notice(
                "The move was cancelled: nothing was changed, and what it had copied was removed.".into(),
            )],
            Err(e) => vec![Event::Error(format!(
                "The move failed: {e}. The old folder is unchanged and still in use; anything the move had made was removed."
            ))],
        }
    }

    fn set_settings(&mut self, new: Settings) -> Result<(), String> {
        if self.move_job.is_some() {
            return Err("wait for the move of the node's data to finish (or cancel it)".into());
        }
        let old = &self.settings;
        let node_changed = new.network != old.network
            || new.node_kind != old.node_kind
            || new.control != old.control
            || new.seeds != old.seeds
            || new.listen != old.listen
            || new.inbound_port != old.inbound_port
            || new.external_node != old.external_node
            || new.data_dir != old.data_dir
            || new.program_dir != old.program_dir;
        let miner_changed = new.miner_backend != old.miner_backend
            || new.miner_cores != old.miner_cores
            || new.miner_gpu_device != old.miner_gpu_device
            || new.miner_gpu_auto_batch != old.miner_gpu_auto_batch
            || new.miner_pace_secs != old.miner_pace_secs
            || new.miner_account != old.miner_account;
        if node_changed && (self.node.is_some() || self.node_proc.is_some()) {
            return Err("stop the node before changing its settings".into());
        }
        if miner_changed && self.miner_proc.is_some() {
            return Err("stop the miner before changing its settings".into());
        }
        if new.network != old.network && self.purse.is_some() {
            // each network has its own wallets folder and its own node: the open wallet belongs to the old one
            return Err(
                "lock the wallet before changing the network (each network has its own wallets)"
                    .into(),
            );
        }
        if (new.wallet_file != old.wallet_file || new.wallets_dir != old.wallets_dir)
            && self.purse.is_some()
        {
            return Err("lock the wallet before choosing another wallet file".into());
        }
        if !MinerBackend::for_network(new.network).contains(&new.miner_backend) {
            return Err(format!(
                "the {} network cannot be mined with the {} backend",
                new.network.name(),
                new.miner_backend.name()
            ));
        }
        new.save(&self.app_dir)?;
        self.settings = new;
        self.refresh_wallets();
        self.data = None;
        self.refresh_due = true;
        // look again at once: a node may already be running at the new place
        self.node_view = NodeView::Stopped;
        self.poll_node(true);
        Ok(())
    }

    // ---- the clock --------------------------------------------------------------------------------------

    /// One pass of background work: look at the node and the miner, scan a slice of the chain, keep the wallet file
    /// current. Called by the loop every half second or so; cheap when there is nothing to do.
    pub fn tick(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        if self.pending_stop {
            events.extend(self.do_stop_node());
            return self.with_snapshot(events);
        }
        let before = self.snapshot();
        events.extend(self.poll_move());
        self.poll_node(false);
        self.poll_miner();
        if self.node.is_none() && self.miner_proc.is_some() {
            self.stop_miner();
        }
        if let Err(e) = self.scan_slice() {
            events.push(Event::Notice(format!("scanning stopped for now: {e}")));
            self.node = None;
        }
        if self.dirty && (self.last_save.elapsed() > Duration::from_secs(30) || self.caught_up()) {
            if let Err(e) = self.save_wallet() {
                events.push(Event::Error(format!("could not save the wallet file: {e}")));
            }
        }
        let after = self.snapshot();
        if after != before {
            events.push(Event::Snapshot(Box::new(after)));
        }
        events
    }

    fn caught_up(&self) -> bool {
        self.data.as_ref().is_some_and(|d| d.synced)
    }

    /// Reads up to a slice of new blocks into every account, then (if anything changed, or now and then) rebuilds
    /// what the window shows.
    fn scan_slice(&mut self) -> Result<(), String> {
        let (Some(purse), Some(node)) = (self.purse.as_mut(), self.node.as_ref()) else {
            return Ok(());
        };
        let (tip, _) = node.tip()?;
        // the first block some account has not read
        let next = purse
            .accounts()
            .iter()
            .map(|a| {
                a.wallet()
                    .scanned_height()
                    .map_or(a.wallet().birth_height(), |h| h + 1)
            })
            .min()
            .unwrap_or(0);
        if next <= tip || self.last_tip != Some(tip) {
            // a bounded slice, so the window stays alive during a long first scan; a reorganisation is noticed by
            // the wallet's own sync
            let view = Capped {
                node,
                cap: next.saturating_add(SCAN_SLICE).min(tip),
            };
            let r = purse.sync(&view).map_err(purse_err)?;
            if r.blocks_scanned > 0 || r.outputs_found > 0 || r.blocks_rolled_back > 0 {
                self.dirty = true;
            }
            if r.outputs_found > 0
                || r.blocks_rolled_back > 0
                || next.saturating_add(SCAN_SLICE) > tip
            {
                self.refresh_due = true;
            }
            self.last_tip = Some(tip);
        }
        // a restored wallet looks for its other accounts once it has read the whole chain
        if self.needs_discovery
            && purse
                .accounts()
                .iter()
                .all(|a| a.wallet().scanned_height() == Some(tip))
        {
            purse.discover(node).map_err(purse_err)?;
            self.needs_discovery = false;
            self.dirty = true;
            self.refresh_due = true;
        }
        // Rebuilding asks the node about every coin the wallet holds, so it is not done more than every few seconds (a
        // miner on an easy chain finds blocks faster than that), and not at all when nothing has changed for a while.
        let age = self.last_refresh.map(|t| t.elapsed());
        let due = self.refresh_due && age.is_none_or(|a| a > Duration::from_secs(3));
        let stale = age.is_none_or(|a| a > Duration::from_secs(30));
        if due || stale {
            self.rebuild_data()?;
        }
        Ok(())
    }

    fn rebuild_data(&mut self) -> Result<(), String> {
        let Some(node) = self.node.as_ref() else {
            return Ok(());
        };
        let syncing = self.node_info().is_none_or(|i| i.syncing);
        let (tip, _) = node.tip()?;
        let purse = self.purse.as_mut().ok_or("locked")?;
        let mut accounts = Vec::new();
        let labels: Vec<String> = purse
            .accounts()
            .iter()
            .map(|a| a.label().to_string())
            .collect();
        for (i, label) in labels.iter().enumerate() {
            let balance = purse.balance(i, node).map_err(purse_err)?;
            accounts.push(AccountView {
                index: i,
                label: label.clone(),
                address: purse.account(i).map_err(purse_err)?.address().to_text(),
                balance: Some(balance),
            });
        }
        // the total is the accounts added up (asking the node again for every coin would double the work)
        let mut total = Balance::default();
        for a in &accounts {
            if let Some(b) = a.balance {
                total.total += b.total;
                total.spendable += b.spendable;
                total.immature += b.immature;
                total.reserved += b.reserved;
            }
        }
        let history = purse
            .history(node)
            .map_err(purse_err)?
            .into_iter()
            .map(|e| HistoryRow {
                account: e.account,
                account_label: labels.get(e.account).cloned().unwrap_or_default(),
                kind: e.kind,
                amount: e.amount,
                height: e.height,
                id: e.id,
                global_index: e.global_index,
                has_secret: e.has_secret,
                note: e.note,
            })
            .collect();
        let scanned = purse
            .accounts()
            .iter()
            .map(|a| a.wallet().scanned_height())
            .collect::<Option<Vec<_>>>()
            .and_then(|v| v.into_iter().min());
        let synced = !syncing && scanned == Some(tip) && !self.needs_discovery;
        self.data = Some(WalletData {
            accounts,
            total: Some(total),
            history,
            requests: Vec::new(),
            scanned,
            tip: Some(tip),
            synced,
            has_password: self.has_password,
        });
        self.refresh_due = false;
        self.last_refresh = Some(Instant::now());
        // balances changed: a payment prepared on the old ones must be rebuilt
        Ok(())
    }

    // ---- the end ----------------------------------------------------------------------------------------

    fn quit(&mut self) {
        self.end_move();
        self.stop_miner();
        let _ = self.save_wallet();
        // a node this window started goes with it; one it only found running is left alone
        if self.node_proc.is_some() {
            let _ = self.do_stop_node();
        }
        self.purse = None;
        self.pass = None;
    }

    /// A move still running when the window ends is cancelled and waited for, so that what it made is removed (the old folder is never touched).
    fn end_move(&mut self) {
        if let Some(mut j) = self.move_job.take() {
            j.progress.cancel();
            if let Some(t) = j.thread.take() {
                let _ = t.join();
            }
        }
    }
}

impl Drop for Core {
    /// A core that ends without `Quit` (the worker thread panicked, or a test failed) still must not leave the miner
    /// or a node it started running with nobody holding them: the miner is ended and the node is asked to stop.
    fn drop(&mut self) {
        self.end_move();
        self.stop_miner();
        if self.node_proc.is_some() {
            let _ = self.do_stop_node();
        }
    }
}
