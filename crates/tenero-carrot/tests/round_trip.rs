//! Outputs made by `output` are found by `scan`, with the right amount, address and spend keys, in every shape of
//! transaction; and the attacks Carrot exists to stop (Janus, a forged commitment, scanning with the wrong keys) fail.
//! These check the Rust against itself; agreement with Monero is in `upstream_*.rs`.

use curve25519_dalek::scalar::Scalar;
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use std::collections::HashMap;
use tenero_carrot::account::{AccountSecrets, AddressIndex, Destination};
use tenero_carrot::derive::*;
use tenero_carrot::output::*;
use tenero_carrot::scan::*;
use tenero_carrot::{CarrotError, Enote, EnoteType, NULL_PAYMENT_ID};

fn rng(seed: u64) -> ChaCha20Rng {
    ChaCha20Rng::seed_from_u64(seed)
}

fn account(rng: &mut ChaCha20Rng) -> AccountSecrets {
    let mut s = [0u8; 32];
    rng.fill_bytes(&mut s);
    AccountSecrets::from_master(&s)
}

fn key_image_bytes(rng: &mut ChaCha20Rng) -> [u8; 32] {
    let mut wide = [0u8; 64];
    rng.fill_bytes(&mut wide);
    curve25519_dalek::edwards::EdwardsPoint::mul_base(&Scalar::from_bytes_mod_order_wide(&wide))
        .compress()
        .to_bytes()
}

/// The wallet's table of its own addresses: main, and subaddresses (0, 1..4) and (1, 0..2).
fn table(a: &AccountSecrets) -> HashMap<[u8; 32], AddressIndex> {
    let mut t = HashMap::new();
    for (major, minor) in [
        (0, 0),
        (0, 1),
        (0, 2),
        (0, 3),
        (0, 4),
        (1, 0),
        (1, 1),
        (1, 2),
    ] {
        let i = AddressIndex { major, minor };
        t.insert(a.address(i).unwrap().spend_pubkey, i);
    }
    t
}

/// Everything a view-all wallet finds in a set of outputs, and checks: the spend keys open the key, and the key image a
/// view-all wallet computes is the spender's.
fn find(a: &AccountSecrets, enotes: &[Enote], pid_enc: &[u8; 8]) -> Vec<(Received, AddressIndex)> {
    let t = table(a);
    let va = a.view_all();
    let mut out = vec![];
    for e in enotes {
        if let Some((r, i)) = scan_as_view_all(&va, e, Some(pid_enc), |k| t.get(k).copied()) {
            let keys = spend_keys(a, i, &r, &e.onetime_address)
                .expect("the spend keys open the output key");
            assert_eq!(
                key_image(&keys.x, &e.onetime_address),
                key_image_view_all(&va, &a.s_generate_address, i, &r, &e.onetime_address)
            );
            out.push((r, i));
        }
    }
    out
}

fn selfsend(to: &Destination, amount: u64, enote_type: EnoteType) -> SelfSendProposal {
    SelfSendProposal {
        destination_spend_pubkey: to.spend_pubkey,
        is_subaddress: to.is_subaddress,
        amount,
        enote_type,
        ephemeral_privkey: None,
        internal_message: None,
    }
}

#[test]
fn a_two_output_payment_to_a_main_address_with_change() {
    let mut r = rng(1);
    let (alice, bob) = (account(&mut r), account(&mut r));
    let ki = key_image_bytes(&mut r);
    let pay = PaymentProposal::new(bob.address(AddressIndex::MAIN).unwrap(), 7_000, &mut r);
    let change = selfsend(
        &alice.address(AddressIndex::MAIN).unwrap(),
        3_000,
        EnoteType::Change,
    );
    let mut dummy_pid = [0u8; 8];
    r.fill_bytes(&mut dummy_pid);
    for key in [
        SelfSendKey::ViewBalance(&alice.s_view_balance),
        SelfSendKey::ViewIncoming(&alice.k_view_incoming),
    ] {
        let (outs, pid_enc, order) =
            output_set(&[pay], &[change], Some(dummy_pid), key, &ki).unwrap();
        assert_eq!(outs.len(), 2);
        assert_eq!(
            outs[0].enote.ephemeral_pubkey, outs[1].enote.ephemeral_pubkey,
            "2 outputs share D_e"
        );
        assert!(
            outs[0].enote.onetime_address < outs[1].enote.onetime_address,
            "sorted"
        );
        assert_eq!(pid_enc, dummy_pid);
        assert_eq!(order.len(), 2);
        let enotes: Vec<Enote> = outs.iter().map(|o| o.enote).collect();

        let b = find(&bob, &enotes, &pid_enc);
        assert_eq!(b.len(), 1);
        assert_eq!(
            (b[0].0.amount, b[0].0.payment_id, b[0].1),
            (7_000, NULL_PAYMENT_ID, AddressIndex::MAIN)
        );
        assert_eq!(b[0].0.found, Found::External);
        assert_eq!(b[0].0.enote_type, EnoteType::Payment);

        let a = find(&alice, &enotes, &pid_enc);
        assert_eq!(a.len(), 1);
        assert_eq!(
            (a[0].0.amount, a[0].0.enote_type),
            (3_000, EnoteType::Change)
        );
        let expected = match key {
            SelfSendKey::ViewBalance(_) => Found::Internal,
            SelfSendKey::ViewIncoming(_) => Found::External,
        };
        assert_eq!(a[0].0.found, expected);

        // the commitments open with what the sender kept
        for o in &outs {
            assert_eq!(
                commit_amount(o.amount, &o.blinding_factor),
                o.enote.amount_commitment
            );
        }
    }
}

