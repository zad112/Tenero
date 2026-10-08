//! Message signatures and payment proofs on Carrot (`src/proofs.rs`, `docs/WALLET_PROOFS.md`): against the independent Python
//! reference's vectors (`tests/vectors/wallet_proofs.json`), and on a test chain with real payments, real block rewards and
//! a node that checks every proof. The signatures are our own construction, unreviewed. **A test chain, not a real one.**

use std::path::PathBuf;

use curve25519_dalek::scalar::Scalar;
use rand_core::{CryptoRng, OsRng, RngCore};
use tenero_chain::Sha256Pow;
use tenero_core::v2::ids::PowKind;
use tenero_core::vectors::{hex, load};
use tenero_net::sim::{test_chain_params, LABEL};
use tenero_node::{Node, NodeConfig};
use tenero_store::Store;
use tenero_wallet::proofs::{
    check_anchor, check_payment, sign_message, verify_message, PaymentProof, ProofError, Signature,
};
use tenero_wallet::testing::{test_block_to, READY};
use tenero_wallet::{Address, FeeLevel, Kind, Network, Purse};

/// A "random" source that gives the fixed bytes of a vector.
struct Fixed(Vec<u8>);

impl RngCore for Fixed {
    fn next_u32(&mut self) -> u32 {
        unreachable!()
    }
    fn next_u64(&mut self) -> u64 {
        unreachable!()
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        assert_eq!(
            dest.len(),
            self.0.len(),
            "the signer draws exactly 32 bytes"
        );
        dest.copy_from_slice(&self.0);
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}
impl CryptoRng for Fixed {}

fn h(v: &serde_json::Value) -> Vec<u8> {
    hex(v.as_str().unwrap()).unwrap()
}

fn b32(v: &serde_json::Value) -> [u8; 32] {
    h(v).try_into().unwrap()
}

fn scalar(v: &serde_json::Value) -> Scalar {
    Option::from(Scalar::from_canonical_bytes(b32(v))).unwrap()
}

/// An address made of raw keys (the vectors' refused cases use keys no real address has).
fn raw_address(spend: [u8; 32], view: [u8; 32]) -> Address {
    Address {
        network: Network::Gamma,
        kind: Kind::Main,
        spend_pubkey: spend,
        view_pubkey: view,
        payment_id: [0; 8],
    }
}

#[test]
fn the_generator_t_is_the_one_the_reference_uses() {
    let v = load("wallet_proofs").unwrap();
    assert_eq!(
        tenero_carrot::points::compress(&tenero_carrot::points::T),
        b32(&v["generators"]["T"])
    );
}

#[test]
fn signatures_are_the_references_byte_for_byte_and_the_refused_ones_are_refused() {
    let v = load("wallet_proofs").unwrap();
    let cases = v["signatures"].as_array().unwrap();
    assert!(cases.len() >= 4);
    for c in cases {
        let address = raw_address(b32(&c["spend"]), b32(&c["view"]));
        let msg = h(&c["message"]);
        let sig = sign_message(
            &scalar(&c["a"]),
            &scalar(&c["b"]),
            &address,
            &msg,
            &mut Fixed(h(&c["rnd"])),
        );
        assert_eq!(sig.to_bytes().to_vec(), h(&c["signature"]));
        assert_eq!(sig.to_text(), c["text"].as_str().unwrap());
        assert_eq!(Signature::from_text(&sig.to_text()), Ok(sig));
        assert!(verify_message(&address, &msg, &sig));
    }
    for c in v["refused_signatures"].as_array().unwrap() {
        let address = raw_address(b32(&c["spend"]), b32(&c["view"]));
        let ok = Signature::from_bytes(&h(&c["signature"]))
            .is_ok_and(|s| verify_message(&address, &h(&c["message"]), &s));
        assert!(!ok, "{}", c["note"]);
    }
}

#[test]
fn proofs_are_the_references_byte_for_byte_and_the_malformed_ones_are_refused() {
    let v = load("wallet_proofs").unwrap();
    for c in v["proofs"].as_array().unwrap() {
        let bytes = h(&c["bytes"]);
        let p = PaymentProof::from_bytes(&bytes).unwrap();
        assert_eq!(p.address.to_text(), c["address"].as_str().unwrap());
        assert_eq!(p.height, c["height"].as_u64().unwrap());
        assert_eq!(p.onetime_address, b32(&c["onetime_address"]));
        assert_eq!(p.anchor.to_vec(), h(&c["anchor"]));
        assert_eq!(p.to_bytes(), bytes);
        assert_eq!(p.to_text(), c["text"].as_str().unwrap());
        assert_eq!(PaymentProof::from_text(&p.to_text()), Ok(p.clone()));
        // a received proof's signature: made again from the same secrets and random bytes, the same bytes
        if !c["signature"].is_null() {
            let mut unsigned = PaymentProof {
                signature: None,
                ..p.clone()
            };
            unsigned
                .sign(
                    &scalar(&c["a"]),
                    &scalar(&c["b"]),
                    &h(&c["message"]),
                    &mut Fixed(h(&c["rnd"])),
                )
                .unwrap();
            assert_eq!(unsigned, p);
        }
    }
    for c in v["invalid_proofs"].as_array().unwrap() {
        assert!(
            PaymentProof::from_bytes(&h(&c["bytes"])).is_err(),
            "{}",
            c["note"]
        );
    }
    assert!(PaymentProof::from_text("TENsig1abc").is_err());
    assert!(Signature::from_text("TENpay1abc").is_err());
}

// ---- on a chain -----------------------------------------------------------------------------------------------------------

const T0: u64 = 1_700_000_000;

struct Rig {
    path: PathBuf,
    store: Store,
    params: tenero_chain::ChainParams,
}

impl Rig {
    fn new(tag: &str) -> Rig {
        let path =
            std::env::temp_dir().join(format!("tenero-proofs-{}-{tag}.redb", std::process::id()));
        remove(&path);
        Rig {
            store: Store::open(&path, LABEL, PowKind::Sha256).unwrap(),
            path,
            params: test_chain_params(),
        }
    }
    fn node(&self) -> Node<'_> {
        Node::new(&self.store, &self.params, &Sha256Pow, NodeConfig::default()).unwrap()
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

fn mine(node: &mut Node<'_>, to: &Address) {
    let height = node.tip().unwrap().0 + 1;
    let ts = T0 + 60 * height;
    let block = test_block_to(node, to, ts);
    node.submit_block(&block, ts + 10).unwrap();
}

fn purse(tag: u8) -> Purse {
    Purse::from_seed(&[tag; 32], Network::Test, 0)
}

#[test]
fn a_payment_is_proved_by_its_sender_and_its_receiver_and_nothing_else_passes() {
    let rig = Rig::new("pay");
    let mut node = rig.node();
    let (mut alice, mut bob) = (purse(1), purse(2));
    let alice_addr = alice.account(0).unwrap().address();
    for _ in 0..READY {
        mine(&mut node, &alice_addr);
    }
    alice.sync(&node).unwrap();
    // Alice pays Bob's subaddress, and a block takes it in
    let bob_sub = {
        let mut w = tenero_wallet::Wallet::from_seed(
            &tenero_wallet::purse::account_seed(&[2; 32], 0),
            Network::Test,
            0,
        );
        w.subaddress(3).unwrap()
    };
    let built = alice
        .pay(
            0,
            &mut node,
            &mut OsRng,
            &bob_sub,
            700_000_000,
            FeeLevel::Low,
            T0,
        )
        .unwrap();
    mine(&mut node, &alice_addr);
    let paid_at = node.tip().unwrap().0;
    bob.sync(&node).unwrap();
    // (Bob's wallet watches its first subaddresses, so it found the payment)
    let got = bob
        .account(0)
        .unwrap()
        .wallet()
        .owned()
        .iter()
        .find(|o| o.amount == 700_000_000)
        .cloned()
        .expect("Bob found the payment");

    // the SENDER's proof: the anchor she kept
    let sent = alice.prove_sent(&built.id, &node).unwrap();
    assert_eq!((sent.height, sent.address), (paid_at, bob_sub));
    assert!(sent.signature.is_none());
    let c = check_payment(&node, &sent, b"").unwrap();
    assert_eq!(
        (c.amount, c.global_index, c.coinbase, c.signed),
        (700_000_000, got.global_index, false, false)
    );
    assert_eq!(c.confirmations, 1);
    // through its text, as a person would pass it on
    let back = PaymentProof::from_text(&sent.to_text()).unwrap();
    assert_eq!(
        check_payment(&node, &back, b"").unwrap().amount,
        700_000_000
    );

    // the RECEIVER's proof: the anchor decrypted from the output, and Bob's signature over it and a message
    let received = bob
        .prove_received(0, got.global_index, &node, b"invoice 42", &mut OsRng)
        .unwrap();
    assert_eq!(
        received.anchor, sent.anchor,
        "the same anchor, from the other side"
    );
    let c = check_payment(&node, &received, b"invoice 42").unwrap();
    assert!(c.signed && c.amount == 700_000_000);
    // the signature binds the message
    assert_eq!(
        check_payment(&node, &received, b"invoice 43"),
        Err(ProofError::BadSignature)
    );

    // the "payment key" alone (the anchor) and the address: the output is found by looking for it
    let anchor = alice.payment_anchor(&built.id).unwrap();
    let c = check_anchor(&node, &anchor, &bob_sub, paid_at.saturating_sub(5)).unwrap();
    assert_eq!((c.height, c.amount), (paid_at, 700_000_000));

    // what must NOT pass: another address, another anchor, another height, a signature by someone else
    let mut wrong = sent.clone();
    wrong.address = alice_addr;
    assert_eq!(
        check_payment(&node, &wrong, b""),
        Err(ProofError::NotThisAddress)
    );
    let mut wrong = sent.clone();
    wrong.anchor[0] ^= 1;
    assert_eq!(
        check_payment(&node, &wrong, b""),
        Err(ProofError::NotThisAddress)
    );
    let mut wrong = sent.clone();
    wrong.height -= 1;
    assert_eq!(check_payment(&node, &wrong, b""), Err(ProofError::NoOutput));
    let mut wrong = sent.clone();
    wrong.height = 10_000;
    assert_eq!(
        check_payment(&node, &wrong, b""),
        Err(ProofError::NoBlock(10_000))
    );
    let mut forged = sent.clone();
    let (_, alice_sig) = alice.sign_message(0, b"x", &mut OsRng).unwrap();
    forged.signature = Some(alice_sig);
    assert_eq!(
        check_payment(&node, &forged, b"x"),
        Err(ProofError::BadSignature)
    );
    assert!(check_anchor(&node, &anchor, &alice_addr, 0).is_err());

    // the change that came back to Alice is not something she can "prove she received"
    alice.sync(&node).unwrap();
    let change = alice
        .account(0)
        .unwrap()
        .wallet()
        .owned()
        .iter()
        .find(|o| o.internal)
        .cloned()
        .expect("the change");
    let e = alice
        .prove_received(0, change.global_index, &node, b"", &mut OsRng)
        .unwrap_err();
    assert!(e.to_string().contains("change"), "{e}");
}

#[test]
fn a_block_reward_is_proved_by_its_miner() {
    let rig = Rig::new("reward");
    let mut node = rig.node();
    let mut alice = purse(1);
    let addr = alice.account(0).unwrap().address();
    for _ in 0..3 {
        mine(&mut node, &addr);
    }
    alice.sync(&node).unwrap();
    let reward = alice.account(0).unwrap().wallet().owned()[1].clone();
    let p = alice
        .prove_received(0, reward.global_index, &node, b"mine", &mut OsRng)
        .unwrap();
    let c = check_payment(&node, &p, b"mine").unwrap();
    assert!(c.coinbase && c.signed);
    assert_eq!((c.height, c.amount), (reward.height, reward.amount));
    assert_eq!(c.confirmations, 3 - reward.height + 1);
    // the anchor alone finds it too
    assert_eq!(
        check_anchor(&node, &p.anchor, &addr, 0).unwrap().height,
        reward.height
    );
}

#[test]
fn a_message_is_signed_by_an_account_and_verifies_for_its_address_and_no_other() {
    let alice = purse(1);
    let mut both = purse(1);
    both.add_account("Savings", 0).unwrap();
    let (addr, sig) = alice.sign_message(0, b"I am Alice", &mut OsRng).unwrap();
    assert_eq!(addr, alice.account(0).unwrap().address());
    assert!(verify_message(&addr, b"I am Alice", &sig));
    assert!(!verify_message(&addr, b"I am Alice.", &sig));
    let savings = both.account(1).unwrap().address();
    assert!(!verify_message(&savings, b"I am Alice", &sig));
    // through text, and a signature of an integrated address's main keys is the same
    let sig = Signature::from_text(&sig.to_text()).unwrap();
    let integrated = addr.with_payment_id([7; 8]).unwrap();
    assert!(verify_message(&integrated, b"I am Alice", &sig));
    // a subaddress signs as itself (its keys are not the main address's)
    let mut w = tenero_wallet::Wallet::from_seed(&[9; 32], Network::Test, 0);
    let sub = w.subaddress(5).unwrap();
    let index = tenero_carrot::account::AddressIndex { major: 0, minor: 5 };
    let (signed_as, sig) = w.sign_message(index, b"sub", &mut OsRng).unwrap();
    assert_eq!(signed_as, sub);
    assert!(verify_message(&sub, b"sub", &sig));
    assert!(!verify_message(&w.address(), b"sub", &sig));
}
