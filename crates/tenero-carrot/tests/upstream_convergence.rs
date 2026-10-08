//! Monero's Carrot convergence values (`tests/vectors/upstream_monero_carrot_convergence.json`, imported verbatim from
//! Monero's `tests/unit_tests/carrot_convergence.cpp` by `reference/tools/import_upstream_carrot.py`). Each test here is
//! one test of that file, with the same inputs: our Rust must give Monero's bytes.

use std::collections::HashMap;

use curve25519_dalek::edwards::EdwardsPoint;
use curve25519_dalek::scalar::Scalar;
use tenero_carrot::account::{subaddress, AccountPublic, AccountSecrets, AddressIndex};
use tenero_carrot::derive::*;
use tenero_carrot::points::{compress, decompress};
use tenero_carrot::{EnoteType, InputContext};
use tenero_core::vectors::{hex, load};

struct V {
    hex: HashMap<String, Vec<u8>>,
    int: HashMap<String, u64>,
}

impl V {
    fn load() -> V {
        let file = load("upstream_monero_carrot_convergence").unwrap();
        let mut hex_values = HashMap::new();
        let mut ints = HashMap::new();
        for (name, v) in file["values"].as_object().unwrap() {
            if let Some(h) = v["hex"].as_str() {
                hex_values.insert(name.clone(), hex(h).unwrap());
            } else {
                ints.insert(name.clone(), v["int"].as_u64().unwrap());
            }
        }
        V {
            hex: hex_values,
            int: ints,
        }
    }

    fn b<const N: usize>(&self, name: &str) -> [u8; N] {
        self.hex[name]
            .clone()
            .try_into()
            .unwrap_or_else(|_| panic!("{name} is not {N} bytes"))
    }

    fn s(&self, name: &str) -> Scalar {
        Option::from(Scalar::from_canonical_bytes(self.b(name)))
            .unwrap_or_else(|| panic!("{name} is not a scalar"))
    }

    fn ctx(&self) -> InputContext {
        InputContext(self.b("input_context"))
    }

    fn major(&self) -> u32 {
        self.int["address_index_major"] as u32
    }

    fn minor(&self) -> u32 {
        self.int["address_index_minor"] as u32
    }
}

// --- the account ---

#[test]
fn make_carrot_provespend_key() {
    let v = V::load();
    assert_eq!(make_provespend_key(&v.b("s_master")), v.s("k_prove_spend"));
}

#[test]
fn make_carrot_viewbalance_secret() {
    let v = V::load();
    assert_eq!(
        make_viewbalance_secret(&v.b("s_master")),
        v.b::<32>("s_view_balance")
    );
}

#[test]
fn make_carrot_partial_spend_pubkey() {
    let v = V::load();
    assert_eq!(
        make_partial_spend_pubkey(&v.s("k_prove_spend")),
        v.b::<32>("partial_spend_pubkey")
    );
}

#[test]
fn make_carrot_generateimage_preimage() {
    let v = V::load();
    assert_eq!(
        make_generateimage_preimage(&v.b("s_view_balance")),
        v.b::<32>("s_generate_image_preimage")
    );
}

#[test]
fn make_carrot_generateimage_key() {
    let v = V::load();
    assert_eq!(
        make_generateimage_key(
            &v.b("s_generate_image_preimage"),
            &v.b("partial_spend_pubkey")
        ),
        v.s("k_generate_image")
    );
}

#[test]
fn make_carrot_viewincoming_key() {
    let v = V::load();
    assert_eq!(
        make_viewincoming_key(&v.b("s_view_balance")),
        v.s("k_view_incoming")
    );
}

#[test]
fn make_carrot_generateaddress_secret() {
    let v = V::load();
    assert_eq!(
        make_generateaddress_secret(&v.b("s_view_balance")),
        v.b::<32>("s_generate_address")
    );
}

#[test]
fn make_carrot_spend_pubkey() {
    let v = V::load();
    assert_eq!(
        make_spend_pubkey(&v.s("k_generate_image"), &v.s("k_prove_spend")),
        v.b::<32>("account_spend_pubkey")
    );
}

#[test]
fn make_view_pubkey() {
    let v = V::load();
    let k_s = decompress(&v.b("account_spend_pubkey")).unwrap();
    assert_eq!(
        compress(&(k_s * v.s("k_view_incoming"))),
        v.b::<32>("account_view_pubkey")
    );
}

