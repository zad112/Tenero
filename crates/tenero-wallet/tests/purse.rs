//! The seed phrase and the purse (several accounts from one seed): spelling, derivation, the file, restoring, paying.
//! The chain is the SHA-256 test chain with real proof checks, as in `pay.rs`. **A test chain, not a real one.**

use std::path::PathBuf;

use rand_core::OsRng;
use tenero_chain::{ChainParams, Sha256Pow, Submitted};
use tenero_core::u256::U256;
use tenero_core::v2::ids::PowKind;
use tenero_core::v3::ids::block_id;
use tenero_net::sim::{test_chain_params, LABEL};
use tenero_node::{Node, NodeConfig};
use tenero_store::Store;
use tenero_wallet::purse::{account_seed, GAP, MAX_ACCOUNTS, MAX_LABEL};
use tenero_wallet::{
    coinbase_payout, phrase_of, seed_of, Address, EntryKind, FeeLevel, FileError, KdfParams,
    Network, PhraseError, Purse, PurseError, SentStatus, Wallet, WalletError,
};

const T0: u64 = 1_700_000_000;
/// Blocks mined to an account before it spends: the rewards of blocks 1 to 5 are spendable then (a block reward enters the
/// curve tree 60 blocks after its block).
const READY: usize = 64;

// ---------------------------------------------------------------------------------------------------------------
// the words
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn the_published_bip39_vectors_for_256_bit_entropy_come_out_right() {
    // from the BIP-39 reference test vectors (all-zero, all-ones and 0x7f entropy)
    let zeros = format!("{}art", "abandon ".repeat(23));
    assert_eq!(*phrase_of(&[0u8; 32]), zeros);
    assert_eq!(*seed_of(&zeros).unwrap(), [0u8; 32]);
    let ones = format!("{}vote", "zoo ".repeat(23));
    assert_eq!(*phrase_of(&[0xff; 32]), ones);
    assert_eq!(*seed_of(&ones).unwrap(), [0xff; 32]);
    let legal = "legal winner thank year wave sausage worth useful legal winner thank year wave sausage worth useful legal winner thank year wave sausage worth title";
    assert_eq!(*phrase_of(&[0x7f; 32]), legal);
    assert_eq!(*seed_of(legal).unwrap(), [0x7f; 32]);
}

#[test]
fn a_phrase_round_trips_for_many_seeds_and_every_phrase_has_24_words() {
    let mut x = 1u64;
    for _ in 0..300 {
        let mut seed = [0u8; 32];
        for b in seed.iter_mut() {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (x >> 33) as u8;
        }
        let p = phrase_of(&seed);
        assert_eq!(p.split(' ').count(), 24);
        assert_eq!(*seed_of(&p).unwrap(), seed);
    }
}

#[test]
fn case_and_spacing_do_not_matter_but_mistakes_are_named() {
    let seed = [9u8; 32];
    let p = phrase_of(&seed);
    let messy = format!("  {}  ", p.to_uppercase().replace(' ', " \n\t "));
    assert_eq!(*seed_of(&messy).unwrap(), seed);

    // too few and too many words
    let words: Vec<&str> = p.split(' ').collect();
    assert_eq!(
        seed_of(&words[..23].join(" ")).err(),
        Some(PhraseError::WrongCount(23))
    );
    assert_eq!(
        seed_of(&format!("{} abandon", p.as_str())).err(),
        Some(PhraseError::WrongCount(25))
    );
    assert_eq!(seed_of("").err(), Some(PhraseError::WrongCount(0)));
    // a twelve-word phrase (a different size) is refused too, not taken for something else
    assert_eq!(
        seed_of(&words[..12].join(" ")).err(),
        Some(PhraseError::WrongCount(12))
    );

    // a word that is not a word: its position is said (counting from 1)
    let mut bad = words.clone();
    bad[6] = "tenero";
    assert_eq!(
        seed_of(&bad.join(" ")).err(),
        Some(PhraseError::UnknownWord(7))
    );

    // two words swapped: all real, but the checksum fails (unless they are equal)
    let mut swapped = words.clone();
    let (i, j) = (0..24)
        .flat_map(|i| (i + 1..24).map(move |j| (i, j)))
        .find(|&(i, j)| words[i] != words[j])
        .unwrap();
    swapped.swap(i, j);
    let r = seed_of(&swapped.join(" "));
    assert!(
        matches!(r, Err(PhraseError::Checksum)) || *r.unwrap() != seed,
        "a swap must never give the same seed back"
    );
}

