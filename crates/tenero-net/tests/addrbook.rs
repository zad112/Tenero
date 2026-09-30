//! The address book and the ban list, on their own (no network): what they accept, what they bound, how they
//! choose whom to dial, and that a saved copy that has been damaged in any way is refused.

use std::collections::HashSet;

use tenero_core::hash::sha256;
use tenero_net::addrbook::{
    group_of, host_of, is_routable, peer_addr_to_string, string_to_peer_addr, v4, AddrBook,
    AddrBookConfig, BanList,
};

const NOW: u64 = 1_700_000_000;

fn book() -> AddrBook {
    AddrBook::new(AddrBookConfig::default())
}

fn sa(s: &str) -> std::net::SocketAddr {
    s.parse().unwrap()
}

// ---- what is an address ---------------------------------------------------------------------------

#[test]
fn only_public_addresses_are_routable() {
    for ok in [
        "8.8.8.8:8333",
        "45.33.32.156:1",
        "[2a00:1450::1]:8333",
        "[::ffff:8.8.4.4]:8333",
    ] {
        assert!(is_routable(&sa(ok)), "{ok}");
    }
    for bad in [
        "0.0.0.0:8333",
        "127.0.0.1:8333",
        "10.1.2.3:8333",
        "172.16.0.1:8333",
        "172.31.255.255:8333",
        "192.168.1.1:8333",
        "169.254.1.1:8333",
        "224.0.0.1:8333",
        "255.255.255.255:8333",
        "192.0.2.1:8333",
        "198.51.100.7:8333",
        "203.0.113.9:8333",
        "8.8.8.8:0",
        "[::]:8333",
        "[::1]:8333",
        "[fc00::1]:8333",
        "[fd12:3456::1]:8333",
        "[fe80::1]:8333",
        "[ff02::1]:8333",
        "[2001:db8::1]:8333",
        "[::ffff:10.0.0.1]:8333",
        "[::ffff:127.0.0.1]:8333",
    ] {
        assert!(!is_routable(&sa(bad)), "{bad} must not be routable");
    }
}

#[test]
fn network_groups_and_hosts() {
    assert_eq!(group_of("45.33.32.156:8333"), "v4:45.33");
    assert_eq!(group_of("45.33.99.1:9"), "v4:45.33");
    assert_ne!(group_of("45.34.0.1:8333"), group_of("45.33.0.1:8333"));
    assert_eq!(group_of("[::ffff:45.33.1.1]:8333"), "v4:45.33");
    assert_eq!(group_of("[2a00:1450:4001::1]:8333"), "v6:2a00:1450");
    assert_eq!(group_of("[2a00:1450:9999::1]:1"), "v6:2a00:1450");
    assert_eq!(group_of("node-7"), "raw:node-7");
    // a ban applies to the host, whatever port an inbound peer connected from
    assert_eq!(host_of("45.33.32.156:8333"), host_of("45.33.32.156:51000"));
    assert_ne!(host_of("45.33.32.156:8333"), host_of("45.33.32.157:8333"));
    assert_eq!(
        host_of("[::ffff:45.33.32.156]:1"),
        host_of("45.33.32.156:2")
    );
    assert_eq!(host_of("evil"), "raw:evil");
}

#[test]
fn gossiped_addresses_convert_both_ways() {
    for text in ["45.33.32.156:8333", "[2a00:1450::1]:65535"] {
        let a = string_to_peer_addr(text, 77).unwrap();
        assert_eq!(a.last_seen, 77);
        assert_eq!(peer_addr_to_string(&a).unwrap(), text);
    }
    // an IPv4 address is carried IPv4-mapped
    let a = string_to_peer_addr("1.2.3.4:5", 0).unwrap();
    assert_eq!(&a.ip[..12], &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff]);
    assert_eq!(&a.ip[12..], &[1, 2, 3, 4]);
    assert!(string_to_peer_addr("not an address", 0).is_none());
}

