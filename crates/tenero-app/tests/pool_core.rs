//! The pool's bookkeeping: share targets, nonce prefixes, the adjusting of a miner's difficulty, the PPLNS accounts and their file.
//! No network and no node in these tests: every rule of `pool_core.rs` is checked on its own.

use tenero_app::pool_core::*;
use tenero_core::u256::U256;
use tenero_wallet::{Address, Wallet};

fn address(n: u8) -> Address {
    Wallet::from_seed(&[n; 32], 0).address()
}

// ---- share targets ------------------------------------------------------------------------------------------------------

#[test]
fn a_share_target_is_the_block_target_times_the_ratio_and_never_easier_than_two_to_the_255() {
    let block = U256::pow2(237).unwrap();
    assert_eq!(
        share_target(&block, 1),
        block,
        "ratio 1: the share is a block"
    );
    assert_eq!(share_target(&block, 16), U256::pow2(241).unwrap());
    assert_eq!(share_target(&block, 0), block, "a ratio of 0 is taken as 1");
    // too easy: capped
    assert_eq!(
        share_target(&U256::pow2(250).unwrap(), 1 << 20),
        easiest_share_target()
    );
    assert_eq!(share_target(&U256::MAX, 5), easiest_share_target());
    // never zero
    assert_eq!(share_target(&U256::ZERO, 7), U256::ONE);
}

#[test]
fn a_share_is_worth_the_attempts_its_target_stands_for() {
    assert_eq!(work_of(&U256::pow2(237).unwrap()), 1 << 19);
    assert_eq!(work_of(&U256::pow2(241).unwrap()), 1 << 15);
    // a sixteenth of a block's work at a target 16 times easier, whatever the block
    for bits in [200u32, 220, 237, 250] {
        let block = U256::pow2(bits).unwrap();
        assert_eq!(work_of(&block), 16 * work_of(&share_target(&block, 16)));
    }
    // too much work for a u64 saturates, and the work of the easiest target is 2
    assert_eq!(work_of(&U256::ONE), u64::MAX);
    assert_eq!(work_of(&easiest_share_target()), 2);
}

// ---- nonce prefixes ----------------------------------------------------------------------------------------------------

#[test]
fn every_connected_miner_has_its_own_prefix_and_a_prefix_comes_back() {
    for (miners, bits) in [
        (1usize, 1u8),
        (2, 1),
        (3, 2),
        (256, 8),
        (257, 9),
        (1000, 10),
    ] {
        let p = Prefixes::for_miners(miners);
        assert_eq!(p.bits(), bits, "{miners} miners");
        assert!(p.capacity() >= miners);
    }
    let mut p = Prefixes::for_miners(4);
    let got: Vec<u64> = (0..4).map(|_| p.take().unwrap()).collect();
    let mut sorted = got.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 4, "four different prefixes: {got:?}");
    assert_eq!(p.take(), None, "all are in use");
    assert_eq!(p.in_use(), 4);
    p.give_back(got[2]);
    assert_eq!(p.take(), Some(got[2]), "the one given back is the one free");
    // a prefix that does not exist is ignored
    p.give_back(1 << 40);
}

#[test]
fn a_nonce_belongs_to_the_slice_of_its_top_bits() {
    let bits = 8;
    for prefix in [0u64, 1, 37, 255] {
        let first = first_nonce_of(prefix, bits);
        assert!(nonce_in_prefix(first, prefix, bits));
        assert!(
            nonce_in_prefix(first | ((1 << 56) - 1), prefix, bits),
            "the last of the slice"
        );
        assert!(
            !nonce_in_prefix(first.wrapping_add(1 << 56), prefix, bits),
            "the next slice"
        );
        assert!(
            !nonce_in_prefix(first.wrapping_sub(1), prefix, bits),
            "the slice before"
        );
    }
    // no bits: every nonce is the miner's
    assert!(nonce_in_prefix(u64::MAX, 0, 0));
    assert_eq!(first_nonce_of(0, 0), 0);
    // 32 bits is the most the protocol allows
    assert!(nonce_in_prefix(
        first_nonce_of(0xDEAD_BEEF, 32) + 5,
        0xDEAD_BEEF,
        32
    ));
}

// ---- the adjusting of difficulty ---------------------------------------------------------------------------------------

