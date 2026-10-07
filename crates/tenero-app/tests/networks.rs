//! The release networks, "alpha" (M11.2) and "beta" (Beta.1, the fresh network after the hard fork), beside `test` (SHA-256) and `dev` (the development network). Its chain id, its parameters
//! and its genesis are decisions, written down in `docs/CONSENSUS_V2.md` and `docs/M10_M11_PLAN.md`, and held here so that none of them changes by accident.
//! **Still a test network: nothing on it has value, and nothing here is audited.**

use std::path::PathBuf;

use tenero_app::config::{Network, Raw};
use tenero_app::daemon;
use tenero_core::u256::U256;
use tenero_core::v2::ids::PowKind;
use tenero_store::Store;

/// The chain id of "tenero alpha network 1": SHA-256("tenero genesis id v2" || the genesis header), checked against the reference in
/// `tests/vectors/v2_genesis.json` below. A peer on any other chain is refused at the handshake because its id differs.
const ALPHA_CHAIN_ID: &str = "430ca70081d3e52c618fd9af46fecdf6d6fc8f7965dc8ed2aa53c92ecfe069d3";
/// The chain id of "tenero beta network 1", from the same reference vectors.
const BETA_CHAIN_ID: &str = "577cc63dfdf445eb26b712fa422b95a082ddcac2249e34de9489cb87be23641b";

