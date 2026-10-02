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
        // beside the data directory when that does not exist yet (the node makes it, and has to be the one to)
        let log_file = if cfg.data.exists() {
            cfg.data.join("node.log")
        } else {
            cfg.data.with_extension("log")
        };
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

// ---- the miner program, a process of its own ---------------------------------------------------------------------

struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn miner_cmd(node: &Running, data: &std::path::Path, extra: &[&str]) -> std::process::Command {
    let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_tenero-miner"));
    c.args(["--data", data.to_str().unwrap()])
        .args(["--control", &node.ready.control.to_string()])
        .args(extra)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    c
}

#[test]
fn the_miner_program_mines_for_a_node_in_another_process() {
    let dir = Dir::new("miner-proc");
    let node = Running::start(config(&dir.0, ""));
    let mut alice = tenero_wallet::Wallet::from_seed(&[1; 32], 0);
    let addr = alice.address().to_text();
    let mut child = Child(
        miner_cmd(
            &node,
            &dir.0,
            &["--address", &addr, "--backend", "sha256", "--pace", "0"],
        )
        .spawn()
        .unwrap(),
    );
    node.wait_height(8, 60);
    drop(node.client());
    // the node, which was not mining itself, has a chain made by the other process, and its rewards are Alice's
    {
        use tenero_wallet::ChainView;
        let c = node.client();
        alice.sync(&c).unwrap();
        let tip = c.tip().unwrap().0;
        assert_eq!(alice.owned().len() as u64, tip, "one reward in every block");
    }
    // stopping the miner leaves the node running and the chain where it was
    let h = node.height();
    let _ = child.0.kill();
    let _ = child.0.wait();
    thread::sleep(Duration::from_millis(500));
    assert!(
        node.height() <= h + 2,
        "the node carries on without the miner"
    );
    assert!(node.client().info().is_ok());
}

#[test]
fn the_miner_program_refuses_a_backend_that_does_not_fit_the_network_and_bad_settings() {
    let dir = Dir::new("miner-bad");
    let node = Running::start(config(&dir.0, ""));
    let addr = tenero_wallet::Wallet::from_seed(&[1; 32], 0)
        .address()
        .to_text();
    let run = |extra: &[&str]| {
        let out = miner_cmd(&node, &dir.0, extra).output().unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    };
    // the test network needs sha256
    let (code, err) = run(&["--address", &addr, "--backend", "gpu"]);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("needs sha256"), "{err}");
    let (code, err) = run(&["--address", &addr, "--backend", "cpu"]);
    assert_eq!(code, Some(2));
    assert!(err.contains("needs sha256"), "{err}");
    // settings
    for (args, expect) in [
        (vec!["--backend", "sha256"], "--address is required"),
        (
            vec!["--address", "nobody", "--backend", "sha256"],
            "--address",
        ),
        (vec!["--address", addr.as_str()], "--backend must be"),
        (
            vec!["--address", addr.as_str(), "--backend", "fast"],
            "--backend must be",
        ),
        (
            vec![
                "--address",
                addr.as_str(),
                "--backend",
                "sha256",
                "--cores",
                "7",
            ],
            "--cores must be",
        ),
        (
            vec![
                "--address",
                addr.as_str(),
                "--backend",
                "sha256",
                "--cores",
                "x",
            ],
            "not a number",
        ),
        (
            vec![
                "--address",
                addr.as_str(),
                "--backend",
                "sha256",
                "--nope",
                "1",
            ],
            "unknown option",
        ),
        (
            vec![
                "--address",
                addr.as_str(),
                "--backend",
                "sha256",
                "--pace",
                "1",
                "--pace",
                "2",
            ],
            "given twice",
        ),
        (
            vec![
                "--address",
                addr.as_str(),
                "--backend",
                "sha256",
                "--gpu-batch",
                "0",
            ],
            "at least 1",
        ),
    ] {
        let (code, err) = run(&args);
        assert_eq!(code, Some(2), "{args:?}: {err}");
        assert!(err.contains(expect), "{args:?}: {err}");
    }
    // help works
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tenero-miner"))
        .arg("help")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("tenero-miner"));
}

