//! The release network, "alpha" (M11.2): a third network beside `test` (SHA-256) and `dev` (the development network). Its chain id, its parameters
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

fn hex(id: &[u8; 32]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

fn config(extra: &str) -> Result<tenero_app::config::Config, String> {
    Raw::from_file_text(&format!("data = d\nnetwork = alpha\n{extra}"))
        .and_then(|r| r.into_config())
        .map_err(|e| e.0)
}

#[test]
fn alpha_is_a_third_network_with_its_own_name_port_and_epoch() {
    assert_eq!(Network::parse("alpha"), Some(Network::Alpha));
    assert_eq!(Network::Alpha.name(), "alpha");
    assert_eq!(Network::ALL, [Network::Test, Network::Dev, Network::Alpha]);
    assert!(Network::Alpha.real_pow() && Network::Dev.real_pow() && !Network::Test.real_pow());
    // three networks, three control ports: nodes of two networks on one machine do not meet
    let ports: Vec<u16> = Network::ALL
        .iter()
        .map(|n| n.default_control_port())
        .collect();
    assert_eq!(ports, vec![18332, 28332, 38332]);
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
    assert_ne!(ids[0], ids[1]);
    assert_ne!(ids[0], ids[2]);
    assert_ne!(
        ids[1], ids[2],
        "a dev node and an alpha node must not accept each other's chain"
    );
    assert_eq!(hex(&ids[2]), ALPHA_CHAIN_ID);
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
    // the SHA-256 test network has no dataset at all
    assert_eq!(daemon::proof_of_work_datasets(Network::Test).unwrap(), None);
}
