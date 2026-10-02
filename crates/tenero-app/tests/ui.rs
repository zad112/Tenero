//! The screen (M10.1): the exact text of every kind of line, so the output of the programs cannot drift. Golden tests are on purpose
//! literal: a change of wording is a change of this file, which a reviewer sees.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tenero_app::daemon::{dir_size, sync_rate, MiningShared};
use tenero_app::ui::{
    clock, color_enabled, error_event, format_amount, format_bytes, format_duration, group_digits,
    hint_for, miner_event_to_ui, render_banner, render_event, render_miner_block,
    render_miner_line, render_status_block, render_status_line, Banner, ColorChoice, Event,
    MinerStatus, MiningStatus, NodeLink, NodeStatus, Screen, Severity, SyncProgress, Theme,
    Verbosity, MAX_LINE,
};
use tenero_miner::gpu_stats::GpuReading;
use tenero_miner::rate::Rates;

const OFF: Theme = Theme { color: false };
const ON: Theme = Theme { color: true };

fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' && it.peek() == Some(&'[') {
            for d in it.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ---- numbers and times -----------------------------------------------------------------------------------------------------------

#[test]
fn numbers_get_thousands_separators() {
    for (n, s) in [
        (0, "0"),
        (7, "7"),
        (999, "999"),
        (1000, "1,000"),
        (1204, "1,204"),
        (12_345, "12,345"),
        (123_456, "123,456"),
        (1_234_567, "1,234,567"),
        (u64::MAX, "18,446,744,073,709,551,615"),
    ] {
        assert_eq!(group_digits(n), s);
    }
}

#[test]
fn durations_read_as_people_say_them() {
    for (secs, s) in [
        (0, "0s"),
        (41, "41s"),
        (59, "59s"),
        (60, "1m 00s"),
        (185, "3m 05s"),
        (3599, "59m 59s"),
        (3600, "1h 00m"),
        (7500, "2h 05m"),
        (86_399, "23h 59m"),
        (86_400, "1d 00h"),
        (3 * 86_400 + 4 * 3600 + 59, "3d 04h"),
    ] {
        assert_eq!(format_duration(secs), s, "{secs}");
    }
}

#[test]
fn sizes_use_binary_units_and_never_print_1024_of_a_unit() {
    for (b, s) in [
        (0, "0 B"),
        (512, "512 B"),
        (1023, "1023 B"),
        (1024, "1.0 KiB"),
        (1536, "1.5 KiB"),
        (1_048_575, "1.0 MiB"),
        (1_048_576, "1.0 MiB"),
        (12_900_000, "12.3 MiB"),
        (4 * 1024 * 1024 * 1024, "4.0 GiB"),
        (3 * 1024u64.pow(4), "3.0 TiB"),
        (5000 * 1024u64.pow(4), "5000.0 TiB"),
    ] {
        assert_eq!(format_bytes(b), s, "{b}");
    }
}

#[test]
fn amounts_carry_the_ticker_and_the_wallets_decimals() {
    assert_eq!(format_amount(0), "0 TNR");
    assert_eq!(format_amount(150_000_000), "1.5 TNR");
    assert_eq!(format_amount(1_230_000_000), "12.3 TNR");
    assert_eq!(format_amount(1), "0.00000001 TNR");
}

#[test]
fn the_clock_is_hours_minutes_seconds_of_the_utc_day() {
    assert_eq!(clock(0), "00:00:00");
    assert_eq!(clock(3661), "01:01:01");
    assert_eq!(clock(86_399), "23:59:59");
    assert_eq!(clock(86_400 + 5), "00:00:05");
    assert_eq!(clock(1_700_000_000), "22:13:20");
}

// ---- colour ----------------------------------------------------------------------------------------------------------------------

#[test]
fn colour_follows_the_user_then_no_color_then_the_terminal() {
    use ColorChoice::*;
    // (choice, is a terminal, NO_COLOR, CLICOLOR_FORCE, TERM) -> colour?
    let cases = [
        (Never, true, false, true, Some("xterm"), false),
        (Always, false, true, false, Some("dumb"), true),
        (Auto, true, false, false, Some("xterm-256color"), true),
        (Auto, true, false, false, None, true),
        (Auto, false, false, false, Some("xterm"), false),
        (Auto, true, true, false, Some("xterm"), false),
        (Auto, true, false, false, Some("dumb"), false),
        (Auto, false, false, true, Some("dumb"), true),
        (Auto, true, true, true, Some("xterm"), false),
    ];
    for (choice, tty, no_color, force, term, want) in cases {
        assert_eq!(
            color_enabled(choice, tty, no_color, force, term),
            want,
            "{choice:?} tty={tty} NO_COLOR={no_color} FORCE={force} TERM={term:?}"
        );
    }
    assert_eq!(ColorChoice::parse("auto"), Some(Auto));
    assert_eq!(ColorChoice::parse("always"), Some(Always));
    assert_eq!(ColorChoice::parse("never"), Some(Never));
    assert_eq!(ColorChoice::parse("yes"), None);
}

#[test]
fn colour_is_escape_codes_that_can_be_removed_and_off_is_the_plain_text() {
    assert_eq!(OFF.green("ok"), "ok");
    assert_eq!(ON.green("ok"), "\x1b[32mok\x1b[0m");
    assert_eq!(ON.red("x"), "\x1b[31mx\x1b[0m");
    assert_eq!(ON.yellow("x"), "\x1b[33mx\x1b[0m");
    assert_eq!(ON.bold("x"), "\x1b[1mx\x1b[0m");
    assert_eq!(ON.dim("x"), "\x1b[2mx\x1b[0m");
    assert_eq!(ON.cyan("x"), "\x1b[36mx\x1b[0m");
    assert_eq!(strip_ansi(&ON.red("a b")), "a b");
}

// ---- sync progress -----------------------------------------------------------------------------------------------------------------

#[test]
fn sync_progress_gives_percent_rate_and_time_left() {
    let p = SyncProgress {
        current: 1204,
        target: 5000,
        rate: Some(41.0),
    };
    assert_eq!(p.percent(), 24);
    assert_eq!(p.eta_secs(), Some(93)); // 3796 / 41 = 92.6, rounded up
    assert_eq!(
        p.describe(),
        "syncing 1,204 of 5,000 (24%) | 41 blocks/s | 1m 33s left"
    );
    // a slow rate keeps a decimal
    let slow = SyncProgress {
        current: 10,
        target: 20,
        rate: Some(0.5),
    };
    assert_eq!(
        slow.describe(),
        "syncing 10 of 20 (50%) | 0.5 blocks/s | 20s left"
    );
    // no rate yet
    let none = SyncProgress {
        current: 10,
        target: 20,
        rate: None,
    };
    assert_eq!(none.eta_secs(), None);
    assert_eq!(
        none.describe(),
        "syncing 10 of 20 (50%) | estimating the time left"
    );
    assert_eq!(
        SyncProgress {
            rate: Some(0.0),
            ..none
        }
        .eta_secs(),
        None,
        "a rate of nothing is no estimate"
    );
}

#[test]
fn the_percent_is_never_100_until_the_node_is_there() {
    let at = |current, target| SyncProgress {
        current,
        target,
        rate: None,
    };
    assert_eq!(at(0, 100).percent(), 0);
    assert_eq!(at(99, 100).percent(), 99);
    assert_eq!(at(9999, 10_000).percent(), 99);
    assert_eq!(at(100, 100).percent(), 100);
    assert_eq!(at(101, 100).percent(), 100);
    assert_eq!(at(0, 0).percent(), 100);
    assert_eq!(at(100, 100).eta_secs(), None);
}

// ---- the status block ----------------------------------------------------------------------------------------------------------------

fn status() -> NodeStatus {
    NodeStatus {
        height: 1204,
        tip: "a1b2c3d4".to_string(),
        last_block_age_secs: 41,
        peers_in: 2,
        peers_out: 3,
        out_groups: 3,
        sync: None,
        mempool: 7,
        uptime_secs: 7500,
        disk_bytes: Some(12_900_000),
        pruned_below: 0,
        alarms: vec![],
        mining: None,
    }
}

#[test]
fn the_status_block_of_a_node_in_sync() {
    let got = render_status_block(&status(), &OFF).join("\n");
    assert_eq!(
        got,
        "\
-- status ------------------------------------------------------------
  chain    height 1,204 (a1b2c3d4) | last block 41s ago
  sync     in sync
  peers    5 (in 2, out 3) | outbound in 3 network groups
  node     mempool 7 | up 2h 05m | disk 12.3 MiB
  alarms   none"
    );
}

#[test]
fn the_status_block_while_syncing_mining_pruned_and_with_alarms() {
    let mut s = status();
    s.sync = Some(SyncProgress {
        current: 1204,
        target: 5000,
        rate: Some(41.0),
    });
    s.pruned_below = 1000;
    s.out_groups = 1;
    s.alarms = vec!["stale-tip".into(), "few-outbound".into()];
    s.mining = Some(MiningStatus {
        backend: "cpu, 2 threads".into(),
        blocks_found: 3,
        blocks_accepted: 2,
        paused: true,
        rates: Rates::default(),
        gpu: None,
    });
    let got = render_status_block(&s, &OFF).join("\n");
    assert_eq!(
        got,
        "\
-- status ------------------------------------------------------------
  chain    height 1,204 (a1b2c3d4) | last block 41s ago
  sync     syncing 1,204 of 5,000 (24%) | 41 blocks/s | 1m 33s left
  peers    5 (in 2, out 3) | outbound in 1 network group
  node     mempool 7 | up 2h 05m | disk 12.3 MiB | pruned below 1,000
  mining   paused | 3 found, 2 in the chain
  backend  cpu, 2 threads
  hashrate (idle) starting
  alarms   stale-tip, few-outbound"
    );
    // a mining node that is running shows its rate when there is one
    s.mining = Some(MiningStatus {
        backend: "gpu".into(),
        blocks_found: 0,
        blocks_accepted: 0,
        paused: false,
        rates: busy(),
        gpu: None,
    });
    let got = render_status_block(&s, &OFF).join("\n");
    assert!(
        got.contains(
            "  mining   mining | 0 found, 0 in the chain
  backend  gpu
  hashrate 10s 1.23M | 60s 1.20M | 15m 1.10M | avg 1.15M attempts/s
"
        ),
        "{got}"
    );
}

#[test]
fn every_status_line_is_ascii_and_fits_an_80_column_window() {
    let mut s = status();
    s.sync = Some(SyncProgress {
        current: 123_456_789,
        target: 987_654_321,
        rate: Some(1234.0),
    });
    s.pruned_below = 123_456_789;
    s.alarms = vec![
        "stale-tip".into(),
        "behind-peers".into(),
        "network-ahead".into(),
        "few-outbound".into(),
        "few-groups".into(),
    ];
    s.mining = Some(MiningStatus {
        backend: "cpu, 6 threads".into(),
        blocks_found: 123_456,
        blocks_accepted: 123_456,
        paused: true,
        rates: Rates {
            s10: Some(99_999_999_999.0),
            s60: Some(99_999_999_999.0),
            m15: Some(99_999_999_999.0),
            average: Some(99_999_999_999.0),
            searching: true,
        },
        gpu: None,
    });
    for theme in [OFF, ON] {
        for l in render_status_block(&s, &theme) {
            let plain = strip_ansi(&l);
            assert!(plain.is_ascii(), "{plain}");
            assert!(plain.len() <= MAX_LINE, "{} long: {plain}", plain.len());
        }
    }
    assert_eq!(MAX_LINE, 78);
    assert!(render_status_line(&s).is_ascii());
}

#[test]
fn colour_in_the_status_block_changes_nothing_but_the_escape_codes() {
    let mut s = status();
    s.alarms = vec!["stale-tip".into()];
    s.sync = Some(SyncProgress {
        current: 1,
        target: 2,
        rate: None,
    });
    let plain = render_status_block(&s, &OFF);
    let colour = render_status_block(&s, &ON);
    assert_eq!(plain.len(), colour.len());
    for (p, c) in plain.iter().zip(&colour) {
        assert_eq!(&strip_ansi(c), p);
    }
    assert!(colour.iter().any(|l| l.contains("\x1b[")));
    // in sync is green, syncing is yellow, an alarm is red
    assert!(render_status_block(&status(), &ON)[2].contains("\x1b[32min sync\x1b[0m"));
    assert!(colour[2].contains("\x1b[33m"));
    assert!(colour.last().unwrap().contains("\x1b[31mstale-tip\x1b[0m"));
}

#[test]
fn the_plain_status_line_says_the_same_on_one_line() {
    assert_eq!(
        render_status_line(&status()),
        "height 1,204 (a1b2c3d4) | in sync | peers 5 (in 2, out 3) | mempool 7 | up 2h 05m"
    );
    let mut s = status();
    s.sync = Some(SyncProgress {
        current: 10,
        target: 20,
        rate: None,
    });
    s.alarms = vec!["stale-tip".into(), "few-groups".into()];
    s.mining = Some(MiningStatus {
        backend: "sha256".into(),
        blocks_found: 4,
        blocks_accepted: 3,
        paused: true,
        rates: Rates::default(),
        gpu: None,
    });
    assert_eq!(
        render_status_line(&s),
        "height 1,204 (a1b2c3d4) | syncing 10 of 20 (50%) | estimating the time left | peers 5 (in 2, out 3) | mempool 7 | up 2h 05m | mining sha256: 4 found, 3 in the chain (paused) | hashrate starting | ALARMS: stale-tip, few-groups"
    );
}

// ---- the banner -----------------------------------------------------------------------------------------------------------------------

#[test]
fn the_banner_says_what_this_is_and_what_it_is_not() {
    let b = Banner {
        role: "node".into(),
        version: "v0.0.0".into(),
        network: "test".into(),
        network_note: "SHA-256 test chain".into(),
        details: vec![
            "  data     C:\\data\\n1".into(),
            "  log      C:\\data\\n1.log".into(),
        ],
    };
    assert_eq!(
        render_banner(&b, &OFF).join("\n"),
        "\
TENERO node v0.0.0 | network: test (SHA-256 test chain)
EXPERIMENTAL and UNAUDITED. Nothing on this network has any value.
  data     C:\\data\\n1
  log      C:\\data\\n1.log
Times are UTC. Press Ctrl-C to stop cleanly."
    );
}

// ---- events -----------------------------------------------------------------------------------------------------------------------------

fn text(e: &Event) -> String {
    render_event(e, &OFF).join("\n")
}

#[test]
fn every_event_in_plain_words() {
    assert_eq!(
        text(&Event::Listening {
            p2p: Some("127.0.0.1:18331".into()),
            control: "127.0.0.1:18361".into()
        }),
        "listening for peers on 127.0.0.1:18331; control on 127.0.0.1:18361"
    );
    assert_eq!(
        text(&Event::Listening {
            p2p: None,
            control: "127.0.0.1:18361".into()
        }),
        "not listening for peers (outbound only); control on 127.0.0.1:18361"
    );
    assert_eq!(
        text(&Event::BlockMined {
            height: 1204,
            secs: 0.4,
            reward: Some(1_230_000_000)
        }),
        "block 1,204 mined in 0.4 s, reward 12.3 TNR"
    );
    assert_eq!(
        text(&Event::BlockMined {
            height: 5,
            secs: 12.0,
            reward: None
        }),
        "block 5 mined in 12.0 s"
    );
    assert_eq!(
        text(&Event::BlockLostRace { height: 1204 }),
        "block 1,204 was found but another block won the race: no reward for it"
    );
    assert_eq!(
        text(&Event::BlockRefused {
            height: 1204,
            why: "bad proof of work".into()
        }),
        "block 1,204 was refused by the node: bad proof of work\n  what to do: this should not happen; keep the log file and report it"
    );
    assert_eq!(
        text(&Event::Synced { height: 5000 }),
        "synced: the chain is up to date at height 5,000"
    );
    assert_eq!(
        text(&Event::AlarmBegan("no new block for 10 minutes".into())),
        "WARNING: no new block for 10 minutes"
    );
    assert_eq!(
        text(&Event::AlarmEnded("stale-tip".into())),
        "alarm ended: stale-tip"
    );
    assert_eq!(
        text(&Event::MiningPaused),
        "mining paused: the node is syncing"
    );
    assert_eq!(text(&Event::MiningResumed), "mining resumed");
    assert_eq!(
        text(&Event::Connected("the node".into())),
        "connected to the node"
    );
    assert_eq!(
        text(&Event::Lost("the node".into())),
        "lost the node; trying again"
    );
    assert_eq!(
        text(&Event::Warn("pool not saved".into())),
        "warning: pool not saved"
    );
    assert_eq!(text(&Event::ShuttingDown), "shutting down...");
    assert_eq!(
        text(&Event::Stopped {
            height: 1204,
            tip: "a1b2c3d4".into()
        }),
        "stopped at height 1,204 (a1b2c3d4)"
    );
}

#[test]
fn an_error_says_what_to_do_when_there_is_something_to_say() {
    assert_eq!(
        text(&Event::Error {
            what: "disk full".into(),
            hint: None
        }),
        "error: disk full"
    );
    assert_eq!(
        text(&Event::Error {
            what: "disk full".into(),
            hint: Some("free some space".into())
        }),
        "error: disk full\n  what to do: free some space"
    );
    // colour marks errors red but the words are the same
    assert_eq!(
        render_event(
            &Event::Error {
                what: "x".into(),
                hint: None
            },
            &ON
        )[0],
        "\x1b[31merror: x\x1b[0m"
    );
}

#[test]
fn events_have_a_severity_and_every_event_line_is_ascii() {
    use Severity::*;
    let all = [
        (
            Event::Listening {
                p2p: None,
                control: "c".into(),
            },
            Info,
        ),
        (
            Event::BlockMined {
                height: 1,
                secs: 1.0,
                reward: None,
            },
            Good,
        ),
        (Event::BlockLostRace { height: 1 }, Warn),
        (
            Event::BlockRefused {
                height: 1,
                why: "w".into(),
            },
            Error,
        ),
        (Event::Synced { height: 1 }, Info),
        (Event::AlarmBegan("a".into()), Warn),
        (Event::AlarmEnded("a".into()), Info),
        (Event::MiningPaused, Warn),
        (Event::MiningResumed, Info),
        (Event::Connected("n".into()), Info),
        (Event::Lost("n".into()), Warn),
        (Event::Warn("w".into()), Warn),
        (
            Event::Error {
                what: "e".into(),
                hint: None,
            },
            Error,
        ),
        (Event::ShuttingDown, Info),
        (
            Event::Stopped {
                height: 1,
                tip: "t".into(),
            },
            Info,
        ),
    ];
    for (e, sev) in all {
        assert_eq!(e.severity(), sev, "{e:?}");
        for l in render_event(&e, &OFF) {
            assert!(l.is_ascii(), "{l}");
        }
    }
    assert!(Info < Good && Good < Warn && Warn < Error);
}

#[test]
fn the_common_failures_come_with_what_to_do() {
    let h = |e: &str| hint_for(e).unwrap_or_default();
    assert!(h("cannot listen: Address already in use (os error 98)")
        .contains("different `listen` or `control`"));
    assert!(h("cannot listen: Only one usage of each socket address (protocol/network address/port) is normally permitted. (os error 10048)").contains("using that port"));
    assert!(h("cannot open the chain database: the process cannot access the file because it is being used by another process").contains("another node may be running"));
    assert!(h("Database already open. Cannot acquire lock.").contains("tenerod stop"));
    assert!(h("cannot read the cookie file: not found").contains("start the node first"));
    assert!(
        h("cannot reach the node: Connection refused (os error 111)")
            .contains("is the node running?")
    );
    assert!(h("--address holds an invalid key").contains("tni1"));
    assert!(h("this store belongs to a different chain").contains("another network"));
    // nothing useful to say: nothing is made up
    assert_eq!(hint_for("something nobody has seen before"), None);
    assert_eq!(hint_for(""), None);
    // an error event looks its hint up
    match error_event("cannot listen: Address already in use") {
        Event::Error { what, hint } => {
            assert_eq!(what, "cannot listen: Address already in use");
            assert!(hint.is_some());
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        error_event("odd"),
        Event::Error { hint: None, .. }
    ));
}

// ---- the screen itself ----------------------------------------------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Buf {
    fn take(&self) -> String {
        String::from_utf8(std::mem::take(&mut *self.0.lock().unwrap())).unwrap()
    }
}

