//! `tests/vectors/carrot_monero.json`: what MONERO'S OWN `carrot_core` (stressnet v0.19.0.0-beta.3.0) computes on fixed
//! pseudo-random inputs, made by `reference/tools/carrot_harness` in WSL. Every account, every output of every output
//! set, every coinbase output, and what each account finds in each output (spend keys and key image included) must be
//! reproduced here bit for bit; an account that Monero's code does not list for an output must find nothing.

use std::collections::HashMap;

use curve25519_dalek::scalar::Scalar;
use serde_json::Value;
use tenero_carrot::account::{AccountSecrets, AddressIndex, Destination};
use tenero_carrot::derive::key_image_generator;
use tenero_carrot::output::{
    coinbase_enotes, output_set, PaymentProposal, SelfSendKey, SelfSendProposal,
};
use tenero_carrot::points::{compress, x25519_mul};
use tenero_carrot::scan::{
    key_image, scan_as_view_all, scan_coinbase, shared_secret, spend_keys, Found, Received,
};
use tenero_carrot::{CoinbaseEnote, Enote, EnoteType};
use tenero_core::vectors::{hex, load};

fn b<const N: usize>(v: &Value) -> [u8; N] {
    hex(v.as_str().unwrap()).unwrap().try_into().unwrap()
}

fn s(v: &Value) -> Scalar {
    Option::from(Scalar::from_canonical_bytes(b(v))).expect("a canonical scalar")
}

fn opt<T>(v: &Value, f: impl Fn(&Value) -> T) -> Option<T> {
    (!v.is_null()).then(|| f(v))
}

fn enote_type(v: &Value) -> EnoteType {
    match v.as_u64().unwrap() {
        0 => EnoteType::Payment,
        1 => EnoteType::Change,
        t => panic!("enote type {t}"),
    }
}

fn index(v: &Value) -> AddressIndex {
    AddressIndex {
        major: v["major"].as_u64().unwrap() as u32,
        minor: v["minor"].as_u64().unwrap() as u32,
    }
}

fn destination(v: &Value) -> Destination {
    Destination {
        spend_pubkey: b(&v["spend_pubkey"]),
        view_pubkey: b(&v["view_pubkey"]),
        is_subaddress: v["is_subaddress"].as_bool().unwrap(),
        payment_id: b(&v["payment_id"]),
    }
}

fn payment(v: &Value) -> PaymentProposal {
    PaymentProposal {
        destination: destination(&v["destination"]),
        amount: v["amount"].as_u64().unwrap(),
        randomness: b(&v["randomness"]),
    }
}

fn file() -> Value {
    load("carrot_monero").unwrap()
}

