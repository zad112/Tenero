//! The rules' constants for one network (`docs/CONSENSUS_V2.md`).

use tenero_core::difficulty::DifficultyParams;
use tenero_core::emission::Emission;
use tenero_core::fees::V2_MIN_BLOCK_MEDIAN;
use tenero_core::u256::U256;
use tenero_core::v2::ids::PowKind;

/// Every ring has exactly this many members (`CONSENSUS_V2.md` 6.3).
pub const RING_SIZE: usize = 16;
/// The limits of the first test release (alpha.4), which the `alpha` network keeps so that a node of this version judges its transactions exactly as
/// the older nodes still on that network do: at most this many inputs and a `proof_data` of at most this many bytes. Every other network has only the
/// size limit (`MAX_TX_SIZE`).
pub const LEGACY_MAX_INPUTS: usize = 32;
pub const LEGACY_MAX_PROOF: usize = 32 * 1024;
/// A coinbase output can be spent or used in a ring this many blocks after its block.
pub const COINBASE_MATURITY: u64 = 60;
/// Any other output can be used this many blocks after its block.
pub const SPEND_MATURITY: u64 = 10;
/// A block more than this many seconds ahead of the clock is not yet acceptable (never permanently invalid).
pub const FUTURE_LIMIT_SECONDS: u64 = 120;

/// The consensus constants of a network. The defaults are the version 2 rules; the fields exist so that a
/// test network can use a cheaper proof of work or a shorter chain, and so that a later rules version
/// (`CONSENSUS_V2.md` section 10) has somewhere to change them.
#[derive(Clone, Debug)]
pub struct ChainParams {
    /// The network label, which fixes the genesis block and so the chain id.
    pub label: String,
    pub pow_kind: PowKind,
    pub emission: Emission,
    pub difficulty: DifficultyParams,
    /// The block-size median never goes below this (150,000 bytes in version 2).
    pub min_block_median: u64,
    pub ring_size: usize,
    pub coinbase_maturity: u64,
    pub spend_maturity: u64,
    pub future_limit_seconds: u64,
    /// Judge transactions by the limits of alpha.4 (`LEGACY_MAX_INPUTS`, `LEGACY_MAX_PROOF`) as well as by `MAX_TX_SIZE`: only the `alpha` network.
    pub legacy_tx_limits: bool,
}

impl ChainParams {
    /// The version 2 rules for a network called `label`, with the given proof of work and starting target.
    /// Emission is the 8-decimal schedule (20 coins halving every 525,600 blocks, a 20,000,000-coin cap, a
    /// 0.5-coin tail), and difficulty aims at 60-second blocks over a window of 30.
    pub fn version_2(label: &str, pow_kind: PowKind, start_target: U256) -> ChainParams {
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
            min_block_median: V2_MIN_BLOCK_MEDIAN,
            ring_size: RING_SIZE,
            coinbase_maturity: COINBASE_MATURITY,
            spend_maturity: SPEND_MATURITY,
            future_limit_seconds: FUTURE_LIMIT_SECONDS,
            legacy_tx_limits: false,
        }
    }
}
