//! The seed health check (M9, `docs/SEED_POLICY.md`). The judging is pure and is tested on made-up probes, rule by rule and to the boundary;
//! the probing is tested against fake seeds on real sockets that behave well, badly or not at all. (A real node is probed in `daemon.rs`.)

use std::collections::BTreeSet;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::channel;
use std::thread;
use std::time::{Duration, Instant};

use tenero_app::seedcheck::{
    evaluate, history_line, parse_args, parse_seeds_file, probe, uptime, EvalConfig, Probe,
    ProbeConfig, Severity,
};
use tenero_net::addrbook::string_to_peer_addr;
use tenero_net::noise::{handshake_responder, prologue, NodeKey, SecureReader, SecureWriter};
use tenero_net::wire::{encode, FrameDecoder};
use tenero_net::{Hello, Message, PROTOCOL_VERSION};

const CHAIN: [u8; 32] = [7; 32];

// ---- made-up probes ---------------------------------------------------------------------------------------------------------

fn addrs(n: usize) -> Vec<String> {
    // routable, each in a network group of its own
    (0..n)
        .map(|i| format!("{}.{}.1.1:8333", 40 + i / 200, 1 + i % 200))
        .collect()
}

fn good(seed: &str, height: u64, tip: u8, n_addrs: usize) -> Probe {
    Probe {
        seed: seed.to_string(),
        stage: "addrs",
        error: None,
        connect_ms: 10,
        handshake_ms: 10,
        hello_ms: 10,
        hello: Some(Hello {
            version: PROTOCOL_VERSION,
            chain_id: CHAIN,
            tip_height: height,
            cumulative_work: [0; 32],
            tip_id: [tip; 32],
            pruned_below: 0,
            nonce: 1,
        }),
        addrs: addrs(n_addrs),
    }
}

fn failed(seed: &str, stage: &'static str, why: &str) -> Probe {
    Probe {
        seed: seed.to_string(),
        stage,
        error: Some(why.to_string()),
        ..Probe::default()
    }
}

fn three() -> Vec<Probe> {
    vec![
        good("30.1.1.1:8333", 100, 1, 8),
        good("31.1.1.1:8333", 100, 1, 8),
        good("32.1.1.1:8333", 101, 2, 8),
    ]
}

fn cfg() -> EvalConfig {
    EvalConfig::new(CHAIN)
}

fn texts(findings: &[tenero_app::seedcheck::Finding]) -> Vec<String> {
    findings.iter().map(|f| f.text.clone()).collect()
}

#[test]
fn three_good_seeds_on_one_chain_in_three_groups_are_all_well() {
    let r = evaluate(&three(), &cfg());
    assert_eq!(r.severity, Severity::Ok, "{}", r.to_text());
    assert_eq!(r.answered, 3);
    assert!(r.list.is_empty());
    for s in &r.seeds {
        assert_eq!(
            (s.severity, s.addrs, s.routable, s.groups),
            (Severity::Ok, 8, 8, 8)
        );
        assert!(s.findings.is_empty(), "{:?}", s.findings);
    }
    assert!(r.to_text().contains("result: OK (exit code 0)"));
}

#[test]
fn a_seed_that_cannot_be_reached_fails_and_says_where_it_stopped() {
    for (stage, word) in [
        ("resolve", "does not resolve"),
        ("connect", "cannot connect"),
        ("handshake", "handshake did not complete"),
        ("hello", "no hello"),
    ] {
        let mut v = three();
        v[0] = failed("30.1.1.1:8333", stage, "the reason");
        let r = evaluate(&v, &cfg());
        assert_eq!(r.seeds[0].severity, Severity::Fail, "{stage}");
        let t = texts(&r.seeds[0].findings).join("|");
        assert!(t.contains(word) && t.contains("the reason"), "{stage}: {t}");
        assert_eq!(r.severity, Severity::Fail);
        assert_eq!(r.severity.exit_code(), 2);
        assert_eq!(r.answered, 2, "{stage}");
        // the others are still judged on their own
        assert_eq!(r.seeds[1].severity, Severity::Ok);
    }
}

#[test]
fn the_wrong_protocol_version_or_chain_fails() {
    let mut v = three();
    v[0].hello.as_mut().unwrap().version = PROTOCOL_VERSION + 1;
    v[1].hello.as_mut().unwrap().chain_id = [9; 32];
    let r = evaluate(&v, &cfg());
    assert_eq!(r.seeds[0].severity, Severity::Fail);
    assert!(texts(&r.seeds[0].findings)[0].contains("protocol version"));
    assert_eq!(r.seeds[1].severity, Severity::Fail);
    assert!(texts(&r.seeds[1].findings).contains(&"a different chain".to_string()));
    // a seed on another chain or version is not compared with the others
    assert!(!texts(&r.seeds[2].findings)
        .iter()
        .any(|t| t.contains("behind")));
}

