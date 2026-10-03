//! The wallet app's logic, without a window: it owns the wallet (unlocked in memory only while the person has it open),
//! the node and miner processes this program started, and the one payment that is waiting for a yes. The window drives it
//! with [`Cmd`]s and draws [`Snapshot`]s; `backend.rs` runs it on a thread. Everything here is tested without a window.
//!
//! What it keeps in memory while a wallet is open, said plainly: the seed (inside the purse) and the password (so the
//! wallet file can be rewritten as it scans and sends; it is zeroed when the wallet is locked or the program ends). It
//! writes neither to a log or to a screen except when the person asks to see the words.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rand_core::OsRng;
use tenero_app::client::{RemoteNode, COOKIE_FILE};
use tenero_app::control::NodeInfo;
use tenero_app::miner_report::MinerReport;
use tenero_wallet::amount::parse_coins;
use tenero_wallet::{
    Address, Built, ChainView, FeeLevel, FileError, KdfParams, Purse, PurseError, Rules, ScanBlock,
};
use zeroize::Zeroizing;

use crate::procs::{self, Proc};
use crate::settings::{MinerBackend, Settings};
use crate::view::*;

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
    fn output_count(&self) -> Result<u64, String> {
        self.node.output_count()
    }
    fn key_image_spent(&self, k: &[u8; 32]) -> Result<bool, String> {
        self.node.key_image_spent(k)
    }
    fn rules(&self) -> Result<Rules, String> {
        self.node.rules()
    }
}

struct Prepared {
    account: usize,
    to: Address,
    to_text: String,
    built: Built,
    level: FeeLevel,
}

pub struct Core {
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
        let mut c = Core {
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
        };
        // a node an earlier run (or the owner) left going is picked up, not started a second time
        c.poll_node(true);
        c
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
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
            wallet: match (&self.purse, &self.data) {
                (Some(_), Some(d)) => WalletView::Unlocked(Box::new(d.clone())),
                (Some(p), None) => WalletView::Unlocked(Box::new(self.bare_data(p))),
                (None, _) if self.wallet_exists() => WalletView::Locked,
                (None, _) => WalletView::NoWallet,
            },
            node: self.node_view.clone(),
            miner: self.miner_view.clone(),
            prepared: self.prepared.as_ref().map(|p| Quote {
                account: p.account,
                to: p.to_text.clone(),
                amount: p.built.amount,
                fee: p.built.fee,
                change: p.built.change,
                level: p.level,
            }),
            busy: None,
        }
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
            Cmd::CreateWallet { password } => self.create_wallet(password, &mut events),
            Cmd::RestoreWallet {
                phrase,
                password,
                birth,
            } => self.restore_wallet(&phrase, password, birth),
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
            } => self.estimate(account, &to, &amount, &mut events),
            Cmd::PreparePayment {
                account,
                to,
                amount,
                level,
            } => self.prepare(account, &to, &amount, level),
            Cmd::SendPrepared => self.send_prepared(&mut events),
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

    fn create_wallet(&mut self, password: Password, events: &mut Vec<Event>) -> Result<(), String> {
        if self.wallet_exists() {
            return Err("a wallet file already exists here; unlock it, or choose another file in the settings".into());
        }
        let pw = pass_ok(&password)?;
        let purse = Purse::create(&mut OsRng, self.birth_now());
        let words = purse.phrase();
        self.open(purse, pw);
        self.save_wallet()?;
        events.push(Event::Phrase { words, new: true });
        Ok(())
    }

    fn restore_wallet(
        &mut self,
        phrase: &str,
        password: Password,
        birth: Option<u64>,
    ) -> Result<(), String> {
        if self.wallet_exists() {
            return Err("a wallet file already exists here; it is not overwritten".into());
        }
        let pw = pass_ok(&password)?;
        let seed = tenero_wallet::seed_of(phrase).map_err(|e| e.to_string())?;
        let purse = Purse::from_seed(&seed, birth.unwrap_or(0));
        self.open(purse, pw);
        // the number of accounts is not in the words: look for them once the node can be read
        self.needs_discovery = true;
        self.save_wallet()
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
        let built = purse
            .build_payment(account, node, &mut OsRng, &addr, units, FeeLevel::Low)
            .map_err(purse_err)?;
        let size = tenero_core::v2::Wire::to_bytes(&built.tx)
            .map_err(|e| e.to_string())?
            .len() as u64;
        let rules = node.rules()?;
        let min = tenero_core::fees::dynamic_min_fee(size, rules.reward, rules.median)?;
        let mut fees = [0u64; 3];
        for (f, level) in fees.iter_mut().zip(FeeLevel::ALL) {
            *f = min.saturating_mul(level.percent_of_minimum()) / 100 + 1;
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
    ) -> Result<(), String> {
        self.prepared = None;
        let (addr, units) = self.parse_payment(to, amount)?;
        self.ready_to_pay()?;
        let node = self.node.as_ref().expect("checked");
        let purse = self.purse.as_mut().ok_or("the wallet is locked")?;
        let built = purse
            .build_payment(account, node, &mut OsRng, &addr, units, level)
            .map_err(purse_err)?;
        self.prepared = Some(Prepared {
            account,
            to: addr,
            to_text: addr.to_text(),
            built,
            level,
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
            purse
                .send(p.account, &mut node, &p.built, &p.to, p.level, now_unix())
                .map_err(purse_err)
        };
        self.node = Some(node);
        result?;
        // the reservation and the record must reach the file, or a restart would pick the same coins
        self.save_wallet()?;
        self.refresh_due = true;
        events.push(Event::Sent {
            id: p.built.id,
            fee: p.built.fee,
        });
        Ok(())
    }

    // ---- the node ---------------------------------------------------------------------------------------

    fn start_node(&mut self, events: &mut Vec<Event>) -> Result<(), String> {
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

    fn set_settings(&mut self, new: Settings) -> Result<(), String> {
        let old = &self.settings;
        let node_changed = new.network != old.network
            || new.node_kind != old.node_kind
            || new.control != old.control
            || new.seeds != old.seeds
            || new.listen != old.listen
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
        if new.wallet_file != old.wallet_file && self.purse.is_some() {
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
        let stale = self
            .last_refresh
            .is_none_or(|t| t.elapsed() > Duration::from_secs(10));
        if self.refresh_due || stale {
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
        let total = purse.total_balance(node).map_err(purse_err)?;
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
        self.stop_miner();
        let _ = self.save_wallet();
        // a node this window started goes with it; one it only found running is left alone
        if self.node_proc.is_some() {
            let _ = self.do_stop_node();
        }
        self.purse = None;
        self.pass = None;
    }
}

impl Drop for Core {
    /// A core that ends without `Quit` (the worker thread panicked, or a test failed) still must not leave the miner
    /// or a node it started running with nobody holding them: the miner is ended and the node is asked to stop.
    fn drop(&mut self) {
        self.stop_miner();
        if self.node_proc.is_some() {
            let _ = self.do_stop_node();
        }
    }
}
