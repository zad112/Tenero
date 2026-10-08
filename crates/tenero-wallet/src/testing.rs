//! For tests of the programs built on the wallet: blocks of the SHA-256 test chain whose rewards are real Carrot outputs
//! to an address, so that a wallet finds and spends them. **Not for a real chain** (it searches nonces with SHA-256).

use rand_core::OsRng;
use tenero_core::u256::U256;
use tenero_core::v2::ids::PowKind;
use tenero_core::v3::ids::block_id;
use tenero_core::v3::Block;
use tenero_node::Node;

use crate::{coinbase_payout, Address};

/// A mined block of the SHA-256 test chain on the node's tip, at time `ts`, with the pool's transactions and its reward
/// paid to the main address `to`. The caller hands it to the node (or an engine).
pub fn test_block_to(node: &Node<'_>, to: &Address, ts: u64) -> Block {
    let height = node.tip().expect("a tip").0 + 1;
    let mut b = node
        .block_template(ts, u64::MAX, &|amount| {
            coinbase_payout(&mut OsRng, to, height, amount).expect("a main address")
        })
        .expect("a template");
    let target = node.next_block().expect("the next block").target;
    for nonce in 0.. {
        b.header.nonce = nonce;
        if U256::from_be_bytes(&block_id(&b.header, PowKind::Sha256)) < target {
            break;
        }
    }
    b
}

/// How many blocks paid to an address make its first rewards spendable: a block reward enters the curve tree 60 blocks
/// after its block, so with 64 blocks the rewards of blocks 1 to 5 can be spent in the next.
pub const READY: u64 = 64;