#[test]
fn a_pruned_seed_is_a_warning_unless_archives_are_not_expected() {
    let mut v = three();
    v[0].hello.as_mut().unwrap().pruned_below = 50;
    let r = evaluate(&v, &cfg());
    assert_eq!(r.seeds[0].severity, Severity::Warn);
    assert!(texts(&r.seeds[0].findings)[0].contains("pruned below height 50"));
    let mut c = cfg();
    c.expect_archive = false;
    assert_eq!(evaluate(&v, &c).seeds[0].severity, Severity::Ok);
}

#[test]
fn a_slow_seed_is_a_warning_just_over_the_limit_and_not_at_it() {
    let mut v = three();
    // 3000 ms in all is the limit
    v[0].connect_ms = 1000;
    v[0].handshake_ms = 1000;
    v[0].hello_ms = 1000;
    assert_eq!(evaluate(&v, &cfg()).seeds[0].severity, Severity::Ok);
    v[0].hello_ms = 1001;
    let r = evaluate(&v, &cfg());
    assert_eq!(r.seeds[0].severity, Severity::Warn);
    assert!(texts(&r.seeds[0].findings)[0].contains("3001 ms"));
    assert_eq!(r.seeds[0].latency_ms, Some(3001));
}

#[test]
fn the_address_answer_is_judged_on_how_many_how_routable_and_how_spread() {
    let one = |p: Probe| {
        evaluate(
            &[
                p,
                good("31.1.1.1:8333", 100, 1, 8),
                good("32.1.1.1:8333", 100, 1, 8),
            ],
            &cfg(),
        )
    };
    // none; fewer than five; exactly five
    let r = one(good("30.1.1.1:8333", 100, 1, 0));
    assert_eq!(r.seeds[0].severity, Severity::Warn);
    assert!(texts(&r.seeds[0].findings)[0].contains("no addresses"));
    let r = one(good("30.1.1.1:8333", 100, 1, 4));
    assert!(texts(&r.seeds[0].findings)[0].contains("only 4 addresses"));
    assert_eq!(
        one(good("30.1.1.1:8333", 100, 1, 5)).seeds[0].severity,
        Severity::Ok
    );
    // routable: 10 addresses of which 6 are is no warning; of which 4 are is one; of which 5 are is none
    let mut p = good("30.1.1.1:8333", 100, 1, 6);
    p.addrs.extend((0..4).map(|i| format!("10.0.{i}.1:8333")));
    let r = one(p);
    assert!(r.seeds[0].findings.is_empty(), "{:?}", r.seeds[0].findings);
    let mut p = good("30.1.1.1:8333", 100, 1, 4);
    p.addrs.extend((0..6).map(|i| format!("10.0.{i}.1:8333")));
    let r = one(p);
    assert!(
        texts(&r.seeds[0].findings)
            .iter()
            .any(|t| t.contains("only 4 of its 10 addresses are routable")),
        "{:?}",
        r.seeds[0].findings
    );
    let mut p = good("30.1.1.1:8333", 100, 1, 5);
    p.addrs.extend((0..5).map(|i| format!("10.0.{i}.1:8333")));
    assert_eq!(one(p).seeds[0].severity, Severity::Ok, "half is enough");
    // spread: five addresses in one /16
    let mut p = good("30.1.1.1:8333", 100, 1, 0);
    p.addrs = (0..5).map(|i| format!("50.1.{i}.1:8333")).collect();
    let r = one(p.clone());
    assert!(texts(&r.seeds[0].findings)
        .iter()
        .any(|t| t.contains("one network group")));
    // a private network need not have routable addresses or several groups
    let mut c = cfg();
    c.private_network = true;
    let mut q = p;
    q.addrs.extend((0..5).map(|i| format!("10.0.{i}.1:8333")));
    let r = evaluate(
        &[
            q,
            good("31.1.1.1:8333", 100, 1, 8),
            good("32.1.1.1:8333", 100, 1, 8),
        ],
        &c,
    );
    assert_eq!(
        r.seeds[0].severity,
        Severity::Ok,
        "{:?}",
        r.seeds[0].findings
    );
}