// ---- what the book accepts ---------------------------------------------------------------------

#[test]
fn the_book_refuses_what_it_should_not_dial() {
    let mut b = book();
    assert!(b.add(&v4(45, 33, 1, 1, 8333), NOW, "g", NOW));
    for bad in [
        v4(10, 0, 0, 1, 8333),  // private
        v4(127, 0, 0, 1, 8333), // loopback
        v4(45, 33, 1, 2, 0),    // port 0
        "not-an-address".to_string(),
    ] {
        assert!(!b.add(&bad, NOW, "g", NOW), "{bad}");
    }
    assert_eq!(b.len(), 1);
    // a private network can say so
    let mut p = AddrBook::new(AddrBookConfig {
        accept_private: true,
        ..AddrBookConfig::default()
    });
    assert!(p.add(&v4(10, 0, 0, 1, 8333), NOW, "g", NOW));
    assert!(
        !p.add(&v4(10, 0, 0, 2, 0), NOW, "g", NOW),
        "port 0 is never valid"
    );
    assert!(
        !p.add("node-3", NOW, "g", NOW),
        "and a name is not an address"
    );
}

#[test]
fn stale_and_future_claims_are_handled() {
    let mut b = book();
    let month = 30 * 24 * 3600;
    assert!(
        !b.add(&v4(45, 33, 1, 1, 1), NOW - month - 1, "g", NOW),
        "stale"
    );
    assert!(
        b.add(&v4(45, 33, 1, 2, 1), NOW - month, "g", NOW),
        "exactly at the limit"
    );
    // a peer cannot claim an address was seen in the future
    assert!(b.add(&v4(45, 33, 1, 3, 1), NOW + 10_000, "g", NOW));
    assert_eq!(b.get(&v4(45, 33, 1, 3, 1)).unwrap().last_seen, NOW);
    // a known address keeps its record, and its last_seen only moves forward
    let a = v4(45, 33, 1, 2, 1);
    assert!(b.add(&a, NOW - 5, "someone else", NOW));
    assert_eq!(b.get(&a).unwrap().last_seen, NOW - 5);
    assert_eq!(b.get(&a).unwrap().source, "g", "the first source is kept");
    assert!(b.add(&a, NOW - 100, "g", NOW));
    assert_eq!(b.get(&a).unwrap().last_seen, NOW - 5, "never moves back");
}

#[test]
fn one_source_group_can_fill_only_its_share() {
    let mut b = AddrBook::new(AddrBookConfig {
        max_new_per_source: 5,
        ..AddrBookConfig::default()
    });
    let mut accepted = 0;
    for i in 0..50u8 {
        if b.add(&v4(45, 33, 1, i + 1, 8333), NOW, "attacker", NOW) {
            accepted += 1;
        }
    }
    assert_eq!(accepted, 5);
    assert_eq!(b.from_source("attacker"), 5);
    // another source is not held back by it
    assert!(b.add(&v4(46, 1, 1, 1, 8333), NOW, "honest", NOW));
    // seeds are exempt from the cap
    for i in 0..10u8 {
        assert!(b.add(&v4(47, 1, 1, i + 1, 8333), NOW, "seed", NOW));
    }
    // an address that has connected no longer counts as new, so the source may add more
    b.mark_success(&v4(45, 33, 1, 1, 8333), NOW);
    assert!(b.add(&v4(45, 33, 9, 9, 8333), NOW, "attacker", NOW));
}

