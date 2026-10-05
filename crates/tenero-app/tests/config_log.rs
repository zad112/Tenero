//! Settings and logging.

use std::path::PathBuf;

use tenero_app::config::{
    check_seed_list, effective_seeds, Config, ConfigError, MineMode, Network, Raw, ALPHA_SEEDS,
};
use tenero_app::log::{utc_timestamp, Level, Logger};

fn parse(file: &str, args: &[&str]) -> Result<Config, ConfigError> {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    Raw::from_file_text(file)?.with_args(&args)?.into_config()
}

fn ok(file: &str) -> Config {
    parse(file, &[]).unwrap_or_else(|e| panic!("{e}"))
}

fn err(file: &str) -> String {
    parse(file, &[]).unwrap_err().0
}

const ADDRESS_OF_SEED_1: &str = "tni1ca69dc577e885b06854cfb655a2bdb5e7a4612b5bd0d874c11b13ad4d86577aebea3b307da85900a9500ab83f39fe2d40ce59dc6ce4f2c5957892d250441930376cce398";

// ---- defaults and the file format -------------------------------------------------------------------------------

#[test]
fn the_defaults_are_what_the_documentation_says() {
    let c = ok("data = d\nnetwork = test");
    assert_eq!(c.data, PathBuf::from("d"));
    assert_eq!(c.network, Network::Test);
    assert_eq!(c.listen, None);
    assert!(c.seeds.is_empty());
    assert_eq!((c.peer_target, c.max_inbound), (50, 64));
    assert!(
        c.allow_private_peers,
        "the test network may run on one machine"
    );
    assert_eq!(c.control, "127.0.0.1:18332".parse().unwrap());
    assert_eq!(c.prune_keep, 0, "an archive node unless told otherwise");
    assert_eq!(c.assume_valid, None, "assume-valid is off unless asked for");
    assert_eq!(c.mine, MineMode::Off);
    assert_eq!(c.mine_pace, 5);
    assert_eq!(c.mine_cores, 6);
    assert_eq!((c.gpu_device, c.gpu_batch), (0, 128));
    assert_eq!(c.log_level, Level::Info);
    assert_eq!(c.log_file, None);
    assert_eq!(c.status_every, 60);
    let d = ok("data = d\nnetwork = dev");
    assert_eq!(d.network, Network::Dev);
    assert_eq!(d.control, "127.0.0.1:28332".parse().unwrap());
    assert!(
        !d.allow_private_peers,
        "a real-looking network does not dial private addresses"
    );
    assert_eq!(d.mine_pace, 0);
}

#[test]
fn the_file_may_have_comments_blank_lines_and_spaces() {
    let c = ok(
        "# a node\n\n  data   =  some dir/x  # where\nnetwork=dev\n\t\nlisten = 0.0.0.0:8333 \n",
    );
    assert_eq!(c.data, PathBuf::from("some dir/x"));
    assert_eq!(c.listen, Some("0.0.0.0:8333".parse().unwrap()));
}

#[test]
fn seeds_may_repeat_and_nothing_else_may() {
    let c = ok("data=d\nnetwork=dev\nseed=1.2.3.4:5\nseed = 6.7.8.9:10");
    assert_eq!(c.seeds, vec!["1.2.3.4:5", "6.7.8.9:10"]);
    assert!(err("data=d\ndata=e\nnetwork=dev").contains("`data`: given twice"));
    assert!(err("data=d\nnetwork=dev\nnetwork=test").contains("given twice"));
}

#[test]
fn a_typo_is_an_error_not_a_silent_default() {
    assert!(err("data=d\nnetwork=dev\npeer = 5").contains("unknown setting `peer`"));
    assert!(err("data=d\nnetwork=dev\nthis is not a setting").contains("line 3"));
    assert!(err("data=d\nnetwork=dev\nlisten =").contains("no value"));
    assert!(parse("data=d\nnetwork=dev", &["--nope", "1"])
        .unwrap_err()
        .0
        .contains("unknown option"));
    assert!(parse("data=d\nnetwork=dev", &["--peers"])
        .unwrap_err()
        .0
        .contains("needs a value"));
    assert!(parse("data=d\nnetwork=dev", &["peers", "3"])
        .unwrap_err()
        .0
        .contains("unexpected argument"));
}