fn screen(interactive: bool, verbosity: Verbosity, buf: &Buf, clock: &Arc<AtomicU64>) -> Screen {
    let c = Arc::clone(clock);
    Screen::new(
        Box::new(buf.clone()),
        interactive,
        OFF,
        verbosity,
        60,
        Box::new(move || c.load(Ordering::SeqCst)),
    )
}

#[test]
fn on_a_terminal_the_status_block_is_redrawn_in_place_and_events_go_above_it() {
    let (buf, clock) = (Buf::default(), Arc::new(AtomicU64::new(0)));
    let s = screen(true, Verbosity::Normal, &buf, &clock);
    let n = render_status_block(&status(), &OFF).len();
    assert_eq!(n, 6);
    // the first block is just drawn
    s.status(&status());
    let first = buf.take();
    assert_eq!(
        first,
        render_status_block(&status(), &OFF).join("\n") + "\n"
    );
    // the next one first moves the cursor up over the old one and clears
    let mut s2 = status();
    s2.height = 1205;
    s.status(&s2);
    let second = buf.take();
    assert_eq!(
        second,
        format!(
            "\x1b[6A\r\x1b[J{}\n",
            render_status_block(&s2, &OFF).join("\n")
        )
    );
    // an event is written where the block was, and the block is drawn again below it: no time in front (the terminal has its own)
    s.event(&Event::ShuttingDown);
    let third = buf.take();
    assert_eq!(
        third,
        format!(
            "\x1b[6A\r\x1b[Jshutting down...\n{}\n",
            render_status_block(&s2, &OFF).join("\n")
        )
    );
    assert!(s.is_interactive());
}

