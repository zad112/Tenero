//! The rules' constants for one network (`docs/CONSENSUS_V2.md`).

use tenero_core::difficulty::DifficultyParams;
use tenero_core::emission::Emission;
use tenero_core::u256::U256;
use tenero_core::v2::ids::PowKind;

/// When an output enters the curve tree and so can be spent: a coinbase output 60 blocks after its block, any other 10
/// (`docs/CONSENSUS_V2.md` 15.6). Fixed by consensus (the store grows the tree by them), not per network.
pub use tenero_core::v3::rules::{COINBASE_MATURITY, SPEND_MATURITY};
/// A block more than this many seconds ahead of the clock is not yet acceptable (never permanently invalid).
pub const FUTURE_LIMIT_SECONDS: u64 = 120;

/// The consensus constants of a network. The defaults are the version 3 rules; the fields exist so that a test network
/// can use a cheaper proof of work or a shorter chain.
#[derive(Clone, Debug)]
pub struct ChainParams {
    /// The network label, which fixes the genesis block and so the chain id.
    pub label: String,
    pub pow_kind: PowKind,
    pub emission: Emission,
    pub difficulty: DifficultyParams,
    pub future_limit_seconds: u64,
}

impl ChainParams {
    /// The version 3 rules for a network called `label`, with the given proof of work and starting target. Emission is
    /// the 8-decimal schedule (20 coins halving every 525,600 blocks, a 20,000,000-coin cap, a 0.5-coin tail), and
    /// difficulty aims at 60-second blocks over a window of 30: both as version 2.
    pub fn version_3(label: &str, pow_kind: PowKind, start_target: U256) -> ChainParams {
        ChainParams {
            label: label.to_string(),
            pow_kind,
            emission: Emission {
                initial_reward: 2_000_000_000,
                halving_interval: 525_600,
                max_supply: 2_000_000_000_000_000,
                tail_reward: 50_000_000,
            },
            difficulty: DifficultyParams {
                block_time: 60,
                window: 30,
                start_target,
            },
            future_limit_seconds: FUTURE_LIMIT_SECONDS,
        }
    }
}