#[test]
fn the_whole_account_from_the_master_secret() {
    let v = V::load();
    let a = AccountSecrets::from_master(&v.b("s_master"));
    assert_eq!(a.k_prove_spend, v.s("k_prove_spend"));
    assert_eq!(a.s_view_balance, v.b::<32>("s_view_balance"));
    assert_eq!(
        a.s_generate_image_preimage,
        v.b::<32>("s_generate_image_preimage")
    );
    assert_eq!(a.k_generate_image, v.s("k_generate_image"));
    assert_eq!(a.k_view_incoming, v.s("k_view_incoming"));
    assert_eq!(a.s_generate_address, v.b::<32>("s_generate_address"));
    assert_eq!(a.public.spend_pubkey, v.b::<32>("account_spend_pubkey"));
    assert_eq!(a.public.view_pubkey, v.b::<32>("account_view_pubkey"));
    assert_eq!(
        a.public.main_view_pubkey,
        compress(&EdwardsPoint::mul_base(&v.s("k_view_incoming")))
    );
}

// --- the subaddress ---

#[test]
fn make_carrot_address_index_preimage_1() {
    let v = V::load();
    assert_eq!(
        make_address_index_preimage_1(&v.b("s_generate_address"), v.major(), v.minor()),
        v.b::<32>("address_index_preimage_1")
    );
}

#[test]
fn make_carrot_address_index_preimage_2() {
    let v = V::load();
    assert_eq!(
        make_address_index_preimage_2(
            &v.b("address_index_preimage_1"),
            v.major(),
            v.minor(),
            &v.b("account_spend_pubkey"),
            &v.b("account_view_pubkey")
        ),
        v.b::<32>("address_index_preimage_2")
    );
}

#[test]
fn make_carrot_subaddress_scalar() {
    let v = V::load();
    assert_eq!(
        make_subaddress_scalar(
            &v.b("address_index_preimage_2"),
            &v.b("account_spend_pubkey")
        ),
        v.s("subaddress_scalar")
    );
}

#[test]
fn make_carrot_subaddress_v1() {
    let v = V::load();
    let public = AccountPublic {
        spend_pubkey: v.b("account_spend_pubkey"),
        view_pubkey: v.b("account_view_pubkey"),
        main_view_pubkey: [0; 32], // not used by a subaddress
    };
    let index = AddressIndex {
        major: v.major(),
        minor: v.minor(),
    };
    let sub = subaddress(&public, &v.b("s_generate_address"), index).unwrap();
    assert_eq!(sub.spend_pubkey, v.b::<32>("subaddress_spend_pubkey"));
    assert_eq!(sub.view_pubkey, v.b::<32>("subaddress_view_pubkey"));
    assert!(sub.is_subaddress);
}

// --- the enote ---

#[test]
fn make_carrot_enote_ephemeral_privkey() {
    let v = V::load();
    assert_eq!(
        make_enote_ephemeral_privkey(
            &v.b("anchor_norm"),
            &v.ctx(),
            &v.b("subaddress_spend_pubkey"),
            &v.b("subaddress_view_pubkey"),
            &v.b("payment_id")
        ),
        v.s("enote_ephemeral_privkey")
    );
}

#[test]
fn make_carrot_enote_ephemeral_pubkey_cryptonote() {
    let v = V::load();
    assert_eq!(
        make_enote_ephemeral_pubkey_cryptonote(&v.s("enote_ephemeral_privkey")),
        v.b::<32>("enote_ephemeral_pubkey_cryptonote")
    );
}

#[test]
fn make_carrot_enote_ephemeral_pubkey_subaddress() {
    let v = V::load();
    assert_eq!(
        make_enote_ephemeral_pubkey_subaddress(
            &v.s("enote_ephemeral_privkey"),
            &v.b("subaddress_spend_pubkey")
        ),
        Some(v.b::<32>("enote_ephemeral_pubkey_subaddress"))
    );
}

#[test]
fn try_make_carrot_shared_key_receiver() {
    let v = V::load();
    assert_eq!(
        make_shared_key_receiver(
            &v.s("k_view_incoming"),
            &v.b("enote_ephemeral_pubkey_subaddress")
        ),
        v.b::<32>("s_sender_receiver")
    );
}