#[test]
fn a_single_wrong_word_is_caught_for_all_but_about_one_in_256() {
    // Not all: the checksum is 8 bits, so one wrong word slips through about 1 time in 256. Measured here, so the
    // number in the documentation is a measurement.
    let seed = [3u8; 32];
    let words: Vec<String> = phrase_of(&seed).split(' ').map(str::to_string).collect();
    // 300 different words, taken from the phrases of other seeds
    let mut alternatives: Vec<String> = Vec::new();
    let mut x = 7u64;
    while alternatives.len() < 300 {
        let mut other = [0u8; 32];
        for b in other.iter_mut() {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (x >> 33) as u8;
        }
        for w in phrase_of(&other).split(' ') {
            if !alternatives.iter().any(|a| a == w) {
                alternatives.push(w.to_string());
            }
        }
    }
    let (mut caught, mut slipped, mut same) = (0, 0, 0);
    for pos in 0..24 {
        for alt in &alternatives {
            if *alt == words[pos] {
                same += 1;
                continue;
            }
            let mut w = words.clone();
            w[pos] = alt.clone();
            match seed_of(&w.join(" ")) {
                Err(PhraseError::Checksum) => caught += 1,
                Ok(s) => {
                    assert_ne!(*s, seed, "a changed word cannot give the same seed");
                    slipped += 1;
                }
                Err(e) => panic!("unexpected {e:?}"),
            }
        }
    }
    let total = caught + slipped;
    println!("wrong word: caught {caught}, slipped {slipped} of {total} ({same} same)");
    assert!(caught > 0);
    // expected about 1 in 256; the bound is loose (3 in 100) so that the test is not a coin flip
    assert!(
        slipped * 100 < total * 3,
        "{slipped} of {total} wrong words slipped"
    );
}

// ---------------------------------------------------------------------------------------------------------------
// accounts
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn account_zero_is_the_master_seed_and_the_others_are_distinct_and_repeatable() {
    let m = [5u8; 32];
    assert_eq!(*account_seed(&m, 0), m);
    let a1 = account_seed(&m, 1);
    let a2 = account_seed(&m, 2);
    assert_ne!(*a1, m);
    assert_ne!(*a1, *a2);
    assert_eq!(*a1, *account_seed(&m, 1));
    assert_ne!(*a1, *account_seed(&[6u8; 32], 1));
    // distinct addresses, so a payment to one is not seen by another
    let mut p = Purse::from_seed(&m, Network::Test, 0);
    p.add_account("Savings", 0).unwrap();
    p.add_account("Bills", 0).unwrap();
    let addrs: Vec<Address> = p.accounts().iter().map(|a| a.address()).collect();
    assert!(addrs[0] != addrs[1] && addrs[1] != addrs[2] && addrs[0] != addrs[2]);
    // an old one-account wallet of the same seed has the address of account 0
    assert_eq!(addrs[0], Wallet::from_seed(&m, Network::Test, 0).address());
}

#[test]
fn account_names_are_checked_and_the_number_of_accounts_is_limited() {
    let mut p = Purse::from_seed(&[1; 32], Network::Test, 0);
    assert_eq!(p.accounts()[0].label(), "Main");
    assert_eq!(p.add_account("", 0).err(), Some(PurseError::BadLabel));
    assert_eq!(p.add_account("   ", 0).err(), Some(PurseError::BadLabel));
    assert_eq!(p.add_account("a\nb", 0).err(), Some(PurseError::BadLabel));
    assert_eq!(
        p.add_account(&"x".repeat(MAX_LABEL + 1), 0).err(),
        Some(PurseError::BadLabel)
    );
    assert_eq!(p.add_account(&"x".repeat(MAX_LABEL), 0), Ok(1));
    assert_eq!(p.add_account("  Rent  ", 0), Ok(2));
    assert_eq!(p.accounts()[2].label(), "Rent");
    assert_eq!(p.rename(1, ""), Err(PurseError::BadLabel));
    assert_eq!(p.rename(9, "x"), Err(PurseError::NoSuchAccount(9)));
    p.rename(1, "Savings").unwrap();
    while p.accounts().len() < MAX_ACCOUNTS {
        p.add_account("more", 0).unwrap();
    }
    assert_eq!(
        p.add_account("one too many", 0),
        Err(PurseError::TooManyAccounts)
    );
    assert!(matches!(
        p.account(MAX_ACCOUNTS),
        Err(PurseError::NoSuchAccount(_))
    ));
}