#[test]
fn a_seed_far_behind_the_others_fails_and_one_just_inside_the_limit_does_not() {
    let heights = |h: [u64; 3]| {
        let v = vec![
            good("30.1.1.1:8333", h[0], 1, 8),
            good("31.1.1.1:8333", h[1], 1, 8),
            good("32.1.1.1:8333", h[2], 1, 8),
        ];
        evaluate(&v, &cfg())
    };
    // the middle is 100; three blocks behind is allowed, four is not
    assert_eq!(heights([97, 100, 100]).seeds[0].severity, Severity::Ok);
    let r = heights([96, 100, 100]);
    assert_eq!(r.seeds[0].severity, Severity::Fail);
    assert!(
        texts(&r.seeds[0].findings)[0].contains("behind the other seeds: tip 96 against about 100")
    );
    assert_eq!(r.severity, Severity::Fail);
    // three ahead is allowed; four is a warning (they may be the ones that are behind)
    assert_eq!(heights([103, 100, 100]).seeds[0].severity, Severity::Ok);
    let r = heights([104, 100, 100]);
    assert_eq!(r.seeds[0].severity, Severity::Warn);
    assert!(texts(&r.seeds[0].findings)[0].contains("ahead of the other seeds"));
    // with two seeds the middle is the higher one
    let r = evaluate(
        &[
            good("30.1.1.1:8333", 100, 1, 8),
            good("31.1.1.1:8333", 110, 1, 8),
        ],
        &cfg(),
    );
    assert_eq!(r.seeds[0].severity, Severity::Fail);
    assert_eq!(r.seeds[1].severity, Severity::Ok);
    // one seed on its own has nothing to be compared with
    let r = evaluate(&[good("30.1.1.1:8333", 1, 1, 8)], &cfg());
    assert!(r.seeds[0].findings.is_empty());
}

#[test]
fn a_seed_on_another_tip_at_the_height_of_most_is_a_warning_only_when_most_agree() {
    let v = |tips: [u8; 3]| {
        evaluate(
            &[
                good("30.1.1.1:8333", 100, tips[0], 8),
                good("31.1.1.1:8333", 100, tips[1], 8),
                good("32.1.1.1:8333", 100, tips[2], 8),
            ],
            &cfg(),
        )
    };
    let r = v([1, 1, 2]);
    assert_eq!(r.seeds[2].severity, Severity::Warn);
    assert!(texts(&r.seeds[2].findings)[0].contains("different tip"));
    assert_eq!(r.seeds[0].severity, Severity::Ok);
    // no majority (three different tips): nothing to say about any one
    assert_eq!(v([1, 2, 3]).severity, Severity::Ok);
    assert_eq!(v([1, 1, 1]).severity, Severity::Ok);
    // two seeds on two tips: no majority
    let r = evaluate(
        &[
            good("30.1.1.1:8333", 100, 1, 8),
            good("31.1.1.1:8333", 100, 2, 8),
        ],
        &cfg(),
    );
    assert!(r.seeds.iter().all(|s| s.findings.is_empty()));
    // a different tip at a different height is a matter of lag, not a fork
    let r = evaluate(
        &[
            good("30.1.1.1:8333", 100, 1, 8),
            good("31.1.1.1:8333", 100, 1, 8),
            good("32.1.1.1:8333", 101, 2, 8),
        ],
        &cfg(),
    );
    assert_eq!(r.severity, Severity::Ok, "{}", r.to_text());
}

#[test]
fn the_list_itself_is_judged_too() {
    let mut c = cfg();
    // two listed
    let r = evaluate(&three()[..2], &c);
    assert_eq!(r.severity, Severity::Warn);
    assert!(texts(&r.list)[0].contains("only 2 seeds listed"));
    // three listed, two answered
    let mut v = three();
    v[2] = failed("32.1.1.1:8333", "connect", "refused");
    let r = evaluate(&v, &c);
    assert!(
        texts(&r.list)
            .iter()
            .any(|t| t.contains("only 2 of 3 seeds answered")),
        "{:?}",
        r.list
    );
    // a seed listed twice
    let mut v = three();
    v.push(good("30.1.1.1:8333", 100, 1, 8));
    let r = evaluate(&v, &c);
    assert!(texts(&r.list)
        .iter()
        .any(|t| t.contains("30.1.1.1:8333 is listed twice")));
    // two seeds in one network group count as one answer
    let mut v = three();
    v[1] = good("30.1.2.1:8333", 100, 1, 8);
    let r = evaluate(&v, &c);
    let t = texts(&r.list).join("|");
    assert!(
        t.contains("30.1.1.1:8333") && t.contains("30.1.2.1:8333") && t.contains("one answer"),
        "{t}"
    );
    // ... but not on a private network
    c.private_network = true;
    assert!(!texts(&evaluate(&v, &c).list)
        .iter()
        .any(|t| t.contains("one answer")));
    // the least number of seeds is a setting
    c.min_seeds = 2;
    assert!(evaluate(&three()[..2], &c).list.is_empty());
}