#[test]
fn a_full_book_drops_the_worst_entry_and_never_a_seed_or_a_tried_one_first() {
    let mut b = AddrBook::new(AddrBookConfig {
        max_entries: 6,
        max_new_per_source: 100,
        ..AddrBookConfig::default()
    });
    let seed = v4(60, 0, 0, 1, 8333);
    let tried = v4(61, 0, 0, 1, 8333);
    let old_new = v4(62, 0, 0, 1, 8333);
    let failing = v4(63, 0, 0, 1, 8333);
    assert!(b.add(&seed, 0, "seed", 0));
    assert!(b.add(&tried, NOW - 1000, "g", NOW));
    b.mark_success(&tried, NOW - 1000);
    assert!(b.add(&old_new, NOW - 5000, "g", NOW));
    assert!(b.add(&failing, NOW - 10, "g", NOW));
    b.mark_failure(&failing, 1);
    b.mark_failure(&failing, 2);
    assert!(b.add(&v4(64, 0, 0, 1, 8333), NOW, "g", NOW));
    assert!(b.add(&v4(65, 0, 0, 1, 8333), NOW, "g", NOW));
    assert_eq!(b.len(), 6);
    // full: a new address pushes out the never-worked entry with the most failures first
    assert!(b.add(&v4(66, 0, 0, 1, 8333), NOW, "g", NOW));
    assert!(b.get(&failing).is_none(), "the failing one goes first");
    assert!(b.add(&v4(67, 0, 0, 1, 8333), NOW, "g", NOW));
    assert!(b.get(&old_new).is_none(), "then the stalest new one");
    assert!(b.get(&seed).is_some() && b.get(&tried).is_some());
    // however many more come, the seed and the tried address stay
    for i in 0..30u8 {
        b.add(&v4(70, 0, 0, i + 1, 8333), NOW, "g", NOW);
    }
    assert!(b.get(&seed).is_some() && b.get(&tried).is_some());
    assert_eq!(b.len(), 6);
    // a book of nothing but seeds cannot make room
    let mut only_seeds = AddrBook::new(AddrBookConfig {
        max_entries: 2,
        ..AddrBookConfig::default()
    });
    assert!(only_seeds.add(&v4(60, 0, 0, 1, 8333), 0, "seed", 0));
    assert!(only_seeds.add(&v4(60, 0, 0, 2, 8333), 0, "seed", 0));
    assert!(!only_seeds.add(&v4(60, 0, 0, 3, 8333), NOW, "g", NOW));
}

#[test]
fn stale_entries_expire_but_seeds_do_not() {
    let mut b = book();
    let month = 30 * 24 * 3600;
    assert!(b.add(&v4(45, 0, 0, 1, 1), NOW - 1000, "g", NOW));
    assert!(b.add(&v4(46, 0, 0, 1, 1), 0, "seed", 0));
    b.expire(NOW + month - 1500); // it was last seen at NOW - 1000, so it is month - 500 old: not yet stale
    assert_eq!(b.len(), 2, "not yet stale");
    b.expire(NOW + month + 1000);
    assert_eq!(b.len(), 1);
    assert!(b.get(&v4(46, 0, 0, 1, 1)).is_some());
}

// ---- failures, backoff, and choosing whom to dial ----------------------------------------------

#[test]
fn backoff_doubles_up_to_a_ceiling() {
    let b = AddrBook::new(AddrBookConfig {
        backoff_base_ms: 1000,
        backoff_max_ms: 30_000,
        ..AddrBookConfig::default()
    });
    let waits: Vec<u64> = (0..8).map(|f| b.backoff_ms(f)).collect();
    assert_eq!(
        waits,
        vec![0, 1000, 2000, 4000, 8000, 16_000, 30_000, 30_000]
    );
    assert_eq!(b.backoff_ms(u32::MAX), 30_000, "no overflow");
}

fn dial(b: &mut AddrBook, now_ms: u64, limit: usize) -> Vec<String> {
    b.candidates(now_ms, limit, &|_| false, &|_| false)
}

