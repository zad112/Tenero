//! The interim output scheme against the independent Python reference's vectors, and its rules one by one.

use rand_core::{CryptoRng, OsRng, RngCore};
use tenero_core::v2::{CoinbaseOutput, Output};
use tenero_crypto::ringct;
use tenero_wallet::interim::{
    coinbase_context, create_enote, scan_coinbase_output, scan_output, tx_context, Address,
    AddressError, Keys,
};

/// Hands out exactly the bytes it was given (the vectors fix the sender's randomness).
struct ScriptRng {
    bytes: Vec<u8>,
    at: usize,
}

impl RngCore for ScriptRng {
    fn next_u32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        self.fill_bytes(&mut b);
        u32::from_le_bytes(b)
    }
    fn next_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        self.fill_bytes(&mut b);
        u64::from_le_bytes(b)
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        let end = self.at + dest.len();
        dest.copy_from_slice(&self.bytes[self.at..end]);
        self.at = end;
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

impl CryptoRng for ScriptRng {}

fn unhex<const N: usize>(s: &str) -> [u8; N] {
    assert_eq!(s.len(), 2 * N);
    let mut out = [0u8; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

fn unhex_vec(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

fn vectors() -> serde_json::Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/vectors/interim_scheme.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn keys(tag: u8) -> Keys {
    Keys::from_seed(&[tag; 32])
}

fn tx_output(e: &tenero_wallet::interim::Enote) -> Output {
    e.to_output()
}

// ------------------------------------------------------------------------------------------------
// the vectors
// ------------------------------------------------------------------------------------------------

#[test]
fn every_enote_vector_matches_the_python_reference() {
    let v = vectors();
    let cases = v["enotes"].as_array().unwrap();
    assert!(cases.len() >= 6);
    for c in cases {
        let note = c["note"].as_str().unwrap();
        let k = Keys::from_seed(&unhex::<32>(c["seed"].as_str().unwrap()));
        let address = k.address();
        assert_eq!(address.to_text(), c["address"].as_str().unwrap(), "{note}");
        let ctx: [u8; 32] = unhex(c["context"].as_str().unwrap());
        // the context itself is part of the scheme
        match c["context_kind"].as_str().unwrap() {
            "tx" => assert_eq!(
                tx_context(&unhex(c["context_input"].as_str().unwrap())),
                ctx,
                "{note}"
            ),
            _ => assert_eq!(
                coinbase_context(c["context_input"].as_u64().unwrap()),
                ctx,
                "{note}"
            ),
        }
        let coinbase = c["coinbase"].as_bool().unwrap();
        let amount = c["amount"].as_u64().unwrap();
        let index = c["index"].as_u64().unwrap() as u32;
        let mut rng = ScriptRng {
            bytes: unhex_vec(c["rng"].as_str().unwrap()),
            at: 0,
        };
        let e = create_enote(&mut rng, &address, amount, &ctx, index, coinbase).unwrap();
        assert_eq!(
            rng.at, 80,
            "{note}: draws exactly the 80 bytes the reference uses"
        );
        let want = &c["enote"];
        assert_eq!(
            e.onetime_address,
            unhex::<32>(want["onetime_address"].as_str().unwrap()),
            "{note}"
        );
        assert_eq!(
            e.amount_commitment,
            unhex::<32>(want["amount_commitment"].as_str().unwrap()),
            "{note}"
        );
        assert_eq!(
            e.amount_enc,
            unhex::<8>(want["amount_enc"].as_str().unwrap()),
            "{note}"
        );
        assert_eq!(
            e.view_tag,
            unhex::<3>(want["view_tag"].as_str().unwrap()),
            "{note}"
        );
        assert_eq!(
            e.ephemeral_pubkey,
            unhex::<32>(want["ephemeral_pubkey"].as_str().unwrap()),
            "{note}"
        );
        assert_eq!(
            e.anchor_enc,
            unhex::<16>(want["anchor_enc"].as_str().unwrap()),
            "{note}"
        );
        assert_eq!(
            e.mask,
            unhex::<32>(want["mask"].as_str().unwrap()),
            "{note}"
        );

        // the receiver reads it back
        let got = if coinbase {
            scan_coinbase_output(
                &k.view_keys(),
                &e.to_coinbase_output(amount),
                c["context_input"].as_u64().unwrap(),
                index,
            )
        } else {
            scan_output(&k.view_keys(), &e.to_output(), &ctx, index)
        }
        .unwrap_or_else(|| panic!("{note}: not recognised"));
        assert_eq!(got.amount, amount, "{note}");
        assert_eq!(got.mask, e.mask, "{note}");
        assert_eq!(
            got.offset,
            unhex::<32>(c["offset"].as_str().unwrap()),
            "{note}"
        );
        let secret = k.onetime_secret(&got.offset).unwrap();
        assert_eq!(
            secret,
            unhex::<32>(c["onetime_secret"].as_str().unwrap()),
            "{note}"
        );
        // and the secret really is the discrete log of the one-time address (what the prover needs)
        assert_eq!(
            ringct::public_key(&secret).unwrap(),
            e.onetime_address,
            "{note}"
        );
    }
}

#[test]
fn the_address_vectors() {
    let v = vectors();
    for c in v["addresses"]["valid"].as_array().unwrap() {
        let a = Address::from_text(c["address"].as_str().unwrap()).unwrap();
        assert_eq!(a.spend, unhex::<32>(c["spend"].as_str().unwrap()));
        assert_eq!(a.view, unhex::<32>(c["view"].as_str().unwrap()));
        assert_eq!(a.to_text(), c["address"].as_str().unwrap());
        assert_eq!(
            Keys::from_seed(&unhex(c["seed"].as_str().unwrap())).address(),
            a
        );
    }
    let invalid = v["addresses"]["invalid"].as_array().unwrap();
    assert!(invalid.len() >= 8);
    for c in invalid {
        let want = match c["error"].as_str().unwrap() {
            "format" => AddressError::Format,
            "checksum" => AddressError::Checksum,
            _ => AddressError::BadKey,
        };
        assert_eq!(
            Address::from_text(c["address"].as_str().unwrap()),
            Err(want),
            "{}",
            c["note"]
        );
    }
}

// ------------------------------------------------------------------------------------------------
// the rules
// ------------------------------------------------------------------------------------------------

fn make(to: &Address, amount: u64, ctx: &[u8; 32], index: u32) -> tenero_wallet::interim::Enote {
    create_enote(&mut OsRng, to, amount, ctx, index, false).unwrap()
}

#[test]
fn a_wallet_finds_its_own_outputs_and_nobody_elses() {
    let (alice, bob) = (keys(1), keys(2));
    let ctx = tx_context(&[9; 32]);
    let e = make(&alice.address(), 1_000, &ctx, 0);
    let out = tx_output(&e);
    let got = scan_output(&alice.view_keys(), &out, &ctx, 0).unwrap();
    assert_eq!((got.amount, got.mask), (1_000, e.mask));
    assert!(scan_output(&bob.view_keys(), &out, &ctx, 0).is_none());
}

#[test]
fn the_output_number_and_the_context_are_bound() {
    let alice = keys(1);
    let ctx = tx_context(&[9; 32]);
    let e = make(&alice.address(), 5, &ctx, 3);
    let out = e.to_output();
    assert!(scan_output(&alice.view_keys(), &out, &ctx, 3).is_some());
    assert!(
        scan_output(&alice.view_keys(), &out, &ctx, 2).is_none(),
        "another index"
    );
    assert!(
        scan_output(&alice.view_keys(), &out, &tx_context(&[8; 32]), 3).is_none(),
        "another transaction"
    );
    assert!(
        scan_output(&alice.view_keys(), &out, &coinbase_context(3), 3).is_none(),
        "a coinbase context is not a transaction's"
    );
}

#[test]
fn the_view_tag_and_the_one_time_address_are_each_checked() {
    let alice = keys(1);
    let ctx = tx_context(&[9; 32]);
    let e = make(&alice.address(), 77, &ctx, 0);
    let mut bad_tag = e.to_output();
    bad_tag.view_tag[0] ^= 1;
    assert!(scan_output(&alice.view_keys(), &bad_tag, &ctx, 0).is_none());
    // the tag is right but the address is another point of the same wallet's
    let other = make(&alice.address(), 77, &ctx, 1);
    let mut bad_key = e.to_output();
    bad_key.onetime_address = other.onetime_address;
    assert!(scan_output(&alice.view_keys(), &bad_key, &ctx, 0).is_none());
}

#[test]
fn a_changed_amount_or_commitment_is_not_accepted() {
    let alice = keys(1);
    let ctx = tx_context(&[9; 32]);
    let e = make(&alice.address(), 4_242, &ctx, 0);
    // the amount is read from `amount_enc`: change it and the commitment no longer matches
    let mut wrong_amount = e.to_output();
    wrong_amount.amount_enc[0] ^= 1;
    assert!(scan_output(&alice.view_keys(), &wrong_amount, &ctx, 0).is_none());
    // a commitment to something else
    let mut wrong_commitment = e.to_output();
    wrong_commitment.amount_commitment = ringct::commit(&e.mask, 4_243).unwrap();
    assert!(scan_output(&alice.view_keys(), &wrong_commitment, &ctx, 0).is_none());
}

#[test]
fn an_invalid_ephemeral_key_is_ignored_not_a_panic() {
    let alice = keys(1);
    let ctx = tx_context(&[9; 32]);
    let e = make(&alice.address(), 1, &ctx, 0);
    for bad in [[0u8; 32], [0xffu8; 32], {
        let mut x = [0u8; 32];
        x[0] = 1; // the identity point, encoded as y = 1
        x
    }] {
        let mut out = e.to_output();
        out.ephemeral_pubkey = bad;
        assert!(scan_output(&alice.view_keys(), &out, &ctx, 0).is_none());
    }
}

#[test]
fn a_small_order_ephemeral_key_is_ignored() {
    let alice = keys(1);
    let ctx = tx_context(&[9; 32]);
    let e = make(&alice.address(), 1, &ctx, 0);
    let mut out = e.to_output();
    // an order-8 point: without the prime-order check (or the cofactor) it would give a shared secret that a
    // third party can compute
    out.ephemeral_pubkey =
        unhex("c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a");
    assert!(scan_output(&alice.view_keys(), &out, &ctx, 0).is_none());
}

#[test]
fn zero_and_maximum_amounts_work() {
    let alice = keys(1);
    let ctx = tx_context(&[9; 32]);
    for amount in [0u64, 1, u64::MAX] {
        let e = make(&alice.address(), amount, &ctx, 0);
        let got = scan_output(&alice.view_keys(), &e.to_output(), &ctx, 0).unwrap();
        assert_eq!(got.amount, amount);
    }
}

#[test]
fn every_output_is_valid_for_the_proof_system() {
    // what the verifier demands of an output: a prime-order, non-identity one-time key and commitment
    let alice = keys(1);
    let ctx = tx_context(&[9; 32]);
    for i in 0..20 {
        let e = make(&alice.address(), 100 + i, &ctx, i as u32);
        assert_ne!(e.onetime_address, [0; 32]);
        // the commitment is exactly mask*G + amount*H
        assert_eq!(
            e.amount_commitment,
            ringct::commit(&e.mask, 100 + i).unwrap()
        );
    }
}

#[test]
fn a_coinbase_output_has_a_public_amount_and_the_fixed_commitment() {
    let alice = keys(1);
    let height = 77;
    let e = create_enote(
        &mut OsRng,
        &alice.address(),
        5_000,
        &coinbase_context(height),
        0,
        true,
    )
    .unwrap();
    assert_eq!(e.amount_commitment, ringct::public_amount_commitment(5_000));
    assert_eq!(e.amount_enc, [0; 8]);
    let mut one = [0u8; 32];
    one[0] = 1;
    assert_eq!(e.mask, one, "the mask of a public amount is 1");
    let out = e.to_coinbase_output(5_000);
    let got = scan_coinbase_output(&alice.view_keys(), &out, height, 0).unwrap();
    assert_eq!((got.amount, got.mask), (5_000, one));
    assert!(
        scan_coinbase_output(&alice.view_keys(), &out, height + 1, 0).is_none(),
        "another height"
    );
    assert!(
        scan_coinbase_output(&alice.view_keys(), &out, height, 1).is_none(),
        "another index"
    );
    assert!(
        scan_coinbase_output(&keys(2).view_keys(), &out, height, 0).is_none(),
        "another wallet"
    );
    // the amount is public: it is whatever the block says, so changing it is the validator's business
    let mut more: CoinbaseOutput = out.clone();
    more.amount = 9_999;
    assert_eq!(
        scan_coinbase_output(&alice.view_keys(), &more, height, 0)
            .unwrap()
            .amount,
        9_999
    );
}

#[test]
fn an_address_with_an_invalid_key_makes_no_output() {
    let ctx = tx_context(&[9; 32]);
    let bad = Address {
        spend: [0; 32],
        view: keys(1).address().view,
    };
    assert!(create_enote(&mut OsRng, &bad, 1, &ctx, 0, false).is_none());
    let bad = Address {
        spend: keys(1).address().spend,
        view: [0xff; 32],
    };
    assert!(create_enote(&mut OsRng, &bad, 1, &ctx, 0, false).is_none());
}

#[test]
fn two_outputs_to_one_address_do_not_look_alike() {
    let alice = keys(1);
    let ctx = tx_context(&[9; 32]);
    let a = make(&alice.address(), 1, &ctx, 0);
    let b = make(&alice.address(), 1, &ctx, 1);
    assert_ne!(a.onetime_address, b.onetime_address);
    assert_ne!(a.ephemeral_pubkey, b.ephemeral_pubkey);
    assert_ne!(a.amount_commitment, b.amount_commitment);
}

#[test]
fn keys_are_a_function_of_the_seed_and_are_not_printed() {
    let a = Keys::from_seed(&[1; 32]);
    let b = Keys::from_seed(&[1; 32]);
    let c = Keys::from_seed(&[2; 32]);
    assert_eq!(a.address(), b.address());
    assert_ne!(a.address(), c.address());
    assert_ne!(a.address().spend, a.address().view);
    assert_eq!(format!("{a:?}"), "Keys(..)");
    assert_eq!(format!("{:?}", a.view_keys()), "ViewKeys(..)");
}

#[test]
fn a_view_only_wallet_reads_outputs_but_the_secret_needs_the_spend_key() {
    let alice = keys(1);
    let ctx = tx_context(&[9; 32]);
    let e = make(&alice.address(), 31, &ctx, 0);
    let view = alice.view_keys();
    assert_eq!(view.address(), alice.address());
    let got = scan_output(&view, &e.to_output(), &ctx, 0).unwrap();
    // the offset alone is not the secret
    assert_ne!(ringct::public_key(&got.offset).unwrap(), e.onetime_address);
    let secret = alice.onetime_secret(&got.offset).unwrap();
    assert_eq!(ringct::public_key(&secret).unwrap(), e.onetime_address);
    // a non-canonical offset is refused
    assert!(alice.onetime_secret(&[0xff; 32]).is_none());
}

#[test]
fn an_address_survives_its_text_form() {
    for t in 1..20u8 {
        let a = keys(t).address();
        assert_eq!(Address::from_text(&a.to_text()), Ok(a));
        assert!(a.to_text().starts_with("tni1"));
    }
}