fn accounts(f: &Value) -> Vec<AccountSecrets> {
    f["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| AccountSecrets::from_master(&b(&a["s_master"])))
        .collect()
}

fn table(a: &AccountSecrets, f: &Value, ai: usize) -> HashMap<[u8; 32], AddressIndex> {
    f["accounts"][ai]["addresses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| (a.address(index(x)).unwrap().spend_pubkey, index(x)))
        .collect()
}

/// Checks what we found against Monero's record, including the spend keys and the key image.
fn check_found(
    a: &AccountSecrets,
    got: &Received,
    at: AddressIndex,
    ko: &[u8; 32],
    want: &Value,
    what: &str,
) {
    let kind = match got.found {
        Found::External => "external",
        Found::Internal => "internal",
        Found::Coinbase => "coinbase",
    };
    assert_eq!(kind, want["kind"].as_str().unwrap(), "{what}: kind");
    assert_eq!(at, index(want), "{what}: address index");
    assert_eq!(
        got.amount,
        want["amount"].as_u64().unwrap(),
        "{what}: amount"
    );
    assert_eq!(
        got.blinding_factor,
        s(&want["blinding_factor"]),
        "{what}: blinding factor"
    );
    assert_eq!(
        got.enote_type,
        enote_type(&want["enote_type"]),
        "{what}: enote type"
    );
    assert_eq!(
        got.payment_id,
        b::<8>(&want["payment_id"]),
        "{what}: payment ID"
    );
    assert_eq!(
        got.sender_extension_g,
        s(&want["sender_extension_g"]),
        "{what}: k_g"
    );
    assert_eq!(
        got.sender_extension_t,
        s(&want["sender_extension_t"]),
        "{what}: k_t"
    );
    assert_eq!(
        got.internal_message,
        opt(&want["internal_message"], b::<16>),
        "{what}: internal message"
    );
    let keys = spend_keys(a, at, got, ko)
        .unwrap_or_else(|| panic!("{what}: the spend keys do not open the output"));
    assert_eq!(keys.x, s(&want["x"]), "{what}: x");
    assert_eq!(keys.y, s(&want["y"]), "{what}: y");
    assert_eq!(
        key_image(&keys.x, ko),
        b::<32>(&want["key_image"]),
        "{what}: key image"
    );
}

#[test]
fn every_account_matches_monero() {
    let f = file();
    for (ai, (a, want)) in accounts(&f)
        .iter()
        .zip(f["accounts"].as_array().unwrap())
        .enumerate()
    {
        assert_eq!(a.k_prove_spend, s(&want["k_prove_spend"]), "account {ai}");
        assert_eq!(a.s_view_balance, b::<32>(&want["s_view_balance"]));
        assert_eq!(
            a.s_generate_image_preimage,
            b::<32>(&want["s_generate_image_preimage"])
        );
        assert_eq!(a.k_generate_image, s(&want["k_generate_image"]));
        assert_eq!(a.k_view_incoming, s(&want["k_view_incoming"]));
        assert_eq!(a.s_generate_address, b::<32>(&want["s_generate_address"]));
        assert_eq!(
            a.view_all().partial_spend_pubkey,
            b::<32>(&want["partial_spend_pubkey"])
        );
        assert_eq!(a.public.spend_pubkey, b::<32>(&want["spend_pubkey"]));
        assert_eq!(a.public.view_pubkey, b::<32>(&want["view_pubkey"]));
        assert_eq!(
            a.public.main_view_pubkey,
            b::<32>(&want["main_view_pubkey"])
        );
        for x in want["addresses"].as_array().unwrap() {
            let i = index(x);
            let d = a.address(i).unwrap();
            assert_eq!(
                d.spend_pubkey,
                b::<32>(&x["spend_pubkey"]),
                "account {ai} {i:?}"
            );
            assert_eq!(
                d.view_pubkey,
                b::<32>(&x["view_pubkey"]),
                "account {ai} {i:?}"
            );
            assert_eq!(d.is_subaddress, i.is_subaddress());
            let scalar =
                tenero_carrot::account::subaddress_scalar(&a.public, &a.s_generate_address, i);
            assert_eq!(scalar, s(&x["subaddress_scalar"]), "account {ai} {i:?}");
        }
    }
}

#[test]
fn every_output_set_matches_monero() {
    let f = file();
    let accts = accounts(&f);
    for set in f["output_sets"].as_array().unwrap() {
        let name = set["name"].as_str().unwrap();
        let sender = &accts[set["sender"].as_u64().unwrap() as usize];
        let normal: Vec<_> = set["normal"]
            .as_array()
            .unwrap()
            .iter()
            .map(payment)
            .collect();
        let selfsend: Vec<_> = set["selfsend"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| SelfSendProposal {
                destination_spend_pubkey: b(&p["destination_spend_pubkey"]),
                is_subaddress: p["is_subaddress"].as_bool().unwrap(),
                amount: p["amount"].as_u64().unwrap(),
                enote_type: enote_type(&p["enote_type"]),
                ephemeral_privkey: opt(&p["ephemeral_privkey"], s),
                internal_message: opt(&p["internal_message"], b::<16>),
            })
            .collect();
        let key = if set["view_balance"].as_bool().unwrap() {
            SelfSendKey::ViewBalance(&sender.s_view_balance)
        } else {
            SelfSendKey::ViewIncoming(&sender.k_view_incoming)
        };
        let (outs, pid_enc, order) = output_set(
            &normal,
            &selfsend,
            opt(&set["dummy_encrypted_payment_id"], b::<8>),
            key,
            &b(&set["first_key_image"]),
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            pid_enc,
            b::<8>(&set["encrypted_payment_id"]),
            "{name}: encrypted payment ID"
        );
        let want_order: Vec<(bool, usize)> = set["order"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| (w[0].as_bool().unwrap(), w[1].as_u64().unwrap() as usize))
            .collect();
        assert_eq!(order, want_order, "{name}: order");
        let want_outs = set["outputs"].as_array().unwrap();
        assert_eq!(outs.len(), want_outs.len(), "{name}");
        for (oi, (got, want)) in outs.iter().zip(want_outs).enumerate() {
            let what = format!("{name}, output {oi}");
            let e = &want["enote"];
            let want_enote = Enote {
                onetime_address: b(&e["onetime_address"]),
                amount_commitment: b(&e["amount_commitment"]),
                amount_enc: b(&e["amount_enc"]),
                view_tag: b(&e["view_tag"]),
                ephemeral_pubkey: b(&e["ephemeral_pubkey"]),
                anchor_enc: b(&e["anchor_enc"]),
                tx_first_key_image: b(&e["tx_first_key_image"]),
            };
            assert_eq!(got.enote, want_enote, "{what}");
            assert_eq!(got.amount, want["amount"].as_u64().unwrap(), "{what}");
            assert_eq!(got.blinding_factor, s(&want["blinding_factor"]), "{what}");

            // each account scans it as a view-all wallet; only those Monero lists find it
            let found: HashMap<usize, &Value> = want["found"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| (x["account"].as_u64().unwrap() as usize, &x["found"]))
                .collect();
            for (ai, a) in accts.iter().enumerate() {
                let t = table(a, &f, ai);
                let ours = scan_as_view_all(&a.view_all(), &got.enote, Some(&pid_enc), |k| {
                    t.get(k).copied()
                });
                match (ours, found.get(&ai)) {
                    (Some((r, at)), Some(w)) => check_found(
                        a,
                        &r,
                        at,
                        &got.enote.onetime_address,
                        w,
                        &format!("{what}, account {ai}"),
                    ),
                    (None, None) => {}
                    (o, w) => panic!(
                        "{what}, account {ai}: found by us {}, by Monero {}",
                        o.is_some(),
                        w.is_some()
                    ),
                }
            }
        }
    }
}

#[test]
fn the_coinbase_outputs_match_monero() {
    let f = file();
    let accts = accounts(&f);
    let c = &f["coinbase"];
    let block = c["block_index"].as_u64().unwrap();
    let normal: Vec<_> = c["normal"]
        .as_array()
        .unwrap()
        .iter()
        .map(payment)
        .collect();
    let got = coinbase_enotes(&normal, block).unwrap();
    let want = c["outputs"].as_array().unwrap();
    assert_eq!(got.len(), want.len());
    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        let want_enote = CoinbaseEnote {
            onetime_address: b(&w["onetime_address"]),
            amount: w["amount"].as_u64().unwrap(),
            view_tag: b(&w["view_tag"]),
            ephemeral_pubkey: b(&w["ephemeral_pubkey"]),
            anchor_enc: b(&w["anchor_enc"]),
            block_index: w["block_index"].as_u64().unwrap(),
        };
        assert_eq!(*g, want_enote, "coinbase output {i}");
        let found: HashMap<usize, &Value> = w["found"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| (x["account"].as_u64().unwrap() as usize, &x["found"]))
            .collect();
        for (ai, a) in accts.iter().enumerate() {
            let s_sr = shared_secret(&a.k_view_incoming, &g.ephemeral_pubkey);
            let ours = scan_coinbase(
                g,
                &s_sr,
                &[a.public.spend_pubkey],
                &a.public.main_view_pubkey,
            );
            match (ours, found.get(&ai)) {
                (Some(r), Some(wf)) => check_found(
                    a,
                    &r,
                    AddressIndex::MAIN,
                    &g.onetime_address,
                    wf,
                    &format!("coinbase output {i}, account {ai}"),
                ),
                (None, None) => {}
                (o, wf) => panic!(
                    "coinbase {i}, account {ai}: found by us {}, by Monero {}",
                    o.is_some(),
                    wf.is_some()
                ),
            }
        }
    }
}

#[test]
fn unclamped_x25519_matches_monero_including_edge_inputs() {
    for (i, c) in file()["x25519"].as_array().unwrap().iter().enumerate() {
        assert_eq!(
            x25519_mul(&s(&c["scalar"]), &b(&c["u"])),
            b::<32>(&c["product"]),
            "case {i}"
        );
    }
}

#[test]
fn the_unbiased_hash_to_point_matches_monero() {
    for (i, c) in file()["hash_to_point"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        assert_eq!(
            compress(&key_image_generator(&b(&c["input"]))),
            b::<32>(&c["point"]),
            "case {i}"
        );
    }
}