#[test]
fn the_report_reads_as_a_table_with_the_findings_under_each_seed() {
    let mut v = three();
    v[0] = failed("30.1.1.1:8333", "connect", "refused");
    v[1].hello.as_mut().unwrap().pruned_below = 5;
    let text = evaluate(&v, &cfg()).to_text();
    assert!(text.starts_with("3 seeds checked, 2 answered\n"), "{text}");
    assert!(
        text.contains("  FAIL  30.1.1.1:8333  no hello\n          FAIL: cannot connect: refused\n"),
        "{text}"
    );
    assert!(
        text.contains(
            "  WARN  31.1.1.1:8333  tip 100, 30 ms, 8 addresses (8 routable, 8 groups)\n"
        ),
        "{text}"
    );
    assert!(text.contains("  OK    32.1.1.1:8333  tip 101"), "{text}");
    assert!(
        text.contains("list  WARN: only 2 of 3 seeds answered"),
        "{text}"
    );
    assert!(text.ends_with("result: FAIL (exit code 2)\n"), "{text}");
    assert_eq!(
        (
            Severity::Ok.exit_code(),
            Severity::Warn.exit_code(),
            Severity::Fail.exit_code()
        ),
        (0, 1, 2)
    );
    assert!(Severity::Ok < Severity::Warn && Severity::Warn < Severity::Fail);
}

// ---- history ---------------------------------------------------------------------------------------------------------------------

#[test]
fn a_history_line_says_when_who_up_or_down_how_bad_how_fast_and_how_far() {
    let r = evaluate(&three(), &cfg());
    assert_eq!(
        history_line(1_700_000_000, &r.seeds[0]),
        "1700000000\t30.1.1.1:8333\tup\tOK\t30\t100"
    );
    let mut v = three();
    v[0] = failed("30.1.1.1:8333", "connect", "refused");
    let r = evaluate(&v, &cfg());
    assert_eq!(
        history_line(5, &r.seeds[0]),
        "5\t30.1.1.1:8333\tdown\tFAIL\t-\t-"
    );
}

#[test]
fn uptime_counts_the_last_checks_of_one_seed() {
    let h = "\
1\tA\tup\tOK\t1\t1
2\tB\tdown\tFAIL\t-\t-
3\tA\tdown\tFAIL\t-\t-
4\tA\tup\tOK\t1\t1
5\tA\tup\tWARN\t1\t1
";
    assert_eq!(uptime(h, "A", 50), (3, 4));
    assert_eq!(uptime(h, "A", 3), (2, 3));
    assert_eq!(uptime(h, "A", 1), (1, 1));
    // the LAST checks count, not the first: two ups and then two downs
    let h2 =
        "1\tA\tup\tOK\t1\t1\n2\tA\tup\tOK\t1\t1\n3\tA\tdown\tFAIL\t-\t-\n4\tA\tdown\tFAIL\t-\t-\n";
    assert_eq!(uptime(h2, "A", 2), (0, 2));
    assert_eq!(uptime(h2, "A", 3), (1, 3));
    assert_eq!(uptime(h, "B", 50), (0, 1));
    assert_eq!(uptime(h, "C", 50), (0, 0));
    assert_eq!(uptime("", "A", 50), (0, 0));
    // damaged lines are skipped, not counted
    assert_eq!(
        uptime("garbage\n7\tA\n8\tA\tup\tOK\t1\t1\n", "A", 50),
        (1, 1)
    );
}

// ---- the command line ------------------------------------------------------------------------------------------------------------

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn no_files(_: &str) -> Result<String, String> {
    Err("no such file".into())
}

#[test]
fn the_command_line_gives_defaults_and_reads_every_option() {
    let o = parse_args(&args(&["--network", "test", "--seed", "a:1"]), &no_files).unwrap();
    assert_eq!(
        (
            o.timeout_s,
            o.addr_wait_s,
            o.max_lag,
            o.min_addrs,
            o.min_seeds,
            o.private
        ),
        (8, 3, 3, 5, 3, true),
        "{o:?}"
    );
    assert!(o.history.is_none());
    // the dev network is public by default
    assert!(
        !parse_args(&args(&["--network", "dev", "--seed", "a:1"]), &no_files)
            .unwrap()
            .private
    );
    // a time of zero would make every step fail at once: it is raised to one second
    let z = parse_args(
        &args(&[
            "--network",
            "test",
            "--seed",
            "a:1",
            "--timeout",
            "0",
            "--addr-wait",
            "0",
        ]),
        &no_files,
    )
    .unwrap();
    assert_eq!((z.timeout_s, z.addr_wait_s), (1, 1));
    let o = parse_args(
        &args(&[
            "--network",
            "dev",
            "--seed",
            "a:1",
            "--seed",
            "b:2",
            "--timeout",
            "20",
            "--addr-wait",
            "5",
            "--max-lag",
            "10",
            "--min-addrs",
            "1",
            "--min-seeds",
            "2",
            "--private",
            "yes",
            "--history",
            "h.tsv",
        ]),
        &no_files,
    )
    .unwrap();
    assert_eq!(o.seeds, vec!["a:1", "b:2"]);
    assert_eq!(
        (
            o.timeout_s,
            o.addr_wait_s,
            o.max_lag,
            o.min_addrs,
            o.min_seeds,
            o.private
        ),
        (20, 5, 10, 1, 2, true)
    );
    assert_eq!(o.history.as_deref(), Some("h.tsv"));
    // a seeds file adds to --seed
    let read = |p: &str| {
        assert_eq!(p, "seeds.txt");
        Ok("# the seeds\nc:3  # one\n\n  d:4\nc:3\n".to_string())
    };
    let o = parse_args(
        &args(&[
            "--network",
            "test",
            "--seed",
            "a:1",
            "--seeds-file",
            "seeds.txt",
        ]),
        &read,
    )
    .unwrap();
    assert_eq!(o.seeds, vec!["a:1", "c:3", "d:4", "c:3"]);
}