#[test]
fn off_a_terminal_everything_is_plain_lines_with_the_time_and_status_comes_once_a_minute() {
    let (buf, clock) = (Buf::default(), Arc::new(AtomicU64::new(1_700_000_000)));
    let s = screen(false, Verbosity::Normal, &buf, &clock);
    s.event(&Event::MiningResumed);
    assert_eq!(buf.take(), "22:13:20  mining resumed\n");
    s.status(&status());
    assert_eq!(
        buf.take(),
        "22:13:20  status: height 1,204 (a1b2c3d4) | in sync | peers 5 (in 2, out 3) | mempool 7 | up 2h 05m\n"
    );
    // not again until a minute has passed
    clock.store(1_700_000_059, Ordering::SeqCst);
    s.status(&status());
    assert_eq!(buf.take(), "");
    clock.store(1_700_000_060, Ordering::SeqCst);
    s.status(&status());
    assert!(buf.take().starts_with("22:14:20  status: height 1,204"));
    // a two-line event has the time on both lines
    s.event(&Event::Error {
        what: "x".into(),
        hint: Some("y".into()),
    });
    assert_eq!(
        buf.take(),
        "22:14:20  error: x\n22:14:20    what to do: y\n"
    );
    assert!(!s.is_interactive());
}

#[test]
fn quiet_shows_only_warnings_and_errors_and_nothing_else() {
    let (buf, clock) = (Buf::default(), Arc::new(AtomicU64::new(0)));
    let s = screen(false, Verbosity::Quiet, &buf, &clock);
    let banner = Banner {
        role: "node".into(),
        version: "v".into(),
        network: "test".into(),
        network_note: "n".into(),
        details: vec![],
    };
    s.banner(&banner);
    s.status(&status());
    s.event(&Event::Synced { height: 1 });
    s.event(&Event::BlockMined {
        height: 1,
        secs: 1.0,
        reward: None,
    });
    s.event(&Event::Listening {
        p2p: None,
        control: "c".into(),
    });
    s.detail("a line of the log");
    assert_eq!(buf.take(), "");
    s.event(&Event::Warn("w".into()));
    s.event(&Event::BlockLostRace { height: 2 });
    s.event(&Event::AlarmBegan("a".into()));
    s.event(&Event::Error {
        what: "e".into(),
        hint: None,
    });
    let out = buf.take();
    assert_eq!(out.lines().count(), 4, "{out}");
    assert!(out.contains("warning: w") && out.contains("error: e") && out.contains("WARNING: a"));
}

