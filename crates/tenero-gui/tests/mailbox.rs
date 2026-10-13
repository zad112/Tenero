//! What the worker sends the window while the window reads nothing (minimized for a night): the snapshots do not pile up.

use tenero_gui::backend::mailbox;
use tenero_gui::settings::Settings;
use tenero_gui::view::{Event, MinerView, NodeView, Snapshot, WalletView};

fn snapshot(busy: &str) -> Snapshot {
    Snapshot {
        settings: Settings::defaults(
            &std::env::temp_dir().join("tenero-gui-mailbox-test"),
            tenero_app::config::Network::Test,
        ),
        wallets: Vec::new(),
        wallet: WalletView::NoWallet,
        node: NodeView::Stopped,
        miner: MinerView::Off,
        prepared: None,
        busy: Some(busy.to_string()),
        moving: None,
    }
}

/// A night of snapshots (two a second for eight hours: what a mining wallet made while minimized) and a few notices among
/// them, read only at the end: the window gets every notice, in order, and ONE snapshot, the last. Up to 0.4.1 it got all
/// 57,600 snapshots, held in memory until then (about 20 GB with a long history).
#[test]
fn a_window_that_reads_nothing_for_a_night_gets_the_notices_and_only_the_latest_snapshot() {
    let (out, inbox) = mailbox();
    let n = 2 * 60 * 60 * 8;
    for i in 0..n {
        out.send(Event::Snapshot(Box::new(snapshot(&i.to_string()))))
            .unwrap();
        if i % 20_000 == 0 {
            out.send(Event::Notice(format!("notice {i}"))).unwrap();
        }
    }
    let mut got = Vec::new();
    while let Some(e) = inbox.try_recv() {
        got.push(e);
    }
    let notices: Vec<String> = got
        .iter()
        .filter_map(|e| match e {
            Event::Notice(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(notices, ["notice 0", "notice 20000", "notice 40000"]);
    let snapshots: Vec<&Snapshot> = got
        .iter()
        .filter_map(|e| match e {
            Event::Snapshot(s) => Some(&**s),
            _ => None,
        })
        .collect();
    assert_eq!(snapshots.len(), 1, "only the latest snapshot is kept");
    assert_eq!(
        snapshots[0].busy.as_deref(),
        Some((n - 1).to_string().as_str())
    );
    // and it comes after the notices of its change
    assert!(matches!(got.last(), Some(Event::Snapshot(_))));
    // nothing left once read; a new one is delivered again
    assert!(inbox.try_recv().is_none());
    out.send(Event::Snapshot(Box::new(snapshot("again"))))
        .unwrap();
    assert!(
        matches!(inbox.try_recv(), Some(Event::Snapshot(s)) if s.busy.as_deref() == Some("again"))
    );
}

/// Once the window is gone, a queued event says so (the worker ends on it); a snapshot is simply kept.
#[test]
fn sending_to_a_closed_window_is_an_error_for_queued_events() {
    let (out, inbox) = mailbox();
    drop(inbox);
    assert!(out.send(Event::Notice("x".into())).is_err());
    assert!(out.send(Event::Snapshot(Box::new(snapshot("x")))).is_ok());
}