#[test]
fn the_command_line_refuses_what_it_cannot_make_sense_of() {
    let bad = |v: &[&str], why: &str| {
        let e = parse_args(&args(v), &no_files).unwrap_err();
        assert!(e.contains(why), "{v:?}: {e}");
    };
    bad(
        &["--seed", "a:1"],
        "--network must be test, dev, beta or alpha",
    );
    bad(
        &["--network", "main", "--seed", "a:1"],
        "--network must be test, dev, beta or alpha",
    );
    bad(&["--network", "test"], "no seeds");
    bad(
        &["--network", "test", "--seed", "a:1", "--network", "dev"],
        "--network given twice",
    );
    bad(
        &["--network", "test", "--seed", "a:1", "--timeout", "x"],
        "--timeout: `x` is not a number",
    );
    bad(
        &["--network", "test", "--seed", "a:1", "--private", "maybe"],
        "--private: `maybe` is not yes or no",
    );
    bad(
        &["--network", "test", "--seed", "a:1", "--wat", "1"],
        "unknown option `--wat`",
    );
    bad(&["--network", "test", "--seed"], "--seed needs a value");
    bad(&["network", "test"], "unexpected argument `network`");
    bad(
        &["--network", "test", "--seeds-file", "x.txt"],
        "--seeds-file: no such file",
    );
}

#[test]
fn a_seeds_file_drops_comments_and_blank_lines_and_keeps_duplicates() {
    assert_eq!(
        parse_seeds_file("a:1\n\n# c\n  b:2 # d\r\na:1\n"),
        vec!["a:1", "b:2", "a:1"]
    );
    assert!(parse_seeds_file("").is_empty());
    assert!(parse_seeds_file("# only\n   \n").is_empty());
}

// ---- fake seeds on real sockets ----------------------------------------------------------------------------------------------

struct Srv {
    stream: TcpStream,
    r: SecureReader,
    w: SecureWriter,
    dec: FrameDecoder,
}

impl Srv {
    fn accept(l: &TcpListener, chain: [u8; 32]) -> Option<Srv> {
        let (mut stream, _) = l.accept().ok()?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .ok()?;
        let s = handshake_responder(
            &mut stream,
            &NodeKey::generate(),
            &prologue(PROTOCOL_VERSION, &chain),
        )
        .ok()?;
        Some(Srv {
            stream,
            r: s.reader,
            w: s.writer,
            dec: FrameDecoder::new(),
        })
    }

    fn send(&mut self, m: &Message) {
        let sealed = self.w.seal(&encode(m).unwrap()).unwrap();
        let _ = self.stream.write_all(&sealed);
    }

    fn recv(&mut self, wait: Duration) -> Option<Message> {
        self.stream.set_read_timeout(Some(wait)).unwrap();
        loop {
            if let Ok(Some(m)) = self.dec.next_message() {
                return Some(m);
            }
            match self.r.read_chunk(&mut self.stream) {
                Ok(chunk) => self.dec.push(&chunk),
                Err(_) => return None,
            }
        }
    }

    fn hello(&self, height: u64) -> Message {
        Message::Hello(Hello {
            version: PROTOCOL_VERSION,
            chain_id: CHAIN,
            tip_height: height,
            cumulative_work: [0; 32],
            tip_id: [3; 32],
            pruned_below: 0,
            nonce: 99,
        })
    }
}

fn listener() -> (TcpListener, String) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = l.local_addr().unwrap().to_string();
    (l, a)
}

fn quick() -> ProbeConfig {
    ProbeConfig {
        timeout: Duration::from_millis(1500),
        addr_wait: Duration::from_millis(1500),
    }
}

fn pa(a: &str) -> tenero_net::message::PeerAddr {
    string_to_peer_addr(a, 1_700_000_000).unwrap()
}

