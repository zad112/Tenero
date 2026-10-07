//! The wallet app's settings file: defaults, a round trip, and that a mistake is an error that names the key.

use std::path::Path;

use tenero_app::config::Network;
use tenero_gui::procs::{miner_args, node_args};
use tenero_gui::settings::{MinerBackend, NodeKind, Settings};

fn app() -> &'static Path {
    Path::new("appdir")
}

#[test]
fn the_defaults_are_the_test_network_an_archive_node_and_a_miner_that_is_not_started() {
    let s = Settings::parse(app(), "").unwrap();
    assert_eq!(s, Settings::defaults(app(), Network::Test));
    assert_eq!(s.network, Network::Test);
    assert_eq!(s.node_kind, NodeKind::Archive);
    assert_eq!(s.control.to_string(), "127.0.0.1:18332");
    assert_eq!(s.miner_backend, MinerBackend::Sha256);
    assert!(s.data_dir.ends_with("node") && s.wallet_file.ends_with("wallet-test.twl"));
    let d = Settings::defaults(app(), Network::Dev);
    assert_eq!(d.control.to_string(), "127.0.0.1:28332");
    assert_eq!(d.miner_backend, MinerBackend::Gpu);
    assert_ne!(d.wallet_file, s.wallet_file, "one wallet file per network");
    assert_ne!(d.data_dir, s.data_dir);
}

#[test]
fn settings_survive_the_text_form() {
    let mut s = Settings::defaults(app(), Network::Dev);
    s.node_kind = NodeKind::Pruned { keep: 5000 };
    s.seeds = vec!["seed1.example:1234".into(), "10.0.0.5:9".into()];
    s.listen = Some("0.0.0.0:18333".into());
    s.inbound_port = Some(38333);
    s.external_node = true;
    s.miner_backend = MinerBackend::Cpu;
    s.miner_cores = 3;
    s.miner_gpu_device = 1;
    s.miner_gpu_auto_batch = true;
    s.miner_pace_secs = 7;
    s.miner_account = 2;
    s.program_dir = Some("C:/tenero/bin".into());
    assert_eq!(Settings::parse(app(), &s.to_text()).unwrap(), s);
}

#[test]
fn a_mistake_is_an_error_that_says_which_setting() {
    let bad = |text: &str, needle: &str| {
        let e = Settings::parse(app(), text).unwrap_err();
        assert!(
            e.contains(needle),
            "`{text}` gave `{e}`, expected it to mention `{needle}`"
        );
    };
    bad("netwrok = test", "unknown setting");
    bad("network = main", "network");
    bad("network = test\nnetwork = dev", "twice");
    bad("control = 8.8.8.8:18332", "loopback");
    bad("control = nonsense", "control");
    bad("node_kind = pruned:10", "too few");
    bad("node_kind = sometimes", "node_kind");
    bad("miner_cores = 0", "1 to 6");
    bad("miner_cores = 7", "1 to 6");
    bad("miner_backend = gpu", "cannot mine"); // the test network is SHA-256
    bad("network = dev\nminer_backend = sha256", "cannot mine");
    bad("external_node = maybe", "yes or no");
    bad("seed = two words", "host:port");
    bad("inbound_port = 0", "port");
    bad("inbound_port = 70000", "port");
    bad("inbound_port = web", "port");
    bad("just a line", "key = value");
    // comments and blank lines are fine, and so is a repeated seed
    let s = Settings::parse(app(), "# hi\n\nseed = a:1\nseed = b:2\n").unwrap();
    assert_eq!(s.seeds, ["a:1", "b:2"]);
}

