//! The wallet app's logic without a window, against REAL `tenerod` and `tenero-miner` processes on the SHA-256 test
//! chain (a CPU mines a block in an instant; no GPU, no 4 GiB dataset). **A test chain, not a real one.**
//!
//! These tests start and stop real child processes. Each one uses its own folder under the temp folder and its own
//! loopback address (127.0.0.7x), and ends what it started (a `Core` dropped without `Quit` stops its node).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tenero_app::config::Network;
use tenero_gui::core::Core;
use tenero_gui::settings::{MinerBackend, Settings};
use tenero_gui::view::*;
use tenero_wallet::{EntryKind, FeeLevel, KdfParams, SentStatus};
use zeroize::Zeroizing;

fn pw(s: &str) -> Zeroizing<String> {
    Zeroizing::new(s.to_string())
}

/// The folder the programs were built into (`target/release` or `target/debug`). They are built (a no-op when up to
/// date) once per test run, so a test never runs against programs older than the code.
fn programs() -> PathBuf {
    static BUILT: std::sync::Once = std::sync::Once::new();
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap().to_path_buf();
    BUILT.call_once(|| {
        let release = dir.file_name().is_some_and(|n| n == "release");
        let mut cmd =
            std::process::Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
        cmd.args(["build", "-p", "tenero-app", "--bins"]);
        if release {
            cmd.arg("--release");
        }
        assert!(
            cmd.status().unwrap().success(),
            "could not build the programs"
        );
    });
    let ext = if cfg!(windows) { ".exe" } else { "" };
    for n in ["tenerod", "tenero-miner"] {
        assert!(
            dir.join(format!("{n}{ext}")).exists(),
            "{n} missing in {}",
            dir.display()
        );
    }
    dir
}

struct Rig {
    dir: PathBuf,
    settings: Settings,
}