#[test]
fn the_command_line_overrides_the_file() {
    let c = parse(
        "data=d\nnetwork=dev\npeers=10\nseed=1.1.1.1:1",
        &["--peers", "20", "--network", "test"],
    )
    .unwrap();
    assert_eq!((c.peer_target, c.network), (20, Network::Test));
    assert_eq!(
        c.seeds,
        vec!["1.1.1.1:1"],
        "no --seed on the command line: the file's stay"
    );
    // a command line that names seeds means exactly those
    let c = parse(
        "data=d\nnetwork=dev\nseed=1.1.1.1:1",
        &["--seed", "2.2.2.2:2", "--seed", "3.3.3.3:3"],
    )
    .unwrap();
    assert_eq!(c.seeds, vec!["2.2.2.2:2", "3.3.3.3:3"]);
    // a setting twice on the command line is a mistake
    assert!(
        parse("data=d\nnetwork=dev", &["--peers", "1", "--peers", "2"])
            .unwrap_err()
            .0
            .contains("given twice")
    );
    // and everything can come from the command line alone
    let c = parse("", &["--data", "x", "--network", "test"]).unwrap();
    assert_eq!(c.data, PathBuf::from("x"));
}

// ---- validation -------------------------------------------------------------------------------------------------

#[test]
fn the_network_and_the_data_directory_are_required() {
    assert!(err("network=dev").contains("`data`"));
    assert!(err("data=d").contains("`network`"));
    assert!(err("data=d\nnetwork=main").contains("not `test`, `dev` or `alpha`"));
}

#[test]
fn the_control_interface_cannot_be_exposed() {
    for bad in ["0.0.0.0:1", "192.168.1.5:1", "[::]:1", "8.8.8.8:80"] {
        let e = err(&format!("data=d\nnetwork=dev\ncontrol={bad}"));
        assert!(e.contains("loopback"), "{bad}: {e}");
    }
    assert!(err("data=d\nnetwork=dev\ncontrol=localhost").contains("not ip:port"));
    assert_eq!(
        ok("data=d\nnetwork=dev\ncontrol=127.0.0.1:9")
            .control
            .port(),
        9
    );
    assert!(ok("data=d\nnetwork=dev\ncontrol=[::1]:9")
        .control
        .ip()
        .is_loopback());
}

#[test]
fn numbers_are_checked() {
    assert!(err("data=d\nnetwork=dev\npeers=0").contains("at least 1"));
    assert!(err("data=d\nnetwork=dev\npeers=many").contains("not valid"));
    assert!(err("data=d\nnetwork=dev\nmax_inbound=-1").contains("not valid"));
    assert!(err("data=d\nnetwork=dev\nstatus_every=0").contains("at least 1"));
    assert!(err("data=d\nnetwork=dev\ngpu_batch=0").contains("at least 1"));
    assert!(err("data=d\nnetwork=dev\nmine_cores=0").contains("1 to"));
    assert!(err("data=d\nnetwork=dev\nmine_cores=7").contains("1 to"));
    assert_eq!(ok("data=d\nnetwork=dev\nmine_cores=6").mine_cores, 6);
    assert_eq!(ok("data=d\nnetwork=dev\nmine_cores=1").mine_cores, 1);
    assert!(err("data=d\nnetwork=dev\nlog_level=loud").contains("not error"));
    assert_eq!(
        ok("data=d\nnetwork=dev\nlog_level=debug").log_level,
        Level::Debug
    );
    assert!(err("data=d\nnetwork=dev\nlisten=nowhere").contains("not ip:port"));
    assert!(err("data=d\nnetwork=dev\nseed=nowhere").contains("not ip:port"));
    assert!(
        err("data=d\nnetwork=dev\nlisten=0.0.0.0:1\nadvertise=nowhere").contains("not ip:port")
    );
    assert_eq!(ok("data=d\nnetwork=dev").advertise, None);
    assert_eq!(
        ok("data=d\nnetwork=dev\nlisten=0.0.0.0:8333\nadvertise=203.0.113.9:8333")
            .advertise
            .as_deref(),
        Some("203.0.113.9:8333")
    );
    // `0.0.0.0:PORT` is "the address you see me at": a node at home does not know its IP, which changes
    assert_eq!(
        ok("data=d\nnetwork=dev\nlisten=0.0.0.0:8333\nadvertise=0.0.0.0:8333")
            .advertise
            .as_deref(),
        Some("0.0.0.0:8333")
    );
    // announcing an address while not listening is a mistake, said plainly
    let e = err("data=d\nnetwork=dev\nadvertise=203.0.113.9:8333");
    assert!(
        e.contains("advertise") && e.contains("not listening"),
        "{e}"
    );
    assert!(err("data=d\nnetwork=dev\nallow_private_peers=maybe").contains("not yes or no"));
    assert!(ok("data=d\nnetwork=dev\nallow_private_peers=yes").allow_private_peers);
    assert!(!ok("data=d\nnetwork=test\nallow_private_peers=off").allow_private_peers);
}