#[test]
fn payments_to_subaddresses_and_an_integrated_address_in_one_transaction() {
    let mut r = rng(2);
    let (alice, bob, carol) = (account(&mut r), account(&mut r), account(&mut r));
    let ki = key_image_bytes(&mut r);
    let bob_sub = bob.address(AddressIndex { major: 1, minor: 2 }).unwrap();
    let carol_sub = carol.address(AddressIndex { major: 0, minor: 3 }).unwrap();
    let pid = *b"invoice1";
    let bob_int = bob.public.integrated_address(pid);
    let pays = [
        PaymentProposal::new(bob_sub, 100, &mut r),
        PaymentProposal::new(carol_sub, 200, &mut r),
        PaymentProposal::new(bob_int, 300, &mut r),
    ];
    let mut change = selfsend(
        &alice.address(AddressIndex { major: 0, minor: 1 }).unwrap(),
        50,
        EnoteType::Change,
    );
    change.ephemeral_privkey = Some(Scalar::from(987_654_321u64));
    let (outs, pid_enc, _) = output_set(
        &pays,
        &[change],
        None,
        SelfSendKey::ViewBalance(&alice.s_view_balance),
        &ki,
    )
    .unwrap();
    let enotes: Vec<Enote> = outs.iter().map(|o| o.enote).collect();
    let mut keys: Vec<_> = enotes.iter().map(|e| e.ephemeral_pubkey).collect();
    keys.sort();
    keys.dedup();
    assert_eq!(
        keys.len(),
        4,
        "more than two outputs: unique ephemeral keys"
    );

    let mut b: Vec<_> = find(&bob, &enotes, &pid_enc)
        .into_iter()
        .map(|(r, i)| (r.amount, r.payment_id, i))
        .collect();
    b.sort();
    assert_eq!(
        b,
        [
            (100, NULL_PAYMENT_ID, AddressIndex { major: 1, minor: 2 }),
            (300, pid, AddressIndex::MAIN)
        ]
    );
    let c = find(&carol, &enotes, &pid_enc);
    assert_eq!(
        (c.len(), c[0].0.amount, c[0].1),
        (1, 200, AddressIndex { major: 0, minor: 3 })
    );
    let a = find(&alice, &enotes, &pid_enc);
    assert_eq!(
        (a.len(), a[0].0.amount, a[0].0.found),
        (1, 50, Found::Internal)
    );
}

#[test]
fn the_sender_can_prove_what_it_paid_from_its_anchor() {
    let mut r = rng(3);
    let (alice, bob) = (account(&mut r), account(&mut r));
    let ki = key_image_bytes(&mut r);
    let to = bob
        .public
        .integrated_address(*b"\x01\x02\x03\x04\x05\x06\x07\x08");
    let pay = PaymentProposal::new(to, 42, &mut r);
    let change = selfsend(
        &alice.address(AddressIndex::MAIN).unwrap(),
        1,
        EnoteType::Change,
    );
    let (outs, pid_enc, order) = output_set(
        &[pay],
        &[change],
        None,
        SelfSendKey::ViewBalance(&alice.s_view_balance),
        &ki,
    )
    .unwrap();
    let paid = outs[order.iter().position(|w| *w == (false, 0)).unwrap()].enote;
    let r1 = scan_external_as_sender(&paid, Some(&pid_enc), &to, &pay.randomness, true).unwrap();
    assert_eq!((r1.amount, r1.payment_id), (42, to.payment_id));
    // another anchor or another address: no proof
    assert!(scan_external_as_sender(&paid, Some(&pid_enc), &to, &[9; 16], true).is_none());
    let other = bob.address(AddressIndex { major: 0, minor: 1 }).unwrap();
    assert!(
        scan_external_as_sender(&paid, Some(&pid_enc), &other, &pay.randomness, false).is_none()
    );
}