#[test]
fn a_good_seed_is_read_through_to_its_addresses_and_pings_are_answered() {
    let (l, addr) = listener();
    let (tx, rx) = channel();
    let server = thread::spawn(move || {
        let mut s = Srv::accept(&l, CHAIN).unwrap();
        // it gets our hello first, and says its own
        assert!(matches!(
            s.recv(Duration::from_secs(5)),
            Some(Message::Hello(_))
        ));
        s.send(&s.hello(50));
        // the node announcing itself comes before the answer: one address, then six (one of them the same again), then a ping
        assert!(matches!(
            s.recv(Duration::from_secs(5)),
            Some(Message::GetAddrs)
        ));
        s.send(&Message::Addrs {
            addrs: vec![pa("20.1.1.1:8333")],
        });
        s.send(&Message::Addrs {
            addrs: [
                "20.1.1.1:8333",
                "21.1.1.1:8333",
                "22.1.1.1:8333",
                "23.1.1.1:8333",
                "24.1.1.1:8333",
                "25.1.1.1:8333",
            ]
            .iter()
            .map(|a| pa(a))
            .collect(),
        });
        s.send(&Message::Ping(5));
        tx.send(s.recv(Duration::from_secs(5))).unwrap();
        // the connection stays open: only the quiet after the answer can end the wait
        thread::sleep(Duration::from_secs(2));
    });
    let t = Instant::now();
    let p = probe(&addr, CHAIN, &quick());
    assert_eq!((p.stage, p.error.clone()), ("addrs", None), "{p:?}");
    assert_eq!(p.hello.as_ref().unwrap().tip_height, 50);
    assert_eq!(p.addrs.len(), 6, "{:?}", p.addrs);
    assert!(
        p.addrs.windows(2).all(|w| w[0] < w[1]),
        "distinct and sorted"
    );
    // the wait ends shortly after the answer, not at the end of the whole allowance
    assert!(
        t.elapsed() < Duration::from_millis(1400),
        "{:?}",
        t.elapsed()
    );
    assert_eq!(rx.recv().unwrap(), Some(Message::Pong(5)));
    server.join().unwrap();
}

#[test]
fn a_seed_that_answers_late_is_still_read_and_one_that_never_does_has_no_addresses() {
    let (l, addr) = listener();
    let server = thread::spawn(move || {
        let mut s = Srv::accept(&l, CHAIN).unwrap();
        s.recv(Duration::from_secs(5));
        s.send(&s.hello(7));
        s.recv(Duration::from_secs(5));
        thread::sleep(Duration::from_millis(600));
        s.send(&Message::Addrs {
            addrs: vec![pa("20.1.1.1:8333"), pa("21.1.1.1:8333")],
        });
        thread::sleep(Duration::from_millis(500));
    });
    let p = probe(&addr, CHAIN, &quick());
    assert_eq!((p.stage, p.error.clone()), ("addrs", None));
    assert_eq!(p.addrs.len(), 2);
    server.join().unwrap();

    let (l, addr) = listener();
    let server = thread::spawn(move || {
        let mut s = Srv::accept(&l, CHAIN).unwrap();
        s.recv(Duration::from_secs(5));
        s.send(&s.hello(7));
        // it never answers the address request (and keeps the connection open while it is not answered)
        s.recv(Duration::from_secs(3));
        thread::sleep(Duration::from_millis(1500));
    });
    let t = Instant::now();
    let cfg = ProbeConfig {
        timeout: Duration::from_secs(2),
        addr_wait: Duration::from_millis(700),
    };
    let p = probe(&addr, CHAIN, &cfg);
    assert_eq!((p.stage, p.error.clone()), ("addrs", None), "{p:?}");
    assert!(p.addrs.is_empty());
    assert!(
        t.elapsed() >= Duration::from_millis(650) && t.elapsed() < Duration::from_millis(2500),
        "{:?}",
        t.elapsed()
    );
    // ... which the judge calls a warning, not a failure
    let r = evaluate(
        &[p],
        &EvalConfig {
            min_seeds: 1,
            ..cfg_private()
        },
    );
    assert_eq!(r.seeds[0].severity, Severity::Warn);
    server.join().unwrap();
}

fn cfg_private() -> EvalConfig {
    EvalConfig {
        private_network: true,
        ..EvalConfig::new(CHAIN)
    }
}

#[test]
fn a_seed_that_completes_the_handshake_and_never_says_hello_fails_at_hello() {
    let (l, addr) = listener();
    let server = thread::spawn(move || {
        let mut s = Srv::accept(&l, CHAIN).unwrap();
        s.recv(Duration::from_secs(3));
        // it has read our hello and does not answer; the connection stays open
        thread::sleep(Duration::from_secs(3));
    });
    let t = Instant::now();
    let p = probe(&addr, CHAIN, &quick());
    assert_eq!(p.stage, "hello", "{p:?}");
    assert!(p.error.as_deref().unwrap().contains("no hello"), "{p:?}");
    assert!(p.hello.is_none());
    assert!(
        t.elapsed() >= Duration::from_millis(1400) && t.elapsed() < Duration::from_millis(4000),
        "{:?}",
        t.elapsed()
    );
    server.join().unwrap();
}