#[test]
fn try_make_carrot_shared_key_sender() {
    let v = V::load();
    assert_eq!(
        make_shared_key_sender(
            &v.s("enote_ephemeral_privkey"),
            &v.b("subaddress_view_pubkey")
        ),
        Some(v.b::<32>("s_sender_receiver"))
    );
}

#[test]
fn make_carrot_contextualized_sender_receiver_secret() {
    let v = V::load();
    assert_eq!(
        make_sender_receiver_secret(
            &v.b("s_sender_receiver"),
            &v.b("enote_ephemeral_pubkey_subaddress"),
            &v.ctx()
        ),
        v.b::<32>("s_sender_receiver_ctx")
    );
}

#[test]
fn make_carrot_amount_blinding_factor_payment() {
    let v = V::load();
    assert_eq!(
        make_amount_blinding_factor(
            &v.b("s_sender_receiver_ctx"),
            v.int["amount"],
            &v.b("subaddress_spend_pubkey"),
            EnoteType::Payment
        ),
        v.s("amount_blinding_factor_payment")
    );
}

#[test]
fn make_carrot_amount_blinding_factor_change() {
    let v = V::load();
    assert_eq!(
        make_amount_blinding_factor(
            &v.b("s_sender_receiver_ctx"),
            v.int["amount"],
            &v.b("subaddress_spend_pubkey"),
            EnoteType::Change
        ),
        v.s("amount_blinding_factor_change")
    );
}

#[test]
fn commit() {
    let v = V::load();
    assert_eq!(
        commit_amount(v.int["amount"], &v.s("amount_blinding_factor_payment")),
        v.b::<32>("amount_commitment")
    );
}

#[test]
fn try_make_carrot_onetime_address_coinbase() {
    let v = V::load();
    assert_eq!(
        make_onetime_address_coinbase(
            &v.b("subaddress_spend_pubkey"),
            &v.b("s_sender_receiver_ctx"),
            v.int["amount"]
        ),
        Some(v.b::<32>("onetime_address_coinbase"))
    );
}

#[test]
fn try_make_carrot_onetime_address() {
    let v = V::load();
    assert_eq!(
        make_onetime_address(
            &v.b("subaddress_spend_pubkey"),
            &v.b("s_sender_receiver_ctx"),
            &v.b("amount_commitment")
        ),
        Some(v.b::<32>("onetime_address"))
    );
}

#[test]
fn make_carrot_view_tag() {
    let v = V::load();
    assert_eq!(
        make_view_tag(&v.b("s_sender_receiver"), &v.ctx(), &v.b("onetime_address")),
        v.b::<3>("view_tag")
    );
}

#[test]
fn make_carrot_anchor_encryption_mask() {
    let v = V::load();
    assert_eq!(
        make_anchor_encryption_mask(&v.b("s_sender_receiver_ctx"), &v.b("onetime_address")),
        v.b::<16>("anchor_encryption_mask")
    );
}

#[test]
fn make_carrot_amount_encryption_mask() {
    let v = V::load();
    assert_eq!(
        make_amount_encryption_mask(&v.b("s_sender_receiver_ctx"), &v.b("onetime_address")),
        v.b::<8>("amount_encryption_mask")
    );
}

#[test]
fn make_carrot_payment_id_encryption_mask() {
    let v = V::load();
    assert_eq!(
        make_payment_id_encryption_mask(&v.b("s_sender_receiver_ctx"), &v.b("onetime_address")),
        v.b::<8>("payment_id_encryption_mask")
    );
}

#[test]
fn make_carrot_janus_anchor_special() {
    let v = V::load();
    assert_eq!(
        make_janus_anchor_special(
            &v.b("enote_ephemeral_pubkey_cryptonote"),
            &v.ctx(),
            &v.b("onetime_address"),
            &v.s("k_view_incoming")
        ),
        v.b::<16>("anchor_special")
    );
}

#[test]
fn the_extensions_recover_the_address_spend_key_from_the_onetime_address() {
    let v = V::load();
    let ctx = v.b("s_sender_receiver_ctx");
    let c = v.b("amount_commitment");
    let g = make_sender_extension_g(&ctx, &c);
    let t = make_sender_extension_t(&ctx, &c);
    assert_eq!(
        recover_address_spend_pubkey(&v.b("onetime_address"), &g, &t),
        Some(v.b::<32>("subaddress_spend_pubkey"))
    );
}
