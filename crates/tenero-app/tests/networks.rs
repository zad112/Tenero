//! The networks of the 0.3.0 programs: `gamma` (0.3.0-gamma.1: FCMP++ and Carrot from its first block), beside `test`
//! (SHA-256) and `dev` (the development network). Their chain ids, parameters and genesis are decisions, written down in
//! `docs/CONSENSUS_V2.md` 15 and `docs/FCMP_CARROT_PLAN.md` (P2, P3, F11), and held here so that none of them changes by
//! accident. **Still test networks: nothing on them has value, and nothing here is audited.**

use std::path::PathBuf;

use tenero_app::config::{Network, Raw};
use tenero_app::daemon;
use tenero_core::u256::U256;
use tenero_core::v2::ids::PowKind;
use tenero_store::Store;

/// The chain id of "tenero gamma network 1": SHA-256("tenero genesis id v3" || the genesis header), checked against the
/// reference in `tests/vectors/v3_genesis.json` below. A peer on any other chain is refused at the handshake because its id
/// differs.
const GAMMA_CHAIN_ID: &str = "bd366b37dc59f25d5d2e15aecd1d5c14810b5c20643f2c0f90384db9ee4c28c4";

fn hex(id: &[u8; 32]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

fn config_for(network: &str, extra: &str) -> Result<tenero_app::config::Config, String> {
    Raw::from_file_text(&format!("data = d\nnetwork = {network}\n{extra}"))
        .and_then(|r| r.into_config())
        .map_err(|e| e.0)
}

#[test]
fn the_programs_run_gamma_dev_and_test_each_with_its_own_name_port_and_epoch() {
    assert_eq!(Network::parse("gamma"), Some(Network::Gamma));
    assert_eq!(Network::Gamma.name(), "gamma");
    // beta and alpha carry on with the 0.2.0 programs (decision P2)
    assert_eq!(Network::parse("beta"), None);
    assert_eq!(Network::parse("alpha"), None);
    assert_eq!(Network::ALL, [Network::Test, Network::Dev, Network::Gamma]);
    assert!(Network::Gamma.real_pow() && Network::Dev.real_pow() && !Network::Test.real_pow());
    // three networks, three control ports (gamma's next to beta's 38342 and alpha's 38332): nodes of two networks on one
    // machine do not meet
    let ports: Vec<u16> = Network::ALL
        .iter()
        .map(|n| n.default_control_port())
        .collect();
    assert_eq!(ports, vec![18332, 28332, 38352]);
    // the epoch of the real proof of work: 100 blocks, the owner's choice of 2026-10-04
    assert_eq!(Network::Gamma.epoch_blocks(), 100);
    assert_eq!(Network::Dev.epoch_blocks(), 100);
    assert_eq!(daemon::DEV_EPOCH_BLOCKS, 100);
    // each network's addresses are its own
    assert_eq!(Network::Gamma.wallet_network().prefix(), "TENg");
    assert_eq!(Network::Dev.wallet_network().prefix(), "TENd");
    assert_eq!(Network::Test.wallet_network().prefix(), "TENt");
}

#[test]
fn gamma_has_the_decided_parameters() {
    let p = daemon::params_of(Network::Gamma).unwrap();
    assert_eq!(p.label, "tenero gamma network 1");
    assert_eq!(p.label, daemon::GAMMA_LABEL);
    assert_eq!(
        p.pow_kind,
        PowKind::Matmul,
        "the real proof of work, not SHA-256"
    );
    // beta's real starting difficulty, block time and window (decision P3: the version 2 difficulty unchanged)
    assert_eq!(p.difficulty.start_target, U256::pow2(237).unwrap());
    assert_eq!(p.difficulty.block_time, 60);
    assert_eq!(p.difficulty.window, 30);
    // 2^237 is about 524,000 attempts a block: at the measured 34,000 attempts a second of one RTX 5070 Ti, a block takes
    // about 15 s, never under a second
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
    // the version 2 emission unchanged (decision P3)
    assert_eq!(p.emission.initial_reward, 2_000_000_000);
    assert_eq!(p.emission.halving_interval, 525_600);
    assert_eq!(p.emission.max_supply, 2_000_000_000_000_000);
    assert_eq!(p.emission.tail_reward, 50_000_000);
    // dev differs in its start and its label only
    let dev = daemon::params_of(Network::Dev).unwrap();
    assert_ne!(dev.difficulty.start_target, p.difficulty.start_target);
    assert_eq!(dev.label, daemon::DEV_LABEL);
    assert_eq!(p.emission, dev.emission);
    assert_eq!(p.difficulty.window, dev.difficulty.window);
}

#[test]
fn the_test_network_s_difficulty_is_fixed_so_a_reward_is_spendable_in_minutes() {
    // a CPU mines it on the clock; with the 60-block wait for a reward, an adjusting difficulty would make it a block a minute
    let t = daemon::params_of(Network::Test).unwrap();
    assert_eq!(t.difficulty.window, 0);
    assert_eq!(t.pow_kind, PowKind::Sha256);
    assert_eq!(
        t.difficulty.start_target,
        tenero_net::sim::test_chain_params().difficulty.start_target
    );
    assert_eq!(t.label, tenero_net::sim::LABEL);
}

#[test]
fn the_chain_ids_differ_and_are_the_ones_in_the_reference_vectors() {
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
    assert_eq!(hex(&ids[2]), GAMMA_CHAIN_ID);
    // and the independent Python reference gives the same id for each network's label
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/vectors/v3_genesis.json");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    for (n, id) in Network::ALL.iter().zip(&ids) {
        let label = daemon::params_of(*n).unwrap().label;
        let case = v["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["label"] == label.as_str())
            .unwrap_or_else(|| panic!("the {label} label is in the vectors"));
        assert_eq!(case["chain_id"], hex(id).as_str(), "{}", n.name());
    }
}

/// The gathered proof-of-work attempt (`CONSENSUS.md` 8.3, `THREAT_MODEL.md` E11) from block 0 on gamma and dev: the first
/// design's weakness never reaches a version 3 chain. The SHA-256 test network has no matmulhash. Checked on the checker the
/// node really builds.
#[test]
fn the_gathered_attempt_is_required_from_block_0_on_gamma_and_dev() {
    assert_eq!(daemon::gather_fork_of(Network::Gamma).unwrap(), Some(0));
    assert_eq!(daemon::gather_fork_of(Network::Dev).unwrap(), Some(0));
    assert_eq!(daemon::gather_fork_of(Network::Test).unwrap(), None);
    for n in Network::ALL {
        if let Some(h) = daemon::gather_fork_of(n).unwrap() {
            assert_eq!(h, n.gather_from(), "{}", n.name());
        }
    }
}

/// NO PREMINE, checked on the real genesis of every network: it creates no output, so every coin comes from a mined block,
/// the owner's included.
#[test]
fn the_genesis_of_every_network_creates_no_output() {
    for network in Network::ALL {
        let p = daemon::params_of(network).unwrap();
        let dir = std::env::temp_dir().join(format!(
            "tenero-genesis-test-{}-{}",
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
        assert_eq!(tip.header.version, 3, "{}", network.name());
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn settings_for_gamma() {
    let c = config_for("gamma", "").unwrap();
    assert_eq!(c.network, Network::Gamma);
    assert_eq!(c.control.port(), 38352);
    assert!(
        !c.allow_private_peers,
        "a release network does not take 127.0.0.2-style peers by default"
    );
    // pruned unless told otherwise (decision F11): a seed or an explorer sets 0
    assert_eq!(c.prune_keep, tenero_app::config::DEFAULT_PRUNE_KEEP);
    assert_eq!(
        config_for("gamma", "prune_keep = 0\n").unwrap().prune_keep,
        0
    );
    // the author's two servers are built in, on the gamma peer port, and the list passes the checks a program can make
    // (public, listed once, no two in one network group)
    let seeds = tenero_app::config::GAMMA_SEEDS;
    assert_eq!(seeds, ["195.26.244.245:38353", "194.238.27.60:38353"]);
    assert!(tenero_app::config::check_seed_list(seeds).is_ok());
    assert_eq!(c.seeds, seeds.to_vec());
    assert_ne!(
        tenero_net::addrbook::group_of(seeds[0]),
        tenero_net::addrbook::group_of(seeds[1])
    );
    let c = config_for("gamma", "seed = 203.0.113.9:38353\n").unwrap();
    assert!(c.seeds.contains(&"203.0.113.9:38353".to_string()) && c.seeds.len() == seeds.len() + 1);
    let c = config_for("gamma", "no_builtin_seeds = yes\n").unwrap();
    assert!(c.seeds.is_empty());
    // the real proof of work: cpu or gpu, never sha256
    let e = config_for("gamma", "mine = sha256\nmine_to = x\n").unwrap_err();
    assert!(e.contains("gamma") && e.contains("cpu or gpu"), "{e}");
    let address = tenero_wallet::Wallet::from_seed(&[1; 32], tenero_wallet::Network::Gamma, 0)
        .address()
        .to_text();
    assert!(address.starts_with("TENg"));
    for mode in ["cpu", "gpu"] {
        let c = config_for("gamma", &format!("mine = {mode}\nmine_to = {address}\n")).unwrap();
        assert_eq!(c.network, Network::Gamma);
    }
}

#[test]
fn a_node_holds_one_proof_of_work_dataset_so_it_stays_inside_8_gb() {
    // Two datasets are 8.0 GiB (measured 2026-10-04: a process went from 4.0 to 8.0 GiB at the first epoch boundary and
    // stayed there), which with the rest of the node is more than the owner's limit of 8 GB. A node on a real-proof-of-work
    // network keeps one.
    assert_eq!(
        daemon::proof_of_work_datasets(Network::Gamma).unwrap(),
        Some(1)
    );
    assert_eq!(
        daemon::proof_of_work_datasets(Network::Dev).unwrap(),
        Some(1)
    );
    // the SHA-256 test network has no dataset at all
    assert_eq!(daemon::proof_of_work_datasets(Network::Test).unwrap(), None);
}

#[test]
fn a_seed_repeats_its_address_answer_for_15_minutes_on_a_public_network_and_never_on_a_private_one()
{
    // public: one network group is held to one sample every 15 minutes (it was a day: a node that had just become reachable
    // was not passed on for up to a day). Private (the test network by default): every node is one "group", and the first,
    // empty answer would otherwise be given to every later node (found by the local soak test)
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