#[test]
fn a_miner_whose_shares_come_too_fast_gets_a_harder_target_and_one_who_has_none_an_easier() {
    let mut v = VarDiff::new(64, 1000);
    // before the first window is over, nothing happens
    for _ in 0..50 {
        v.share();
    }
    assert_eq!(v.tick(1000 + RETARGET_SECS - 1), None);
    // 50 shares in 30 s: 0.6 s each, 25 times faster than wanted: the ratio falls by the most it may, 4 times
    assert_eq!(v.tick(1000 + RETARGET_SECS), Some(16));
    // no shares in a window: 4 times easier
    assert_eq!(v.tick(1000 + 2 * RETARGET_SECS), Some(64));
    // two shares in 30 s is one every 15 s: just what is wanted, so no change
    v.share();
    v.share();
    assert_eq!(v.tick(1000 + 3 * RETARGET_SECS), None);
    assert_eq!(v.ratio, 64);
    // one share in 30 s is twice as slow as wanted: twice as easy
    v.share();
    assert_eq!(v.tick(1000 + 4 * RETARGET_SECS), Some(128));
}

#[test]
fn the_ratio_stays_between_one_and_the_most() {
    let mut v = VarDiff::new(1, 0);
    for k in 1..=3u64 {
        for _ in 0..1000 {
            v.share();
        }
        assert_eq!(
            v.tick(k * RETARGET_SECS),
            None,
            "already at the hardest: no change to report"
        );
        assert_eq!(v.ratio, 1);
    }
    let mut v = VarDiff::new(MAX_RATIO, 0);
    assert_eq!(v.tick(RETARGET_SECS), None, "already at the easiest");
    assert_eq!(v.ratio, MAX_RATIO);
    assert_eq!(VarDiff::new(0, 0).ratio, 1);
    assert_eq!(VarDiff::new(u64::MAX, 0).ratio, MAX_RATIO);
}

// ---- PPLNS ------------------------------------------------------------------------------------------------------------

fn books(n: usize) -> (Accounts, Vec<AddrId>) {
    let mut a = Accounts::new();
    let ids = (0..n)
        .map(|i| a.id_of(&address(i as u8 + 1)).unwrap())
        .collect();
    (a, ids)
}

#[test]
fn a_block_is_shared_by_the_work_of_the_last_shares() {
    let (mut a, id) = books(3);
    // Alice 3 shares of 10, Bob 1 of 10: 40 of work; a block of work 40 shares its reward 75 : 25
    for _ in 0..3 {
        a.add_share(id[0], 10, 40);
    }
    a.add_share(id[1], 10, 40);
    let credits = a.block_found(100, [7; 32], 1000, 0, 40);
    assert_eq!(credits, vec![(id[0], 750), (id[1], 250)]);
    assert_eq!(
        credits.iter().map(|c| c.1).sum::<u64>(),
        1000,
        "the credits add up to the reward"
    );
    // pending: nothing is anyone's yet
    assert_eq!(a.balance(id[0]), 0);
    assert_eq!(a.pending().len(), 1);
    assert_eq!(a.blocks_found, 1);
}

#[test]
fn only_the_newest_shares_that_make_up_the_window_count_and_the_last_counts_in_part() {
    let (mut a, id) = books(2);
    // an old share by Bob, then five by Alice, then a block that needs 25 of work: Alice's five (50) fill it, Bob's is out
    a.add_share(id[1], 10, 1000);
    for _ in 0..5 {
        a.add_share(id[0], 10, 1000);
    }
    let credits = a.block_found(5, [1; 32], 100, 0, 25);
    assert_eq!(
        credits,
        vec![(id[0], 100)],
        "Bob's old share is outside the window"
    );
    // the share that crosses the window counts only for what is missing: a window of 25 is two shares and half of a third
    let (mut a, id) = books(3);
    a.add_share(id[2], 10, 1000);
    a.add_share(id[1], 10, 1000);
    a.add_share(id[0], 10, 1000);
    let credits = a.block_found(5, [1; 32], 100, 0, 25);
    // newest first: Alice 10, Bob 10, Carol 5 of her 10: 40% 40% 20%
    assert_eq!(credits, vec![(id[0], 40), (id[1], 40), (id[2], 20)]);
}

