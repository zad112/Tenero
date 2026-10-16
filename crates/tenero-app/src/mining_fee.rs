//! Who the miner's fee pays (`tenero_miner::fee`: 1% of the mining time), per network and backend. The owner's decision
//! (2026-10-10), agreed with the AMD backend's contributor and announced to the community:
//!
//! * `gpu` (NVIDIA, CUDA): the project's author;
//! * `amd` (AMD, HIP): the AMD backend's author, qcvibes1;
//! * `cpu` and `sha256`: **no fee**, ever (a test refuses an entry for them).
//!
//! **On `gamma` from the release after 0.3.0-gamma.1** (the owner's decision, 2026-10-10); `dev` and `test` have none. At
//! main-net its own addresses replace these. An entry here is a change of the program and needs a new release, and the fee is
//! fixed: there is no switch to turn it off (the code is open, and a person who wants none can build without it).

use crate::config::Network;
use tenero_miner::fee::{FeeSchedule, MiningFee};

pub use tenero_miner::fee::FEE_PERCENT;

/// One address the fee pays.
#[derive(Clone, Copy, Debug)]
pub struct FeeRecipient {
    /// The network's name (`gamma`).
    pub network: &'static str,
    /// The backend (`gpu` or `amd`).
    pub backend: &'static str,
    /// A MAIN address of that network.
    pub address: &'static str,
    /// Who it is, for the screen.
    pub who: &'static str,
}

/// The backends that may carry a fee (never `cpu` or `sha256`).
pub const FEE_BACKENDS: &[&str] = &["gpu", "amd"];

/// The fee's addresses: one per network and backend. Wallets used for nothing else, given by their owners (2026-10-10).
pub const FEE_RECIPIENTS: &[FeeRecipient] = &[
    FeeRecipient {
        network: "gamma",
        backend: "gpu",
        address: "TENgJhGzz31cRRVv9Xp5BEecw9Kxzf4qGiSCqYBH4JtFMfarVL2VvkbYFCPSGKfss7746sr65PuBqRoNWpShiR1yaNpE7jvrJTb",
        who: "the project's author (zad112), for the NVIDIA backend",
    },
    FeeRecipient {
        network: "gamma",
        backend: "amd",
        address: "TENgJhA3h9GDHeXx3PVt7dT7Ef46C1rZ5VJWbohaourDDBNPGji6me2XgrmmXQ66vSgtnDGVkBpGyMR2sXdYcFaAedbkphU1sSL",
        who: "the AMD backend's author (qcvibes1)",
    },
];

/// The fee a miner of `backend` on `network` pays, if any: from [`FEE_RECIPIENTS`].
pub fn fee_for(network: Network, backend: &str) -> Result<Option<MiningFee>, String> {
    fee_from(FEE_RECIPIENTS, network, backend)
}

/// As [`fee_for`], from a given list (for the tests).
pub fn fee_from(
    list: &[FeeRecipient],
    network: Network,
    backend: &str,
) -> Result<Option<MiningFee>, String> {
    let Some(r) = list
        .iter()
        .find(|r| r.network == network.name() && r.backend == backend)
    else {
        return Ok(None);
    };
    if !FEE_BACKENDS.contains(&backend) {
        return Err(format!("the {backend} backend carries no fee"));
    }
    let address = tenero_wallet::Address::parse(r.address, network.wallet_network())
        .map_err(|e| format!("the fee's address for {backend} on {}: {e}", network.name()))?;
    MiningFee::new(address, r.who, FeeSchedule::standard(r.address))
        .map(Some)
        .ok_or_else(|| {
            format!(
                "the fee's address for {backend} on {} is not a main address",
                network.name()
            )
        })
}

/// What the screen says of the fee: one line, whether there is one or not.
pub fn fee_line(fee: Option<&MiningFee>, network: Network) -> String {
    match fee {
        Some(f) => f.describe(),
        None => format!("none on the {} network", network.name()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn main_address(network: Network) -> String {
        tenero_wallet::Wallet::from_seed(&[7u8; 32], network.wallet_network(), 0)
            .address()
            .to_text()
    }

    #[test]
    fn gamma_s_gpu_and_amd_miners_pay_the_fee_to_their_authors_and_nothing_else_does() {
        let gpu = fee_for(Network::Gamma, "gpu")
            .unwrap()
            .expect("gpu on gamma");
        assert_eq!(
            gpu.address_text(),
            "TENgJhGzz31cRRVv9Xp5BEecw9Kxzf4qGiSCqYBH4JtFMfarVL2VvkbYFCPSGKfss7746sr65PuBqRoNWpShiR1yaNpE7jvrJTb"
        );
        let amd = fee_for(Network::Gamma, "amd")
            .unwrap()
            .expect("amd on gamma");
        assert_eq!(
            amd.address_text(),
            "TENgJhA3h9GDHeXx3PVt7dT7Ef46C1rZ5VJWbohaourDDBNPGji6me2XgrmmXQ66vSgtnDGVkBpGyMR2sXdYcFaAedbkphU1sSL"
        );
        assert!(amd.to.contains("qcvibes1"));
        for n in &Network::ALL {
            for b in ["sha256", "cpu"] {
                assert!(fee_for(*n, b).unwrap().is_none(), "{b} on {}", n.name());
            }
        }
        for n in [Network::Dev, Network::Test] {
            for b in ["gpu", "amd"] {
                assert!(fee_for(n, b).unwrap().is_none(), "{b} on {}", n.name());
            }
        }
    }

    #[test]
    fn every_listed_address_is_a_main_address_of_its_network_and_only_gpu_and_amd_pay() {
        for (i, r) in FEE_RECIPIENTS.iter().enumerate() {
            assert!(FEE_BACKENDS.contains(&r.backend), "{r:?}");
            let n = Network::parse(r.network).expect("a network");
            assert!(fee_for(n, r.backend).unwrap().is_some(), "{r:?}");
            // one entry per network and backend
            assert!(
                !FEE_RECIPIENTS[..i]
                    .iter()
                    .any(|o| o.network == r.network && o.backend == r.backend),
                "{r:?}"
            );
        }
    }

    #[test]
    fn a_fee_is_made_from_its_entry_and_cpu_never_pays_one() {
        let network = Network::Dev;
        let address = main_address(network);
        let list = [FeeRecipient {
            network: "dev",
            backend: "amd",
            address: Box::leak(address.clone().into_boxed_str()),
            who: "the AMD backend's author (qcvibes1)",
        }];
        let fee = fee_from(&list, network, "amd").unwrap().expect("a fee");
        assert_eq!(fee.address_text(), address);
        assert!(fee.describe().starts_with(
            "1% of the mining time (60 s in every 100 min) goes to the AMD backend's author (qcvibes1)"
        ));
        // another backend or network: none
        assert!(fee_from(&list, network, "gpu").unwrap().is_none());
        assert!(fee_from(&list, Network::Gamma, "amd").unwrap().is_none());
        // an entry for the CPU is refused
        let cpu = [FeeRecipient {
            backend: "cpu",
            ..list[0]
        }];
        assert!(fee_from(&cpu, network, "cpu").is_err());
        // an address of another network is refused
        let other = [FeeRecipient {
            network: "gamma",
            ..list[0]
        }];
        assert!(fee_from(&other, Network::Gamma, "amd").is_err());
    }

    #[test]
    fn the_screen_says_when_there_is_no_fee() {
        assert_eq!(fee_line(None, Network::Dev), "none on the dev network");
    }
}