#[test]
fn verbose_adds_the_lines_of_the_log_and_the_others_do_not() {
    let (buf, clock) = (Buf::default(), Arc::new(AtomicU64::new(1_700_000_000)));
    let normal = screen(false, Verbosity::Normal, &buf, &clock);
    normal.detail("2026-10-02T09:05:03Z INFO  peer 3 connected to 127.0.0.4:18331");
    assert_eq!(buf.take(), "");
    let verbose = screen(false, Verbosity::Verbose, &buf, &clock);
    verbose.detail("2026-10-02T09:05:03Z INFO  peer 3 connected to 127.0.0.4:18331");
    assert_eq!(
        buf.take(),
        "2026-10-02T09:05:03Z INFO  peer 3 connected to 127.0.0.4:18331\n"
    );
}

#[test]
fn the_banner_is_written_once_in_full_and_not_when_quiet() {
    let (buf, clock) = (Buf::default(), Arc::new(AtomicU64::new(0)));
    let s = screen(true, Verbosity::Normal, &buf, &clock);
    let banner = Banner {
        role: "miner".into(),
        version: "v0.0.0".into(),
        network: "dev".into(),
        network_note: "matmulhash".into(),
        details: vec![],
    };
    s.banner(&banner);
    let out = buf.take();
    assert_eq!(out.lines().count(), 3, "{out}");
    assert!(out.starts_with("TENERO miner v0.0.0 | network: dev (matmulhash)\n"));
    assert!(out.contains("EXPERIMENTAL and UNAUDITED"));
}

