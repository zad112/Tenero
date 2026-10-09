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
use tenero_wallet::{EntryKind, FeeLevel, KdfParams, SentStatus, ViewTier};
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
        name: None,
        password: Password::Set(pw("short")),
    });
    assert_eq!(errors(&ev).len(), 1);
    assert!(!rig.settings.wallet_file.exists());

    let ev = c.handle(Cmd::CreateWallet {
        name: None,
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
    assert!(d.accounts[0].address.starts_with("TENt"));
    assert_eq!(d.accounts[0].balance, None);
    assert_eq!(d.total, None);
    assert!(!d.synced, "with no node nothing is final");
    assert!(d.has_password);

    // a second wallet is not made on top of the first
    let ev = c.handle(Cmd::CreateWallet {
        name: None,
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

    // a view key, too, only after the password; it makes a view-only wallet of the same address
    let view_key = |ev: &[Event]| {
        ev.iter().find_map(|e| match e {
            Event::ViewKey { key, received } => Some((key.to_string(), *received)),
            _ => None,
        })
    };
    let ev = c.handle(Cmd::RevealViewKey {
        password: pw("wrong"),
        account: 0,
        received: false,
    });
    assert!(view_key(&ev).is_none() && errors(&ev).len() == 1);
    for received in [false, true] {
        let ev = c.handle(Cmd::RevealViewKey {
            password: pw("correct horse battery"),
            account: 0,
            received,
        });
        let (key, r) = view_key(&ev).unwrap();
        assert_eq!(r, received);
        let v = tenero_wallet::Wallet::from_view_key(&key, tenero_wallet::Network::Test).unwrap();
        assert_eq!(v.address().to_text(), unlocked(&c).accounts[0].address);
        assert!(v.seed().is_none());
    }

    // an integrated address: the account's address with a new random payment ID inside
    let integrated = |ev: &[Event]| {
        ev.iter().find_map(|e| match e {
            Event::Integrated {
                account,
                address,
                payment_id,
            } => Some((*account, address.clone(), *payment_id)),
            _ => None,
        })
    };
    let (account, text, id) = integrated(&c.handle(Cmd::MakeIntegrated { account: 0 })).unwrap();
    assert_eq!(account, 0);
    assert_ne!(id, [0; 8]);
    let a = tenero_wallet::Address::parse(&text, tenero_wallet::Network::Test).unwrap();
    assert_eq!(
        (a.kind, a.payment_id),
        (tenero_wallet::Kind::Integrated, id)
    );
    let main = tenero_wallet::Address::parse(
        &unlocked(&c).accounts[0].address,
        tenero_wallet::Network::Test,
    )
    .unwrap();
    assert_eq!(a.spend_pubkey, main.spend_pubkey);
    let (_, _, other) = integrated(&c.handle(Cmd::MakeIntegrated { account: 0 })).unwrap();
    assert_ne!(other, id, "a new ID each time");
}

#[test]
fn the_words_restore_the_same_wallet_with_a_different_password_and_accounts_can_be_added_and_named()
{
    let rig = Rig::new("restore", 18472);
    let mut c = rig.core();
    let ev = c.handle(Cmd::CreateWallet {
        name: None,
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
    other.wallets_dir = rig.dir.join("other-computer");
    other.wallet_file = other.wallets_dir.join("Restored.twl");
    other.legacy_wallet_file = rig.dir.join("not-here.twl");
    let mut c2 = Core::new(&rig.dir, other, KdfParams::TEST_ONLY_WEAK);
    let ev = c2.handle(Cmd::RestoreWallet {
        name: Some("Restored".into()),
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
    other2.wallets_dir = rig.dir.join("third-computer");
    other2.wallet_file = other2.wallets_dir.join("Never.twl");
    other2.legacy_wallet_file = rig.dir.join("not-here.twl");
    let mut c3 = Core::new(&rig.dir, other2.clone(), KdfParams::TEST_ONLY_WEAK);
    let mut bad: Vec<&str> = words.split(' ').collect();
    bad[3] = "tenero";
    let ev = c3.handle(Cmd::RestoreWallet {
        name: Some("Never".into()),
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
        name: None,
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
        name: None,
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
        note: None,
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

    // the three fee levels, quoted before anything is sent. The miner is stopped meanwhile: a proof's size, and so the fee,
    // grows when the curve tree gains a layer, and a block between the quote and the build could add one
    c.handle(Cmd::StopMiner);
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
            Event::Estimate { fees, .. } => Some(*fees),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no estimate: {:?}", errors(&ev)));
    assert!(fees[0] < fees[1] && fees[1] < fees[2], "{fees:?}");

    for (i, level) in FeeLevel::ALL.into_iter().enumerate() {
        let t0 = Instant::now();
        let ev = c.handle(Cmd::PreparePayment {
            note: Some("  Rent  ".into()),
            account: 0,
            to: saving.clone(),
            amount: amount.clone(),
            level,
        });
        println!("prepare ({level:?}) took {:?}", t0.elapsed());
        assert!(errors(&ev).is_empty(), "{level:?}: {:?}", errors(&ev));
        let q = c.snapshot().prepared.expect("a payment waits for a yes");
        assert_eq!((q.account, q.amount, q.level), (0, 50_000_000, level));
        assert_eq!(
            q.note.as_deref(),
            Some("Rent"),
            "the note is trimmed and shown before sending"
        );
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

    // the miner again, so a block takes the payment in; the history says so
    let ev = c.handle(Cmd::StartMiner);
    assert!(errors(&ev).is_empty(), "{:?}", errors(&ev));
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

    // ---- proofs, signatures and the payment key, through the same screen logic -----------------------------------------
    let sent_row = d
        .history
        .iter()
        .find(|h| matches!(h.kind, EntryKind::Sent { .. }))
        .unwrap()
        .clone();
    assert!(sent_row.has_secret, "a payment sent now keeps its anchor");
    assert_eq!(
        sent_row.note.as_deref(),
        Some("Rent"),
        "what it was for is in the history"
    );
    let id = sent_row.id.unwrap();
    let proof_of = |c: &mut Core, req: ProofRequest| -> String {
        let ev = c.handle(Cmd::MakeProof(req));
        assert!(errors(&ev).is_empty(), "{:?}", errors(&ev));
        ev.iter()
            .find_map(|e| match e {
                Event::Proof { text, .. } => Some(text.clone()),
                _ => None,
            })
            .expect("a proof")
    };
    let check = |c: &mut Core, text: &str| -> Result<CheckedView, String> {
        let ev = c.handle(Cmd::CheckProof {
            text: text.to_string(),
        });
        ev.into_iter()
            .find_map(|e| match e {
                Event::ProofChecked(r) => Some(r),
                _ => None,
            })
            .expect("an answer")
    };
    // the sender's proof
    let text = proof_of(&mut c, ProofRequest::Sent { id, key: false });
    assert!(text.starts_with("TENpay1"));
    let v = check(&mut c, &text).unwrap();
    assert_eq!(
        (v.amount, v.address.as_str(), v.kind),
        (50_000_000, saving.as_str(), "payment")
    );
    assert!(v.confirmations >= 1 && !v.block_reward);
    // a proof with one character changed is refused, with the reason
    let mut bad = text.clone().into_bytes();
    let at = bad.len() / 2;
    bad[at] = if bad[at] == b'2' { b'3' } else { b'2' };
    assert!(check(&mut c, &String::from_utf8(bad).unwrap()).is_err());
    // the receiving account proves its receipt from the history, signed by its address
    let got = d
        .history
        .iter()
        .find(|h| h.account == 1 && h.kind == EntryKind::Received)
        .unwrap();
    let text = proof_of(
        &mut c,
        ProofRequest::Received {
            account: 1,
            global_index: got.global_index.unwrap(),
        },
    );
    let v = check(&mut c, &text).unwrap();
    assert_eq!(
        (v.amount, v.address.as_str(), v.kind),
        (50_000_000, saving.as_str(), "received")
    );
    // account 0 cannot make a proof of an output that is account 1's
    let ev = c.handle(Cmd::MakeProof(ProofRequest::Received {
        account: 0,
        global_index: got.global_index.unwrap(),
    }));
    assert_eq!(errors(&ev).len(), 1);

    // the payment key is shown only when asked for: the payment's anchor, 32 hexadecimal digits
    let ev = c.handle(Cmd::RevealTxKey { id });
    let key_hex = ev
        .iter()
        .find_map(|e| match e {
            Event::TxKey { key, .. } => Some(key.to_string()),
            _ => None,
        })
        .expect("the key");
    assert_eq!(key_hex.len(), 32);
    assert!(
        !format!("{:?}", c.snapshot().wallet).contains(&key_hex),
        "the key is not part of what the window holds"
    );
    // the key and the address: the output the key made is looked for
    let run_key = |c: &mut Core,
                   key: &str,
                   address: &str,
                   from: Option<u64>|
     -> Result<CheckedView, String> {
        let ev = c.handle(Cmd::CheckKey {
            key: key.to_string(),
            address: address.to_string(),
            from_height: from,
        });
        ev.into_iter()
            .find_map(|e| match e {
                Event::ProofChecked(r) => Some(r),
                _ => None,
            })
            .expect("an answer")
    };
    let v = run_key(&mut c, &key_hex, &saving, None).unwrap();
    assert_eq!(
        (v.amount, v.address.as_str()),
        (50_000_000, saving.as_str())
    );
    assert!(run_key(&mut c, &key_hex, &saving, Some(v.height)).is_ok());
    assert!(run_key(&mut c, &key_hex, &saving, Some(v.height + 1)).is_err());
    assert!(run_key(&mut c, &key_hex, &d.accounts[0].address, None).is_err());
    assert!(run_key(&mut c, "xyz", &saving, None)
        .unwrap_err()
        .contains("32"));
    assert!(run_key(&mut c, &key_hex, "TENtnonsense", None)
        .unwrap_err()
        .contains("address"));

    // signing needs the wallet; verifying needs neither the wallet nor the node
    let ev = c.handle(Cmd::SignMessage {
        account: 1,
        message: "this is mine".into(),
    });
    let sig = ev
        .iter()
        .find_map(|e| match e {
            Event::Signed { signature } => Some(signature.clone()),
            _ => None,
        })
        .expect("a signature");
    let sig = tenero_wallet::proofs::Signature::from_text(&sig).unwrap();
    let addr = tenero_wallet::Address::parse(&saving, tenero_wallet::Network::Test).unwrap();
    assert!(tenero_wallet::proofs::verify_message(
        &addr,
        b"this is mine",
        &sig
    ));
    assert!(!tenero_wallet::proofs::verify_message(
        &addr,
        b"this is mine!",
        &sig
    ));
    c.handle(Cmd::Lock);
    assert_eq!(
        errors(&c.handle(Cmd::SignMessage {
            account: 1,
            message: "x".into()
        }))
        .len(),
        1,
        "no signing with the wallet locked"
    );
    assert!(
        check(&mut c, &text).is_ok(),
        "a proof is checked with the wallet locked"
    );
    c.handle(Cmd::Unlock {
        password: pw("a long enough password"),
    });

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

fn notices(events: &[Event]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Notice(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

/// Something that looks like a node's data folder, in `dir`.
fn put_node_data(dir: &Path, big: usize) {
    std::fs::create_dir_all(dir.join("chain.redb.segments")).unwrap();
    let blob: Vec<u8> = (0..big)
        .map(|i| (i.wrapping_mul(2654435761) >> 7) as u8)
        .collect();
    std::fs::write(dir.join("chain.redb"), &blob).unwrap();
    std::fs::write(dir.join("chain.redb.segments").join("a"), b"segment").unwrap();
    std::fs::write(dir.join("node.key"), [7u8; 32]).unwrap();
    std::fs::write(dir.join("control.cookie"), b"old cookie").unwrap();
}

#[test]
fn the_nodes_data_is_moved_checked_and_only_then_used_and_the_old_folder_is_left() {
    let rig = Rig::new("movedata", 18496);
    let mut c = rig.core();
    let old = c.settings().data_dir.clone();
    put_node_data(&old, 3 * 1024 * 1024 + 11);
    let new = rig.dir.join("elsewhere");
    let ev = c.handle(Cmd::MoveNodeData { to: new.clone() });
    assert!(errors(&ev).is_empty(), "{:?}", errors(&ev));
    // it runs on its own thread: the snapshot says so, and the setting has not moved yet
    assert!(c.snapshot().moving.is_some());
    assert_eq!(
        c.settings().data_dir,
        old,
        "not used until it has been checked"
    );
    let ev = wait(&mut c, 60, "the move to end", |s| s.moving.is_none());
    assert!(errors(&ev).is_empty(), "{:?}", errors(&ev));
    assert!(
        notices(&ev)
            .iter()
            .any(|n| n.contains("was moved to") && n.contains("delete the old folder yourself")),
        "{:?}",
        notices(&ev)
    );
    // now it is used, and that is saved for the next run
    assert_eq!(c.settings().data_dir, new);
    assert_eq!(Settings::load(&rig.dir).unwrap().data_dir, new);
    // the copy is whole (and has no old cookie); the old folder is exactly as it was
    assert_eq!(
        std::fs::read(new.join("chain.redb")).unwrap(),
        std::fs::read(old.join("chain.redb")).unwrap()
    );
    assert_eq!(std::fs::read(new.join("node.key")).unwrap(), [7u8; 32]);
    assert!(new.join("chain.redb.segments").join("a").is_file());
    assert!(!new.join("control.cookie").exists());
    assert_eq!(
        std::fs::read(old.join("control.cookie")).unwrap(),
        b"old cookie"
    );
    assert_eq!(
        std::fs::read(old.join("chain.redb")).unwrap().len(),
        3 * 1024 * 1024 + 11
    );
}

#[test]
fn a_real_node_stops_its_data_is_moved_and_it_starts_again_from_the_new_folder() {
    // the point of it all: tenerod accepts the folder the move made (it refuses one that other accounts can open) and runs from it
    let rig = Rig::new("movereal", 18500);
    let mut c = rig.core();
    let old = c.settings().data_dir.clone();
    assert!(errors(&c.handle(Cmd::StartNode)).is_empty());
    wait(&mut c, 90, "the node to answer", |s| {
        matches!(s.node, NodeView::Running { .. })
    });
    // a node that is running cannot have its data moved
    let ev = c.handle(Cmd::MoveNodeData {
        to: rig.dir.join("too-soon"),
    });
    assert_eq!(
        errors(&ev).len(),
        1,
        "a running node's data was not refused"
    );
    assert!(errors(&c.handle(Cmd::StopNode)).is_empty());
    c.tick();
    assert_eq!(c.snapshot().node, NodeView::Stopped);
    assert!(
        old.join("chain.redb").is_file(),
        "the node made its data here"
    );

    let new = rig.dir.join("moved");
    assert!(errors(&c.handle(Cmd::MoveNodeData { to: new.clone() })).is_empty());
    let ev = wait(&mut c, 60, "the move to end", |s| s.moving.is_none());
    assert!(errors(&ev).is_empty(), "{:?}", errors(&ev));
    assert_eq!(c.settings().data_dir, new);

    let ev = c.handle(Cmd::StartNode);
    assert!(errors(&ev).is_empty(), "{:?}", errors(&ev));
    wait(&mut c, 90, "the node to answer from the new folder", |s| {
        matches!(s.node, NodeView::Running { .. } | NodeView::Failed { .. })
    });
    match c.snapshot().node {
        NodeView::Running { info, ours } => {
            assert!(ours);
            assert_eq!(info.network, "test");
        }
        other => panic!("the node did not start from the moved folder: {other:?}"),
    }
    assert!(
        new.join("control.cookie").is_file(),
        "it made its own cookie in the new folder"
    );
    assert!(errors(&c.handle(Cmd::StopNode)).is_empty());
    c.tick();
    assert_eq!(c.snapshot().node, NodeView::Stopped);
}

#[test]
fn a_move_with_nothing_to_move_just_uses_the_new_folder() {
    let rig = Rig::new("movenothing", 18497);
    let mut c = rig.core();
    let new = rig.dir.join("fresh");
    let ev = c.handle(Cmd::MoveNodeData { to: new.clone() });
    assert!(errors(&ev).is_empty(), "{:?}", errors(&ev));
    assert!(c.snapshot().moving.is_none(), "there was nothing to copy");
    assert_eq!(c.settings().data_dir, new);
    assert_eq!(Settings::load(&rig.dir).unwrap().data_dir, new);
}

#[test]
fn a_bad_destination_changes_nothing() {
    let rig = Rig::new("movebad", 18498);
    let mut c = rig.core();
    let old = c.settings().data_dir.clone();
    put_node_data(&old, 1000);
    let full = rig.dir.join("full");
    std::fs::create_dir_all(&full).unwrap();
    std::fs::write(full.join("x"), b"x").unwrap();
    for (to, why) in [
        (full.clone(), "not empty"),
        (old.clone(), "itself"),
        (old.join("inside"), "inside"),
        (PathBuf::from("relative"), "full path"),
        (
            rig.dir.join("no").join("such").join("parent"),
            "does not exist",
        ),
    ] {
        let ev = c.handle(Cmd::MoveNodeData { to });
        let e = errors(&ev);
        assert_eq!(e.len(), 1, "{why}: {e:?}");
        assert!(e[0].contains(why), "{why}: {e:?}");
        assert!(c.snapshot().moving.is_none());
        assert_eq!(c.settings().data_dir, old);
    }
    assert!(!old.join("inside").exists(), "nothing was made by refusing");
}

#[test]
fn while_the_data_is_being_moved_nothing_else_may_change_it_and_a_cancel_changes_nothing() {
    let rig = Rig::new("movecancel", 18499);
    let mut c = rig.core();
    let old = c.settings().data_dir.clone();
    put_node_data(&old, 160 * 1024 * 1024);
    let new = rig.dir.join("never");
    assert!(errors(&c.handle(Cmd::MoveNodeData { to: new.clone() })).is_empty());
    // a second move, a node, a miner and a settings change are all refused meanwhile
    for (cmd, name) in [
        (
            Cmd::MoveNodeData {
                to: rig.dir.join("other"),
            },
            "a second move",
        ),
        (Cmd::StartNode, "the node"),
        (Cmd::StartMiner, "the miner"),
        (
            Cmd::SetSettings(Box::new(rig.settings.clone())),
            "a settings change",
        ),
    ] {
        let ev = c.handle(cmd);
        assert_eq!(errors(&ev).len(), 1, "{name} was not refused");
    }
    assert!(errors(&c.handle(Cmd::CancelMove)).is_empty());
    let ev = wait(&mut c, 60, "the cancelled move to end", |s| {
        s.moving.is_none()
    });
    assert!(errors(&ev).is_empty(), "{:?}", errors(&ev));
    assert!(
        notices(&ev).iter().any(|n| n.contains("cancelled")),
        "{:?}",
        notices(&ev)
    );
    assert_eq!(c.settings().data_dir, old, "still the old folder");
    assert!(!new.exists(), "what it had made is removed");
    assert_eq!(
        std::fs::read(old.join("chain.redb")).unwrap().len(),
        160 * 1024 * 1024
    );
    // and a window closed during a move ends it the same way
    let ev = c.handle(Cmd::MoveNodeData { to: new.clone() });
    assert!(errors(&ev).is_empty());
    let ev = c.handle(Cmd::Quit);
    assert!(ev.iter().any(|e| matches!(e, Event::Quit)));
    assert!(
        !new.exists(),
        "closing the window cancelled the move and cleaned up"
    );
    assert_eq!(
        std::fs::read(old.join("chain.redb")).unwrap().len(),
        160 * 1024 * 1024
    );
}

#[test]
fn an_old_settings_draft_does_not_stop_other_settings_from_applying_while_a_wallet_is_open() {
    // The Settings screen's draft is made when the tab is first opened and lives until Apply. Meanwhile the real settings move on (creating or
    // opening a wallet changes `wallet_file`; the Mining tab changes the account). The whole draft sent back used to put the OLD wallet file
    // back, and the core refuses that while a wallet is open, with every other change: "settings do not apply unless the wallet is locked".
    let rig = Rig::new("staledraft", 18495);
    let mut c = rig.core();
    let base = c.settings().clone(); // the draft is made now, before any wallet exists
    let mut draft = base.clone();
    draft.inbound_port = Some(38333); // the user ticks "let other nodes connect" and changes the pace
    draft.miner_pace_secs = 9;
    c.handle(Cmd::CreateWallet {
        name: Some("Main".into()),
        password: Password::None,
    });
    assert_ne!(
        c.settings().wallet_file,
        base.wallet_file,
        "creating the wallet moved the real setting"
    );
    // sending the whole old draft is what failed
    assert_eq!(
        errors(&c.handle(Cmd::SetSettings(Box::new(draft.clone())))).len(),
        1,
        "the old way: refused because of the stale wallet file"
    );
    // only what the user changed, over the settings as they are now
    let wallet_now = c.settings().wallet_file.clone();
    let merged = c.settings().with_changes(&base, &draft);
    assert!(
        errors(&c.handle(Cmd::SetSettings(Box::new(merged)))).is_empty(),
        "the user's changes apply with the wallet open"
    );
    assert_eq!(c.settings().inbound_port, Some(38333));
    assert_eq!(c.settings().miner_pace_secs, 9);
    assert_eq!(
        c.settings().wallet_file,
        wallet_now,
        "the wallet file was not touched"
    );
    // changing the network is the one thing that does need the wallet locked, and it says so
    let mut other = c.settings().clone();
    other.network = tenero_app::config::Network::Gamma;
    let ev = c.handle(Cmd::SetSettings(Box::new(other)));
    let e = errors(&ev);
    assert_eq!(e.len(), 1);
    assert!(format!("{e:?}").contains("changing the network"), "{e:?}");
}

#[test]
fn settings_cannot_be_changed_under_a_running_node_or_an_open_wallet() {
    let rig = Rig::new("settings", 18479);
    let mut c = rig.core();
    c.handle(Cmd::CreateWallet {
        name: None,
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

#[test]
fn several_wallets_are_made_listed_and_switched_and_none_overwrites_another() {
    let rig = Rig::new("many", 18481);
    let mut c = rig.core();
    let created = |ev: &[Event]| words_of(ev).expect("the words, once").0;

    // the first wallet, by name
    let ev = c.handle(Cmd::CreateWallet {
        name: Some("Main wallet".into()),
        password: Password::Set(pw("first password")),
    });
    assert!(errors(&ev).is_empty(), "{:?}", errors(&ev));
    let words_main = created(&ev);
    let main_addr = unlocked(&c).accounts[0].address.clone();
    assert!(rig.settings.wallets_dir.join("Main wallet.twl").is_file());

    // another cannot be made or restored while this one is open
    let ev = c.handle(Cmd::CreateWallet {
        name: Some("Second".into()),
        password: Password::None,
    });
    assert_eq!(errors(&ev).len(), 1);
    assert!(errors(&ev)[0].contains("lock the open wallet"));
    assert!(!rig.settings.wallets_dir.join("Second.twl").exists());

    // lock, and make a second one with another password
    c.handle(Cmd::Lock);
    let ev = c.handle(Cmd::CreateWallet {
        name: Some("Savings stash".into()),
        password: Password::Set(pw("second password")),
    });
    assert!(errors(&ev).is_empty(), "{:?}", errors(&ev));
    let other_addr = unlocked(&c).accounts[0].address.clone();
    assert_ne!(main_addr, other_addr, "a different seed");
    let names: Vec<String> = c
        .snapshot()
        .wallets
        .iter()
        .map(|w| w.name.clone())
        .collect();
    assert_eq!(names, ["Main wallet", "Savings stash"]);

    // names that are not safe, or already used (without regard to case), are refused and nothing is written
    c.handle(Cmd::Lock);
    for bad in [
        "",
        "   ",
        "../evil",
        "a/b",
        "a\\b",
        "con",
        "NUL",
        "name.twl",
        "MAIN WALLET",
        "main wallet ",
        &"x".repeat(41),
    ] {
        let ev = c.handle(Cmd::CreateWallet {
            name: Some(bad.to_string()),
            password: Password::None,
        });
        assert_eq!(errors(&ev).len(), 1, "`{bad}` was accepted");
    }
    assert_eq!(
        c.snapshot().wallets.len(),
        2,
        "no file was made by a refused name"
    );
    assert_eq!(
        std::fs::read_dir(&rig.settings.wallets_dir)
            .unwrap()
            .count(),
        2
    );

    // switching: select, unlock with THAT wallet's password
    let list = c.snapshot().wallets.clone();
    let (main, stash) = (&list[0], &list[1]);
    assert!(errors(&c.handle(Cmd::SelectWallet {
        path: main.path.clone()
    }))
    .is_empty());
    assert_eq!(c.snapshot().settings.wallet_file, main.path);
    assert_eq!(
        errors(&c.handle(Cmd::Unlock {
            password: pw("second password")
        }))
        .len(),
        1,
        "the other wallet's password"
    );
    assert!(errors(&c.handle(Cmd::Unlock {
        password: pw("first password")
    }))
    .is_empty());
    assert_eq!(unlocked(&c).accounts[0].address, main_addr);
    // a wallet cannot be selected while another is open, nor a file that is not in the list
    assert_eq!(
        errors(&c.handle(Cmd::SelectWallet {
            path: stash.path.clone()
        }))
        .len(),
        1
    );
    c.handle(Cmd::Lock);
    assert_eq!(
        errors(&c.handle(Cmd::SelectWallet {
            path: rig.dir.join("elsewhere.twl")
        }))
        .len(),
        1
    );
    c.handle(Cmd::SelectWallet {
        path: stash.path.clone(),
    });
    assert!(errors(&c.handle(Cmd::Unlock {
        password: pw("second password")
    }))
    .is_empty());
    assert_eq!(unlocked(&c).accounts[0].address, other_addr);

    // the selection is remembered by the next run, which opens on the chooser
    c.handle(Cmd::Lock);
    drop(c);
    let again = Core::new(
        &rig.dir,
        Settings::load(&rig.dir).unwrap(),
        KdfParams::TEST_ONLY_WEAK,
    );
    assert_eq!(again.snapshot().settings.wallet_file, stash.path);
    assert_eq!(again.snapshot().wallets.len(), 2);
    assert_eq!(again.snapshot().wallet, WalletView::Locked);
    let mut c = again;

    // restoring into a new name from the first wallet's words gives that wallet's address; the file is a new one
    let ev = c.handle(Cmd::RestoreWallet {
        phrase: pw(&words_main),
        password: Password::Set(pw("third password")),
        birth: None,
        name: Some("Copy of main".into()),
    });
    assert!(errors(&ev).is_empty(), "{:?}", errors(&ev));
    assert_eq!(unlocked(&c).accounts[0].address, main_addr);
    assert_eq!(c.snapshot().wallets.len(), 3);
    // and restoring onto a name that is taken is refused, leaving the wallet that was there alone
    c.handle(Cmd::Lock);
    let ev = c.handle(Cmd::RestoreWallet {
        phrase: pw(&words_main),
        password: Password::None,
        birth: None,
        name: Some("Savings stash".into()),
    });
    assert_eq!(errors(&ev).len(), 1);
    c.handle(Cmd::SelectWallet {
        path: stash.path.clone(),
    });
    assert!(
        errors(&c.handle(Cmd::Unlock {
            password: pw("second password")
        }))
        .is_empty(),
        "the wallet was not overwritten"
    );
    assert_eq!(unlocked(&c).accounts[0].address, other_addr);
}

#[test]
fn a_wallet_file_from_before_names_is_listed_and_the_folder_cannot_change_under_an_open_wallet() {
    let rig = Rig::new("legacy", 18482);
    let mut c = rig.core();
    // no name: the selected file's own place (what the older app did)
    let ev = c.handle(Cmd::CreateWallet {
        name: None,
        password: Password::None,
    });
    assert!(errors(&ev).is_empty());
    assert!(rig.settings.wallet_file.is_file());
    let names: Vec<String> = c
        .snapshot()
        .wallets
        .iter()
        .map(|w| w.name.clone())
        .collect();
    assert_eq!(names, ["wallet-test"]);
    // an old wallet is still there when a named one is added
    c.handle(Cmd::Lock);
    c.handle(Cmd::CreateWallet {
        name: Some("Newer".into()),
        password: Password::None,
    });
    let names: Vec<String> = c
        .snapshot()
        .wallets
        .iter()
        .map(|w| w.name.clone())
        .collect();
    assert_eq!(names, ["Newer", "wallet-test"]);
    // the folder cannot move under an open wallet
    let mut s = c.settings().clone();
    s.wallets_dir = rig.dir.join("elsewhere");
    assert_eq!(errors(&c.handle(Cmd::SetSettings(Box::new(s)))).len(), 1);
}

#[test]
fn payment_requests_are_made_kept_in_the_wallet_file_and_removed() {
    let rig = Rig::new("requests", 18483);
    let mut c = rig.core();
    c.handle(Cmd::CreateWallet {
        name: Some("Main".into()),
        password: Password::Set(pw("a long enough password")),
    });
    c.handle(Cmd::AddAccount {
        label: "Savings".into(),
    });
    let d = unlocked(&c);
    assert!(d.requests.is_empty());
    let add = |c: &mut Core, account: usize, amount: &str, label: &str, message: &str| {
        c.handle(Cmd::AddRequest {
            account,
            amount: amount.into(),
            label: label.into(),
            message: message.into(),
        })
    };
    // a full request, and one with nothing but the address
    assert!(errors(&add(&mut c, 0, "1.5", "Rent", "October rent")).is_empty());
    assert!(errors(&add(&mut c, 1, "", "", "")).is_empty());
    let d = unlocked(&c);
    assert_eq!(d.requests.len(), 2);
    assert_eq!(
        d.requests[0].uri,
        format!(
            "tenero:{}?amount=1.5&label=Rent&message=October%20rent",
            d.accounts[0].address
        )
    );
    assert_eq!(
        (d.requests[0].amount, d.requests[0].account_label.as_str()),
        (Some(150_000_000), "Main")
    );
    assert_eq!(
        d.requests[1].uri,
        format!("tenero:{}", d.accounts[1].address)
    );
    // the link reads back as what was asked
    let back =
        tenero_wallet::PaymentRequest::from_uri(&d.requests[0].uri, tenero_wallet::Network::Test)
            .unwrap();
    assert_eq!(
        (back.amount, back.label.as_deref(), back.message.as_deref()),
        (Some(150_000_000), Some("Rent"), Some("October rent"))
    );
    // refused: a bad or zero amount, a label that is too long or has a line break, an account that is not there
    assert_eq!(errors(&add(&mut c, 0, "1.123456789", "x", "")).len(), 1);
    assert_eq!(errors(&add(&mut c, 0, "0", "x", "")).len(), 1);
    assert_eq!(errors(&add(&mut c, 0, "abc", "x", "")).len(), 1);
    assert_eq!(errors(&add(&mut c, 0, "", &"x".repeat(65), "")).len(), 1);
    assert_eq!(errors(&add(&mut c, 0, "", "a\nb", "")).len(), 1);
    assert_eq!(errors(&add(&mut c, 9, "", "x", "")).len(), 1);
    assert_eq!(
        unlocked(&c).requests.len(),
        2,
        "a refused request is not kept"
    );

    // kept in the file: locked and opened again, they are there; and removal is kept too
    c.handle(Cmd::Lock);
    assert!(
        errors(&add(&mut c, 0, "", "x", "")).len() == 1,
        "no requests with the wallet locked"
    );
    assert!(errors(&c.handle(Cmd::Unlock {
        password: pw("a long enough password")
    }))
    .is_empty());
    assert_eq!(unlocked(&c).requests.len(), 2);
    assert!(errors(&c.handle(Cmd::DeleteRequest { index: 0 })).is_empty());
    assert_eq!(errors(&c.handle(Cmd::DeleteRequest { index: 5 })).len(), 1);
    assert_eq!(unlocked(&c).requests.len(), 1);
    c.handle(Cmd::Lock);
    c.handle(Cmd::Unlock {
        password: pw("a long enough password"),
    });
    let d = unlocked(&c);
    assert_eq!(d.requests.len(), 1);
    assert_eq!(d.requests[0].account_label, "Savings");
}

// ---------------------------------------------------------------------------------------------------------------
// combining coins, through the same screen logic
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn combining_coins_is_previewed_cancelled_sent_and_never_shown_as_money_received() {
    let rig = Rig::new("combine", 18491);
    let mut c = rig.core();
    c.handle(Cmd::CreateWallet {
        name: None,
        password: Password::Set(pw("a long enough password")),
    });
    // no node: refused with the reason
    let ev = c.handle(Cmd::PrepareCombine {
        account: 0,
        coins: Some(3),
        level: FeeLevel::Low,
    });
    assert!(
        errors(&ev)[0].contains("node is not running"),
        "{:?}",
        errors(&ev)
    );
    c.handle(Cmd::StartNode);
    wait(&mut c, 90, "the node", |s| {
        matches!(s.node, NodeView::Running { .. })
    });
    let ev = c.handle(Cmd::StartMiner);
    assert!(errors(&ev).is_empty(), "{:?}", errors(&ev));
    // a dozen SPENDABLE rewards: a reward may be spent 60 blocks after its block
    wait(
        &mut c,
        240,
        "a dozen spendable block rewards",
        |s| match &s.wallet {
            WalletView::Unlocked(d) => {
                d.synced
                    && d.history
                        .iter()
                        .filter(|h| h.kind == EntryKind::Mined)
                        .count()
                        >= 72
            }
            _ => false,
        },
    );
    c.handle(Cmd::StopMiner);

    // a number that cannot be combined is refused before anything is built
    let ev = c.handle(Cmd::PrepareCombine {
        account: 0,
        coins: Some(1),
        level: FeeLevel::Low,
    });
    assert!(!errors(&ev).is_empty());
    assert!(c.snapshot().prepared.is_none());

    // three coins: a preview that says what it is, and nothing is sent by looking
    let ev = c.handle(Cmd::PrepareCombine {
        account: 0,
        coins: Some(3),
        level: FeeLevel::Normal,
    });
    assert!(errors(&ev).is_empty(), "{:?}", errors(&ev));
    let q = c.snapshot().prepared.expect("a combine waits for a yes");
    assert_eq!((q.transactions, q.coins), (1, 3));
    assert!(q.own.is_some() && q.to.is_empty());
    assert!(q.fee > 0 && q.amount > 0);
    assert_eq!((q.unsent_payments, q.unsent_total), (0, 0));
    assert!(unlocked(&c)
        .history
        .iter()
        .all(|h| !matches!(h.kind, EntryKind::Sent { .. })));
    c.handle(Cmd::CancelPrepared);
    assert!(c.snapshot().prepared.is_none());

    // all of them: the same, and then it is sent
    let ev = c.handle(Cmd::PrepareCombine {
        account: 0,
        coins: None,
        level: FeeLevel::Low,
    });
    assert!(errors(&ev).is_empty(), "{:?}", errors(&ev));
    let q = c.snapshot().prepared.expect("a combine waits for a yes");
    assert!(q.coins >= 3 && q.own.is_some(), "{} coins", q.coins);
    let (coins, fee) = (q.coins, q.fee);
    let ev = c.handle(Cmd::SendPrepared);
    assert!(errors(&ev).is_empty(), "{:?}", errors(&ev));
    assert!(ev
        .iter()
        .any(|e| matches!(e, Event::Sent { transactions: 1, fee: f, .. } if *f == fee)));
    assert!(c.snapshot().prepared.is_none());
    assert!(coins >= 3);

    // the node's own miner is stopped, so the pool holds it until a block comes: start the miner again for a moment
    c.handle(Cmd::StartMiner);
    wait(&mut c, 180, "the combine to be confirmed", |s| {
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
    c.handle(Cmd::StopMiner);
    let d = unlocked(&c);
    // the combined coin is not listed as money that arrived from someone
    assert!(
        d.history.iter().all(|h| h.kind != EntryKind::Received),
        "{:?}",
        d.history
    );
    let row = d
        .history
        .iter()
        .find(|h| matches!(h.kind, EntryKind::Sent { .. }))
        .unwrap();
    assert!(row
        .note
        .as_deref()
        .is_some_and(|n| n.starts_with("Combined")));
    c.handle(Cmd::StopNode);
}

// ---------------------------------------------------------------------------------------------------------------
// mining for a pool needs no node
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn a_miner_for_a_pool_starts_with_no_node_and_the_reasons_it_does_not_start_are_said() {
    let rig = Rig::new("poolmine", 18493);
    let mut c = rig.core();
    c.handle(Cmd::CreateWallet {
        name: None,
        password: Password::Set(pw("a long enough password")),
    });
    let mut s = c.snapshot().settings;
    s.mining_mode = tenero_gui::settings::MiningMode::Pool;
    c.handle(Cmd::SetSettings(Box::new(s.clone())));
    assert_eq!(
        c.snapshot().settings.mining_mode,
        tenero_gui::settings::MiningMode::Pool
    );
    // no pool is built in, so with nothing typed in it says so (and not "start the node")
    let ev = c.handle(Cmd::StartMiner);
    let e = errors(&ev);
    assert_eq!(e.len(), 1, "{e:?}");
    assert!(
        e[0].contains("no pool is built into this program"),
        "{}",
        e[0]
    );
    // a pool typed in needs its key
    s.pool = "127.0.0.1:1".into();
    c.handle(Cmd::SetSettings(Box::new(s.clone())));
    let ev = c.handle(Cmd::StartMiner);
    assert!(
        errors(&ev)[0].contains("needs its key"),
        "{:?}",
        errors(&ev)
    );
    // with a key it starts, although no node is running: the miner program tries the pool and keeps trying
    s.pool_key = "ab".repeat(32);
    c.handle(Cmd::SetSettings(Box::new(s)));
    assert!(matches!(c.snapshot().node, NodeView::Stopped));
    let ev = c.handle(Cmd::StartMiner);
    assert!(errors(&ev).is_empty(), "{:?}", errors(&ev));
    wait(&mut c, 60, "the miner's report", |s| {
        matches!(&s.miner, MinerView::Running { .. })
    });
    assert!(
        matches!(c.snapshot().node, NodeView::Stopped),
        "mining for a pool started no node"
    );
    c.handle(Cmd::StopMiner);
    assert!(matches!(c.snapshot().miner, MinerView::Off));
}

#[test]
fn a_view_only_wallet_is_made_from_a_view_key_and_refuses_what_needs_the_words_or_the_spend_key() {
    let rig = Rig::new("viewonly", 18484);
    let mut c = rig.core();
    let ev = c.handle(Cmd::CreateWallet {
        name: Some("Main".into()),
        password: Password::Set(pw("correct horse battery")),
    });
    assert!(errors(&ev).is_empty(), "{:?}", errors(&ev));
    let main_addr = unlocked(&c).accounts[0].address.clone();
    assert_eq!(unlocked(&c).tier, ViewTier::Full);
    let key_of = |c: &mut Core, received: bool| {
        c.handle(Cmd::RevealViewKey {
            password: pw("correct horse battery"),
            account: 0,
            received,
        })
        .iter()
        .find_map(|e| match e {
            Event::ViewKey { key, .. } => Some(key.to_string()),
            _ => None,
        })
        .unwrap()
    };
    let keys = [(false, key_of(&mut c, false)), (true, key_of(&mut c, true))];
    c.handle(Cmd::Lock);

    // a bad key is refused and nothing is written
    let ev = c.handle(Cmd::RestoreViewOnly {
        key: pw("TENview1notakey"),
        password: Password::None,
        name: Some("Bad".into()),
    });
    assert_eq!(errors(&ev).len(), 1);
    assert_eq!(c.snapshot().wallets.len(), 1);

    for (received, key) in keys {
        let name = if received { "Watch in" } else { "Watch all" };
        let ev = c.handle(Cmd::RestoreViewOnly {
            key: pw(&key),
            password: Password::Set(pw("watching password")),
            name: Some(name.into()),
        });
        assert!(errors(&ev).is_empty(), "{:?}", errors(&ev));
        let d = unlocked(&c);
        let tier = if received {
            ViewTier::ViewReceived
        } else {
            ViewTier::ViewAll
        };
        assert_eq!(d.tier, tier);
        assert_eq!(d.accounts.len(), 1);
        assert_eq!(
            d.accounts[0].address, main_addr,
            "the same account, watched"
        );

        // no words, no new accounts, no signatures: each refused with the reason
        let ev = c.handle(Cmd::RevealPhrase {
            password: pw("watching password"),
        });
        assert!(words_of(&ev).is_none());
        assert!(errors(&ev)[0].contains("view-only"), "{:?}", errors(&ev));
        let ev = c.handle(Cmd::AddAccount {
            label: "More".into(),
        });
        assert!(errors(&ev)[0].contains("view-only"), "{:?}", errors(&ev));
        let ev = c.handle(Cmd::SignMessage {
            account: 0,
            message: "hello".into(),
        });
        assert!(errors(&ev)[0].contains("view-only"), "{:?}", errors(&ev));
        assert_eq!(unlocked(&c).accounts.len(), 1);
        // what needs only the view key still works
        let ev = c.handle(Cmd::MakeIntegrated { account: 0 });
        assert!(errors(&ev).is_empty(), "{:?}", errors(&ev));
        let ev = c.handle(Cmd::RevealViewKey {
            password: pw("watching password"),
            account: 0,
            received: true,
        });
        assert!(
            errors(&ev).is_empty(),
            "either tier gives a view-received key"
        );

        // it stays view-only through its file
        c.handle(Cmd::Lock);
        assert!(errors(&c.handle(Cmd::Unlock {
            password: pw("watching password")
        }))
        .is_empty());
        assert_eq!(unlocked(&c).tier, tier);
        c.handle(Cmd::Lock);
    }
    assert_eq!(c.snapshot().wallets.len(), 3);
}