#[test]
fn the_file_is_written_and_read_back_and_a_missing_one_gives_the_defaults() {
    let dir = std::env::temp_dir().join(format!("tenero-gui-settings-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        Settings::load(&dir).unwrap(),
        Settings::defaults(&dir, Network::Test)
    );
    let mut s = Settings::defaults(&dir, Network::Test);
    s.miner_pace_secs = 11;
    s.save(&dir).unwrap();
    assert_eq!(Settings::load(&dir).unwrap(), s);
    std::fs::write(dir.join("settings.conf"), "bogus = 1").unwrap();
    let e = Settings::load(&dir).unwrap_err();
    assert!(e.contains("settings.conf") && e.contains("bogus"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn what_the_user_changed_is_laid_over_the_settings_as_they_are_now() {
    let base = Settings::defaults(app(), Network::Alpha);
    // the real settings moved on while the draft was open: another wallet file, another mining account
    let mut now = base.clone();
    now.wallet_file = "appdir/wallets-alpha/Main wallet.twl".into();
    now.miner_account = 2;
    // the user changed two things on the draft and left the rest alone
    let mut draft = base.clone();
    draft.inbound_port = Some(38333);
    draft.seeds = vec!["194.238.27.60:38333".into()];
    let merged = now.with_changes(&base, &draft);
    assert_eq!(merged.inbound_port, Some(38333));
    assert_eq!(merged.seeds, ["194.238.27.60:38333"]);
    assert_eq!(
        merged.wallet_file, now.wallet_file,
        "untouched fields keep the current value"
    );
    assert_eq!(merged.miner_account, 2);
    // nothing changed: nothing is overwritten
    assert_eq!(now.with_changes(&base, &base), now);
    // a field the user did change wins, even over a newer value
    let mut edit = base.clone();
    edit.miner_account = 5;
    assert_eq!(now.with_changes(&base, &edit).miner_account, 5);
    // choosing another network resets the draft to that network's defaults: all of it is taken (wallet folder included)
    let other = Settings::defaults(app(), Network::Dev);
    let switched = now.with_changes(&base, &other);
    assert_eq!(switched.network, Network::Dev);
    assert_eq!(switched.wallets_dir, other.wallets_dir);
}

#[test]
fn the_tick_box_for_inbound_connections_listens_and_says_where_without_naming_an_ip() {
    let args = |s: &Settings| {
        node_args(s)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    };
    // off (the default): the node only dials out
    let mut s = Settings::defaults(app(), Network::Alpha);
    assert_eq!(s.inbound_port, None);
    let off = args(&s);
    assert!(
        !off.contains("--listen") && !off.contains("--advertise"),
        "{off}"
    );
    // on: it listens on the port and tells each peer "reach me on this port at the address you see me at", so a changing home IP needs nothing
    s.inbound_port = Some(38333);
    let on = args(&s);
    assert!(
        on.contains("--listen 0.0.0.0:38333") && on.contains("--advertise 0.0.0.0:38333"),
        "{on}"
    );
    assert_eq!(on.matches("--listen").count(), 1);
    // the older free-form `listen` alone still works, but tells nobody where to find it
    let mut old = Settings::defaults(app(), Network::Alpha);
    old.listen = Some("0.0.0.0:5555".into());
    let text = args(&old);
    assert!(
        text.contains("--listen 0.0.0.0:5555") && !text.contains("--advertise"),
        "{text}"
    );
    // both set: the tick box wins, and `--listen` is never given twice (the node refuses a repeated setting)
    s.listen = Some("0.0.0.0:5555".into());
    let both = args(&s);
    assert_eq!(both.matches("--listen").count(), 1, "{both}");
    assert!(both.contains("--listen 0.0.0.0:38333"));
    // the port each network suggests is the one its seed uses
    assert_eq!(
        tenero_gui::settings::default_inbound_port(Network::Alpha),
        38333
    );
    assert_eq!(
        tenero_gui::settings::default_inbound_port(Network::Test),
        18331
    );
}

#[test]
fn the_node_is_started_with_no_mining_and_the_miner_gets_only_what_its_backend_uses() {
    let mut s = Settings::defaults(app(), Network::Test);
    s.seeds = vec!["a:1".into()];
    let args: Vec<String> = node_args(&s)
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let text = args.join(" ");
    assert!(
        text.contains("--network test")
            && text.contains("--seed a:1")
            && text.contains("--color never")
    );
    assert!(
        !text.contains("--mine"),
        "the node never mines by itself here: {text}"
    );
    assert!(text.contains("--prune_keep 0"));
    s.node_kind = NodeKind::Pruned { keep: 2000 };
    let text = node_args(&s)
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains("--prune_keep 2000"));

    let m = |s: &Settings| {
        miner_args(s, "tni1abc", Path::new("st.txt"))
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    };
    let sha = m(&s);
    assert!(
        sha.contains("--backend sha256")
            && sha.contains("--status-file st.txt")
            && !sha.contains("--cores")
            && !sha.contains("--gpu")
    );
    let mut d = Settings::defaults(app(), Network::Dev);
    d.miner_backend = MinerBackend::Cpu;
    d.miner_cores = 3;
    assert!(m(&d).contains("--cores 3") && !m(&d).contains("--gpu-device"));
    d.miner_backend = MinerBackend::Gpu;
    d.miner_gpu_auto_batch = true;
    let g = m(&d);
    assert!(
        g.contains("--gpu-device 0") && g.contains("--gpu-batch auto") && !g.contains("--cores")
    );
}

// ---- mining for a pool ----------------------------------------------------------------------------------------------------

use tenero_gui::settings::MiningMode;

const KEY: &str = "0f0e0d0c0b0a09080706050403020100ffeeddccbbaa99887766554433221100";

#[test]
fn the_pool_settings_default_to_mining_alone_and_survive_the_text_form() {
    let s = Settings::defaults(app(), Network::Beta);
    assert_eq!(
        s.mining_mode,
        MiningMode::Solo,
        "a person who has chosen nothing mines alone"
    );
    assert!(s.pool.is_empty() && s.pool_key.is_empty() && s.pool_worker.is_empty());
    let mut p = s.clone();
    p.mining_mode = MiningMode::Pool;
    p.pool = "pool.example:38335".into();
    p.pool_key = KEY.into();
    p.pool_worker = "garage rig".into();
    let text = p.to_text();
    assert!(text.contains("mining_mode = pool") && text.contains("pool = pool.example:38335"));
    assert_eq!(Settings::parse(app(), &text).unwrap(), p);
    // a pool setting is not written when it is empty
    assert!(!s.to_text().contains("pool ="));
}

#[test]
fn a_mistake_in_a_pool_setting_is_an_error_that_names_it() {
    let bad = |t: &str| Settings::parse(app(), t).unwrap_err();
    assert!(bad("mining_mode = both").contains("mining_mode"));
    assert!(bad("pool = no-port-here").contains("pool"));
    assert!(bad("pool = a b:1").contains("pool"));
    assert!(bad("pool_key = 1234").contains("pool_key"));
    assert!(bad(&format!("pool_key = {}z", &KEY[..63])).contains("pool_key"));
    assert!(bad(&format!("pool_worker = {}", "w".repeat(33))).contains("pool_worker"));
    assert!(bad("pool_worker = a\u{7}b").contains("pool_worker"));
    // an empty pool means the built-in one, and the key may be written in capitals
    let s = Settings::parse(app(), &format!("pool =\npool_key = {}", KEY.to_uppercase())).unwrap();
    assert!(s.pool.is_empty());
    assert_eq!(s.pool_key, KEY);
}

#[test]
fn only_the_pool_fields_the_person_changed_are_taken_from_the_screen() {
    let base = Settings::defaults(app(), Network::Beta);
    let mut now = base.clone();
    now.miner_account = 2; // changed meanwhile, on another screen
    let mut draft = base.clone();
    draft.mining_mode = MiningMode::Pool;
    draft.pool = "p:1".into();
    let merged = now.with_changes(&base, &draft);
    assert_eq!(
        (merged.mining_mode, merged.pool.as_str()),
        (MiningMode::Pool, "p:1")
    );
    assert_eq!(merged.miner_account, 2, "what changed elsewhere stays");
}

#[test]
fn a_pool_miner_is_given_a_pool_a_network_and_no_node() {
    let mut s = Settings::defaults(app(), Network::Beta);
    s.mining_mode = MiningMode::Pool;
    s.miner_backend = MinerBackend::Gpu;
    // the program's own pool: no address, no key
    let a: Vec<String> = miner_args(&s, "tni1abc", Path::new("st.txt"))
        .iter()
        .map(|x| x.to_string_lossy().into_owned())
        .collect();
    let at = |k: &str| a.iter().position(|x| x == k).map(|i| a[i + 1].clone());
    assert_eq!(at("--pool").as_deref(), Some("default"));
    assert_eq!(at("--network").as_deref(), Some("beta"));
    assert_eq!(at("--address").as_deref(), Some("tni1abc"));
    assert!(at("--pool-key").is_none() && at("--worker").is_none());
    for node_only in ["--data", "--control", "--pace"] {
        assert!(
            !a.iter().any(|x| x == node_only),
            "{node_only} is for a node: {a:?}"
        );
    }
    // a pool of the person's choosing, with its key pinned and a name for this computer
    s.pool = "pool.example:38335".into();
    s.pool_key = KEY.into();
    s.pool_worker = "garage".into();
    let a: Vec<String> = miner_args(&s, "tni1abc", Path::new("st.txt"))
        .iter()
        .map(|x| x.to_string_lossy().into_owned())
        .collect();
    let at = |k: &str| a.iter().position(|x| x == k).map(|i| a[i + 1].clone());
    assert_eq!(at("--pool").as_deref(), Some("pool.example:38335"));
    assert_eq!(at("--pool-key").as_deref(), Some(KEY));
    assert_eq!(at("--worker").as_deref(), Some("garage"));
    // and mining alone still gets a node and no pool
    s.mining_mode = MiningMode::Solo;
    let a: Vec<String> = miner_args(&s, "tni1abc", Path::new("st.txt"))
        .iter()
        .map(|x| x.to_string_lossy().into_owned())
        .collect();
    assert!(a.iter().any(|x| x == "--data") && !a.iter().any(|x| x == "--pool"));
}