#[test]
fn a_pruned_node_keeps_a_sensible_history() {
    assert_eq!(ok("data=d\nnetwork=dev\nprune_keep=0").prune_keep, 0);
    assert_eq!(ok("data=d\nnetwork=dev\nprune_keep=1000").prune_keep, 1000);
    assert_eq!(ok("data=d\nnetwork=dev\nprune_keep=5500").prune_keep, 5500);
    for bad in ["1", "999"] {
        let e = err(&format!("data=d\nnetwork=dev\nprune_keep={bad}"));
        assert!(e.contains("at least 1000"), "{bad}: {e}");
    }
}

#[test]
fn assume_valid_is_parsed_exactly() {
    let id = "ab".repeat(32);
    let c = ok(&format!("data=d\nnetwork=dev\nassume_valid=1234:{id}"));
    assert_eq!(c.assume_valid, Some((1234, [0xab; 32])));
    for bad in [
        "1234".to_string(),
        format!("x:{id}"),
        format!("-1:{id}"),
        "1:abcd".to_string(),
        format!("1:{}", "AB".repeat(32)),
        format!("1:{}g", "a".repeat(63)),
        format!("1:{id}00"),
    ] {
        assert!(
            parse(&format!("data=d\nnetwork=dev\nassume_valid={bad}"), &[]).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn mining_needs_the_right_proof_of_work_and_a_valid_address() {
    // sha256 is the test network's, cpu and gpu are the real proof of work's
    assert!(err("data=d\nnetwork=dev\nmine=sha256").contains("test network's"));
    assert!(err("data=d\nnetwork=test\nmine=cpu").contains("sha256"));
    assert!(err("data=d\nnetwork=test\nmine=gpu").contains("sha256"));
    assert!(err("data=d\nnetwork=test\nmine=fast").contains("not off"));
    // an address to pay is required, and must be a real one
    assert!(err("data=d\nnetwork=test\nmine=sha256").contains("`mine_to`"));
    assert!(err("data=d\nnetwork=test\nmine=sha256\nmine_to=nobody").contains("mine_to"));
    let mut wrong_checksum = ADDRESS_OF_SEED_1.to_string();
    wrong_checksum.replace_range(10..11, "0");
    assert!(err(&format!(
        "data=d\nnetwork=test\nmine=sha256\nmine_to={wrong_checksum}"
    ))
    .contains("checksum"));
    let c = ok(&format!(
        "data=d\nnetwork=test\nmine=sha256\nmine_to={ADDRESS_OF_SEED_1}"
    ));
    assert_eq!(c.mine, MineMode::Sha256);
    assert_eq!(c.mine_to.as_deref(), Some(ADDRESS_OF_SEED_1));
    assert_eq!(
        ok(&format!(
            "data=d\nnetwork=dev\nmine=gpu\nmine_to={ADDRESS_OF_SEED_1}"
        ))
        .mine,
        MineMode::Gpu
    );
    assert_eq!(
        ok(&format!(
            "data=d\nnetwork=dev\nmine=cpu\nmine_to={ADDRESS_OF_SEED_1}"
        ))
        .mine,
        MineMode::Cpu
    );
    // not mining: no address needed
    assert_eq!(ok("data=d\nnetwork=dev\nmine=off").mine, MineMode::Off);
}

// ---- logging ----------------------------------------------------------------------------------------------------

#[test]
fn utc_timestamps_are_right() {
    for (secs, text) in [
        (0u64, "1970-01-01T00:00:00Z"),
        (86_399, "1970-01-01T23:59:59Z"),
        (86_400, "1970-01-02T00:00:00Z"),
        (951_782_400, "2000-02-29T00:00:00Z"),
        (1_700_000_000, "2023-11-14T22:13:20Z"),
        (1_709_164_800, "2024-02-29T00:00:00Z"),
        (1_709_251_200, "2024-03-01T00:00:00Z"),
        (4_102_444_800, "2100-01-01T00:00:00Z"),
        (4_107_542_400, "2100-03-01T00:00:00Z"),
        (253_402_300_799, "9999-12-31T23:59:59Z"),
    ] {
        assert_eq!(utc_timestamp(secs), text, "{secs}");
    }
}

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("tenero-log-{}-{name}", std::process::id()))
}

#[test]
fn a_logger_writes_lines_with_a_time_and_a_level_and_filters_by_level() {
    let path = tmp("a.log");
    let _ = std::fs::remove_file(&path);
    let log = Logger::new(Level::Info, Some(&path), false).unwrap();
    log.error("bad");
    log.warn("careful");
    log.info("fine");
    log.debug("noise");
    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    assert!(lines[0].ends_with("ERROR bad"), "{}", lines[0]);
    assert!(lines[1].ends_with("WARN  careful"));
    assert!(lines[2].ends_with("INFO  fine"));
    for l in &lines {
        // 2026-10-02T09:05:03Z LEVEL message
        assert_eq!(&l[4..5], "-");
        assert_eq!(&l[10..11], "T");
        assert_eq!(&l[19..20], "Z");
    }
    assert!(!log.enabled(Level::Debug));
    assert!(log.enabled(Level::Error));
    let quiet = Logger::new(Level::Error, Some(&path), false).unwrap();
    quiet.warn("hidden");
    assert!(!std::fs::read_to_string(&path).unwrap().contains("hidden"));
    let chatty = Logger::new(Level::Debug, Some(&path), false).unwrap();
    chatty.debug("shown");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("DEBUG shown"));
    assert!(text.contains("fine"), "a logger appends to what is there");
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn a_message_cannot_forge_another_log_line() {
    let path = tmp("b.log");
    let _ = std::fs::remove_file(&path);
    let log = Logger::new(Level::Info, Some(&path), false).unwrap();
    log.info("peer said: hi\n2026-01-01T00:00:00Z ERROR everything is fine\r\u{7}");
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 1, "{text:?}");
    assert!(!text.contains('\r') && !text.contains('\u{7}'));
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn the_log_file_is_rotated_when_it_grows_too_big() {
    let path = tmp("c.log");
    let old = tmp("c.log.old");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&old);
    let log = Logger::with_rotation(Level::Info, Some(&path), false, 400).unwrap();
    for i in 0..40 {
        log.info(&format!("line number {i:03}"));
    }
    assert!(old.exists(), "the full file was moved aside");
    let (a, b) = (
        std::fs::metadata(&path).unwrap().len(),
        std::fs::metadata(&old).unwrap().len(),
    );
    assert!(a <= 400 && b <= 400 + 60, "{a} and {b} bytes");
    // the newest lines are in the current file, the file before it in `.old`
    let now = std::fs::read_to_string(&path).unwrap();
    assert!(now.contains("line number 039"));
    assert!(std::fs::read_to_string(&old)
        .unwrap()
        .contains("line number"));
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(&old).unwrap();
}