#[test]
fn a_seed_that_accepts_and_says_nothing_fails_at_the_handshake_within_the_time_allowed() {
    let (l, addr) = listener();
    let server = thread::spawn(move || {
        let (_stream, _) = l.accept().unwrap();
        thread::sleep(Duration::from_secs(3));
    });
    let t = Instant::now();
    let p = probe(&addr, CHAIN, &quick());
    assert_eq!(p.stage, "handshake", "{p:?}");
    assert!(p.error.as_deref().unwrap().contains("timed out"), "{p:?}");
    assert!(
        t.elapsed() < Duration::from_millis(3500),
        "{:?}",
        t.elapsed()
    );
    assert!(p.connect_ms < 1000);
    server.join().unwrap();
}

#[test]
fn a_port_nobody_listens_on_fails_at_connect() {
    let (l, addr) = listener();
    drop(l);
    let p = probe(&addr, CHAIN, &quick());
    assert_eq!(p.stage, "connect", "{p:?}");
    assert!(p.error.is_some());
    assert!(p.hello.is_none() && p.addrs.is_empty());
}

#[test]
fn a_seed_of_another_chain_fails_at_the_handshake() {
    let (l, addr) = listener();
    let server = thread::spawn(move || {
        // it serves chain [8; 32]; the probe speaks [7; 32]: the encrypted handshake itself differs
        let _ = Srv::accept(&l, [8; 32]);
    });
    let p = probe(&addr, CHAIN, &quick());
    assert_eq!(p.stage, "handshake", "{p:?}");
    assert!(
        p.error
            .as_deref()
            .unwrap()
            .contains("another chain or protocol version"),
        "{p:?}"
    );
    let r = evaluate(&[p], &cfg());
    assert_eq!(r.seeds[0].severity, Severity::Fail);
    server.join().unwrap();
}

#[test]
fn the_probes_of_several_seeds_do_not_depend_on_each_other() {
    // two good fake seeds and a dead port, probed in turn: each result is its own
    let mut addrs = Vec::new();
    let mut threads = Vec::new();
    for height in [10u64, 12] {
        let (l, a) = listener();
        addrs.push(a);
        threads.push(thread::spawn(move || {
            let mut s = Srv::accept(&l, CHAIN).unwrap();
            s.recv(Duration::from_secs(5));
            s.send(&s.hello(height));
            s.recv(Duration::from_secs(5));
            s.send(&Message::Addrs {
                addrs: (20..30).map(|i| pa(&format!("{i}.1.1.1:8333"))).collect(),
            });
            thread::sleep(Duration::from_millis(600));
        }));
    }
    let (dead, dead_addr) = listener();
    drop(dead);
    addrs.push(dead_addr);
    let probes: Vec<Probe> = addrs.iter().map(|a| probe(a, CHAIN, &quick())).collect();
    for t in threads {
        t.join().unwrap();
    }
    let r = evaluate(
        &probes,
        &EvalConfig {
            min_seeds: 3,
            ..cfg_private()
        },
    );
    assert_eq!(r.answered, 2);
    assert_eq!(r.seeds[0].tip_height, Some(10));
    assert_eq!(r.seeds[1].tip_height, Some(12));
    assert_eq!(r.seeds[2].severity, Severity::Fail);
    // heights 10 and 12 are within the lag allowed of each other
    assert_eq!(r.seeds[0].severity, Severity::Ok, "{}", r.to_text());
    let names: BTreeSet<&str> = r.seeds.iter().map(|s| s.seed.as_str()).collect();
    assert_eq!(names.len(), 3);
}

#[test]
fn a_seed_that_hangs_up_says_so_in_words() {
    // after the handshake, before its hello
    let (l, addr) = listener();
    let server = thread::spawn(move || {
        let mut s = Srv::accept(&l, CHAIN).unwrap();
        s.recv(Duration::from_secs(3));
        // (dropped here: the connection closes)
    });
    let p = probe(&addr, CHAIN, &quick());
    assert_eq!(p.stage, "hello", "{p:?}");
    assert_eq!(
        p.error.as_deref(),
        Some("the seed closed the connection"),
        "{p:?}"
    );
    server.join().unwrap();
    // before the handshake is done
    let (l, addr) = listener();
    let server = thread::spawn(move || {
        let (stream, _) = l.accept().unwrap();
        drop(stream);
    });
    let p = probe(&addr, CHAIN, &quick());
    assert_eq!(p.stage, "handshake", "{p:?}");
    assert!(
        p.error
            .as_deref()
            .unwrap()
            .starts_with("the seed closed the connection"),
        "{p:?}"
    );
    server.join().unwrap();
}

// ---- cases found by injecting faults -------------------------------------------------------------------------------------------

#[test]
fn a_probe_that_failed_at_the_address_request_is_not_also_told_it_gave_no_addresses() {
    let mut p = good("30.1.1.1:8333", 100, 1, 0);
    p.error = Some("broken pipe".to_string());
    let r = evaluate(
        &[
            p,
            good("31.1.1.1:8333", 100, 1, 8),
            good("32.1.1.1:8333", 100, 1, 8),
        ],
        &cfg(),
    );
    let t = texts(&r.seeds[0].findings);
    assert_eq!(t.len(), 1, "{t:?}");
    assert!(t[0].contains("broken pipe"));
}