#[test]
fn coinbase_outputs_are_found_by_their_miners() {
    let mut r = rng(4);
    let (m1, m2) = (account(&mut r), account(&mut r));
    let pays = [
        PaymentProposal::new(
            m1.address(AddressIndex::MAIN).unwrap(),
            2_000_000_000,
            &mut r,
        ),
        PaymentProposal::new(m2.address(AddressIndex::MAIN).unwrap(), 5, &mut r),
    ];
    let enotes = coinbase_enotes(&pays, 1234).unwrap();
    assert!(enotes[0].onetime_address < enotes[1].onetime_address);
    let mut found = 0;
    for (m, amount) in [(&m1, 2_000_000_000), (&m2, 5)] {
        for e in &enotes {
            let s_sr = shared_secret(&m.k_view_incoming, &e.ephemeral_pubkey);
            if let Some(got) = scan_coinbase(
                e,
                &s_sr,
                &[m.public.spend_pubkey],
                &m.public.main_view_pubkey,
            ) {
                assert_eq!(got.amount, amount);
                assert_eq!(got.blinding_factor, Scalar::ONE);
                assert!(spend_keys(m, AddressIndex::MAIN, &got, &e.onetime_address).is_some());
                found += 1;
            }
        }
    }
    assert_eq!(found, 2);
    // coinbase outputs pay main addresses only
    let sub = PaymentProposal::new(
        m1.address(AddressIndex { major: 0, minor: 1 }).unwrap(),
        1,
        &mut r,
    );
    assert!(matches!(
        coinbase_enotes(&[sub], 1),
        Err(CarrotError::BadAddressType(_))
    ));
}

#[test]
fn a_janus_attack_is_refused() {
    // A sender who knows two of Bob's addresses makes an output to subaddress A but derives its ephemeral key towards
    // subaddress B. If Bob's wallet accepted it, the sender would learn that A and B are one wallet.
    let mut r = rng(5);
    let bob = account(&mut r);
    let ki = key_image_bytes(&mut r);
    let a = bob.address(AddressIndex { major: 0, minor: 1 }).unwrap();
    let b = bob.address(AddressIndex { major: 0, minor: 2 }).unwrap();
    let input_context = make_input_context(&ki);
    let anchor = random_anchor(&mut r);
    // the ephemeral key of a payment to B ...
    let d_e = make_enote_ephemeral_privkey(
        &anchor,
        &input_context,
        &b.spend_pubkey,
        &b.view_pubkey,
        &NULL_PAYMENT_ID,
    );
    let d_pub = make_enote_ephemeral_pubkey(&d_e, &b.spend_pubkey, true).unwrap();
    // ... used with A's view key, so the exchange with Bob works, but the output is for A
    let s_sr = make_shared_key_sender(&d_e, &a.view_pubkey).unwrap();
    let ctx = make_sender_receiver_secret(&s_sr, &d_pub, &input_context);
    let k_a = make_amount_blinding_factor(&ctx, 10, &a.spend_pubkey, EnoteType::Payment);
    let c = commit_amount(10, &k_a);
    let ko = make_onetime_address(&a.spend_pubkey, &ctx, &c).unwrap();
    let enote = Enote {
        onetime_address: ko,
        amount_commitment: c,
        amount_enc: encrypt_amount(10, &ctx, &ko),
        view_tag: make_view_tag(&s_sr, &input_context, &ko),
        ephemeral_pubkey: d_pub,
        anchor_enc: encrypt_anchor(&anchor, &ctx, &ko),
        tx_first_key_image: ki,
    };
    let bob_s_sr = shared_secret(&bob.k_view_incoming, &d_pub);
    assert!(
        scan_external(
            &enote,
            None,
            &bob_s_sr,
            &[bob.public.spend_pubkey],
            &bob.k_view_incoming
        )
        .is_none(),
        "the Janus check must refuse it"
    );
    // the same output made honestly (D_e towards A) is accepted
    let honest = output_normal(
        &PaymentProposal {
            destination: a,
            amount: 10,
            randomness: anchor,
        },
        &ki,
    )
    .unwrap()
    .0
    .enote;
    let s = shared_secret(&bob.k_view_incoming, &honest.ephemeral_pubkey);
    assert!(scan_external(
        &honest,
        None,
        &s,
        &[bob.public.spend_pubkey],
        &bob.k_view_incoming
    )
    .is_some());
}