// ---------------------------------------------------------------------------------------------------------------
// the file
// ---------------------------------------------------------------------------------------------------------------

fn tmp(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("tenero-purse-{}-{tag}.twl", std::process::id()))
}

#[test]
fn a_purse_file_keeps_every_account_and_its_name() {
    let path = tmp("roundtrip");
    let mut p = Purse::from_seed(&[8; 32], Network::Test, 12);
    p.add_account("Savings", 20).unwrap();
    p.add_account("Bills", 30).unwrap();
    p.save(
        &path,
        b"correct horse",
        KdfParams::TEST_ONLY_WEAK,
        &mut OsRng,
    )
    .unwrap();
    let back = Purse::load(&path, b"correct horse").unwrap();
    assert_eq!(back.master_seed(), p.master_seed());
    assert_eq!(back.birth_height(), 12);
    let names: Vec<&str> = back.accounts().iter().map(|a| a.label()).collect();
    assert_eq!(names, ["Main", "Savings", "Bills"]);
    for (a, b) in p.accounts().iter().zip(back.accounts()) {
        assert_eq!(a.address(), b.address());
        assert_eq!(a.wallet().birth_height(), b.wallet().birth_height());
    }
    assert_eq!(
        Purse::load(&path, b"wrong").err(),
        Some(FileError::WrongPassphraseOrCorrupt)
    );
    // an empty password is the app's choice to allow: the file layer works with it, and a wrong one still fails
    p.save(&path, b"", KdfParams::TEST_ONLY_WEAK, &mut OsRng)
        .unwrap();
    assert!(Purse::load(&path, b"").is_ok());
    assert!(Purse::load(&path, b"x").is_err());
    // the old reader refuses the new file by name, not with garbage
    assert_eq!(
        Wallet::load(&path, b"").err(),
        Some(FileError::NotAWalletFile)
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_phrase_of_a_purse_is_not_in_the_file_in_the_clear() {
    let path = tmp("clear");
    let p = Purse::from_seed(&[0x42; 32], Network::Test, 0);
    p.save(&path, b"pw", KdfParams::TEST_ONLY_WEAK, &mut OsRng)
        .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert!(
        !bytes.windows(32).any(|w| w == [0x42; 32]),
        "the seed is encrypted"
    );
    let first_word = p.phrase().split(' ').next().unwrap().to_string();
    assert!(
        !String::from_utf8_lossy(&bytes).contains(&first_word) || first_word.len() < 4,
        "no words in the file"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn an_old_one_account_wallet_file_opens_as_a_purse_and_a_changed_purse_file_is_refused() {
    let path = tmp("old");
    let old = Wallet::from_seed(&[4; 32], Network::Test, 7);
    old.save(&path, b"pw", KdfParams::TEST_ONLY_WEAK, &mut OsRng)
        .unwrap();
    let p = Purse::load(&path, b"pw").unwrap();
    assert_eq!(p.accounts().len(), 1);
    assert_eq!(p.accounts()[0].address(), old.address());
    assert_eq!(Some(p.master_seed()), old.seed());
    assert_eq!(p.birth_height(), 7);
    // saving it writes the new format, which opens again
    p.save(&path, b"pw", KdfParams::TEST_ONLY_WEAK, &mut OsRng)
        .unwrap();
    assert!(Purse::load(&path, b"pw").is_ok());

    // every single flipped byte is refused: the header is authenticated and the body is sealed
    let good = std::fs::read(&path).unwrap();
    for i in (0..good.len()).step_by(7) {
        let mut bad = good.clone();
        bad[i] ^= 1;
        std::fs::write(&path, &bad).unwrap();
        assert!(
            Purse::load(&path, b"pw").is_err(),
            "byte {i} changed and it still opened"
        );
    }
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------------------------------------------
// on a chain
// ---------------------------------------------------------------------------------------------------------------

struct Rig {
    path: PathBuf,
    store: Store,
    params: ChainParams,
}

impl Rig {
    fn new(tag: &str) -> Rig {
        let path = std::env::temp_dir().join(format!(
            "tenero-purse-chain-{}-{tag}.redb",
            std::process::id()
        ));
        remove(&path);
        let store = Store::open(&path, LABEL, PowKind::Sha256).unwrap();
        Rig {
            path,
            store,
            params: test_chain_params(),
        }
    }

    fn node(&self) -> Node<'_> {
        Node::new(&self.store, &self.params, &Sha256Pow, NodeConfig::default())
            .expect("a node with real proofs")
    }
}

fn remove(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let mut s = path.clone().into_os_string();
    s.push(".segments");
    let _ = std::fs::remove_dir_all(PathBuf::from(s));
}

impl Drop for Rig {
    fn drop(&mut self) {
        remove(&self.path);
    }
}

fn mine(node: &mut Node<'_>, to: &Address) -> u64 {
    let height = node.tip().unwrap().0 + 1;
    let ts = T0 + 60 * height;
    let mut block = node
        .block_template(ts, u64::MAX, &|amount| {
            coinbase_payout(&mut OsRng, to, height, amount).expect("a main address")
        })
        .unwrap();
    let target = node.next_block().unwrap().target;
    for nonce in 0.. {
        block.header.nonce = nonce;
        if U256::from_be_bytes(&block_id(&block.header, PowKind::Sha256)) < target {
            break;
        }
    }
    let amount = block.coinbase.outputs[0].amount;
    match node.submit_block(&block, ts + 10).expect("a valid block") {
        Submitted::Extended(_) => {}
        other => panic!("not extended: {other:?}"),
    }
    amount
}

#[test]
fn restoring_from_the_words_finds_the_accounts_that_were_used() {
    let rig = Rig::new("restore");
    let mut node = rig.node();
    let mut p = Purse::from_seed(&[11; 32], Network::Test, 0);
    p.add_account("Savings", 0).unwrap();
    p.add_account("Bills", 0).unwrap();
    let to0 = p.accounts()[0].address();
    let to2 = p.accounts()[2].address();
    let m0 = (0..3).map(|_| mine(&mut node, &to0)).sum::<u64>();
    let m2 = (0..2).map(|_| mine(&mut node, &to2)).sum::<u64>();
    p.sync(&node).unwrap();
    assert_eq!(p.balance(0, &node).unwrap().total, m0);
    assert_eq!(p.balance(1, &node).unwrap().total, 0);
    assert_eq!(p.balance(2, &node).unwrap().total, m2);
    assert_eq!(p.total_balance(&node).unwrap().total, m0 + m2);

    // a new computer: only the words
    let words = p.phrase();
    let seed = seed_of(&words).unwrap();
    let mut back = Purse::from_seed(&seed, Network::Test, 0);
    assert_eq!(back.accounts().len(), 1);
    let n = back.discover(&node).unwrap();
    // 0 used, 1 empty, 2 used, then GAP empty ones
    assert_eq!(n, 3 + GAP);
    assert_eq!(back.total_balance(&node).unwrap().total, m0 + m2);
    assert_eq!(back.accounts()[2].address(), to2);
    assert_eq!(back.accounts()[2].label(), "Account 2");
    // the names are not in the words: they come back as numbers
    assert_eq!(back.accounts()[1].label(), "Account 1");
}

#[test]
fn an_account_after_a_long_enough_gap_is_not_found_by_the_search_but_can_be_added_by_hand() {
    let rig = Rig::new("gap");
    let mut node = rig.node();
    let mut p = Purse::from_seed(&[12; 32], Network::Test, 0);
    for i in 1..=GAP + 1 {
        p.add_account(&format!("A{i}"), 0).unwrap();
    }
    let far = GAP + 1; // GAP empty accounts (1..=GAP) lie between 0 and this one
    let to0 = p.accounts()[0].address();
    let to_far = p.accounts()[far].address();
    let m0 = mine(&mut node, &to0);
    let mf = mine(&mut node, &to_far);

    let mut back = Purse::from_seed(p.master_seed(), Network::Test, 0);
    let n = back.discover(&node).unwrap();
    assert_eq!(
        n,
        1 + GAP,
        "the search stops after {GAP} empty accounts in a row"
    );
    assert_eq!(
        back.total_balance(&node).unwrap().total,
        m0,
        "the far account is missed"
    );
    // the owner knows there was one more: adding accounts by hand finds it
    back.add_account("Far", 0).unwrap();
    back.sync(&node).unwrap();
    assert_eq!(back.total_balance(&node).unwrap().total, m0 + mf);
}

#[test]
fn a_payment_comes_from_one_account_and_lands_in_another() {
    let rig = Rig::new("pay");
    let mut node = rig.node();
    let mut p = Purse::from_seed(&[13; 32], Network::Test, 0);
    p.add_account("Savings", 0).unwrap();
    let (a0, a1) = (p.accounts()[0].address(), p.accounts()[1].address());
    (0..READY).for_each(|_| {
        mine(&mut node, &a0);
    });
    p.sync(&node).unwrap();
    // account 1 has nothing, so it cannot pay even though the purse as a whole could
    assert!(p.total_balance(&node).unwrap().spendable > 0);
    assert!(matches!(
        p.build_payment(1, &node, &mut OsRng, &a0, 1, FeeLevel::Low),
        Err(PurseError::Wallet(WalletError::NotEnough {
            spendable: 0,
            ..
        }))
    ));
    assert!(matches!(
        p.pay(7, &mut node, &mut OsRng, &a0, 1, FeeLevel::Low, 0),
        Err(PurseError::NoSuchAccount(7))
    ));
    // account 0 pays account 1
    let amount = 1_000_000_000;
    let built = p
        .pay(
            0,
            &mut node,
            &mut OsRng,
            &a1,
            amount,
            FeeLevel::Low,
            1_700_000_000,
        )
        .unwrap();
    mine(&mut node, &a0);
    p.sync(&node).unwrap();
    assert_eq!(p.balance(1, &node).unwrap().total, amount);
    assert!(built.fee > 0);
    // and the purse survives a save and a load in the middle of all this
    let path = tmp("midway");
    p.save(&path, b"pw", KdfParams::TEST_ONLY_WEAK, &mut OsRng)
        .unwrap();
    let mut again = Purse::load(&path, b"pw").unwrap();
    again.sync(&node).unwrap();
    assert_eq!(
        again.total_balance(&node).unwrap(),
        p.total_balance(&node).unwrap()
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_three_fee_levels_pay_one_and_a_quarter_two_and_five_times_the_minimum() {
    let rig = Rig::new("fees");
    let mut node = rig.node();
    let mut p = Purse::from_seed(&[14; 32], Network::Test, 0);
    let to = Purse::from_seed(&[15; 32], Network::Test, 0).accounts()[0].address();
    let me = p.accounts()[0].address();
    (0..READY).for_each(|_| {
        mine(&mut node, &me);
    });
    p.sync(&node).unwrap();
    let mut fees = Vec::new();
    for level in FeeLevel::ALL {
        let built = p
            .build_payment(0, &node, &mut OsRng, &to, 1_000, level)
            .unwrap();
        let size = tenero_core::v3::Wire::to_bytes(&built.tx).unwrap().len() as u64;
        let next = node.next_block().unwrap();
        let min = tenero_core::v3::rules::min_fee(size, next.reward, next.median).unwrap();
        let want = min * level.percent_of_minimum() / 100 + 1;
        assert_eq!(
            built.fee,
            want,
            "{}: the fee is exactly {}% of the minimum",
            level.name(),
            level.percent_of_minimum()
        );
        assert!(built.fee >= min, "never below the minimum");
        fees.push(built.fee);
        // the node takes it at every level
        node.submit_tx(built.tx.clone()).unwrap();
        // free the node's pool for the next level: the coins are not reserved by building, so a block takes it
        mine(&mut node, &me);
        p.sync(&node).unwrap();
    }
    assert!(fees[0] < fees[1] && fees[1] < fees[2], "{fees:?}");
    println!("fees at Low, Normal, High: {fees:?}");
}

#[test]
fn the_history_lists_what_came_in_what_went_out_and_where_each_payment_stands_and_never_the_change()
{
    let rig = Rig::new("history");
    let mut node = rig.node();
    let mut alice = Purse::from_seed(&[16; 32], Network::Test, 0);
    let mut bob = Purse::from_seed(&[17; 32], Network::Test, 0);
    let (a, b) = (alice.accounts()[0].address(), bob.accounts()[0].address());
    (0..READY).for_each(|_| {
        mine(&mut node, &a);
    });
    alice.sync(&node).unwrap();
    let amount = 2_500_000_000;
    let built = alice
        .pay(
            0,
            &mut node,
            &mut OsRng,
            &b,
            amount,
            FeeLevel::Normal,
            1_700_000_123,
        )
        .unwrap();
    // waiting in the pool
    let h = alice.history(&node).unwrap();
    let sent = h
        .iter()
        .find(|e| matches!(e.kind, EntryKind::Sent { .. }))
        .unwrap();
    assert_eq!(sent.amount, amount);
    assert_eq!(sent.id, Some(built.id));
    match &sent.kind {
        EntryKind::Sent {
            to,
            fee,
            status,
            time,
        } => {
            assert_eq!(*to, b);
            assert_eq!(*fee, built.fee);
            assert_eq!(*status, SentStatus::Pending);
            assert_eq!(*time, 1_700_000_123);
        }
        _ => unreachable!(),
    }
    // a block takes it
    mine(&mut node, &a);
    alice.sync(&node).unwrap();
    bob.sync(&node).unwrap();
    let h = alice.history(&node).unwrap();
    let confirmed = h
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                EntryKind::Sent {
                    status: SentStatus::Confirmed,
                    ..
                }
            )
        })
        .count();
    assert_eq!(confirmed, 1);
    // the block rewards, and none of them is the change that came back
    let mined: Vec<_> = h.iter().filter(|e| e.kind == EntryKind::Mined).collect();
    let received = h.iter().filter(|e| e.kind == EntryKind::Received).count();
    assert_eq!(mined.len(), READY + 1);
    assert_eq!(received, 0, "the change is not a payment received");
    // the change IS in the balance, though
    let bal = alice.total_balance(&node).unwrap().total;
    let rewards: u64 = mined.iter().map(|e| e.amount).sum();
    assert_eq!(
        bal + amount + built.fee,
        rewards,
        "nothing lost: balance + sent + fee = rewards (the fee came back in the last reward)"
    );
    // newest first
    assert!(h.windows(2).all(|w| w[0].height >= w[1].height));
    // Bob sees one payment received, from nobody in particular
    let hb = bob.history(&node).unwrap();
    assert_eq!(hb.len(), 1);
    assert_eq!(hb[0].kind, EntryKind::Received);
    assert_eq!(hb[0].amount, amount);

    // the records survive the file; a purse restored from the words has the receipts but not the record of sending
    let path = tmp("history");
    alice
        .save(&path, b"pw", KdfParams::TEST_ONLY_WEAK, &mut OsRng)
        .unwrap();
    let back = Purse::load(&path, b"pw").unwrap();
    assert_eq!(back.sent_records(), alice.sent_records());
    let mut restored = Purse::from_seed(alice.master_seed(), Network::Test, 0);
    restored.sync(&node).unwrap();
    let hr = restored.history(&node).unwrap();
    assert!(hr.iter().all(|e| !matches!(e.kind, EntryKind::Sent { .. })));
    // even without the record, the change is not taken for something received: Carrot change is an internal self-send,
    // which the wallet recognises as its own (the interim scheme of beta could not)
    assert_eq!(
        hr.iter().filter(|e| e.kind == EntryKind::Received).count(),
        0
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_payment_the_node_dropped_is_shown_as_not_confirmed_once_its_reservation_runs_out() {
    let rig = Rig::new("dropped");
    let mut node = rig.node();
    let mut p = Purse::from_seed(&[18; 32], Network::Test, 0);
    let me = p.accounts()[0].address();
    let to = Purse::from_seed(&[19; 32], Network::Test, 0).accounts()[0].address();
    (0..READY).for_each(|_| {
        mine(&mut node, &me);
    });
    p.sync(&node).unwrap();
    p.pay(0, &mut node, &mut OsRng, &to, 1_000, FeeLevel::Low, 5)
        .unwrap();
    // the node forgets it (a restart empties the pool) and the chain moves on without it
    let tx_id = p.sent_records()[0].id;
    drop(node);
    let mut node = rig.node();
    assert!(node.pool().is_empty(), "a restart empties the pool");
    for _ in 0..(tenero_wallet::wallet::RESERVE_BLOCKS + 2) {
        mine(&mut node, &me);
    }
    p.sync(&node).unwrap();
    let _ = p.balance(0, &node).unwrap(); // drops the lapsed reservation
    let h = p.history(&node).unwrap();
    let s = h.iter().find(|e| e.id == Some(tx_id)).unwrap();
    assert!(
        matches!(
            s.kind,
            EntryKind::Sent {
                status: SentStatus::NotConfirmed,
                ..
            }
        ),
        "{:?}",
        s.kind
    );
}

// ---------------------------------------------------------------------------------------------------------------
// batches and combining
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn a_batch_is_recorded_one_payment_at_a_time_with_the_fee_once_and_the_history_shows_each() {
    let rig = Rig::new("batchrec");
    let mut node = rig.node();
    let mut alice = Purse::from_seed(&[21; 32], Network::Test, 0);
    let a = alice.accounts()[0].address();
    let others: Vec<Address> = (30..50u8)
        .map(|i| Purse::from_seed(&[i; 32], Network::Test, 0).accounts()[0].address())
        .collect();
    (0..READY).for_each(|_| {
        mine(&mut node, &a);
    });
    alice.sync(&node).unwrap();
    let dests: Vec<(Address, u64)> = others.iter().map(|o| (*o, 100_000_000)).collect();
    let plan = alice
        .build_batch(0, &node, &mut OsRng, &dests, FeeLevel::Low)
        .unwrap();
    assert_eq!(plan.txs.len(), 2, "twenty recipients: fifteen and five");
    let sent = alice
        .send_batch(0, &mut node, &plan.txs, FeeLevel::Low, 1_700_000_500)
        .unwrap();
    assert_eq!((sent.sent, sent.failed.is_none()), (2, true));
    // twenty records, the fee of each transaction on its first record only
    let records = alice.sent_records();
    assert_eq!(records.len(), 20);
    let fees: u64 = records.iter().map(|r| r.fee).sum();
    assert_eq!(fees, plan.txs.iter().map(|t| t.fee).sum::<u64>());
    for r in records {
        assert!(r.anchor.is_some() && r.payment_onetime.is_some());
    }
    let h = alice.history(&node).unwrap();
    let paid: Vec<_> = h
        .iter()
        .filter(|e| matches!(e.kind, EntryKind::Sent { .. }))
        .collect();
    assert_eq!(paid.len(), 20);
    assert!(paid.iter().all(|e| e.amount == 100_000_000));
    // the file keeps them
    let path = tmp("batchrec");
    alice
        .save(&path, b"pw", KdfParams::TEST_ONLY_WEAK, &mut OsRng)
        .unwrap();
    let back = Purse::load(&path, b"pw").unwrap();
    assert_eq!(back.sent_records().len(), 20);
    let _ = std::fs::remove_file(&path);
    // and a block takes both transactions
    mine(&mut node, &a);
    assert_eq!(node.pool().len(), 0);
}

#[test]
fn a_combine_is_recorded_as_a_payment_to_oneself_and_its_coin_is_not_money_received() {
    let rig = Rig::new("combinerec");
    let mut node = rig.node();
    let mut p = Purse::from_seed(&[22; 32], Network::Test, 0);
    let a = p.accounts()[0].address();
    (0..READY).for_each(|_| {
        mine(&mut node, &a);
    });
    p.sync(&node).unwrap();
    let before = p.total_balance(&node).unwrap().total;
    let built = p
        .build_combine(0, &node, &mut OsRng, 5, FeeLevel::Low)
        .unwrap();
    let sent = p
        .send_own(
            0,
            &mut node,
            std::slice::from_ref(&built),
            FeeLevel::Low,
            "combined 5 coins",
            1_700_000_900,
        )
        .unwrap();
    assert_eq!(sent.sent, 1);
    for _ in 0..4 {
        mine(
            &mut node,
            &Purse::from_seed(&[99; 32], Network::Test, 0).accounts()[0].address(),
        );
    }
    p.sync(&node).unwrap();
    assert_eq!(
        p.total_balance(&node).unwrap().total,
        before - built.fee,
        "only the fee is gone"
    );
    let h = p.history(&node).unwrap();
    // the block rewards are listed as mined, and the combined coin is not listed at all as received
    let received_like: Vec<_> = h
        .iter()
        .filter(|e| matches!(e.kind, EntryKind::Received))
        .collect();
    assert!(
        received_like.is_empty(),
        "the combined coin shown as received: {received_like:?}"
    );
    let rec = &p.sent_records()[0];
    assert_eq!(rec.note.as_deref(), Some("combined 5 coins"));
    assert_eq!((rec.to, rec.amount, rec.fee), (a, built.amount, built.fee));
}