#[test]
fn a_miner_who_joins_late_or_leaves_early_is_paid_for_his_shares_whatever_round_they_fell_in() {
    // the point of PPLNS: the window is not emptied by a block
    let (mut a, id) = books(2);
    for _ in 0..4 {
        a.add_share(id[0], 10, 40);
    }
    let first = a.block_found(10, [1; 32], 400, 0, 40);
    assert_eq!(first, vec![(id[0], 400)]);
    // after that block, Bob joins and a second block comes after only 2 shares of his: the window is Alice's 2 and Bob's 2
    a.add_share(id[1], 10, 40);
    a.add_share(id[1], 10, 40);
    let second = a.block_found(11, [2; 32], 400, 0, 40);
    assert_eq!(second, vec![(id[0], 200), (id[1], 200)]);
}

#[test]
fn the_pools_fee_comes_off_first_and_rounding_goes_to_the_biggest_miner_so_nothing_is_lost() {
    let (mut a, id) = books(3);
    for who in &id {
        a.add_share(*who, 10, 30);
    }
    // a reward of 100 with 10% fee: 90 to share three ways: 30 each
    let c = a.block_found(1, [1; 32], 100, 10, 30);
    assert_eq!(c.iter().map(|x| x.1).sum::<u64>(), 90);
    assert!(c.iter().all(|x| x.1 == 30));
    // a reward that does not divide: the credits still add up to exactly the reward less the fee
    let c = a.block_found(2, [2; 32], 101, 0, 30);
    assert_eq!(c.iter().map(|x| x.1).sum::<u64>(), 101);
    let c = a.block_found(3, [3; 32], 1_000_000_007, 3, 30);
    let fee = (1_000_000_007u128 * 3 / 100) as u64;
    assert_eq!(c.iter().map(|x| x.1).sum::<u64>(), 1_000_000_007 - fee);
    // a fee over 100% is 100%: nobody is paid, and nothing underflows
    let c = a.block_found(4, [4; 32], 50, 250, 30);
    assert!(c.is_empty());
}

#[test]
fn a_block_found_with_no_shares_in_the_window_credits_nobody_and_does_not_panic() {
    let (mut a, _) = books(1);
    assert!(a.block_found(1, [1; 32], 100, 0, 40).is_empty());
    assert_eq!(a.pending().len(), 1);
}

#[test]
fn the_window_holds_only_what_the_newest_shares_need() {
    let (mut a, id) = books(1);
    for _ in 0..100 {
        a.add_share(id[0], 10, 55);
    }
    // 55 of work needs six shares of 10 (the one that crosses counts): the older ones are dropped
    assert_eq!(a.window_len(), 6);
    assert_eq!(a.window_work(), 60);
    assert_eq!(a.shares_accepted, 100);
}

#[test]
fn a_block_pays_when_it_has_matured_and_is_still_the_chains_and_pays_nobody_when_another_took_its_place(
) {
    let (mut a, id) = books(2);
    a.add_share(id[0], 10, 20);
    a.add_share(id[1], 10, 20);
    a.block_found(100, [1; 32], 1000, 0, 20);
    a.block_found(101, [2; 32], 1000, 0, 20);
    a.block_found(102, [3; 32], 1000, 0, 20);
    // the chain: 100 is ours, 101 was replaced by another block, 102 is not asked about yet
    let mut chain = |h: u64| match h {
        100 => Some([1u8; 32]),
        101 => Some([9u8; 32]),
        _ => Some([3u8; 32]),
    };
    // too early: nothing matures
    assert_eq!(a.settle(159, 60, &mut chain), (0, 0));
    assert_eq!(a.balance(id[0]), 0);
    // 100 is 60 deep at 160: credited. 101 and 102 are not yet
    assert_eq!(a.settle(160, 60, &mut chain), (1, 0));
    assert_eq!((a.balance(id[0]), a.balance(id[1])), (500, 500));
    // 101 matures at 161: lost, and pays nobody
    assert_eq!(a.settle(161, 60, &mut chain), (0, 1));
    assert_eq!((a.balance(id[0]), a.balance(id[1])), (500, 500));
    assert_eq!(a.pending().len(), 1);
    assert_eq!(a.settle(162, 60, &mut chain), (1, 0));
    assert_eq!((a.balance(id[0]), a.balance(id[1])), (1000, 1000));
    assert!(a.pending().is_empty());
}