#[test]
fn a_tampered_output_is_not_found() {
    let mut r = rng(6);
    let (alice, bob) = (account(&mut r), account(&mut r));
    let ki = key_image_bytes(&mut r);
    let pay = PaymentProposal::new(bob.address(AddressIndex::MAIN).unwrap(), 5, &mut r);
    let change = selfsend(
        &alice.address(AddressIndex::MAIN).unwrap(),
        5,
        EnoteType::Change,
    );
    let (outs, pid_enc, order) = output_set(
        &[pay],
        &[change],
        Some([0; 8]),
        SelfSendKey::ViewBalance(&alice.s_view_balance),
        &ki,
    )
    .unwrap();
    let e = outs[order.iter().position(|w| *w == (false, 0)).unwrap()].enote;
    assert_eq!(find(&bob, &[e], &pid_enc).len(), 1);
    let mut bad = e;
    bad.amount_enc[0] ^= 1;
    assert!(
        find(&bob, &[bad], &pid_enc).is_empty(),
        "a changed amount no longer opens the commitment"
    );
    let mut bad = e;
    bad.tx_first_key_image = key_image_bytes(&mut r);
    assert!(
        find(&bob, &[bad], &pid_enc).is_empty(),
        "another transaction"
    );
    // nobody else finds it
    assert!(find(&account(&mut r), &[e], &pid_enc).is_empty());
}

#[test]
fn a_view_all_wallet_from_its_two_keys_matches_the_full_account() {
    let mut r = rng(7);
    let a = account(&mut r);
    let va = tenero_carrot::account::ViewAll::new(
        a.s_view_balance,
        make_partial_spend_pubkey(&a.k_prove_spend),
    )
    .unwrap();
    assert_eq!(va.public, a.public);
    assert_eq!(va.k_generate_image(), a.k_generate_image);
}

#[test]
fn the_output_set_rules_are_enforced() {
    let mut r = rng(8);
    let a = account(&mut r);
    let ki = key_image_bytes(&mut r);
    let me = a.address(AddressIndex::MAIN).unwrap();
    let key = SelfSendKey::ViewBalance(&a.s_view_balance);
    let p = PaymentProposal::new(
        a.address(AddressIndex { major: 0, minor: 1 }).unwrap(),
        1,
        &mut r,
    );
    // one output
    assert!(output_set(
        &[],
        &[selfsend(&me, 1, EnoteType::Change)],
        Some([0; 8]),
        key,
        &ki
    )
    .is_err());
    // no self-send
    let q = PaymentProposal::new(me, 1, &mut r);
    assert!(output_set(&[p, q], &[], Some([0; 8]), key, &ki).is_err());
    // two integrated addresses
    let i1 = PaymentProposal::new(a.public.integrated_address([1; 8]), 1, &mut r);
    let i2 = PaymentProposal::new(a.public.integrated_address([2; 8]), 1, &mut r);
    let mut ch = selfsend(&me, 1, EnoteType::Change);
    ch.ephemeral_privkey = Some(Scalar::from(5u64));
    assert!(output_set(&[i1, i2], &[ch], None, key, &ki).is_err());
    // no dummy payment ID and no integrated address
    assert!(output_set(&[p], &[selfsend(&me, 1, EnoteType::Change)], None, key, &ki).is_err());
    // repeated anchors
    let mut p2 = p;
    p2.destination = me;
    assert!(output_set(&[p, p2], &[ch], Some([0; 8]), key, &ki).is_err());
    // two self-sends of one type in a 2-output transaction
    let mut c1 = selfsend(&me, 1, EnoteType::Change);
    c1.ephemeral_privkey = Some(Scalar::from(3u64));
    assert!(output_set(
        &[],
        &[c1, selfsend(&me, 2, EnoteType::Change)],
        Some([0; 8]),
        key,
        &ki
    )
    .is_err());
    // a payment-type and a change self-send: fine, and they share D_e
    let (o, _, _) = output_set(
        &[],
        &[c1, selfsend(&me, 2, EnoteType::Payment)],
        Some([0; 8]),
        key,
        &ki,
    )
    .unwrap();
    assert_eq!(o[0].enote.ephemeral_pubkey, o[1].enote.ephemeral_pubkey);
}

#[test]
fn what_output_a_transaction_still_needs() {
    use AdditionalOutputType::*;
    assert_eq!(
        additional_output_type(1, 0, true, false).unwrap(),
        Some(ChangeShared)
    );
    assert_eq!(
        additional_output_type(1, 0, false, false).unwrap(),
        Some(ChangeShared)
    );
    assert_eq!(
        additional_output_type(0, 1, false, false).unwrap(),
        Some(Dummy)
    );
    assert_eq!(
        additional_output_type(0, 1, true, true).unwrap(),
        Some(ChangeShared)
    );
    assert_eq!(
        additional_output_type(0, 1, true, false).unwrap(),
        Some(PaymentShared)
    );
    assert_eq!(
        additional_output_type(2, 0, true, false).unwrap(),
        Some(ChangeUnique)
    );
    assert_eq!(additional_output_type(1, 1, false, false).unwrap(), None);
    assert!(additional_output_type(0, 0, true, false).is_err());
}
