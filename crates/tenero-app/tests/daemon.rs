//! The node program's body and the wallet program's commands, together, on real sockets and real files: nodes that
//! mine, find each other and sync, a wallet that pays through the control interface, a clean shutdown and a restart.
//! The SHA-256 test network (a CPU mines it); **not a real chain**.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tenero_app::client::{read_cookie, RemoteNode, COOKIE_FILE};
use tenero_app::config::{Config, Raw};
use tenero_app::control::NodeKind;
use tenero_app::daemon::{self, Ready, Summary, POOL_FILE};
use tenero_app::log::{Level, Logger};
use tenero_app::wallet_cli::{self, Io};
use zeroize::Zeroizing;

struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Dir {
        let d = std::env::temp_dir().join(format!("tenero-daemon-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Dir(d)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn config(data: &std::path::Path, extra: &str) -> Config {
    let text = format!(
        "data = {}\nnetwork = test\nlisten = 127.0.0.1:0\ncontrol = 127.0.0.1:0\n{extra}",
        data.display()
    );
    Raw::from_file_text(&text)
        .and_then(|r| r.into_config())
        .unwrap_or_else(|e| panic!("{e}"))
}

struct Running {
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<Result<Summary, String>>>,
    ready: Ready,
    data: PathBuf,
    log_file: PathBuf,
}

impl Running {
    fn start(cfg: Config) -> Running {
        Running::try_start(cfg).unwrap_or_else(|e| panic!("{e}"))
    }

    fn try_start(cfg: Config) -> Result<Running, String> {
        let log_file = cfg.data.join("node.log");
        let log = Arc::new(Logger::new(Level::Info, Some(&log_file), false).unwrap());
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let sd = Arc::clone(&shutdown);
        let data = cfg.data.clone();
        let join = thread::spawn(move || daemon::run(&cfg, log, sd, Some(tx)));
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(ready) => Ok(Running {
                shutdown,
                join: Some(join),
                ready,
                data,
                log_file,
            }),
            Err(_) => Err(join
                .join()
                .expect("the node thread")
                .err()
                .unwrap_or("no answer".into())),
        }
    }

    fn client(&self) -> RemoteNode {
        let cookie = read_cookie(&self.data.join(COOKIE_FILE)).unwrap();
        RemoteNode::connect(self.ready.control, &cookie).unwrap()
    }

    fn height(&self) -> u64 {
        self.client().info().unwrap().height
    }

    fn wait_height(&self, h: u64, secs: u64) {
        let end = Instant::now() + Duration::from_secs(secs);
        while self.height() < h {
            assert!(
                Instant::now() < end,
                "height {} < {h}\n{}",
                self.height(),
                self.log()
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log_file).unwrap_or_default()
    }

    fn stop(mut self) -> Summary {
        self.client().stop().unwrap();
        self.join.take().unwrap().join().unwrap().unwrap()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// A scripted terminal.
#[derive(Default)]
struct Script {
    passphrases: Vec<String>,
    said: Vec<String>,
}

impl Io for Script {
    fn passphrase(&mut self, _prompt: &str) -> Result<Zeroizing<String>, String> {
        if self.passphrases.is_empty() {
            return Err("the script ran out of passphrases".into());
        }
        Ok(Zeroizing::new(self.passphrases.remove(0)))
    }
    fn say(&mut self, line: &str) {
        self.said.push(line.to_string());
    }
}

fn cli(args: &[&str], passphrases: &[&str]) -> (Result<(), String>, Vec<String>) {
    let mut io = Script {
        passphrases: passphrases.iter().map(|s| s.to_string()).collect(),
        said: vec![],
    };
    let mut a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    if !a.is_empty() {
        a.insert(1, "--weak-kdf-for-tests".into());
    }
    let r = wallet_cli::run(&a, &mut io);
    (r, io.said)
}

fn find(lines: &[String], prefix: &str) -> String {
    lines
        .iter()
        .find_map(|l| l.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("no line starting {prefix:?} in {lines:#?}"))
        .trim()
        .to_string()
}

// ---- the node program ---------------------------------------------------------------------------------------------

#[test]
fn a_node_starts_answers_on_the_control_interface_and_stops_cleanly() {
    let dir = Dir::new("basic");
    let node = Running::start(config(&dir.0, ""));
    assert!(dir.path("control.cookie").exists());
    assert!(dir.path("node.key").exists());
    assert!(node.ready.control.ip().is_loopback());
    assert!(node.ready.p2p.is_some());
    let info = node.client().info().unwrap();
    assert_eq!(
        (info.network.as_str(), info.height, info.kind),
        ("test", 0, NodeKind::Archive)
    );
    assert!(!info.version.is_empty());
    let summary = node.stop();
    assert_eq!(summary.height, 0);
    assert!(
        dir.path("peers.dat").exists(),
        "the peers are saved at shutdown"
    );
    assert!(
        dir.path(POOL_FILE).exists(),
        "the side-branch pool is saved at shutdown"
    );
    let log = std::fs::read_to_string(dir.path("node.log")).unwrap();
    assert!(
        log.contains("EXPERIMENTAL and UNAUDITED"),
        "every start says what this is"
    );
    assert!(log.contains("shutting down") && log.contains("stopped at height 0"));
    // the cookie is never written to the log
    let cookie = std::fs::read_to_string(dir.path("control.cookie")).unwrap();
    assert!(!log.contains(cookie.trim()));
}

#[test]
fn setting_the_flag_from_outside_stops_a_node_too() {
    let dir = Dir::new("flag");
    let mut node = Running::start(config(&dir.0, ""));
    node.shutdown.store(true, Ordering::SeqCst);
    let summary = node.join.take().unwrap().join().unwrap().unwrap();
    assert_eq!(summary.height, 0);
}

#[test]
fn each_start_makes_a_new_cookie_and_the_old_one_stops_working() {
    let dir = Dir::new("cookie");
    let a = Running::start(config(&dir.0, ""));
    let first = read_cookie(&dir.path("control.cookie")).unwrap();
    a.stop();
    let b = Running::start(config(&dir.0, ""));
    let second = read_cookie(&dir.path("control.cookie")).unwrap();
    assert_ne!(first, second);
    assert!(RemoteNode::connect(b.ready.control, &first).is_err());
    assert!(RemoteNode::connect(b.ready.control, &second).is_ok());
}

#[test]
fn only_one_node_may_use_a_data_directory() {
    let dir = Dir::new("lock");
    let _first = Running::start(config(&dir.0, ""));
    let second = Running::try_start(config(&dir.0, ""));
    let e = second.err().expect("the second node must not start");
    assert!(
        e.contains("another node") || e.contains("cannot open"),
        "{e}"
    );
}

#[test]
fn a_node_that_mines_keeps_its_chain_across_a_restart() {
    let dir = Dir::new("restart");
    let alice = tenero_wallet::Wallet::from_seed(&[1; 32], 0)
        .address()
        .to_text();
    let node = Running::start(config(
        &dir.0,
        &format!("mine = sha256\nmine_to = {alice}\nmine_pace = 0\n"),
    ));
    node.wait_height(5, 30);
    let summary = node.stop();
    assert!(summary.height >= 5);
    let again = Running::start(config(&dir.0, ""));
    let info = again.client().info().unwrap();
    assert_eq!(
        (info.height, info.tip_id),
        (summary.height, summary.tip_id),
        "the same chain, not a new one"
    );
}

#[test]
fn a_pruned_node_says_so() {
    let dir = Dir::new("pruned");
    let node = Running::start(config(&dir.0, "prune_keep = 1000\n"));
    assert_eq!(node.client().info().unwrap().kind, NodeKind::Pruned);
    assert!(node.log().contains("pruned node"));
}

#[test]
fn a_node_with_a_chain_from_another_network_refuses_to_use_it() {
    let dir = Dir::new("wrongchain");
    let a = Running::start(config(&dir.0, ""));
    a.stop();
    // the same directory, a different network: the chain on disk is for another rules set
    let text = format!(
        "data = {}\nnetwork = dev\nlisten = 127.0.0.1:0\ncontrol = 127.0.0.1:0\n",
        dir.0.display()
    );
    let cfg = Raw::from_file_text(&text).unwrap().into_config().unwrap();
    let r = Running::try_start(cfg);
    assert!(r.is_err(), "a dev node must not open a test chain");
}

// ---- two nodes and a wallet --------------------------------------------------------------------------------------

#[test]
fn two_nodes_sync_and_a_wallet_pays_through_the_control_interface() {
    let (wa, wb) = (Dir::new("wallet-a"), Dir::new("wallet-b"));
    let (alice_file, bob_file) = (wa.path("alice.wallet"), wb.path("bob.wallet"));
    let pass = wa.path("pass.txt");
    std::fs::write(&pass, "correct horse battery\n").unwrap();
    let pass_s = pass.to_str().unwrap();

    // two wallets, made with the program's own commands
    let (r, said) = cli(
        &[
            "create",
            "--wallet",
            alice_file.to_str().unwrap(),
            "--birth",
            "0",
            "--passphrase-file",
            pass_s,
        ],
        &[],
    );
    r.unwrap();
    let alice_addr = find(&said, "address:");
    assert!(
        said.iter().any(|l| l.contains("INTERIM")),
        "the banner says the scheme is interim"
    );
    let seed = said
        .iter()
        .find(|l| l.trim().len() == 64 && l.trim().bytes().all(|b| b.is_ascii_hexdigit()))
        .expect("the seed is shown once")
        .trim()
        .to_string();
    let (r, said) = cli(
        &[
            "create",
            "--wallet",
            bob_file.to_str().unwrap(),
            "--birth",
            "0",
            "--passphrase-file",
            pass_s,
        ],
        &[],
    );
    r.unwrap();
    let bob_addr = find(&said, "address:");
    // the wallet file's address and the seed shown agree with a restore
    let restored = wa.path("restored.wallet");
    let (r, said) = cli(
        &[
            "restore",
            "--wallet",
            restored.to_str().unwrap(),
            "--passphrase-file",
            pass_s,
        ],
        &[&seed],
    );
    r.unwrap();
    assert_eq!(find(&said, "address:"), alice_addr);

    // node A mines to Alice, one block a second; node B follows A
    let da = Dir::new("node-a");
    let db = Dir::new("node-b");
    let a = Running::start(config(
        &da.0,
        &format!("mine = sha256\nmine_to = {alice_addr}\nmine_pace = 1\n"),
    ));
    let a_p2p = a.ready.p2p.unwrap();
    let b = Running::start(config(&db.0, &format!("seed = {a_p2p}\n")));
    a.wait_height(8, 60);
    let args = |c: &'static str, extra: &[&str]| -> Vec<String> {
        let mut v = vec![c.to_string()];
        v.extend(extra.iter().map(|s| s.to_string()));
        v
    };
    let _ = args;
    let control = a.ready.control.to_string();
    let data_a = da.0.to_str().unwrap().to_string();

    // Alice's balance, through the control interface (a sync of the whole chain in batches)
    let (r, said) = cli(
        &[
            "balance",
            "--wallet",
            alice_file.to_str().unwrap(),
            "--data",
            &data_a,
            "--control",
            &control,
            "--passphrase-file",
            pass_s,
        ],
        &[],
    );
    r.unwrap();
    let spendable = find(&said, "spendable");
    assert_ne!(spendable, "0", "Alice has mature coins: {said:#?}");

    // she pays Bob 1.5 coins
    let (r, said) = cli(
        &[
            "pay",
            "--wallet",
            alice_file.to_str().unwrap(),
            "--data",
            &data_a,
            "--control",
            &control,
            "--to",
            &bob_addr,
            "--amount",
            "1.5",
            "--passphrase-file",
            pass_s,
        ],
        &[],
    );
    r.unwrap_or_else(|e| panic!("{e}\n{}", a.log()));
    assert!(find(&said, "sent").starts_with("1.5 to"));
    assert_eq!(find(&said, "transaction").len(), 64);
    // the very next payment cannot use the same coins: the wallet file recorded the reservation
    let (_, said2) = cli(
        &[
            "balance",
            "--wallet",
            alice_file.to_str().unwrap(),
            "--data",
            &data_a,
            "--control",
            &control,
            "--passphrase-file",
            pass_s,
        ],
        &[],
    );
    assert_ne!(
        find(&said2, "reserved"),
        "0",
        "the coins sent are promised: {said2:#?}"
    );

    // a block takes it in, and Bob sees it
    let h = a.height();
    a.wait_height(h + 2, 60);
    let (r, said) = cli(
        &[
            "balance",
            "--wallet",
            bob_file.to_str().unwrap(),
            "--data",
            &data_a,
            "--control",
            &control,
            "--passphrase-file",
            pass_s,
        ],
        &[],
    );
    r.unwrap();
    assert_eq!(find(&said, "total"), "1.5", "{said:#?}");

    // node B followed along: the same chain
    let end = Instant::now() + Duration::from_secs(60);
    loop {
        let (ia, ib) = (a.client().info().unwrap(), b.client().info().unwrap());
        if ib.height >= ia.height.saturating_sub(1) && ib.height >= h + 2 {
            break;
        }
        assert!(
            Instant::now() < end,
            "B at {}, A at {}\nB log:\n{}",
            ib.height,
            ia.height,
            b.log()
        );
        thread::sleep(Duration::from_millis(200));
    }
    assert!(b.client().info().unwrap().peers >= 1);
    // the program's `info` command
    let (r, said) = cli(&["info", "--data", &data_a, "--control", &control], &[]);
    r.unwrap();
    assert!(said.iter().any(|l| l.starts_with("network test")));
    assert!(said.iter().any(|l| l.contains("archive node")));
}

// ---- the wallet program on its own ---------------------------------------------------------------------------------

#[test]
fn the_wallet_program_creates_shows_and_protects_a_wallet() {
    let d = Dir::new("walletcli");
    let w = d.path("w.wallet");
    let ws = w.to_str().unwrap();
    // passphrases typed twice must agree, and be long enough
    let (r, _) = cli(
        &["create", "--wallet", ws, "--birth", "5"],
        &["longenough1", "different22"],
    );
    assert!(r.unwrap_err().contains("differ"));
    let (r, _) = cli(
        &["create", "--wallet", ws, "--birth", "5"],
        &["short", "short"],
    );
    assert!(r.unwrap_err().contains("at least 8"));
    assert!(
        !w.exists(),
        "nothing is written when the passphrase is refused"
    );
    let (r, said) = cli(
        &["create", "--wallet", ws, "--birth", "5"],
        &["longenough1", "longenough1"],
    );
    r.unwrap();
    let addr = find(&said, "address:");
    assert!(addr.starts_with("tni1"));
    assert!(said.iter().any(|l| l.contains("start at height 5")));
    // it will not overwrite
    let (r, _) = cli(
        &["create", "--wallet", ws, "--birth", "5"],
        &["longenough1", "longenough1"],
    );
    assert!(r.unwrap_err().contains("already exists"));
    // address and seed need the passphrase
    let (r, said) = cli(&["address", "--wallet", ws], &["longenough1"]);
    r.unwrap();
    assert_eq!(said.last().unwrap(), &addr);
    let (r, _) = cli(&["address", "--wallet", ws], &["wrongwrong1"]);
    assert!(r.unwrap_err().contains("wrong passphrase"));
    let (r, said) = cli(&["seed", "--wallet", ws], &["longenough1"]);
    r.unwrap();
    assert!(said.iter().any(|l| l.trim().len() == 64));
    // a restore needs exactly the seed
    let (r, _) = cli(
        &["restore", "--wallet", d.path("r.wallet").to_str().unwrap()],
        &["nothex", "longenough1", "longenough1"],
    );
    assert!(r.unwrap_err().contains("64 hexadecimal"));
    // without a node, a create that must ask the node says what to do
    let (r, _) = cli(
        &["create", "--wallet", d.path("n.wallet").to_str().unwrap()],
        &["longenough1", "longenough1"],
    );
    assert!(r.unwrap_err().contains("--birth"));
}

#[test]
fn the_wallet_program_refuses_bad_requests_before_it_touches_a_node() {
    let d = Dir::new("walletbad");
    let w = d.path("w.wallet");
    let ws = w.to_str().unwrap();
    cli(
        &["create", "--wallet", ws, "--birth", "0"],
        &["longenough1", "longenough1"],
    )
    .0
    .unwrap();
    let addr = Mutex::new(String::new());
    let (_, said) = cli(&["address", "--wallet", ws], &["longenough1"]);
    *addr.lock().unwrap() = said.last().unwrap().clone();
    let a = addr.lock().unwrap().clone();
    let data = d.0.to_str().unwrap();
    for (args, expect) in [
        (
            vec![
                "pay", "--wallet", ws, "--data", data, "--to", "nobody", "--amount", "1",
            ],
            "--to",
        ),
        (
            vec![
                "pay",
                "--wallet",
                ws,
                "--data",
                data,
                "--to",
                a.as_str(),
                "--amount",
                "1.123456789",
            ],
            "not an amount",
        ),
        (
            vec![
                "pay",
                "--wallet",
                ws,
                "--data",
                data,
                "--to",
                a.as_str(),
                "--amount",
                "-1",
            ],
            "not an amount",
        ),
        (
            vec!["pay", "--wallet", ws, "--data", data, "--to", a.as_str()],
            "--amount is required",
        ),
        (
            vec!["pay", "--wallet", ws, "--data", data, "--amount", "1"],
            "--to is required",
        ),
        (vec!["balance", "--wallet", ws], "--data"),
        (vec!["bogus"], "unknown command"),
        (vec!["address"], "--wallet is required"),
        (
            vec!["address", "--wallet", ws, "--wallet", ws],
            "given twice",
        ),
        (
            vec!["address", "--wallet", ws, "--nope", "1"],
            "unknown option",
        ),
        (vec!["address", "--wallet"], "needs a value"),
        (vec!["address", "wallet"], "unexpected argument"),
        (
            vec!["address", "--wallet", ws, "--control", "nowhere"],
            "not ip:port",
        ),
        (
            vec!["address", "--wallet", ws, "--birth", "x"],
            "not a height",
        ),
        (vec![], "tenero-wallet"),
    ] {
        let (r, _) = cli(&args, &["longenough1"]);
        let e = r.unwrap_err();
        assert!(e.contains(expect), "{args:?}: {e}");
    }
    // no node running at the control address: a message that says so
    let nowhere: SocketAddr = "127.0.0.1:9".parse().unwrap();
    std::fs::write(d.path("control.cookie"), "ab".repeat(32)).unwrap();
    let (r, _) = cli(
        &[
            "balance",
            "--wallet",
            ws,
            "--data",
            data,
            "--control",
            &nowhere.to_string(),
        ],
        &["longenough1"],
    );
    assert!(r.unwrap_err().contains("cannot reach the node"));
    // a non-loopback control address is refused, the cookie never leaves
    let (r, _) = cli(
        &[
            "balance",
            "--wallet",
            ws,
            "--data",
            data,
            "--control",
            "192.0.2.1:9",
        ],
        &["longenough1"],
    );
    assert!(r.unwrap_err().contains("only reachable on this machine"));
    // help works and exits well
    let (r, said) = cli(&["help"], &[]);
    r.unwrap();
    assert!(said[0].contains("tenero-wallet"));
}

#[test]
fn the_assume_valid_setting_reaches_the_engine() {
    // A mines; B trusts a WRONG block id at height 3 (the engine refuses to sync from a peer whose chain does not
    // hash to the checkpoint), C trusts the right one. Only C ends up with the chain.
    let alice = tenero_wallet::Wallet::from_seed(&[1; 32], 0)
        .address()
        .to_text();
    let da = Dir::new("av-a");
    let a = Running::start(config(
        &da.0,
        &format!("mine = sha256\nmine_to = {alice}\nmine_pace = 1\n"),
    ));
    a.wait_height(6, 60);
    let id3 = {
        use tenero_wallet::ChainView;
        let b = a.client().block(3).unwrap().unwrap();
        assert_eq!(b.height, 3);
        b.id
    };
    let hex = |id: &[u8; 32]| id.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let a_p2p = a.ready.p2p.unwrap();
    let (db, dc) = (Dir::new("av-b"), Dir::new("av-c"));
    let b = Running::start(config(
        &db.0,
        &format!("seed = {a_p2p}\nassume_valid = 3:{}\n", "00".repeat(32)),
    ));
    let c = Running::start(config(
        &dc.0,
        &format!("seed = {a_p2p}\nassume_valid = 3:{}\n", hex(&id3)),
    ));
    c.wait_height(6, 60);
    assert!(b.log().contains("assume-valid is ON"));
    assert_eq!(
        b.height(),
        0,
        "a checkpoint that is not on A's chain: nothing is taken from A\n{}",
        b.log()
    );
}