#[test]
fn a_miner_started_before_its_node_waits_and_does_not_give_up() {
    let dir = Dir::new("miner-first");
    let addr = tenero_wallet::Wallet::from_seed(&[1; 32], 0)
        .address()
        .to_text();
    // the node's data directory exists but no node runs: the miner has no cookie to read yet
    let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_tenero-miner"));
    c.args(["--data", dir.0.to_str().unwrap()])
        .args(["--control", "127.0.0.1:18399"])
        .args(["--address", &addr, "--backend", "sha256", "--pace", "0"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let mut child = Child(c.spawn().unwrap());
    thread::sleep(Duration::from_secs(3));
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "the miner gave up instead of waiting"
    );
}

#[test]
fn the_miner_program_refuses_sha256_on_the_dev_network() {
    let dir = Dir::new("miner-dev");
    let text = format!(
        "data = {}\nnetwork = dev\nlisten = 127.0.0.1:0\ncontrol = 127.0.0.1:0\n",
        dir.0.display()
    );
    let cfg = Raw::from_file_text(&text).unwrap().into_config().unwrap();
    let node = Running::start(cfg);
    let addr = tenero_wallet::Wallet::from_seed(&[1; 32], 0)
        .address()
        .to_text();
    let out = miner_cmd(&node, &dir.0, &["--address", &addr, "--backend", "sha256"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("needs cpu or gpu"), "{err}");
}

// ---- a real node refuses a transaction whose proofs are wrong --------------------------------------------------------

#[test]
fn a_real_node_refuses_every_tampered_copy_of_a_good_transaction_and_takes_the_good_one() {
    use rand_core::OsRng;
    use tenero_core::v2::Transaction;
    use tenero_wallet::{Submitter, Wallet};

    let alice = Wallet::from_seed(&[7; 32], 0);
    let mut alice_w = Wallet::from_seed(&[7; 32], 0);
    let bob = Wallet::from_seed(&[8; 32], 0).address();
    let dir = Dir::new("tamper");
    let node = Running::start(config(
        &dir.0,
        &format!(
            "mine = sha256\nmine_to = {}\nmine_pace = 1\n",
            alice.address().to_text()
        ),
    ));
    node.wait_height(10, 60);
    // the node's own mining goes on in the background, so build the payment, then try its copies, quickly
    let mut remote = node.client();
    alice_w.sync(&remote).unwrap();
    let built = alice_w
        .build_payment(&remote, &mut OsRng, &bob, 1_000_000)
        .unwrap();
    let good = built.tx;

    type Tamper = (&'static str, Box<dyn Fn(&mut Transaction)>);
    let n = good.prunable.proof_data.len();
    let tampers: Vec<Tamper> = vec![
        (
            "first byte of the proof data",
            Box::new(|t| t.prunable.proof_data[0] ^= 1),
        ),
        (
            "middle byte of the proof data",
            Box::new(move |t| t.prunable.proof_data[n / 2] ^= 1),
        ),
        (
            "last byte of the proof data",
            Box::new(move |t| t.prunable.proof_data[n - 1] ^= 1),
        ),
        ("the fee raised by one", Box::new(|t| t.prefix.fee += 1)),
        (
            "an output's one-time address",
            Box::new(|t| t.prefix.outputs[0].onetime_address[0] ^= 1),
        ),
        (
            "an output's amount commitment",
            Box::new(|t| t.prefix.outputs[0].amount_commitment[0] ^= 1),
        ),
        ("the extra field", Box::new(|t| t.prefix.extra.push(0))),
    ];
    let mut accepted = vec![];
    for (what, f) in &tampers {
        let mut bad = good.clone();
        f(&mut bad);
        match remote.submit(bad) {
            Err(why) => println!("refused ({what}): {why}"),
            Ok(()) => accepted.push(*what),
        }
    }
    assert!(
        accepted.is_empty(),
        "the node ACCEPTED a tampered transaction: {accepted:?}\n{}",
        node.log()
    );
    // none of them got into the pool or the chain; the good one is taken
    remote.submit(good).unwrap_or_else(|e| panic!("{e}"));
}

// ---- who can read the data directory (M9, threat model G1) --------------------------------------------------------------

#[cfg(windows)]
#[test]
fn a_node_refuses_a_data_directory_other_accounts_can_read_and_starts_with_the_override() {
    // a folder directly under C:\, which Windows opens to every user
    let open = PathBuf::from(format!(r"C:\tenero-open-node-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&open);
    if std::fs::create_dir(&open).is_err() {
        eprintln!("cannot make a directory in the root of C: here: test skipped");
        return;
    }
    struct Gone(PathBuf);
    impl Drop for Gone {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _gone = Gone(open.clone());
    let refused = Running::try_start(config(&open, ""));
    let err = match refused {
        Ok(_) => panic!("the node started in a directory every user can read"),
        Err(e) => e,
    };
    assert!(err.contains("open to other accounts"), "{err}");
    assert!(err.contains("allow_open_data_dir"), "{err}");
    // the override starts it, with a warning in the log
    let node = Running::start(config(&open, "allow_open_data_dir = yes\n"));
    assert!(
        node.log().contains("open to other accounts"),
        "{}",
        node.log()
    );
    node.stop();
}

#[test]
fn a_node_in_a_new_directory_makes_it_private_and_starts() {
    let dir = Dir::new("private-start");
    std::fs::remove_dir_all(&dir.0).unwrap();
    let node = Running::start(config(&dir.0, ""));
    assert_eq!(
        tenero_app::private_dir::check(&dir.0),
        tenero_app::private_dir::Exposure::Private
    );
    node.stop();
}

// ---- pinned peers (M9, threat model C1) -----------------------------------------------------------------------------------

#[test]
fn a_node_with_no_seeds_but_a_pinned_peer_finds_it_and_finds_it_again_after_it_restarts() {
    let da = Dir::new("pin-a");
    let a = Running::start(config(&da.0, ""));
    let a_p2p = a.ready.p2p.unwrap();
    let db = Dir::new("pin-b");
    // no seed, only the pin
    let b = Running::start(config(&db.0, &format!("trusted_peer = {a_p2p}\n")));
    assert!(b.log().contains("pinned peers"), "{}", b.log());
    let end = Instant::now() + Duration::from_secs(30);
    while b.client().info().unwrap().peers < 1 {
        assert!(
            Instant::now() < end,
            "never found its pinned peer\n{}",
            b.log()
        );
        thread::sleep(Duration::from_millis(100));
    }
    // the pinned peer goes away and comes back (on the same address)
    let a_cfg_dir = da.0.clone();
    a.stop();
    let end = Instant::now() + Duration::from_secs(30);
    while b.client().info().unwrap().peers > 0 {
        assert!(Instant::now() < end, "the dead peer was never dropped");
        thread::sleep(Duration::from_millis(100));
    }
    // (the same data directory and the same address, which `config` does not allow, so the settings are written out)
    let again = format!(
        "data = {}\nnetwork = test\nlisten = {a_p2p}\ncontrol = 127.0.0.1:0\n",
        a_cfg_dir.display()
    );
    let a2 = Running::start(Raw::from_file_text(&again).unwrap().into_config().unwrap());
    let _keep = &a2;
    let end = Instant::now() + Duration::from_secs(90);
    while b.client().info().unwrap().peers < 1 {
        assert!(
            Instant::now() < end,
            "the pinned peer was not dialled again\n{}",
            b.log()
        );
        thread::sleep(Duration::from_millis(200));
    }
}