#[test]
fn a_node_that_cannot_say_leaves_the_block_pending_to_be_asked_again() {
    let (mut a, id) = books(1);
    a.add_share(id[0], 10, 10);
    a.block_found(10, [1; 32], 100, 0, 10);
    assert_eq!(a.settle(100, 60, &mut |_| None), (0, 0));
    assert_eq!(a.pending().len(), 1);
    assert_eq!(a.balance(id[0]), 0);
    assert_eq!(a.settle(100, 60, &mut |_| Some([1; 32])), (1, 0));
    assert_eq!(a.balance(id[0]), 100);
}

#[test]
fn what_is_due_is_the_balances_at_the_minimum_largest_first_and_a_payment_is_taken_off() {
    let (mut a, id) = books(4);
    for (who, amount) in [(0usize, 5u64), (1, 30), (2, 10), (3, 20)] {
        a.add_share(id[who], 10, 10);
        a.block_found(who as u64, [who as u8; 32], amount, 0, 10);
    }
    a.settle(1000, 0, &mut |h| Some([h as u8; 32]));
    let due = a.due(10, 10);
    let amounts: Vec<u64> = due.iter().map(|o| o.amount).collect();
    assert_eq!(amounts, vec![30, 20, 10], "5 is under the minimum");
    assert_eq!(a.due(10, 2).len(), 2, "at most that many");
    assert_eq!(due[0].address, address(2));
    a.debit(id[1], 30);
    assert_eq!((a.balance(id[1]), a.paid(id[1]), a.total_paid), (0, 30, 30));
    assert_eq!(a.due(10, 10).len(), 2);
    // paying more than is owed leaves nothing owed, not a huge number
    a.debit(id[3], 999);
    assert_eq!(a.balance(id[3]), 0);
}

#[test]
fn the_same_address_is_the_same_miner_and_the_books_are_bounded() {
    let mut a = Accounts::new();
    let x = a.id_of(&address(1)).unwrap();
    assert_eq!(a.id_of(&address(1)).unwrap(), x);
    assert_ne!(a.id_of(&address(2)).unwrap(), x);
    assert_eq!(a.addresses(), 2);
    assert_eq!(a.id_by_text(&address(2).to_text()), Some(1));
    assert_eq!(a.id_by_text("tni1nothing"), None);
}

// ---- the file -----------------------------------------------------------------------------------------------------------

fn busy() -> Accounts {
    let (mut a, id) = books(3);
    for k in 0..20u64 {
        a.add_share(id[(k % 3) as usize], 10 + k, 150);
    }
    a.block_found(77, [5; 32], 12_345, 2, 150);
    a.settle(0, 60, &mut |_| None);
    a.add_share(id[0], 5, 150);
    let mut a2 = a.clone();
    a2.block_found(78, [6; 32], 1000, 0, 150);
    a2.settle(1000, 0, &mut |h| (h == 77).then_some([5; 32]));
    a2.debit(id[0], 1);
    a2.next_payout = 1_700_003_600;
    a2
}

#[test]
fn the_books_survive_the_file() {
    let a = busy();
    let bytes = a.to_bytes().unwrap();
    let b = Accounts::from_bytes(&bytes).unwrap();
    assert_eq!(a, b);
    assert_eq!(b.to_bytes().unwrap(), bytes, "one state, one file");
    // an empty book too
    let e = Accounts::new();
    assert_eq!(Accounts::from_bytes(&e.to_bytes().unwrap()).unwrap(), e);
}

#[test]
fn a_damaged_or_foreign_file_is_refused_and_every_byte_is_protected() {
    let bytes = busy().to_bytes().unwrap();
    for cut in [0usize, 1, 10, bytes.len() / 2, bytes.len() - 1] {
        assert!(Accounts::from_bytes(&bytes[..cut]).is_err(), "cut at {cut}");
    }
    for i in (0..bytes.len()).step_by(7) {
        let mut b = bytes.clone();
        b[i] ^= 0x40;
        assert!(Accounts::from_bytes(&b).is_err(), "flipped byte {i}");
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(Accounts::from_bytes(&trailing).is_err());
    assert!(
        Accounts::from_bytes(b"not a pool file at all, but long enough to have a checksum")
            .is_err()
    );
}