#[test]
fn a_log_file_in_a_missing_directory_is_an_error() {
    let path = std::env::temp_dir()
        .join("tenero-no-such-dir-abc")
        .join("x.log");
    assert!(Logger::new(Level::Info, Some(&path), false).is_err());
}

// ---- pinned peers (M9, threat model C1) -----------------------------------------------------------------------------

#[test]
fn pinned_peers_may_repeat_default_to_none_and_must_be_ip_and_port() {
    assert!(ok("data=d\nnetwork=dev").trusted_peers.is_empty());
    let c = ok("data=d\nnetwork=dev\ntrusted_peer=1.2.3.4:5\ntrusted_peer = [2001:db8::1]:6");
    assert_eq!(c.trusted_peers, vec!["1.2.3.4:5", "[2001:db8::1]:6"]);
    for bad in [
        "nowhere",
        "1.2.3.4",
        "1.2.3.4:0",
        "1.2.3.4:99999",
        "example.org:5",
    ] {
        let e = err(&format!("data=d\nnetwork=dev\ntrusted_peer={bad}"));
        assert!(
            e.contains("trusted_peer") && e.contains("not ip:port"),
            "{bad}: {e}"
        );
    }
    // not too many
    let many: String = (1..=17)
        .map(|i| format!("trusted_peer=1.2.3.{i}:5\n"))
        .collect();
    assert!(err(&format!("data=d\nnetwork=dev\n{many}")).contains("at most 16"));
    let sixteen: String = (1..=16)
        .map(|i| format!("trusted_peer=1.2.3.{i}:5\n"))
        .collect();
    assert_eq!(
        ok(&format!("data=d\nnetwork=dev\n{sixteen}"))
            .trusted_peers
            .len(),
        16
    );
}