#[test]
fn many_threads_writing_at_once_never_mix_their_lines() {
    let (buf, clock) = (Buf::default(), Arc::new(AtomicU64::new(0)));
    let s = Arc::new(screen(false, Verbosity::Normal, &buf, &clock));
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let s = Arc::clone(&s);
            std::thread::spawn(move || {
                for i in 0..50 {
                    s.event(&Event::Warn(format!("thread {t} line {i}")));
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let out = buf.take();
    assert_eq!(out.lines().count(), 400);
    for l in out.lines() {
        assert!(l.starts_with("00:00:00  warning: thread "), "{l}");
        assert!(l.split(" line ").count() == 2, "{l}");
    }
}

#[test]
fn a_line_that_is_too_long_is_cut_with_dots_and_colour_codes_take_no_room() {
    let mut s = status();
    s.sync = Some(SyncProgress {
        current: 123_456_789,
        target: 987_654_321,
        rate: Some(1234.0),
    });
    let plain = render_status_block(&s, &OFF);
    let sync = &plain[2];
    assert!(sync.len() <= MAX_LINE, "{sync}");
    assert!(sync.ends_with("..."), "{sync}");
    assert!(
        sync.starts_with("  sync     syncing 123,456,789 of 987,654,321 (12%) | 1234 blocks/s"),
        "{sync}"
    );
    // the same line in colour is cut at the same place of the text, and the colour is closed
    let coloured = &render_status_block(&s, &ON)[2];
    assert_eq!(&strip_ansi(coloured), sync);
    assert!(coloured.contains("\x1b[0m..."), "{coloured:?}");
    // a short line in colour is not cut however many codes it holds
    assert_eq!(
        strip_ansi(&render_status_block(&status(), &ON)[2]),
        "  sync     in sync"
    );
}

// ---- the miner program's screen ----------------------------------------------------------------------------------------------------------

/// Rates of a miner that has been searching for a while.
fn busy() -> Rates {
    Rates {
        s10: Some(1_234_567.0),
        s60: Some(1_200_000.0),
        m15: Some(1_100_000.0),
        average: Some(1_150_000.0),
        searching: true,
    }
}

fn miner_status() -> MinerStatus {
    MinerStatus {
        backend: "gpu: NVIDIA RTX 5070 Ti".to_string(),
        link: NodeLink::Connected,
        node_height: 1204,
        rates: busy(),
        gpu: None,
        found: 4,
        accepted: 3,
        lost_race: 1,
        refused: 0,
        uptime_secs: 7500,
    }
}

#[test]
fn the_miners_status_block_in_each_state_of_its_link_to_the_node() {
    let got = render_miner_block(&miner_status(), &OFF).join(
        "
",
    );
    assert_eq!(
        got,
        "-- status ------------------------------------------------------------
  node     connected (height 1,204)
  mining   gpu: NVIDIA RTX 5070 Ti
  hashrate 10s 1.23M | 60s 1.20M | 15m 1.10M | avg 1.15M attempts/s
  blocks   4 found | 3 in the chain | 1 lost a race | 0 refused
  up       2h 05m"
    );
    let mut s = miner_status();
    s.link = NodeLink::Syncing;
    s.rates = Rates::default();
    let got = render_miner_block(&s, &OFF);
    assert_eq!(got[1], "  node     syncing (height 1,204): mining waits");
    assert_eq!(got[2], "  mining   gpu: NVIDIA RTX 5070 Ti");
    assert_eq!(got[3], "  hashrate (idle) starting");
    s.link = NodeLink::Down;
    assert_eq!(
        render_miner_block(&s, &OFF)[1],
        "  node     not reachable: trying again"
    );
    assert_eq!(
        NodeLink::default(),
        NodeLink::Down,
        "until it has heard from the node it is not connected"
    );
    // colour marks the link and nothing else
    let c = render_miner_block(&miner_status(), &ON);
    assert!(c[1].contains("\x1b[32mconnected (height 1,204)\x1b[0m"));
    for (a, b) in render_miner_block(&miner_status(), &OFF).iter().zip(&c) {
        assert_eq!(&strip_ansi(b), a);
    }
    for l in render_miner_block(&s, &OFF) {
        assert!(l.is_ascii() && l.len() <= MAX_LINE, "{l}");
    }
}

#[test]
fn the_miners_plain_status_line() {
    assert_eq!(
        render_miner_line(&miner_status()),
        "node connected (height 1,204) | gpu: NVIDIA RTX 5070 Ti | hashrate 10s 1.23M | 60s 1.20M | 15m 1.10M | avg 1.15M attempts/s | blocks: 4 found, 3 in the chain, 1 lost a race, 0 refused | up 2h 05m"
    );
    let mut s = miner_status();
    s.link = NodeLink::Syncing;
    s.rates = Rates::default();
    assert!(render_miner_line(&s).starts_with("node syncing (height 1,204), mining waits | gpu"));
    assert!(render_miner_line(&s).contains("| hashrate starting |"));
    s.link = NodeLink::Down;
    assert!(render_miner_line(&s).starts_with("node not reachable | "));
}

#[test]
fn a_plain_note_is_shown_as_it_is_and_is_not_a_warning() {
    assert_eq!(
        text(&Event::Info("stopped: 4 blocks found".into())),
        "stopped: 4 blocks found"
    );
    assert_eq!(Event::Info("x".into()).severity(), Severity::Info);
}

#[test]
fn a_miner_event_becomes_the_event_the_screen_shows() {
    use tenero_miner::MinerEvent as M;
    assert_eq!(
        miner_event_to_ui(&M::Started {
            backend: "b".into()
        }),
        None
    );
    assert_eq!(
        miner_event_to_ui(&M::InChain {
            height: 9,
            secs: 0.4,
            reward: 5
        }),
        Some(Event::BlockMined {
            height: 9,
            secs: 0.4,
            reward: Some(5)
        })
    );
    assert_eq!(
        miner_event_to_ui(&M::LostRace { height: 9 }),
        Some(Event::BlockLostRace { height: 9 })
    );
    assert!(matches!(
        miner_event_to_ui(&M::Refused { height: 9 }),
        Some(Event::BlockRefused { height: 9, .. })
    ));
    assert_eq!(miner_event_to_ui(&M::Paused), Some(Event::MiningPaused));
    assert_eq!(miner_event_to_ui(&M::Resumed), Some(Event::MiningResumed));
    assert_eq!(
        miner_event_to_ui(&M::NodeConnected),
        Some(Event::Connected("the node".into()))
    );
    assert_eq!(
        miner_event_to_ui(&M::NodeLost {
            why: "secret detail".into()
        }),
        Some(Event::Lost("the node".into()))
    );
    match miner_event_to_ui(&M::BackendFailed {
        why: "no GPU found".into(),
    }) {
        Some(Event::Error { what, .. }) => assert!(what.contains("no GPU found"), "{what}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_shared_counters_count_what_the_miner_reports() {
    use std::sync::atomic::Ordering::Relaxed;
    use tenero_miner::MinerEvent as M;
    let m = MiningShared::default();
    for e in [
        M::Started {
            backend: "cpu, 2 threads".into(),
        },
        M::InChain {
            height: 1,
            secs: 0.1,
            reward: 1,
        },
        M::InChain {
            height: 2,
            secs: 0.1,
            reward: 1,
        },
        M::LostRace { height: 3 },
        M::Refused { height: 4 },
        M::Paused,
        M::NodeConnected,
        M::NodeLost { why: "x".into() },
        M::BackendFailed { why: "y".into() },
    ] {
        m.record(&e);
    }
    assert_eq!(m.backend.lock().unwrap().as_str(), "cpu, 2 threads");
    assert_eq!(
        (
            m.found.load(Relaxed),
            m.accepted.load(Relaxed),
            m.lost_race.load(Relaxed),
            m.refused.load(Relaxed)
        ),
        (4, 2, 1, 1)
    );
    assert!(m.paused.load(Relaxed));
    m.record(&M::Resumed);
    assert!(!m.paused.load(Relaxed));
}

#[test]
fn a_miner_status_is_redrawn_in_place_like_the_nodes() {
    let (buf, clock) = (Buf::default(), Arc::new(AtomicU64::new(0)));
    let s = screen(true, Verbosity::Normal, &buf, &clock);
    s.miner_status(&miner_status());
    let first = buf.take();
    assert_eq!(
        first,
        render_miner_block(&miner_status(), &OFF).join(
            "
"
        ) + "
"
    );
    let mut next = miner_status();
    next.found = 5;
    s.miner_status(&next);
    assert!(buf.take().starts_with("\x1b[6A\r\x1b[J"));
    // plain: one line, then silence until the interval is over
    let (buf, clock) = (Buf::default(), Arc::new(AtomicU64::new(1_700_000_000)));
    let p = screen(false, Verbosity::Normal, &buf, &clock);
    p.miner_status(&miner_status());
    assert!(buf
        .take()
        .starts_with("22:13:20  status: node connected (height 1,204)"));
    p.miner_status(&miner_status());
    assert_eq!(buf.take(), "");
}

#[test]
fn the_sync_rate_needs_five_seconds_of_samples_and_progress() {
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};
    let t0 = Instant::now();
    let at = |secs: u64, h: u64| (t0 + Duration::from_secs(secs), h);
    let q = |v: &[(Instant, u64)]| v.iter().copied().collect::<VecDeque<_>>();
    assert_eq!(sync_rate(&q(&[])), None);
    assert_eq!(sync_rate(&q(&[at(0, 100)])), None);
    // under five seconds: too soon to say
    assert_eq!(sync_rate(&q(&[at(0, 100), at(4, 500)])), None);
    // five seconds: blocks a second between the first and the last
    assert_eq!(sync_rate(&q(&[at(0, 100), at(5, 600)])), Some(100.0));
    assert_eq!(
        sync_rate(&q(&[at(0, 100), at(2, 300), at(10, 1100)])),
        Some(100.0)
    );
    // no progress, or going backwards (a reorganisation): no rate
    assert_eq!(sync_rate(&q(&[at(0, 100), at(10, 100)])), None);
    assert_eq!(sync_rate(&q(&[at(0, 100), at(10, 90)])), None);
}

#[test]
fn the_size_of_a_data_folder_counts_the_files_in_it_and_below_it() {
    let d = std::env::temp_dir().join(format!("tenero-dirsize-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    assert_eq!(dir_size(&d), 0, "a folder that is not there is empty");
    std::fs::create_dir_all(d.join("a/b/c")).unwrap();
    std::fs::write(d.join("one"), vec![0u8; 1000]).unwrap();
    std::fs::write(d.join("a/two"), vec![0u8; 200]).unwrap();
    std::fs::write(d.join("a/b/c/three"), vec![0u8; 30]).unwrap();
    assert_eq!(dir_size(&d), 1230);
    // very deep folders are not followed for ever
    let mut deep = d.join("a/b/c");
    for i in 0..10 {
        deep = deep.join(format!("d{i}"));
    }
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("far"), vec![0u8; 5]).unwrap();
    assert_eq!(
        dir_size(&d),
        1230,
        "the file ten folders down is not counted"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_sync_rate_of_ten_or_more_is_whole_blocks_and_below_ten_has_a_decimal() {
    let at = |rate| SyncProgress {
        current: 0,
        target: 1000,
        rate: Some(rate),
    };
    assert!(
        at(10.0).describe().contains("| 10 blocks/s |"),
        "{}",
        at(10.0).describe()
    );
    assert!(
        at(9.9).describe().contains("| 9.9 blocks/s |"),
        "{}",
        at(9.9).describe()
    );
    assert!(at(100.4).describe().contains("| 100 blocks/s |"));
}

#[test]
fn the_clock_counts_minutes_in_sixties() {
    assert_eq!(clock(59 * 60), "00:59:00");
    assert_eq!(clock(23 * 3600 + 59 * 60 + 59), "23:59:59");
}

#[test]
fn a_line_of_exactly_the_limit_is_not_cut_and_one_more_is() {
    // "  mining   " (11) + backend
    let mut s = miner_status();
    s.backend = "b".repeat(MAX_LINE - 11);
    let line = &render_miner_block(&s, &OFF)[2];
    assert_eq!(line.len(), MAX_LINE);
    assert!(!line.ends_with("..."), "{line}");
    s.backend.push('b');
    let line = &render_miner_block(&s, &OFF)[2];
    assert_eq!(line.len(), MAX_LINE);
    assert!(line.ends_with("..."), "{line}");
}

#[test]
fn an_unreachable_node_is_red_in_the_miners_block() {
    let mut s = miner_status();
    s.link = NodeLink::Down;
    assert!(render_miner_block(&s, &ON)[1].contains("\x1b[31mnot reachable: trying again\x1b[0m"));
}

#[test]
fn a_failure_to_listen_alone_gets_the_port_hint() {
    assert!(hint_for("cannot listen: no way").unwrap().contains("port"));
}

#[test]
fn the_rates_in_words() {
    use tenero_app::ui::rates_text;
    assert_eq!(rates_text(&Rates::default()), "starting");
    let part = Rates {
        s10: Some(5.4),
        average: Some(7.6),
        ..Rates::default()
    };
    assert_eq!(rates_text(&part), "10s 5 | 60s - | 15m - | avg 8");
    assert_eq!(
        rates_text(&busy()),
        "10s 1.23M | 60s 1.20M | 15m 1.10M | avg 1.15M"
    );
}

#[test]
fn a_miner_that_is_not_searching_says_so_on_its_rate_row_and_one_that_is_does_not() {
    let mut s = miner_status();
    assert!(render_miner_block(&s, &OFF)[3].starts_with("  hashrate 10s 1.23M"));
    s.rates.searching = false;
    assert!(render_miner_block(&s, &OFF)[3]
        .starts_with("  hashrate (idle) 10s 1.23M | 60s 1.20M | 15m 1.10M"));
    for l in render_miner_block(&s, &OFF) {
        assert!(l.len() <= MAX_LINE, "{l}");
    }
}

#[test]
fn the_shared_counters_turn_the_backends_counters_into_rates_with_each_look() {
    use std::sync::atomic::Ordering;
    let shared = MiningShared::default();
    assert_eq!(shared.status().rates, Rates::default(), "no counters yet");
    assert_eq!(
        shared.status_at(0).rates,
        Rates::default(),
        "nothing to look at: harmless"
    );
    let c = Arc::new(tenero_miner::Counters::default());
    c.in_job.store(true, Ordering::SeqCst);
    *shared.counters.lock().unwrap() = Some(Arc::clone(&c));
    for sec in 0..=12u64 {
        c.attempts.store(sec * 1000, Ordering::Relaxed);
        shared.status_at(sec * 1000);
    }
    let r = shared.status().rates;
    assert!(r.searching);
    assert!((r.s10.unwrap() - 1000.0).abs() < 1.0, "{r:?}");
    assert!(r.s60.is_none());
    assert!((r.average.unwrap() - 1000.0).abs() < 1.0);
    // the time is in milliseconds (a status a second apart is a second apart)
    assert!((shared.status_at(13_000).rates.average.unwrap() - 923.0).abs() < 1.0);
    // not in a job: not searching, and what the counter does meanwhile is not counted
    c.in_job.store(false, Ordering::SeqCst);
    c.attempts.store(1_000_000, Ordering::Relaxed);
    shared.status_at(14_000);
    c.attempts.store(2_000_000, Ordering::Relaxed);
    let r = shared.status_at(15_000).rates;
    assert!(!r.searching);
    assert!(r.average.unwrap() < 1000.0, "{r:?}");
}

#[test]
fn the_rates_in_words_when_only_some_windows_have_a_figure() {
    use tenero_app::ui::rates_text;
    let only = |r: Rates| rates_text(&r);
    assert_eq!(
        only(Rates {
            average: Some(9.0),
            ..Rates::default()
        }),
        "10s - | 60s - | 15m - | avg 9"
    );
    assert_eq!(
        only(Rates {
            m15: Some(9.0),
            ..Rates::default()
        }),
        "10s - | 60s - | 15m 9 | avg -"
    );
    assert_eq!(
        only(Rates {
            s60: Some(9.0),
            ..Rates::default()
        }),
        "10s - | 60s 9 | 15m - | avg -"
    );
}

#[test]
fn the_nodes_plain_status_line_carries_the_rates_of_a_miner_that_has_them() {
    let mut s = status();
    s.mining = Some(MiningStatus {
        backend: "gpu".into(),
        blocks_found: 1,
        blocks_accepted: 1,
        paused: false,
        rates: busy(),
        gpu: None,
    });
    let line = render_status_line(&s);
    assert!(
        line.contains("| mining gpu: 1 found, 1 in the chain | hashrate 10s 1.23M | 60s 1.20M | 15m 1.10M | avg 1.15M attempts/s"),
        "{line}"
    );
}

fn card() -> GpuReading {
    GpuReading {
        name: Some("NVIDIA GeForce RTX 5070 Ti".into()),
        temp_c: Some(68),
        power_w: Some(211.6),
        fan_pct: Some(54),
        core_mhz: Some(2625),
        mem_mhz: Some(14001),
        mem_used_mib: Some(9300),
        mem_total_mib: Some(16303),
        busy_pct: Some(99),
        mem_busy_pct: Some(94),
        limited_by: None,
    }
}

#[test]
fn the_miners_block_shows_the_card_and_says_the_memory_reads_are_an_estimate() {
    use tenero_app::ui::gpu_rows;
    // 35,000 attempts a second of 16 MiB slices is 587 GB/s
    let mut r = busy();
    r.s10 = Some(35_000.0);
    assert_eq!(
        gpu_rows(&card(), &r, &OFF),
        vec![
            "  gpu      68 C | 212 W | fan 54% | core 2,625 MHz | mem 14,001 MHz",
            "  memory   9.1 of 15.9 GiB used | controller busy 94%",
            "  reads    ~587 GB/s implied by the rate (an estimate, not measured)",
        ]
    );
    // a made-up speed shows the arithmetic and the commas: 1,234,567 a second is 20,713 GB/s
    assert!(gpu_rows(&card(), &busy(), &OFF)[2].starts_with("  reads    ~20,713 GB/s"));
}

#[test]
fn what_the_card_does_not_report_is_left_out_and_no_reading_is_no_rows() {
    use tenero_app::ui::gpu_rows;
    assert!(gpu_rows(&GpuReading::default(), &Rates::default(), &OFF).is_empty());
    let only_temp = GpuReading {
        temp_c: Some(40),
        ..GpuReading::default()
    };
    assert_eq!(
        gpu_rows(&only_temp, &Rates::default(), &OFF),
        vec!["  gpu      40 C"]
    );
    let no_fan = GpuReading {
        fan_pct: None,
        ..card()
    };
    assert!(!gpu_rows(&no_fan, &busy(), &OFF)[0].contains("fan"));
    // no rate yet: no estimate, and the rest of the memory row stays
    let rows = gpu_rows(&card(), &Rates::default(), &OFF);
    assert_eq!(
        rows[1],
        "  memory   9.1 of 15.9 GiB used | controller busy 94%"
    );
    // the average is used when the 10 s window is not there yet
    let avg_only = Rates {
        average: Some(35_000.0),
        ..Rates::default()
    };
    assert!(gpu_rows(&card(), &avg_only, &OFF)[2].starts_with("  reads    ~587 GB/s"));
}

#[test]
fn a_card_held_back_by_the_driver_says_so_in_yellow() {
    use tenero_app::ui::gpu_rows;
    let hot = GpuReading {
        limited_by: Some("temperature"),
        ..card()
    };
    let rows = gpu_rows(&hot, &busy(), &OFF);
    assert_eq!(
        rows[1],
        "  limited  the driver is holding the clocks down: temperature"
    );
    assert!(gpu_rows(&hot, &busy(), &ON)[1].contains("[33mthe driver is holding"));
    assert_eq!(
        gpu_rows(&card(), &busy(), &OFF).len(),
        3,
        "not limited: no such row"
    );
}

#[test]
fn the_miner_block_and_both_plain_lines_carry_the_card_when_there_is_a_reading() {
    let mut s = miner_status();
    s.gpu = Some(card());
    let block = render_miner_block(&s, &OFF);
    assert_eq!(block.len(), 9);
    assert!(block[4].starts_with("  gpu      68 C"));
    assert!(block[5].starts_with("  memory   9.1 of 15.9 GiB"));
    assert!(block[6].starts_with("  reads    ~20,713 GB/s"));
    assert!(block[7].starts_with("  blocks"));
    for l in &block {
        assert!(l.is_ascii() && l.len() <= MAX_LINE, "{l}");
    }
    assert!(render_miner_line(&s).contains("| hashrate 10s 1.23M"));
    assert!(
        render_miner_line(&s).contains(" | gpu 68 C, 212 W | blocks:"),
        "{}",
        render_miner_line(&s)
    );
    s.gpu = Some(GpuReading {
        limited_by: Some("power cap"),
        ..card()
    });
    assert!(render_miner_line(&s).contains(" | gpu 68 C, 212 W, limited by power cap | blocks:"));
    s.gpu = None;
    assert_eq!(
        render_miner_block(&s, &OFF).len(),
        6,
        "no card: the rows of before"
    );
    // the node's block
    let mut n = status();
    n.mining = Some(MiningStatus {
        backend: "gpu".into(),
        blocks_found: 1,
        blocks_accepted: 1,
        paused: false,
        rates: busy(),
        gpu: Some(card()),
    });
    let block = render_status_block(&n, &OFF);
    let at = block
        .iter()
        .position(|l| l.starts_with("  gpu "))
        .expect("a gpu row");
    assert!(block[at - 1].starts_with("  hashrate"));
    assert!(block[at + 1].starts_with("  memory"));
    assert!(block[at + 2].starts_with("  reads"));
    assert!(render_status_line(&n).contains(
        "| hashrate 10s 1.23M | 60s 1.20M | 15m 1.10M | avg 1.15M attempts/s | gpu 68 C, 212 W"
    ));
}

#[test]
fn the_shared_counters_hand_on_the_last_reading_of_the_card() {
    let shared = MiningShared::default();
    assert!(shared.status().gpu.is_none(), "no card, no reading");
    *shared.gpu.lock().unwrap() = Some(card());
    assert_eq!(shared.status().gpu, Some(card()));
    // with no probe (no NVML, or not a GPU miner) a look at the counters does not make one up or lose the last
    shared.status_at(1000);
    assert_eq!(shared.status().gpu, Some(card()));
}

/// The owner's machine: the shared counters read a real card when a probe is set.
#[test]
#[ignore = "needs an NVIDIA GPU and its driver"]
fn a_real_probe_in_the_shared_counters_gives_a_reading_with_each_look() {
    let shared = MiningShared::default();
    *shared.probe.lock().unwrap() =
        Some(tenero_miner::gpu_stats::GpuProbe::open(0).expect("NVML and GPU 0"));
    assert!(shared.status().gpu.is_none(), "nothing read yet");
    let g = shared.status_at(1000).gpu.expect("a reading");
    assert!(g.temp_c.is_some() && g.mem_total_mib.is_some());
}

#[test]
fn rates_are_shown_the_way_miners_show_them() {
    use tenero_app::ui::format_rate;
    assert_eq!(format_rate(0.0), "0");
    assert_eq!(format_rate(812.4), "812");
    assert_eq!(format_rate(999.4), "999");
    assert_eq!(format_rate(999.5), "1.0k");
    assert_eq!(format_rate(34_738.0), "34.7k");
    assert_eq!(format_rate(30_197.0), "30.2k");
    assert_eq!(format_rate(999_949.0), "999.9k");
    assert_eq!(format_rate(999_950.0), "1.00M");
    assert_eq!(format_rate(1_234_567.0), "1.23M");
    assert_eq!(format_rate(-5.0), "0", "never a negative rate");
}

#[test]
fn a_backend_has_a_short_name_for_the_screen() {
    use tenero_app::ui::short_backend;
    assert_eq!(
        short_backend("matmulhash on the GPU (NVIDIA GeForce RTX 5070 Ti, batch 128)"),
        "GPU: NVIDIA GeForce RTX 5070 Ti, batch 128"
    );
    assert_eq!(
        short_backend("matmulhash on 2 CPU thread(s)"),
        "CPU, 2 threads"
    );
    assert_eq!(
        short_backend("sha256 test chain (CPU)"),
        "sha256 test chain (CPU)"
    );
    assert_eq!(short_backend("starting"), "starting");
}

#[test]
fn a_long_gpu_name_cannot_push_the_counts_off_the_mining_row() {
    // what the owner saw on a real console: the counts were cut off after the long backend name
    let mut s = status();
    s.mining = Some(MiningStatus {
        backend: "matmulhash on the GPU (NVIDIA GeForce RTX 5070 Ti, batch 128)".into(),
        blocks_found: 50,
        blocks_accepted: 50,
        paused: false,
        rates: Rates {
            s10: Some(34_738.0),
            s60: Some(30_279.0),
            m15: None,
            average: Some(30_197.0),
            searching: true,
        },
        gpu: Some(card()),
    });
    let block = render_status_block(&s, &OFF);
    assert!(
        block.contains(&"  mining   mining | 50 found, 50 in the chain".to_string()),
        "{block:#?}"
    );
    assert!(block.contains(&"  backend  GPU: NVIDIA GeForce RTX 5070 Ti, batch 128".to_string()));
    assert!(block
        .contains(&"  hashrate 10s 34.7k | 60s 30.3k | 15m - | avg 30.2k attempts/s".to_string()));
    for l in &block {
        assert!(!l.ends_with("..."), "a line was cut: {l}");
        assert!(l.len() <= MAX_LINE, "{l}");
    }
}

#[test]
fn the_miner_program_shows_the_short_backend_name_too() {
    let mut s = miner_status();
    s.backend = "matmulhash on the GPU (NVIDIA GeForce RTX 5070 Ti, batch 128)".into();
    assert_eq!(
        render_miner_block(&s, &OFF)[2],
        "  mining   GPU: NVIDIA GeForce RTX 5070 Ti, batch 128"
    );
}