#[test]
fn a_failed_address_is_left_alone_until_its_backoff_has_passed() {
    let mut b = AddrBook::new(AddrBookConfig {
        backoff_base_ms: 10_000,
        min_redial_ms: 0,
        ..AddrBookConfig::default()
    });
    let a = v4(45, 0, 0, 1, 8333);
    assert!(b.add(&a, NOW, "g", NOW));
    assert_eq!(dial(&mut b, 1000, 5), vec![a.clone()]);
    b.mark_failure(&a, 1000);
    assert!(dial(&mut b, 10_999, 5).is_empty(), "first retry after 10 s");
    assert_eq!(dial(&mut b, 11_000, 5), vec![a.clone()]);
    b.mark_failure(&a, 11_000);
    assert!(dial(&mut b, 30_999, 5).is_empty(), "second after 20 s");
    assert_eq!(dial(&mut b, 31_000, 5), vec![a.clone()]);
    // success clears it
    b.mark_success(&a, NOW);
    assert_eq!(b.get(&a).unwrap().failures, 0);
    assert!(b.get(&a).unwrap().tried);
}

#[test]
fn an_address_that_never_worked_is_forgotten_after_enough_failures() {
    let mut b = AddrBook::new(AddrBookConfig {
        max_failures: 3,
        ..AddrBookConfig::default()
    });
    let never = v4(45, 0, 0, 1, 1);
    let tried = v4(46, 0, 0, 1, 1);
    let seed = v4(47, 0, 0, 1, 1);
    b.add(&never, NOW, "g", NOW);
    b.add(&tried, NOW, "g", NOW);
    b.add(&seed, 0, "seed", 0);
    b.mark_success(&tried, NOW);
    for _ in 0..5 {
        for a in [&never, &tried, &seed] {
            b.mark_failure(a, 1);
        }
    }
    assert!(b.get(&never).is_none(), "a never-worked address is dropped");
    assert!(
        b.get(&tried).is_some(),
        "an address that once worked is kept"
    );
    assert!(b.get(&seed).is_some(), "a seed is kept");
    assert_eq!(b.get(&tried).unwrap().failures, 5);
}

#[test]
fn candidates_skip_what_is_excluded_and_respect_the_limit() {
    let mut b = book();
    for i in 1..=20u8 {
        b.add(&v4(45 + i, 1, 1, 1, 8333), NOW, "g", NOW);
    }
    let skipped: HashSet<String> = (1..=5u8).map(|i| v4(45 + i, 1, 1, 1, 8333)).collect();
    let got = b.candidates(0, 8, &|a| skipped.contains(a), &|_| false);
    assert_eq!(got.len(), 8);
    assert!(got.iter().all(|a| !skipped.contains(a)));
    assert_eq!(got.iter().collect::<HashSet<_>>().len(), 8, "no duplicates");
    // a group that is full is left out entirely
    let blocked = group_of(&v4(50, 1, 1, 1, 8333));
    let got = b.candidates(0, 100, &|_| false, &|g| g == blocked);
    assert_eq!(got.len(), 19);
    assert!(got.iter().all(|a| group_of(a) != blocked));
    assert!(b.candidates(0, 0, &|_| false, &|_| false).is_empty());
}

#[test]
fn tried_and_new_addresses_are_both_represented() {
    let mut b = book();
    for i in 1..=10u8 {
        let a = v4(45, i, 1, 1, 8333);
        b.add(&a, NOW, "g", NOW);
        b.mark_success(&a, NOW);
        b.add(&v4(46, i, 1, 1, 8333), NOW, "h", NOW);
    }
    let mut saw_tried = 0;
    let mut saw_new = 0;
    for _ in 0..40 {
        for a in b.candidates(0, 4, &|_| false, &|_| false) {
            if b.get(&a).unwrap().tried {
                saw_tried += 1;
            } else {
                saw_new += 1;
            }
        }
    }
    assert!(
        saw_tried > 30 && saw_new > 30,
        "tried {saw_tried}, new {saw_new}"
    );
    // and when only one kind exists, that kind is used
    let mut only_new = book();
    only_new.add(&v4(45, 1, 1, 1, 1), NOW, "g", NOW);
    assert_eq!(only_new.candidates(0, 3, &|_| false, &|_| false).len(), 1);
}