impl Rig {
    fn new(tag: &str, port: u16) -> Rig {
        let dir = std::env::temp_dir().join(format!("tenero-gui-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = Settings::defaults(&dir, Network::Test);
        s.control = format!("127.0.0.7{}:{port}", port % 10).parse().unwrap();
        s.program_dir = Some(programs());
        s.miner_backend = MinerBackend::Sha256;
        s.miner_pace_secs = 0;
        Rig { dir, settings: s }
    }

    fn core(&self) -> Core {
        Core::new(&self.dir, self.settings.clone(), KdfParams::TEST_ONLY_WEAK)
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Ticks until `done` says so (or fails the test after `secs`), collecting the events.
fn wait(
    core: &mut Core,
    secs: u64,
    what: &str,
    mut done: impl FnMut(&Snapshot) -> bool,
) -> Vec<Event> {
    let end = Instant::now() + Duration::from_secs(secs);
    let mut all = Vec::new();
    loop {
        all.extend(core.tick());
        if let MinerView::Failed { why, output } = &core.snapshot().miner {
            panic!(
                "the miner failed while waiting for {what}: {why}
{output}"
            );
        }
        if done(&core.snapshot()) {
            return all;
        }
        assert!(
            Instant::now() < end,
            "timed out after {secs} s waiting for: {what}\nlast: {:?}",
            core.snapshot().node
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn errors(events: &[Event]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Error(m) => Some(m.as_str()),
            _ => None,
        })
        .collect()
}

fn words_of(events: &[Event]) -> Option<(String, bool)> {
    events.iter().find_map(|e| match e {
        Event::Phrase { words, new } => Some((words.to_string(), *new)),
        _ => None,
    })
}

fn unlocked(c: &Core) -> WalletData {
    match c.snapshot().wallet {
        WalletView::Unlocked(d) => *d,
        other => panic!("not unlocked: {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// the wallet, with no node at all (the wallet runs first)
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn the_wallet_opens_with_no_node_and_never_shows_a_balance_it_cannot_know() {
    let rig = Rig::new("nonode", 18471);
    let mut c = rig.core();
    assert_eq!(c.snapshot().wallet, WalletView::NoWallet);
    assert_eq!(c.snapshot().node, NodeView::Stopped);

    // a password that is too short is refused, and nothing is written
    let ev = c.handle(Cmd::CreateWallet {
        password: Password::Set(pw("short")),
    });
    assert_eq!(errors(&ev).len(), 1);
    assert!(!rig.settings.wallet_file.exists());

    let ev = c.handle(Cmd::CreateWallet {
        password: Password::Set(pw("correct horse battery")),
    });
    assert!(errors(&ev).is_empty(), "{:?}", errors(&ev));
    let (words, new) = words_of(&ev).expect("the words are shown once, at creation");
    assert!(new);
    assert_eq!(words.split(' ').count(), 24);
    assert!(rig.settings.wallet_file.exists());

    // open with no node: the account and its address are there, a balance is not (it would be a guess)
    let d = unlocked(&c);
    assert_eq!(d.accounts.len(), 1);
    assert!(d.accounts[0].address.starts_with("tni1"));
    assert_eq!(d.accounts[0].balance, None);
    assert_eq!(d.total, None);
    assert!(!d.synced, "with no node nothing is final");
    assert!(d.has_password);

    // a second wallet is not made on top of the first
    let ev = c.handle(Cmd::CreateWallet {
        password: Password::None,
    });
    assert_eq!(errors(&ev).len(), 1);

    // lock, then a wrong and a right password
    c.handle(Cmd::Lock);
    assert_eq!(c.snapshot().wallet, WalletView::Locked);
    let ev = c.handle(Cmd::Unlock {
        password: pw("nope nope nope"),
    });
    assert_eq!(errors(&ev).len(), 1);
    assert_eq!(c.snapshot().wallet, WalletView::Locked);
    let ev = c.handle(Cmd::Unlock {
        password: pw("correct horse battery"),
    });
    assert!(errors(&ev).is_empty());
    assert_eq!(unlocked(&c).accounts[0].address, d.accounts[0].address);

    // the words come back only after the password is typed again
    let ev = c.handle(Cmd::RevealPhrase {
        password: pw("wrong"),
    });
    assert!(words_of(&ev).is_none() && errors(&ev).len() == 1);
    let ev = c.handle(Cmd::RevealPhrase {
        password: pw("correct horse battery"),
    });
    assert_eq!(words_of(&ev), Some((words.clone(), false)));
}

#[test]
fn the_words_restore_the_same_wallet_with_a_different_password_and_accounts_can_be_added_and_named()
{
    let rig = Rig::new("restore", 18472);
    let mut c = rig.core();
    let ev = c.handle(Cmd::CreateWallet {
        password: Password::Set(pw("first password")),
    });
    let (words, _) = words_of(&ev).unwrap();
    c.handle(Cmd::AddAccount {
        label: "Savings".into(),
    });
    let ev = c.handle(Cmd::AddAccount { label: "".into() });
    assert_eq!(errors(&ev).len(), 1, "an empty name is refused");
    c.handle(Cmd::RenameAccount {
        index: 1,
        label: "Rent".into(),
    });
    let before = unlocked(&c);
    assert_eq!(
        before
            .accounts
            .iter()
            .map(|a| a.label.as_str())
            .collect::<Vec<_>>(),
        ["Main", "Rent"]
    );
    drop(c);

    // another computer: a different file, the words, another password
    let mut other = rig.settings.clone();
    other.wallet_file = rig.dir.join("restored.twl");
    let mut c2 = Core::new(&rig.dir, other, KdfParams::TEST_ONLY_WEAK);
    let ev = c2.handle(Cmd::RestoreWallet {
        phrase: pw(&words.to_uppercase()),
        password: Password::Set(pw("a different one")),
        birth: None,
    });
    assert!(errors(&ev).is_empty(), "{:?}", errors(&ev));
    let after = unlocked(&c2);
    assert_eq!(
        after.accounts[0].address, before.accounts[0].address,
        "the same wallet"
    );
    // the number of accounts is not in the words: until a node can be read, only account 0 is known
    assert_eq!(after.accounts.len(), 1);

    // a bad phrase says what is wrong and writes nothing
    let mut other2 = rig.settings.clone();
    other2.wallet_file = rig.dir.join("never.twl");
    let mut c3 = Core::new(&rig.dir, other2.clone(), KdfParams::TEST_ONLY_WEAK);
    let mut bad: Vec<&str> = words.split(' ').collect();
    bad[3] = "tenero";
    let ev = c3.handle(Cmd::RestoreWallet {
        phrase: pw(&bad.join(" ")),
        password: Password::None,
        birth: None,
    });
    let e = errors(&ev);
    assert!(e.len() == 1 && e[0].contains("word 4"), "{e:?}");
    assert!(!other2.wallet_file.exists());
}

#[test]
fn a_wallet_with_no_password_is_allowed_only_when_chosen_and_is_said_so() {
    let rig = Rig::new("nopw", 18473);
    let mut c = rig.core();
    let ev = c.handle(Cmd::CreateWallet {
        password: Password::None,
    });
    assert!(errors(&ev).is_empty());
    assert!(!unlocked(&c).has_password);
    // adding a password later
    c.handle(Cmd::ChangePassword {
        old: pw(""),
        new: Password::Set(pw("now there is one")),
    });
    assert!(unlocked(&c).has_password);
    let ev = c.handle(Cmd::ChangePassword {
        old: pw("wrong"),
        new: Password::None,
    });
    assert_eq!(errors(&ev).len(), 1);
    c.handle(Cmd::Lock);
    assert!(errors(&c.handle(Cmd::Unlock { password: pw("") })).len() == 1);
    assert!(errors(&c.handle(Cmd::Unlock {
        password: pw("now there is one")
    }))
    .is_empty());
}

// ---------------------------------------------------------------------------------------------------------------
// the node as a process
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn the_node_is_started_found_stopped_cleanly_and_a_second_start_does_nothing() {
    let rig = Rig::new("node", 18474);
    let mut c = rig.core();
    assert_eq!(c.snapshot().node, NodeView::Stopped);
    let ev = c.handle(Cmd::StartNode);
    assert!(errors(&ev).is_empty(), "{:?}", errors(&ev));
    assert_eq!(c.snapshot().node, NodeView::Starting);
    wait(&mut c, 90, "the node to answer", |s| {
        matches!(s.node, NodeView::Running { .. })
    });
    match c.snapshot().node {
        NodeView::Running { info, ours } => {
            assert!(ours);
            assert_eq!(info.network, "test");
        }
        other => panic!("{other:?}"),
    }
    // a second start is a notice, not a second node
    let ev = c.handle(Cmd::StartNode);
    assert!(errors(&ev).is_empty());
    assert!(ev.iter().any(|e| matches!(e, Event::Notice(_))));

    // a second window (or the next run) finds the node already going and does not start another
    let c_other = rig.core();
    match c_other.snapshot().node {
        NodeView::Running { ours, .. } => assert!(!ours, "found, not started by this one"),
        other => panic!("the running node was not found: {other:?}"),
    }
    drop(c_other);

    // stop: "stopping" is shown first, then the stop is done on the next pass
    let ev = c.handle(Cmd::StopNode);
    assert!(errors(&ev).is_empty());
    assert_eq!(c.snapshot().node, NodeView::Stopping);
    let ev = c.tick();
    assert!(
        ev.iter()
            .any(|e| matches!(e, Event::Notice(m) if m.contains("stopped cleanly"))),
        "{:?}",
        ev.iter()
            .filter_map(|e| match e {
                Event::Notice(m) | Event::Error(m) => Some(m.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    );
    assert_eq!(c.snapshot().node, NodeView::Stopped);
    // really gone: nothing answers on the control port any more
    assert!(tenero_gui::procs::reach_node(&rig.settings).is_none());
}

#[test]
fn a_node_that_cannot_start_is_reported_with_what_it_said() {
    let mut rig = Rig::new("badnode", 18475);
    rig.settings.listen = Some("999.1.1.1:1".into());
    let mut c = rig.core();
    let ev = c.handle(Cmd::StartNode);
    assert!(errors(&ev).is_empty());
    wait(&mut c, 30, "the failure", |s| {
        matches!(s.node, NodeView::Failed { .. })
    });
    match c.snapshot().node {
        NodeView::Failed { why, output } => {
            assert!(why.contains("stopped with code"), "{why}");
            assert!(
                output.to_lowercase().contains("listen"),
                "the node's own words are shown: {output}"
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_missing_program_is_said_plainly() {
    let mut rig = Rig::new("noexe", 18476);
    rig.settings.program_dir = Some(rig.dir.join("nowhere"));
    let mut c = rig.core();
    let ev = c.handle(Cmd::StartNode);
    let e = errors(&ev);
    assert!(
        e.len() == 1 && e[0].contains("cannot find the node program"),
        "{e:?}"
    );
    assert_eq!(c.snapshot().node, NodeView::Stopped);
}

// ---------------------------------------------------------------------------------------------------------------
// the whole thing: node, miner, balances, a payment at each fee level, history
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn mine_pay_at_every_fee_level_and_read_the_history() {
    let rig = Rig::new("whole", 18477);
    let mut c = rig.core();
    c.handle(Cmd::CreateWallet {
        password: Password::Set(pw("a long enough password")),
    });
    c.handle(Cmd::AddAccount {
        label: "Savings".into(),
    });
    let saving = unlocked(&c).accounts[1].address.clone();

    // no node yet: mining and paying are refused with the reason
    let ev = c.handle(Cmd::StartMiner);
    assert_eq!(errors(&ev).len(), 1);
    let ev = c.handle(Cmd::PreparePayment {
        account: 0,
        to: saving.clone(),
        amount: "1".into(),
        level: FeeLevel::Low,
    });
    assert!(
        errors(&ev)[0].contains("node is not running"),
        "{:?}",
        errors(&ev)
    );
    // a fee estimate that cannot be made says so with its own event (so the window never says "working" for ever)
    let ev = c.handle(Cmd::EstimateFees {
        account: 0,
        to: saving.clone(),
        amount: "1".into(),
    });
    assert!(errors(&ev).is_empty());
    assert!(ev
        .iter()
        .any(|e| matches!(e, Event::EstimateFailed(m) if m.contains("node is not running"))));

    c.handle(Cmd::StartNode);
    wait(&mut c, 90, "the node", |s| {
        matches!(s.node, NodeView::Running { .. })
    });
    let ev = c.handle(Cmd::StartMiner);
    assert!(errors(&ev).is_empty(), "{:?}", errors(&ev));
    assert_eq!(c.snapshot().miner, MinerView::Starting);

    // the miner reports; blocks arrive; the wallet reads them and shows a balance
    wait(
        &mut c,
        120,
        "the miner's report",
        |s| matches!(&s.miner, MinerView::Running { report, stale: false } if report.found > 0),
    );
    wait(&mut c, 180, "a mature balance", |s| match &s.wallet {
        WalletView::Unlocked(d) => {
            d.synced && d.accounts[0].balance.is_some_and(|b| b.spendable > 0)
        }
        _ => false,
    });
    let d = unlocked(&c);
    assert!(d
        .history
        .iter()
        .any(|h| h.kind == EntryKind::Mined && h.account == 0));
    assert_eq!(d.accounts[1].balance.unwrap().total, 0);

    // the three fee levels, quoted before anything is sent
    let amount = "0.5".to_string();
    let t0 = Instant::now();
    let ev = c.handle(Cmd::EstimateFees {
        account: 0,
        to: saving.clone(),
        amount: amount.clone(),
    });
    println!("estimate took {:?}", t0.elapsed());
    let fees = ev
        .iter()
        .find_map(|e| match e {
            Event::Estimate { fees } => Some(*fees),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no estimate: {:?}", errors(&ev)));
    assert!(fees[0] < fees[1] && fees[1] < fees[2], "{fees:?}");

    for (i, level) in FeeLevel::ALL.into_iter().enumerate() {
        let t0 = Instant::now();
        let ev = c.handle(Cmd::PreparePayment {
            account: 0,
            to: saving.clone(),
            amount: amount.clone(),
            level,
        });
        println!("prepare ({level:?}) took {:?}", t0.elapsed());
        assert!(errors(&ev).is_empty(), "{level:?}: {:?}", errors(&ev));
        let q = c.snapshot().prepared.expect("a payment waits for a yes");
        assert_eq!((q.account, q.amount, q.level), (0, 50_000_000, level));
        assert_eq!(q.to, saving);
        // the fee shown is at least what the estimate said for this level (the exact build can differ by a few
        // units when the size changes)
        let slack = fees[i] / 20 + 2;
        assert!(
            q.fee + slack >= fees[i] && q.fee <= fees[i] + slack,
            "{level:?}: quote {} vs estimate {}",
            q.fee,
            fees[i]
        );
        if level == FeeLevel::High {
            // nothing has been sent by preparing
            assert!(unlocked(&c)
                .history
                .iter()
                .all(|h| !matches!(h.kind, EntryKind::Sent { .. })));
            let ev = c.handle(Cmd::SendPrepared);
            assert!(errors(&ev).is_empty(), "{:?}", errors(&ev));
            assert!(ev.iter().any(|e| matches!(e, Event::Sent { .. })));
            assert!(c.snapshot().prepared.is_none());
        } else {
            c.handle(Cmd::CancelPrepared);
            assert!(c.snapshot().prepared.is_none());
        }
    }

    // the miner is still running, so a block takes the payment in; the history says so
    wait(&mut c, 180, "the payment to be confirmed", |s| {
        match &s.wallet {
            WalletView::Unlocked(d) => d.history.iter().any(|h| {
                matches!(
                    h.kind,
                    EntryKind::Sent {
                        status: SentStatus::Confirmed,
                        ..
                    }
                )
            }),
            _ => false,
        }
    });
    wait(&mut c, 60, "the other account to be paid", |s| {
        match &s.wallet {
            WalletView::Unlocked(d) => d.accounts[1].balance.is_some_and(|b| b.total == 50_000_000),
            _ => false,
        }
    });
    let d = unlocked(&c);
    let sent: Vec<_> = d
        .history
        .iter()
        .filter(|h| matches!(h.kind, EntryKind::Sent { .. }))
        .collect();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].amount, 50_000_000);
    // the account that was paid sees it as received
    assert!(d
        .history
        .iter()
        .any(|h| h.account == 1 && h.kind == EntryKind::Received && h.amount == 50_000_000));

    // stopping the miner is immediate and leaves the node going; stopping the node stops everything
    c.handle(Cmd::StopMiner);
    assert_eq!(c.snapshot().miner, MinerView::Off);
    assert!(matches!(c.snapshot().node, NodeView::Running { .. }));
    // and quitting stops what this window started and says so
    let ev = c.handle(Cmd::Quit);
    assert!(ev.iter().any(|e| matches!(e, Event::Quit)));
    assert!(
        tenero_gui::procs::reach_node(&rig.settings).is_none(),
        "the node this window started is gone"
    );
}

#[test]
fn a_node_that_was_only_found_running_is_left_running_when_the_window_quits() {
    let rig = Rig::new("attach", 18478);
    let mut first = rig.core();
    first.handle(Cmd::StartNode);
    wait(&mut first, 90, "the node", |s| {
        matches!(s.node, NodeView::Running { .. })
    });
    // `first` plays the owner's own node: a second window only finds it
    let mut second = rig.core();
    assert!(matches!(
        second.snapshot().node,
        NodeView::Running { ours: false, .. }
    ));
    let ev = second.handle(Cmd::Quit);
    assert!(ev.iter().any(|e| matches!(e, Event::Quit)));
    assert!(
        tenero_gui::procs::reach_node(&rig.settings).is_some(),
        "it was not the second window's to stop"
    );
    // `first` ends it properly when it goes
    let ev = first.handle(Cmd::Quit);
    assert!(ev.iter().any(|e| matches!(e, Event::Quit)));
    assert!(tenero_gui::procs::reach_node(&rig.settings).is_none());
}

#[test]
fn settings_cannot_be_changed_under_a_running_node_or_an_open_wallet() {
    let rig = Rig::new("settings", 18479);
    let mut c = rig.core();
    c.handle(Cmd::CreateWallet {
        password: Password::None,
    });
    let mut s = rig.settings.clone();
    s.wallet_file = rig.dir.join("other.twl");
    assert_eq!(
        errors(&c.handle(Cmd::SetSettings(Box::new(s)))).len(),
        1,
        "lock the wallet first"
    );
    let mut s = rig.settings.clone();
    s.miner_pace_secs = 9;
    assert!(
        errors(&c.handle(Cmd::SetSettings(Box::new(s)))).is_empty(),
        "miner settings can change while it is off"
    );
    assert_eq!(c.settings().miner_pace_secs, 9);
    // saved: the next run reads them
    assert_eq!(Settings::load(&rig.dir).unwrap().miner_pace_secs, 9);
    // a backend the network cannot use is refused
    let mut s = c.settings().clone();
    s.miner_backend = MinerBackend::Gpu;
    assert_eq!(errors(&c.handle(Cmd::SetSettings(Box::new(s)))).len(), 1);
    let _ = Path::new("");
}

#[test]
fn the_window_can_end_what_it_started_even_when_the_worker_never_answers_quit() {
    use tenero_gui::backend::Backend;
    let rig = Rig::new("emergency", 18480);
    let mut b = Backend::spawn(
        &rig.dir,
        rig.settings.clone(),
        KdfParams::TEST_ONLY_WEAK,
        std::sync::Arc::new(|| {}),
    );
    b.send(Cmd::StartNode);
    let end = Instant::now() + Duration::from_secs(90);
    while tenero_gui::procs::reach_node(&rig.settings).is_none() {
        assert!(Instant::now() < end, "the node did not come up");
        std::thread::sleep(Duration::from_millis(250));
    }
    // the window's own way out, with no help from the worker
    b.emergency_stop(&rig.settings);
    assert!(
        tenero_gui::procs::reach_node(&rig.settings).is_none(),
        "the node this window started was left running"
    );
    // the worker is still there and ends cleanly when asked
    b.send(Cmd::Quit);
    b.join();
}
