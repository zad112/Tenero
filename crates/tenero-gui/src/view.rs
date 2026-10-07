//! What the window shows and what it can ask for. The window holds a [`Snapshot`] (a copy of everything it draws) and sends
//! [`Cmd`]s; it never touches the wallet, the node or a process itself. That is what lets the logic be tested without a
//! window.

use tenero_app::control::NodeInfo;
use tenero_app::miner_report::MinerReport;
use tenero_wallet::{Balance, EntryKind, FeeLevel};
use zeroize::Zeroizing;

use crate::settings::Settings;
use crate::wallets::WalletEntry;

/// Text shown on every screen, at all times (the owner's rule: nothing in the app may say or imply that the coins are
/// money or that payments are anonymous).
pub const BANNER: &str = "TEST NETWORK. NO VALUE. UNAUDITED.";
/// Said wherever an address or a balance is shown.
pub const SCHEME_NOTE: &str =
    "Interim output scheme: not private in Monero's sense (not Carrot). One address per account.";

#[derive(Clone, Debug, PartialEq)]
pub enum NodeView {
    Stopped,
    Starting,
    Running {
        info: NodeInfo,
        /// Started by this window (or one of its earlier runs) rather than by the owner on the command line.
        ours: bool,
    },
    Stopping,
    /// Stopped by itself or never came up: why, and the last lines of what it printed.
    Failed {
        why: String,
        output: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum MinerView {
    Off,
    Starting,
    Running {
        report: Box<MinerReport>,
        /// The miner has not written its state lately.
        stale: bool,
    },
    Failed {
        why: String,
        output: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct AccountView {
    pub index: usize,
    pub label: String,
    pub address: String,
    /// `None` while no node can be asked (a balance needs the chain to say what is spent).
    pub balance: Option<Balance>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HistoryRow {
    pub account: usize,
    pub account_label: String,
    pub kind: EntryKind,
    pub amount: u64,
    pub height: u64,
    pub id: Option<[u8; 32]>,
    /// Received or mined: the output's global index (what a proof of receipt names).
    pub global_index: Option<u64>,
    /// Sent: the wallet still holds the payment's secret (so it can be proved).
    pub has_secret: bool,
    /// Sent: what it was for (the label of the request it answered).
    pub note: Option<String>,
}

/// A payment request this wallet made, with its link (which holds the account's address).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestView {
    pub index: usize,
    pub account: usize,
    pub account_label: String,
    pub amount: Option<u64>,
    pub label: Option<String>,
    pub message: Option<String>,
    pub time: u64,
    pub uri: String,
}

/// What a payment proof that checked shows, for the screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckedView {
    pub kind: &'static str,
    pub address: String,
    pub amount: u64,
    pub height: u64,
    pub global_index: u64,
    /// Blocks on top of it, counting its own (1 = it is in the newest block).
    pub confirmations: u64,
    pub block_reward: bool,
}

/// Which proof to make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProofRequest {
    /// Receipt of an output (made with the view key).
    Received { account: usize, global_index: u64 },
    /// A payment sent: the proof that does not give the secret away, or (`key`) the secret itself as the proof.
    Sent { id: [u8; 32], key: bool },
}

#[derive(Clone, Debug, PartialEq)]
pub struct WalletData {
    pub accounts: Vec<AccountView>,
    pub total: Option<Balance>,
    pub history: Vec<HistoryRow>,
    /// The payment requests made (filled in when the snapshot is taken: they need no node).
    pub requests: Vec<RequestView>,
    /// The last block the wallet has read.
    pub scanned: Option<u64>,
    /// The node's tip, if a node is reachable.
    pub tip: Option<u64>,
    /// The wallet has read every block the node has and the node is not catching up: balances are as final as this
    /// computer can tell. **While this is false the window must not present a balance as final.**
    pub synced: bool,
    /// The wallet file has a password (an empty one is allowed on purpose and is said so on screen).
    pub has_password: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum WalletView {
    /// No wallet file yet: create one or restore one.
    NoWallet,
    /// A wallet file, not opened.
    Locked,
    Unlocked(Box<WalletData>),
}

/// A payment, built and checked but not sent: what the confirmation screen shows. Nothing is reserved or sent until
/// the person confirms.
#[derive(Clone, Debug, PartialEq)]
pub struct Quote {
    pub account: usize,
    pub to: String,
    pub amount: u64,
    pub fee: u64,
    pub change: u64,
    pub level: FeeLevel,
    /// What it is for (the label of the request it answers).
    pub note: Option<String>,
    /// How many transactions it is made of: more than one when it needs more coins than one transaction can carry.
    pub transactions: usize,
    /// How many coins (outputs) it spends in all.
    pub coins: usize,
    /// What could not be made now (the coins that were separate ran out): how many payments, and their worth. Zero when everything is in.
    pub unsent_payments: usize,
    pub unsent_total: u64,
    /// `Some(what)` when this is not a payment but a combining of the account's own coins (`to` is empty then).
    pub own: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub settings: Settings,
    /// The wallet files there are (the selected one is `settings.wallet_file`).
    pub wallets: Vec<WalletEntry>,
    pub wallet: WalletView,
    pub node: NodeView,
    pub miner: MinerView,
    /// The payment waiting for a yes.
    pub prepared: Option<Quote>,
    /// What the worker is busy with, for a "please wait" (a long scan, building a payment).
    pub busy: Option<String>,
    /// The node's data being copied to another folder (the node and the miner are stopped meanwhile).
    pub moving: Option<MoveView>,
}

/// A move of the node's data in progress: from where, to where, and how far (see `movedata`).
#[derive(Clone, Debug, PartialEq)]
pub struct MoveView {
    pub from: std::path::PathBuf,
    pub to: std::path::PathBuf,
    pub phase: crate::movedata::Phase,
    /// Bytes copied (or, in the checking phase, compared) so far, and the bytes to copy in all (0 until measured).
    pub done: u64,
    pub total: u64,
}

/// How the person wants the wallet file locked.
pub enum Password {
    /// At least [`MIN_PASSWORD`] characters.
    Set(Zeroizing<String>),
    /// No password: anyone who can read the file can spend. Allowed only as an explicit choice.
    None,
}

pub const MIN_PASSWORD: usize = 8;

pub enum Cmd {
    CreateWallet {
        password: Password,
        /// The new wallet's name (its file is `NAME.twl` in the wallets folder). `None`: at the selected file's place.
        name: Option<String>,
    },
    RestoreWallet {
        phrase: Zeroizing<String>,
        password: Password,
        /// The height of the first block that could hold the wallet's coins, if known (default: the start).
        birth: Option<u64>,
        name: Option<String>,
    },
    /// Selects another wallet file to open (the wallet must be locked).
    SelectWallet {
        path: std::path::PathBuf,
    },
    Unlock {
        password: Zeroizing<String>,
    },
    Lock,
    /// Shows the words again after asking for the password.
    RevealPhrase {
        password: Zeroizing<String>,
    },
    ChangePassword {
        old: Zeroizing<String>,
        new: Password,
    },
    AddAccount {
        label: String,
    },
    RenameAccount {
        index: usize,
        label: String,
    },
    /// What the three fee levels would cost for this payment.
    EstimateFees {
        account: usize,
        to: String,
        amount: String,
    },
    PreparePayment {
        account: usize,
        to: String,
        amount: String,
        level: FeeLevel,
        /// What it is for: the label of the request being paid, if any.
        note: Option<String>,
    },
    /// Makes and keeps a payment request for an account (`amount` empty = the payer chooses).
    AddRequest {
        account: usize,
        amount: String,
        label: String,
        message: String,
    },
    DeleteRequest {
        index: usize,
    },
    /// Prepares the combining of an account's own coins into fewer, larger ones: `coins` of them (the smallest), or every coin worth combining when
    /// it is `None`. Shown for a yes like a payment (`SendPrepared`).
    PrepareCombine {
        account: usize,
        coins: Option<usize>,
        level: FeeLevel,
    },
    SendPrepared,
    CancelPrepared,
    /// Signs a message with an account's spend key.
    SignMessage {
        account: usize,
        message: String,
    },
    MakeProof(ProofRequest),
    /// Shows the secret of a sent payment (the person clicked to reveal it).
    RevealTxKey {
        id: [u8; 32],
    },
    /// Checks a payment proof against the node (needs no wallet).
    CheckProof {
        text: String,
    },
    /// Checks a transaction key and an address against the node (needs no wallet): the output the key made is searched for from
    /// `from_height` on.
    CheckKey {
        key: String,
        address: String,
        from_height: Option<u64>,
    },
    StartNode,
    StopNode,
    StartMiner,
    StopMiner,
    SetSettings(Box<Settings>),
    /// Copies the node's data (the chain) to a new or empty folder, checks the copy, and then uses the new folder. The old one is left alone. The
    /// node and the miner must be stopped.
    MoveNodeData {
        to: std::path::PathBuf,
    },
    /// Stops a move in progress; what it made is removed and the old folder stays in use.
    CancelMove,
    /// Stop what this window started and end.
    Quit,
}

pub enum Event {
    /// Everything the window draws, after any change.
    Snapshot(Box<Snapshot>),
    /// The 24 words, to show now and then forget. `new` is true right after a wallet was created.
    Phrase {
        words: Zeroizing<String>,
        new: bool,
    },
    Estimate {
        fees: [u64; 3],
    },
    /// The fees could not be worked out, and why (shown under the fee choice, so it never says "working" for ever).
    EstimateFailed(String),
    /// A signature, as text.
    Signed {
        signature: String,
    },
    /// A payment proof, as text, and a line saying what it shows.
    Proof {
        text: String,
        note: String,
    },
    /// The secret of a sent payment, for a window that says what it is.
    TxKey {
        id: [u8; 32],
        key: Zeroizing<String>,
    },
    ProofChecked(Result<CheckedView, String>),
    Sent {
        id: [u8; 32],
        /// The fees of every transaction sent.
        fee: u64,
        /// How many transactions went (more than one when a payment needed several).
        transactions: usize,
    },
    /// Something happened that the person should read (not an error).
    Notice(String),
    Error(String),
    /// The worker is finished; the window may close.
    Quit,
}