#[test]
fn choices_depend_only_on_the_seed() {
    let make = |seed: u64| {
        let mut b = AddrBook::new(AddrBookConfig {
            seed,
            ..AddrBookConfig::default()
        });
        for i in 1..=40u8 {
            b.add(&v4(45, i, 1, 1, 8333), NOW, "g", NOW);
        }
        b
    };
    let (mut a, mut b, mut c) = (make(1), make(1), make(2));
    let (ra, rb, rc) = (
        dial(&mut a, 0, 10),
        dial(&mut b, 0, 10),
        dial(&mut c, 0, 10),
    );
    assert_eq!(ra, rb);
    assert_ne!(ra, rc);
    let (sa_, sb_) = (a.sample(20, NOW), b.sample(20, NOW));
    assert_eq!(sa_, sb_);
}

// ---- what is told to others --------------------------------------------------------------------

#[test]
fn a_sample_is_bounded_fresh_and_without_repeats() {
    let mut b = book();
    let month = 30 * 24 * 3600;
    for i in 1..=30u8 {
        b.add(&v4(45, i, 1, 1, 8333), NOW - u64::from(i), "g", NOW);
    }
    let s = b.sample(10, NOW);
    assert_eq!(s.len(), 10);
    let texts: HashSet<String> = s.iter().map(|a| peer_addr_to_string(a).unwrap()).collect();
    assert_eq!(texts.len(), 10);
    assert_eq!(b.sample(100, NOW).len(), 30, "never more than it has");
    assert!(b.sample(0, NOW).is_empty());
    // entries that have gone stale are not passed on
    assert!(b.sample(100, NOW + month + 1000).is_empty());
    // each entry is sent with its own last_seen
    for a in b.sample(100, NOW) {
        let text = peer_addr_to_string(&a).unwrap();
        assert_eq!(a.last_seen, b.get(&text).unwrap().last_seen);
    }
}

// ---- persistence ------------------------------------------------------------------------------

fn sample_book() -> AddrBook {
    let mut b = book();
    for i in 1..=12u8 {
        let a = v4(45, i, 1, 1, 8333);
        b.add(
            &a,
            NOW - u64::from(i) * 60,
            if i % 2 == 0 { "g" } else { "h" },
            NOW,
        );
        if i % 3 == 0 {
            b.mark_success(&a, NOW - 10);
        }
        if i % 4 == 0 {
            b.mark_failure(&a, 5);
        }
    }
    b.add(&v4(47, 1, 1, 1, 1), 0, "seed", 0);
    b
}

#[test]
fn a_saved_book_loads_back_exactly() {
    let b = sample_book();
    let bytes = b.to_bytes();
    let back = AddrBook::from_bytes(AddrBookConfig::default(), &bytes).unwrap();
    assert_eq!(back.len(), b.len());
    for a in b.addrs() {
        let (x, y) = (b.get(&a).unwrap(), back.get(&a).unwrap());
        assert_eq!(
            (&x.addr, x.last_seen, &x.source, x.tried, x.failures),
            (&y.addr, y.last_seen, &y.source, y.tried, y.failures)
        );
    }
    assert_eq!(back.to_bytes(), bytes, "one book, one encoding");
    let empty = AddrBook::from_bytes(AddrBookConfig::default(), &book().to_bytes()).unwrap();
    assert!(empty.is_empty());
}