fn hex(id: &[u8; 32]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

fn config(extra: &str) -> Result<tenero_app::config::Config, String> {
    config_for("alpha", extra)
}

fn config_for(network: &str, extra: &str) -> Result<tenero_app::config::Config, String> {
    Raw::from_file_text(&format!("data = d\nnetwork = {network}\n{extra}"))
        .and_then(|r| r.into_config())
        .map_err(|e| e.0)
}

#[test]
fn alpha_is_a_third_network_with_its_own_name_port_and_epoch() {
    assert_eq!(Network::parse("alpha"), Some(Network::Alpha));
    assert_eq!(Network::Alpha.name(), "alpha");
    assert_eq!(Network::parse("beta"), Some(Network::Beta));
    assert_eq!(Network::Beta.name(), "beta");
    assert_eq!(
        Network::ALL,
        [Network::Test, Network::Dev, Network::Beta, Network::Alpha]
    );
    assert!(Network::Alpha.real_pow() && Network::Dev.real_pow() && !Network::Test.real_pow());
    // four networks, four control ports: nodes of two networks on one machine do not meet
    let ports: Vec<u16> = Network::ALL
        .iter()
        .map(|n| n.default_control_port())
        .collect();
    assert_eq!(ports, vec![18332, 28332, 38342, 38332]);
    assert!(Network::Beta.real_pow());
    assert_eq!(Network::Beta.epoch_blocks(), 100);
    // the epoch of the real proof of work: 100 blocks, the owner's choice of 2026-10-04
    assert_eq!(Network::Alpha.epoch_blocks(), 100);
    assert_eq!(Network::Dev.epoch_blocks(), 100);
    assert_eq!(daemon::DEV_EPOCH_BLOCKS, 100);
}

#[test]
fn alpha_has_the_decided_parameters() {
    let p = daemon::params_of(Network::Alpha).unwrap();
    assert_eq!(p.label, "tenero alpha network 1");
    assert_eq!(p.label, daemon::ALPHA_LABEL);
    // alpha keeps the limits of alpha.4 (32 inputs, a proof of at most 32 KiB), so that this version and the older nodes on it agree
    assert!(p.legacy_tx_limits);
    assert_eq!(
        p.pow_kind,
        PowKind::Matmul,
        "the real proof of work, not SHA-256"
    );
    assert_eq!(p.difficulty.start_target, U256::pow2(237).unwrap());
    assert_eq!(p.difficulty.block_time, 60);
    assert_eq!(p.difficulty.window, 30);
    // 2^237 is about 524,000 attempts a block: at the measured 34,000 attempts a second of one RTX 5070 Ti, a block takes about 15 s,
    // never under a second (the placeholder 2^253 of the development network is 8 attempts, which is why 50 blocks came in 20 seconds)
    let work = U256::work_of_target(&p.difficulty.start_target).unwrap();
    let bytes = work.to_be_bytes();
    assert!(
        bytes[..24].iter().all(|b| *b == 0),
        "the work fits in 64 bits"
    );
    let attempts = u64::from_be_bytes(bytes[24..].try_into().unwrap());
    assert_eq!(attempts, 1 << 19);
    assert!(
        attempts as f64 / 34_000.0 > 10.0,
        "a first block takes more than 10 s on the owner's GPU"
    );
    let dev = daemon::params_of(Network::Dev).unwrap();
    assert_ne!(dev.difficulty.start_target, p.difficulty.start_target);
    // the rules that are the same on every network
    assert_eq!(p.emission, dev.emission);
    assert_eq!(p.ring_size, dev.ring_size);
    assert_eq!(p.coinbase_maturity, dev.coinbase_maturity);
}

#[test]
fn the_chain_ids_differ_and_alpha_is_the_one_in_the_reference_vectors() {
    let ids: Vec<[u8; 32]> = Network::ALL
        .iter()
        .map(|n| daemon::chain_id_of(*n).unwrap())
        .collect();
    for i in 0..ids.len() {
        for j in i + 1..ids.len() {
            assert_ne!(
                ids[i],
                ids[j],
                "{} and {} must not accept each other's chain",
                Network::ALL[i].name(),
                Network::ALL[j].name()
            );
        }
    }
    assert_eq!(hex(&ids[3]), ALPHA_CHAIN_ID);
    assert_eq!(hex(&ids[2]), BETA_CHAIN_ID);
    // and the independent Python reference gives the same id for the same label
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/vectors/v2_genesis.json");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let case = v["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["label"] == "tenero alpha network 1")
        .expect("the alpha label is in the vectors");
    assert_eq!(case["chain_id"], ALPHA_CHAIN_ID);
    let beta = v["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["label"] == "tenero beta network 1")
        .expect("the beta label is in the vectors");
    assert_eq!(beta["chain_id"], BETA_CHAIN_ID);
}

#[test]
fn beta_has_the_parameters_of_alpha_except_the_old_limits() {
    let a = daemon::params_of(Network::Alpha).unwrap();
    let b = daemon::params_of(Network::Beta).unwrap();
    assert_eq!(b.label, "tenero beta network 1");
    assert_eq!(b.label, daemon::BETA_LABEL);
    assert_eq!(b.pow_kind, PowKind::Matmul);
    assert_eq!(b.difficulty.start_target, a.difficulty.start_target);
    assert_eq!(b.emission, a.emission);
    assert_eq!(b.ring_size, a.ring_size);
    assert_eq!(b.coinbase_maturity, a.coinbase_maturity);
    assert_eq!(b.spend_maturity, a.spend_maturity);
    assert_eq!(b.min_block_median, a.min_block_median);
    // the one difference: beta has only the size limit of a transaction
    assert!(!b.legacy_tx_limits && a.legacy_tx_limits);
    // no network but alpha has the old limits
    for n in [Network::Test, Network::Dev, Network::Beta] {
        assert!(
            !daemon::params_of(n).unwrap().legacy_tx_limits,
            "{}",
            n.name()
        );
    }
}

/// The gather fork (decided by the owner 2026-10-07, beta then at height 280): beta and dev blocks need the gathered proof-of-work attempt from
/// height 500, alpha never; the SHA-256 test network has no matmulhash. Checked on the checker the node really builds.
#[test]
fn the_gather_fork_is_at_500_on_beta_and_dev_and_never_on_alpha() {
    use tenero_app::config::GATHER_FORK_HEIGHT;
    assert_eq!(GATHER_FORK_HEIGHT, 500);
    assert_eq!(daemon::gather_fork_of(Network::Beta).unwrap(), Some(500));
    assert_eq!(daemon::gather_fork_of(Network::Dev).unwrap(), Some(500));
    assert_eq!(
        daemon::gather_fork_of(Network::Alpha).unwrap(),
        Some(u64::MAX)
    );
    assert_eq!(daemon::gather_fork_of(Network::Test).unwrap(), None);
    for n in Network::ALL {
        if let Some(h) = daemon::gather_fork_of(n).unwrap() {
            assert_eq!(h, n.gather_from(), "{}", n.name());
        }
    }
}

/// NO PREMINE, checked on the real genesis of every network: it creates no output, so every coin comes from a mined block, the owner's included.
#[test]
fn the_genesis_of_every_network_creates_no_output() {
    for network in Network::ALL {
        let p = daemon::params_of(network).unwrap();
        let dir = std::env::temp_dir().join(format!(
            "tenero-alpha-test-{}-{}",
            std::process::id(),
            network.name()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(dir.join("chain.redb"), &p.label, p.pow_kind).unwrap();
        let (height, tip) = store.tip().unwrap();
        assert_eq!(height, 0, "{}", network.name());
        assert_eq!(
            tip.output_count,
            0,
            "{}: the genesis block made outputs",
            network.name()
        );
        assert_eq!(tip.tx_count, 0, "{}", network.name());
        assert_eq!(
            store.output_count().unwrap(),
            0,
            "{}: coins exist before block 1",
            network.name()
        );
        assert_eq!(tip.header.timestamp, 0, "{}", network.name());
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn settings_for_alpha() {
    let c = config("").unwrap();
    assert_eq!(c.network, Network::Alpha);
    assert_eq!(c.control.port(), 38332);
    assert!(
        !c.allow_private_peers,
        "a release network does not take 127.0.0.2-style peers by default"
    );
    // the real proof of work: cpu or gpu, never sha256
    let e = config("mine = sha256\nmine_to = x\n").unwrap_err();
    assert!(e.contains("alpha") && e.contains("cpu or gpu"), "{e}");
    let address = tenero_wallet::Wallet::from_seed(&[1; 32], 0)
        .address()
        .to_text();
    for mode in ["cpu", "gpu"] {
        let c = config(&format!("mine = {mode}\nmine_to = {address}\n")).unwrap();
        assert_eq!(c.network, Network::Alpha);
    }
}

#[test]
fn settings_for_beta() {
    let c = config_for("beta", "").unwrap();
    assert_eq!(c.network, Network::Beta);
    assert_eq!(c.control.port(), 38342);
    assert!(!c.allow_private_peers);
    // the author's two beta seeds are built in (not the alpha one: the old alpha seed's machine is the second beta seed, on the beta port),
    // and the list passes the checks a program can make (public, listed once, no two in one network group)
    assert_eq!(
        tenero_app::config::BETA_SEEDS,
        ["195.26.244.245:38343", "194.238.27.60:38343"]
    );
    assert!(tenero_app::config::check_seed_list(tenero_app::config::BETA_SEEDS).is_ok());
    assert_eq!(c.seeds, tenero_app::config::BETA_SEEDS.to_vec());
    assert!(!c
        .seeds
        .iter()
        .any(|s| tenero_app::config::ALPHA_SEEDS.contains(&s.as_str())));
    // the two are on different hosts and in different network groups, so a node can hold a connection to each
    let seeds = tenero_app::config::BETA_SEEDS;
    assert_ne!(
        tenero_net::addrbook::group_of(seeds[0]),
        tenero_net::addrbook::group_of(seeds[1])
    );
    let c = config_for("beta", "seed = 203.0.113.9:38343\n").unwrap();
    assert!(
        c.seeds.contains(&"203.0.113.9:38343".to_string())
            && c.seeds.len() == tenero_app::config::BETA_SEEDS.len() + 1
    );
    let c = config_for("beta", "no_builtin_seeds = yes\n").unwrap();
    assert!(c.seeds.is_empty());
    let e = config_for("beta", "mine = sha256\nmine_to = x\n").unwrap_err();
    assert!(e.contains("beta") && e.contains("cpu or gpu"), "{e}");
}

#[test]
fn a_node_holds_one_proof_of_work_dataset_so_it_stays_inside_8_gb() {
    // Two datasets are 8.0 GiB (measured 2026-10-04: a process went from 4.0 to 8.0 GiB at the first epoch boundary and stayed there), which
    // with the rest of the node is more than the owner's limit of 8 GB. A node on a real-proof-of-work network keeps one.
    assert_eq!(
        daemon::proof_of_work_datasets(Network::Alpha).unwrap(),
        Some(1)
    );
    assert_eq!(
        daemon::proof_of_work_datasets(Network::Dev).unwrap(),
        Some(1)
    );
    assert_eq!(
        daemon::proof_of_work_datasets(Network::Beta).unwrap(),
        Some(1)
    );
    // the SHA-256 test network has no dataset at all
    assert_eq!(daemon::proof_of_work_datasets(Network::Test).unwrap(), None);
}

#[test]
fn a_seed_repeats_its_address_answer_for_15_minutes_on_a_public_network_and_never_on_a_private_one()
{
    // public: one network group is held to one sample every 15 minutes (it was a day: a node that had just become reachable was not passed
    // on for up to a day). Private (the test network by default): every node is one "group", and the first, empty answer would otherwise be
    // given to every later node (found by the local soak test)
    assert_eq!(daemon::address_answer_ttl_ms(false), 15 * 60 * 1000);
    assert_eq!(daemon::address_answer_ttl_ms(true), 0);
}

#[test]
fn an_inbound_limit_of_zero_means_no_limit_and_any_other_number_is_the_operators_choice() {
    // the default: the program sets no limit on inbound peers or on peers in all
    assert_eq!(daemon::inbound_limits(0, 50), (usize::MAX, usize::MAX));
    // a limit the operator asked for: that many inbound, and room for the outbound ones (at least 64) on top
    assert_eq!(daemon::inbound_limits(10, 50), (10, 74));
    assert_eq!(daemon::inbound_limits(10, 100), (10, 110));
    // no overflow
    assert_eq!(
        daemon::inbound_limits(usize::MAX, 50),
        (usize::MAX, usize::MAX)
    );
}