#[test]
fn a_command_line_that_names_pinned_peers_means_exactly_those_and_seeds_are_separate() {
    let c = parse(
        "data=d\nnetwork=dev\ntrusted_peer=1.1.1.1:1\nseed=9.9.9.9:9",
        &[],
    )
    .unwrap();
    assert_eq!(c.trusted_peers, vec!["1.1.1.1:1"]);
    let c = parse(
        "data=d\nnetwork=dev\ntrusted_peer=1.1.1.1:1\nseed=9.9.9.9:9",
        &["--trusted_peer", "2.2.2.2:2", "--trusted_peer", "3.3.3.3:3"],
    )
    .unwrap();
    assert_eq!(c.trusted_peers, vec!["2.2.2.2:2", "3.3.3.3:3"]);
    assert_eq!(
        c.seeds,
        vec!["9.9.9.9:9"],
        "the seeds are untouched by pinned peers on the command line"
    );
    // and the other way round
    let c = parse(
        "data=d\nnetwork=dev\ntrusted_peer=1.1.1.1:1\nseed=9.9.9.9:9",
        &["--seed", "8.8.8.8:8"],
    )
    .unwrap();
    assert_eq!(c.seeds, vec!["8.8.8.8:8"]);
    assert_eq!(c.trusted_peers, vec!["1.1.1.1:1"]);
    // a key that is not repeatable is still refused twice
    assert!(parse("data=d\nnetwork=dev", &["--peers", "5", "--peers", "6"]).is_err());
}

#[test]
fn gpu_batch_can_be_a_number_or_auto() {
    let c = ok("data = d
network = dev
gpu_batch = 256");
    assert_eq!((c.gpu_batch, c.gpu_batch_auto), (256, false));
    let c = ok("data = d
network = dev
gpu_batch = auto");
    assert_eq!(
        (c.gpu_batch, c.gpu_batch_auto),
        (128, true),
        "the fallback is the default"
    );
    let c = ok("data = d
network = dev");
    assert_eq!((c.gpu_batch, c.gpu_batch_auto), (128, false));
    // anything else is still an error that names the setting
    assert!(err("data = d
network = dev
gpu_batch = fast")
    .contains("gpu_batch"));
}

// ---- seeds: the program's own list, and the operator's -------------------------------------------------------------

#[test]
fn the_seeds_a_node_starts_from_are_the_built_in_ones_then_the_configured_ones_each_once() {
    let configured = vec!["203.0.113.9:1".to_string(), "198.51.100.7:1".to_string()];
    let builtin = ["198.51.100.7:1", "192.0.2.5:1"];
    assert_eq!(
        effective_seeds(&builtin, &configured, true),
        ["198.51.100.7:1", "192.0.2.5:1", "203.0.113.9:1"]
    );
    // `no_builtin_seeds yes`: only the operator's own
    assert_eq!(
        effective_seeds(&builtin, &configured, false),
        ["203.0.113.9:1", "198.51.100.7:1"]
    );
    assert!(effective_seeds(&[], &[], true).is_empty());
}

#[test]
fn a_seed_list_with_a_mistake_in_it_is_refused() {
    assert!(check_seed_list(&[]).is_ok());
    assert!(check_seed_list(&["8.8.8.8:38333", "9.9.9.9:38333"]).is_ok());
    for (list, why) in [
        (vec!["8.8.8.8"], "no port"),
        (vec!["8.8.8.8:0"], "port 0"),
        (vec!["seed.example:38333"], "a name, not an address"),
        (vec!["127.0.0.1:38333"], "loopback"),
        (vec!["192.168.1.5:38333"], "a private address"),
        (vec!["8.8.8.8:38333", "8.8.8.8:38333"], "listed twice"),
        (
            vec!["8.8.8.8:38333", "8.8.4.4:38333"],
            "one network group (8.8)",
        ),
    ] {
        assert!(check_seed_list(&list).is_err(), "{why}: {list:?}");
    }
}

#[test]
fn the_built_in_lists_of_every_network_pass_their_own_check() {
    assert!(check_seed_list(ALPHA_SEEDS).is_ok(), "ALPHA_SEEDS");
    for n in Network::ALL {
        assert!(check_seed_list(n.builtin_seeds()).is_ok(), "{n:?}");
    }
    // the private networks never carry built-in seeds
    assert!(Network::Test.builtin_seeds().is_empty());
    assert!(Network::Dev.builtin_seeds().is_empty());
}

#[test]
fn a_node_reads_the_built_in_seed_option_strictly() {
    assert_eq!(ok("data=d\nnetwork=alpha").seeds, ALPHA_SEEDS.to_vec());
    let c = ok("data=d\nnetwork=alpha\nseed=203.0.113.9:1\nno_builtin_seeds=yes");
    assert_eq!(c.seeds, ["203.0.113.9:1"]);
    assert!(err("data=d\nnetwork=alpha\nno_builtin_seeds=maybe").contains("no_builtin_seeds"));
    assert!(
        err("data=d\nnetwork=alpha\nno_builtin_seeds=yes\nno_builtin_seeds=no")
            .contains("no_builtin_seeds")
    );
}