#[test]
fn a_damaged_saved_book_is_refused_in_every_way() {
    let bytes = sample_book().to_bytes();
    let cfg = AddrBookConfig::default;
    // every single-bit flip is caught by the checksum or the format
    for i in 0..bytes.len() {
        let mut bad = bytes.clone();
        bad[i] ^= 1;
        assert!(
            AddrBook::from_bytes(cfg(), &bad).is_err(),
            "a flip at byte {i} was accepted"
        );
    }
    // every truncation, and trailing bytes
    for cut in 0..bytes.len() {
        assert!(
            AddrBook::from_bytes(cfg(), &bytes[..cut]).is_err(),
            "cut at {cut}"
        );
    }
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(AddrBook::from_bytes(cfg(), &longer).is_err());
    assert!(AddrBook::from_bytes(cfg(), b"").is_err());
    assert!(AddrBook::from_bytes(cfg(), b"TAB2xxxxxxxxxxxxxxxx").is_err());
    // more entries than the book may hold
    let small = AddrBookConfig {
        max_entries: 3,
        ..AddrBookConfig::default()
    };
    assert!(AddrBook::from_bytes(small, &bytes).is_err());
}

// ---- the ban list ------------------------------------------------------------------------------

#[test]
fn a_ban_applies_to_the_host_and_ends() {
    let mut l = BanList::new();
    l.ban("99.1.1.1:5000", 10_000);
    assert!(
        l.is_banned("99.1.1.1:6000", 9_999),
        "another port, same host"
    );
    assert!(l.is_banned("99.1.1.1:5000", 9_999));
    assert!(
        !l.is_banned("99.1.1.1:5000", 10_000),
        "it ends exactly at `until`"
    );
    assert!(!l.is_banned("99.1.1.2:5000", 0), "another host");
    l.ban("evil", 50);
    assert!(l.is_banned("evil", 49));
    // a later ban does not shorten an earlier one, and a longer one extends it
    l.ban("99.1.1.1:1", 5_000);
    assert!(l.is_banned("99.1.1.1:1", 9_999));
    l.ban("99.1.1.1:1", 20_000);
    assert!(l.is_banned("99.1.1.1:1", 19_999));
    l.expire(10_000);
    assert_eq!(l.len(), 1, "only the longer ban is left");
    l.expire(20_000);
    assert!(l.is_empty());
}

#[test]
fn a_saved_ban_list_loads_back_and_damage_is_refused() {
    let mut l = BanList::new();
    l.ban("99.1.1.1:5000", 10_000);
    l.ban("[2a00::1]:1", 20_000);
    l.ban("evil", 30_000);
    let bytes = l.to_bytes();
    let back = BanList::from_bytes(&bytes).unwrap();
    assert!(
        back.is_banned("99.1.1.1:1", 9_999) && back.is_banned("2a00::1", 19_999) || back.len() == 3
    );
    assert_eq!(back.to_bytes(), bytes);
    for i in 0..bytes.len() {
        let mut bad = bytes.clone();
        bad[i] ^= 1;
        assert!(BanList::from_bytes(&bad).is_err(), "a flip at byte {i}");
    }
    for cut in 0..bytes.len() {
        assert!(BanList::from_bytes(&bytes[..cut]).is_err(), "cut at {cut}");
    }
    let mut longer = bytes.clone();
    longer.push(9);
    assert!(BanList::from_bytes(&longer).is_err());
    assert!(BanList::from_bytes(&BanList::new().to_bytes())
        .unwrap()
        .is_empty());
}

#[test]
fn an_address_is_not_redialled_at_once_even_if_its_last_connection_worked() {
    let mut b = AddrBook::new(AddrBookConfig {
        min_redial_ms: 30_000,
        ..AddrBookConfig::default()
    });
    let a = v4(45, 0, 0, 1, 8333);
    assert!(b.add(&a, NOW, "g", NOW));
    assert_eq!(dial(&mut b, 1000, 5), vec![a.clone()]);
    b.mark_attempt(&a, 1000);
    b.mark_success(&a, NOW); // it connected, and was dropped straight away
    assert!(
        dial(&mut b, 30_999, 5).is_empty(),
        "not before the minimum gap"
    );
    assert_eq!(dial(&mut b, 31_000, 5), vec![a.clone()]);
    // the gap applies after a failure too, whichever of it and the backoff is longer
    b.mark_failure(&a, 40_000);
    b.mark_failure(&a, 41_000); // two failures: a 60 s backoff, longer than the gap
    assert!(dial(&mut b, 100_999, 5).is_empty());
    assert_eq!(dial(&mut b, 101_000, 5), vec![a]);
}

