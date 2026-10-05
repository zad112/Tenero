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