#[test]
fn on_a_private_network_addresses_need_be_neither_routable_nor_in_several_groups() {
    let mut c = cfg();
    c.private_network = true;
    let mut p = good("30.1.1.1:8333", 100, 1, 0);
    p.addrs = (0..8).map(|i| format!("10.0.0.{}:8333", i + 1)).collect();
    let r = evaluate(
        &[
            p.clone(),
            good("31.1.1.1:8333", 100, 1, 8),
            good("32.1.1.1:8333", 100, 1, 8),
        ],
        &c,
    );
    assert!(r.seeds[0].findings.is_empty(), "{:?}", r.seeds[0].findings);
    // the same seed judged as a public one is warned twice
    let r = evaluate(
        &[
            p,
            good("31.1.1.1:8333", 100, 1, 8),
            good("32.1.1.1:8333", 100, 1, 8),
        ],
        &cfg(),
    );
    assert_eq!(r.seeds[0].findings.len(), 2, "{:?}", r.seeds[0].findings);
}

#[test]
fn a_few_addresses_in_one_group_are_one_complaint_not_two() {
    let mut p = good("30.1.1.1:8333", 100, 1, 0);
    p.addrs = (0..4).map(|i| format!("50.1.{i}.1:8333")).collect();
    let r = evaluate(
        &[
            p,
            good("31.1.1.1:8333", 100, 1, 8),
            good("32.1.1.1:8333", 100, 1, 8),
        ],
        &cfg(),
    );
    let t = texts(&r.seeds[0].findings);
    assert_eq!(t.len(), 1, "{t:?}");
    assert!(t[0].contains("only 4 addresses"));
}

#[test]
fn seeds_of_another_chain_do_not_decide_what_the_height_of_the_good_ones_ought_to_be() {
    let mut other_a = good("32.1.1.1:8333", 300, 5, 8);
    other_a.hello.as_mut().unwrap().chain_id = [9; 32];
    let mut other_b = good("33.1.1.1:8333", 300, 5, 8);
    other_b.hello.as_mut().unwrap().chain_id = [9; 32];
    let r = evaluate(
        &[
            good("30.1.1.1:8333", 100, 1, 8),
            good("31.1.1.1:8333", 100, 1, 8),
            other_a,
            other_b,
        ],
        &cfg(),
    );
    assert!(
        r.seeds[0].findings.is_empty() && r.seeds[1].findings.is_empty(),
        "{}",
        r.to_text()
    );
    assert_eq!(r.seeds[2].severity, Severity::Fail);
}

#[test]
fn a_seed_is_as_bad_as_its_worst_finding() {
    let mut p = good("30.1.1.1:8333", 100, 1, 8);
    p.hello.as_mut().unwrap().pruned_below = 5; // a warning
    p.hello.as_mut().unwrap().version = PROTOCOL_VERSION + 1; // a failure
    let r = evaluate(
        &[
            p,
            good("31.1.1.1:8333", 100, 1, 8),
            good("32.1.1.1:8333", 100, 1, 8),
        ],
        &cfg(),
    );
    assert_eq!(r.seeds[0].findings.len(), 2);
    assert_eq!(r.seeds[0].severity, Severity::Fail);
    // and a seed that is slow, pruned and gave few addresses is a warning, never a failure
    let mut w = good("31.1.1.1:8333", 100, 1, 2);
    w.hello.as_mut().unwrap().pruned_below = 5;
    w.hello_ms = 5000;
    let r = evaluate(
        &[
            w,
            good("30.1.1.1:8333", 100, 1, 8),
            good("32.1.1.1:8333", 100, 1, 8),
        ],
        &cfg(),
    );
    assert_eq!(r.seeds[0].findings.len(), 3);
    assert_eq!(r.seeds[0].severity, Severity::Warn);
}

#[test]
fn a_ping_that_comes_before_the_hello_is_answered() {
    // a seed that will not say hello until its ping has been answered
    let (l, addr) = listener();
    let server = thread::spawn(move || {
        let mut s = Srv::accept(&l, CHAIN).unwrap();
        s.recv(Duration::from_secs(5));
        s.send(&Message::Ping(9));
        if s.recv(Duration::from_secs(2)) == Some(Message::Pong(9)) {
            s.send(&s.hello(5));
            s.recv(Duration::from_secs(5));
            s.send(&Message::Addrs {
                addrs: vec![pa("20.1.1.1:8333")],
            });
        }
        thread::sleep(Duration::from_millis(800));
    });
    let p = probe(&addr, CHAIN, &quick());
    assert_eq!((p.stage, p.error.clone()), ("addrs", None), "{p:?}");
    assert_eq!(p.hello.unwrap().tip_height, 5);
    server.join().unwrap();
}