#[test]
fn forgetting_happens_exactly_at_the_limit() {
    let mut b = AddrBook::new(AddrBookConfig {
        max_failures: 3,
        ..AddrBookConfig::default()
    });
    let a = v4(45, 0, 0, 1, 1);
    b.add(&a, NOW, "g", NOW);
    b.mark_failure(&a, 1);
    b.mark_failure(&a, 2);
    assert!(b.get(&a).is_some(), "two failures of three allowed");
    b.mark_failure(&a, 3);
    assert!(b.get(&a).is_none(), "the third is the last");
}

/// A saved file with a VALID checksum but malformed contents: the checksum only detects damage, so the format's own
/// checks must hold up by themselves.
fn seal(mut body: Vec<u8>) -> Vec<u8> {
    let sum = sha256(&[&body]);
    body.extend_from_slice(&sum[..4]);
    body
}

fn book_entry(addr: &str, tried: u8) -> Vec<u8> {
    let mut e = vec![addr.len() as u8];
    e.extend_from_slice(addr.as_bytes());
    e.extend_from_slice(&5u64.to_le_bytes());
    e.push(1);
    e.push(b'g');
    e.push(tried);
    e.extend_from_slice(&0u32.to_le_bytes());
    e
}

#[test]
fn a_book_with_a_good_checksum_but_a_bad_format_is_refused() {
    let cfg = AddrBookConfig::default;
    let one = |tried: u8, extra: &[u8], magic: &[u8]| {
        let mut body = magic.to_vec();
        body.extend_from_slice(&1u32.to_le_bytes());
        body.extend(book_entry("45.0.0.1:1", tried));
        body.extend_from_slice(extra);
        seal(body)
    };
    // the control: a well-formed file loads
    assert!(AddrBook::from_bytes(cfg(), &one(1, &[], b"TAB1")).is_ok());
    assert!(AddrBook::from_bytes(cfg(), &one(0, &[], b"TAB1")).is_ok());
    // each fault alone
    assert!(
        AddrBook::from_bytes(cfg(), &one(2, &[], b"TAB1")).is_err(),
        "a flag that is neither 0 nor 1"
    );
    assert!(
        AddrBook::from_bytes(cfg(), &one(1, &[9], b"TAB1")).is_err(),
        "a byte after the last entry"
    );
    assert!(
        AddrBook::from_bytes(cfg(), &one(1, &[], b"TAB2")).is_err(),
        "another format"
    );
    // text that is not text
    let mut body = b"TAB1".to_vec();
    body.extend_from_slice(&1u32.to_le_bytes());
    body.push(2);
    body.extend_from_slice(&[0xff, 0xfe]);
    body.extend_from_slice(&[0; 14]);
    assert!(AddrBook::from_bytes(cfg(), &seal(body)).is_err());
}

#[test]
fn a_ban_list_with_a_good_checksum_but_a_bad_format_is_refused() {
    let with = |magic: &[u8], extra: &[u8]| {
        let mut body = magic.to_vec();
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(extra);
        seal(body)
    };
    assert!(BanList::from_bytes(&with(b"TBN1", &[])).is_ok());
    assert!(
        BanList::from_bytes(&with(b"TBN1", &[1])).is_err(),
        "a byte after the last entry"
    );
    assert!(
        BanList::from_bytes(&with(b"TBN2", &[])).is_err(),
        "another format"
    );
    // an implausible count
    let mut body = b"TBN1".to_vec();
    body.extend_from_slice(&2_000_000u32.to_le_bytes());
    assert!(BanList::from_bytes(&seal(body)).is_err());
}
