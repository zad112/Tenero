//! Message signatures and payment proofs: the vectors from the independent Python reference, and every way a signature or a
//! proof can be wrong. **Unaudited** (`docs/WALLET_PROOFS.md`).

use rand_core::{CryptoRng, RngCore};
use serde_json::Value;
use tenero_wallet::interim::{create_enote, tx_context};
use tenero_wallet::proofs::{
    check, key_proof, prove_received, prove_sent, sign_message, verify_message, MessageSignature,
    OutputFields, PaymentProof, ProofError, ProofKind,
};
use tenero_wallet::{Address, Keys, TxSecret};

/// A "random" source that hands out given bytes (the vectors fix what the signer draws).
struct Fixed {
    bytes: Vec<u8>,
    at: usize,
}

impl Fixed {
    fn new(bytes: Vec<u8>) -> Fixed {
        Fixed { bytes, at: 0 }
    }
}

impl RngCore for Fixed {
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
        for d in dest.iter_mut() {
            *d = self.bytes[self.at % self.bytes.len()];
            self.at += 1;
        }
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}
impl CryptoRng for Fixed {}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

fn arr<const N: usize>(s: &str) -> [u8; N] {
    unhex(s).try_into().unwrap()
}

fn vectors() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/vectors/wallet_proofs.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn fields(o: &Value) -> OutputFields {
    OutputFields {
        onetime_address: arr(o["onetime_address"].as_str().unwrap()),
        ephemeral_pubkey: arr(o["ephemeral_pubkey"].as_str().unwrap()),
        amount_commitment: arr(o["amount_commitment"].as_str().unwrap()),
        amount_enc: arr(o["amount_enc"].as_str().unwrap()),
        ctx: arr(o["context"].as_str().unwrap()),
        index: o["index"].as_u64().unwrap() as u32,
        public_amount: o["coinbase"]
            .as_bool()
            .unwrap()
            .then(|| o["public_amount"].as_u64().unwrap()),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// the vectors
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn the_rust_signatures_are_the_references_signatures_bit_for_bit() {
    let v = vectors();
    let list = v["messages"].as_array().unwrap();
    assert!(list.len() >= 4);
    for m in list {
        let keys = Keys::from_seed(&arr(m["seed"].as_str().unwrap()));
        let address = Address::from_text(m["address"].as_str().unwrap()).unwrap();
        assert_eq!(keys.address(), address);
        let message = unhex(m["message"].as_str().unwrap());
        let mut rng = Fixed::new(unhex(m["rnd"].as_str().unwrap()));
        let sig = sign_message(&keys, &mut rng, &message);
        assert_eq!(
            hex(&sig.0),
            m["signature"].as_str().unwrap(),
            "{}",
            m["note"]
        );
        assert_eq!(
            verify_message(&address, &message, &sig),
            Ok(()),
            "{}",
            m["note"]
        );
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn the_rust_proofs_are_the_references_proofs_bit_for_bit_and_all_three_kinds_check() {
    let v = vectors();
    let list = v["proofs"].as_array().unwrap();
    assert!(list.len() >= 9);
    for p in list {
        let note = p["note"].as_str().unwrap();
        let out = fields(&p["output"]);
        let address = Address::from_text(p["address"].as_str().unwrap()).unwrap();
        let (height, gi) = (
            p["height"].as_u64().unwrap(),
            p["global_index"].as_u64().unwrap(),
        );
        let mut rng = Fixed::new(unhex(p["rnd"].as_str().unwrap()));
        let secret = TxSecret::new(arr(p["tx_secret"].as_str().unwrap()));
        let made = match p["kind"].as_u64().unwrap() {
            1 => {
                let keys = Keys::from_seed(&arr(p["receiver_seed"].as_str().unwrap()));
                prove_received(&keys.view_keys(), &mut rng, height, gi, &out).unwrap()
            }
            2 => prove_sent(&secret, &address, &mut rng, height, gi, &out).unwrap(),
            _ => key_proof(&secret, &address, height, gi, &out).unwrap(),
        };
        assert_eq!(
            hex(&made.to_bytes()),
            p["proof"].as_str().unwrap(),
            "{note}"
        );
        // and the reference's bytes are read and checked by the Rust checker
        let parsed = PaymentProof::from_bytes(&unhex(p["proof"].as_str().unwrap())).unwrap();
        let c = check(&parsed, &out).unwrap_or_else(|e| panic!("{note}: {e}"));
        assert_eq!(c.amount, p["amount"].as_u64().unwrap(), "{note}");
        assert_eq!((c.height, c.global_index, c.address), (height, gi, address));
        assert_eq!(c.block_reward, out.public_amount.is_some());
        // the text form round-trips
        assert_eq!(PaymentProof::from_text(&parsed.to_text()).unwrap(), parsed);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// everything that must fail
// ---------------------------------------------------------------------------------------------------------------

fn a_signature() -> (Address, Vec<u8>, MessageSignature) {
    let keys = Keys::from_seed(&[7; 32]);
    let msg = b"I, the holder of this key, wrote this".to_vec();
    let sig = sign_message(&keys, &mut rand_core::OsRng, &msg);
    (keys.address(), msg, sig)
}

#[test]
fn a_signature_fails_for_a_changed_message_a_changed_byte_or_another_address() {
    let (addr, msg, sig) = a_signature();
    assert_eq!(verify_message(&addr, &msg, &sig), Ok(()));
    assert_eq!(
        verify_message(&addr, b"I, the holder of this key, wrote thiS", &sig),
        Err(ProofError::BadSignature)
    );
    assert_eq!(
        verify_message(&addr, &msg[..msg.len() - 1], &sig),
        Err(ProofError::BadSignature)
    );
    assert_eq!(
        verify_message(&addr, &[msg.clone(), vec![0]].concat(), &sig),
        Err(ProofError::BadSignature)
    );
    for i in 0..64 {
        let mut bad = sig.clone();
        bad.0[i] ^= 1;
        assert!(
            verify_message(&addr, &msg, &bad).is_err(),
            "a changed byte {i} was accepted"
        );
    }
    let other = Keys::from_seed(&[8; 32]).address();
    assert_eq!(
        verify_message(&other, &msg, &sig),
        Err(ProofError::BadSignature)
    );
    // the same spend key under another view key is another address: it does not verify either
    let mixed = Address {
        spend: addr.spend,
        view: other.view,
    };
    assert_eq!(
        verify_message(&mixed, &msg, &sig),
        Err(ProofError::BadSignature)
    );
    // an address with an invalid key
    let bad_addr = Address {
        spend: [0; 32],
        view: addr.view,
    };
    assert_eq!(
        verify_message(&bad_addr, &msg, &sig),
        Err(ProofError::BadAddress)
    );
}

#[test]
fn a_signature_with_a_non_canonical_response_is_refused_so_it_cannot_be_reworked_into_a_second_valid_one(
) {
    let (addr, msg, sig) = a_signature();
    // z + L: the same number modulo L, but not the canonical encoding
    const L: [u8; 32] = [
        0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde,
        0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
    ];
    let mut z = [0u8; 32];
    z.copy_from_slice(&sig.0[32..]);
    let mut carry = 0u16;
    for i in 0..32 {
        let t = u16::from(z[i]) + u16::from(L[i]) + carry;
        z[i] = t as u8;
        carry = t >> 8;
    }
    assert_eq!(carry, 0);
    let mut bad = sig.clone();
    bad.0[32..].copy_from_slice(&z);
    assert_eq!(
        verify_message(&addr, &msg, &bad),
        Err(ProofError::BadSignature)
    );
}

#[test]
fn a_signature_text_is_strict() {
    let (_, _, sig) = a_signature();
    let t = sig.to_text();
    assert!(t.starts_with("tnsig1") && t.len() == 6 + 128);
    assert_eq!(MessageSignature::from_text(&t).unwrap(), sig);
    assert_eq!(
        MessageSignature::from_text(&format!("  {t}\n")).unwrap(),
        sig,
        "spaces around it do not matter"
    );
    assert!(MessageSignature::from_text(&t.to_uppercase()).is_err());
    assert!(MessageSignature::from_text(&t.replace("tnsig1", "tnsig2")).is_err());
    assert!(MessageSignature::from_text(&t[..t.len() - 2]).is_err());
    assert!(MessageSignature::from_text(&format!("{t}00")).is_err());
    assert!(MessageSignature::from_text("").is_err());
    assert!(MessageSignature::from_text(&format!("tnsig1{}g", "0".repeat(127))).is_err());
}

/// An output made for `to` by the interim scheme, with the secret that made it.
fn an_output(to: &Address, amount: u64, coinbase: bool) -> (OutputFields, TxSecret) {
    let ki = [9u8; 32];
    let ctx = tx_context(&ki);
    let e = create_enote(&mut rand_core::OsRng, to, amount, &ctx, 1, coinbase).unwrap();
    (
        OutputFields {
            onetime_address: e.onetime_address,
            ephemeral_pubkey: e.ephemeral_pubkey,
            amount_commitment: e.amount_commitment,
            amount_enc: e.amount_enc,
            ctx,
            index: 1,
            public_amount: coinbase.then_some(amount),
        },
        e.tx_secret.clone(),
    )
}

#[test]
fn every_kind_of_proof_shows_the_amount_and_nothing_proves_the_wrong_thing() {
    let receiver = Keys::from_seed(&[1; 32]);
    let stranger = Keys::from_seed(&[2; 32]);
    let to = receiver.address();
    let (out, r) = an_output(&to, 123_456_789, false);
    let rng = &mut rand_core::OsRng;

    let received = prove_received(&receiver.view_keys(), rng, 50, 7, &out).unwrap();
    let sent = prove_sent(&r, &to, rng, 50, 7, &out).unwrap();
    let key = key_proof(&r, &to, 50, 7, &out).unwrap();
    for p in [&received, &sent, &key] {
        let c = check(p, &out).unwrap();
        assert_eq!(
            (c.amount, c.address, c.height, c.global_index),
            (123_456_789, to, 50, 7)
        );
        assert!(!c.block_reward);
    }

    // the stranger cannot prove receipt of an output that is not theirs, and the sender cannot prove it was paid to someone else
    assert_eq!(
        prove_received(&stranger.view_keys(), rng, 50, 7, &out).err(),
        Some(ProofError::NotAddressed)
    );
    assert_eq!(
        prove_sent(&r, &stranger.address(), rng, 50, 7, &out).err(),
        Some(ProofError::NotAddressed)
    );
    assert_eq!(
        key_proof(&r, &stranger.address(), 50, 7, &out).err(),
        Some(ProofError::NotAddressed)
    );
    // a secret that is not this output's
    let (_, other_r) = an_output(&to, 5, false);
    assert!(prove_sent(&other_r, &to, rng, 50, 7, &out).is_err());
    assert!(key_proof(&other_r, &to, 50, 7, &out).is_err());

    // a good proof, about another output, or with a changed claim, fails
    let (other_out, _) = an_output(&to, 123_456_789, false);
    for p in [&received, &sent, &key] {
        assert!(
            check(p, &other_out).is_err(),
            "{:?} checked against another output",
            p.kind
        );
        // the address changed to a stranger's
        let mut bytes = p.to_bytes();
        bytes[17..81]
            .copy_from_slice(&[stranger.address().spend, stranger.address().view].concat());
        assert!(check(&PaymentProof::from_bytes(&bytes).unwrap(), &out).is_err());
        // the height or the output number changed (they are part of what the proof is bound to)
        if p.kind != ProofKind::Key {
            let mut b = p.to_bytes();
            b[1] ^= 1;
            assert_eq!(
                check(&PaymentProof::from_bytes(&b).unwrap(), &out).err(),
                Some(ProofError::BadProof)
            );
            let mut b = p.to_bytes();
            b[9] ^= 1;
            assert_eq!(
                check(&PaymentProof::from_bytes(&b).unwrap(), &out).err(),
                Some(ProofError::BadProof)
            );
        }
    }
    // the kind swapped: a received proof is not a sent proof
    let mut b = received.to_bytes();
    b[0] = 2;
    assert!(check(&PaymentProof::from_bytes(&b).unwrap(), &out).is_err());
    let mut b = sent.to_bytes();
    b[0] = 1;
    assert!(check(&PaymentProof::from_bytes(&b).unwrap(), &out).is_err());
    // every changed byte of every kind is caught
    for p in [&received, &sent, &key] {
        let good = p.to_bytes();
        for i in 0..good.len() {
            // a key proof's height and output number only say where to look: the output found there is what the key is
            // checked against, so they are not part of what it binds (the chain check catches a wrong place)
            if p.kind == ProofKind::Key && (1..17).contains(&i) {
                continue;
            }
            let mut bad = good.clone();
            bad[i] ^= 1;
            let ok = PaymentProof::from_bytes(&bad)
                .ok()
                .is_some_and(|q| check(&q, &out).is_ok());
            assert!(!ok, "{:?}: a changed byte {i} was accepted", p.kind);
        }
    }
    // an output whose amount was changed after the fact no longer matches its commitment
    let mut lied = out.clone();
    lied.amount_enc[0] ^= 1;
    for p in [&received, &sent, &key] {
        assert_eq!(check(p, &lied).err(), Some(ProofError::AmountMismatch));
    }
}

#[test]
fn a_block_reward_is_proved_with_its_public_amount() {
    let receiver = Keys::from_seed(&[3; 32]);
    let to = receiver.address();
    let (out, r) = an_output(&to, 4_000_000_000, true);
    let rng = &mut rand_core::OsRng;
    let p = prove_received(&receiver.view_keys(), rng, 1000, 3, &out).unwrap();
    let c = check(&p, &out).unwrap();
    assert_eq!((c.amount, c.block_reward), (4_000_000_000, true));
    assert!(check(&key_proof(&r, &to, 1000, 3, &out).unwrap(), &out).is_ok());
}

#[test]
fn proof_bytes_and_text_of_the_wrong_shape_are_refused_without_panicking() {
    let receiver = Keys::from_seed(&[1; 32]);
    let (out, _) = an_output(&receiver.address(), 9, false);
    let p = prove_received(&receiver.view_keys(), &mut rand_core::OsRng, 1, 1, &out).unwrap();
    let good = p.to_bytes();
    for n in 0..good.len() {
        assert!(PaymentProof::from_bytes(&good[..n]).is_err(), "{n} bytes");
    }
    assert!(PaymentProof::from_bytes(&[good.clone(), vec![0]].concat()).is_err());
    let mut b = good.clone();
    b[0] = 0;
    assert!(PaymentProof::from_bytes(&b).is_err());
    b[0] = 4;
    assert!(PaymentProof::from_bytes(&b).is_err());
    let t = p.to_text();
    assert_eq!(PaymentProof::from_text(&t).unwrap(), p);
    assert!(PaymentProof::from_text(&t.to_uppercase()).is_err());
    assert!(PaymentProof::from_text(&t.replace("tnpay1", "tnpay9")).is_err());
    assert!(
        PaymentProof::from_text(&format!("{t}0")).is_err(),
        "odd length"
    );
    assert!(PaymentProof::from_text(&"a".repeat(5000)).is_err());
    assert!(PaymentProof::from_text("").is_err());
    // a point that is not on the curve, a small-order point, and a non-canonical scalar in the right places
    let mut bad = good.clone();
    bad[17..49].copy_from_slice(&[0u8; 32]);
    assert!(check(&PaymentProof::from_bytes(&bad).unwrap(), &out).is_err());
    let mut bad = good.clone();
    let n = bad.len();
    bad[n - 1] = 0xff; // z not canonical
    assert!(check(&PaymentProof::from_bytes(&bad).unwrap(), &out).is_err());
}

#[test]
fn proofs_and_signatures_survive_many_random_keys_and_amounts() {
    let rng = &mut rand_core::OsRng;
    for n in 0..40u8 {
        let keys = Keys::from_seed(&[n; 32]);
        let to = keys.address();
        let amount = u64::from(n) * 1_000_003 + 1;
        let (out, r) = an_output(&to, amount, n % 5 == 0);
        for p in [
            prove_received(&keys.view_keys(), rng, u64::from(n), 2, &out).unwrap(),
            prove_sent(&r, &to, rng, u64::from(n), 2, &out).unwrap(),
            key_proof(&r, &to, u64::from(n), 2, &out).unwrap(),
        ] {
            assert_eq!(
                check(&PaymentProof::from_text(&p.to_text()).unwrap(), &out)
                    .unwrap()
                    .amount,
                amount
            );
        }
        let sig = sign_message(&keys, rng, &[n; 3]);
        assert_eq!(verify_message(&to, &[n; 3], &sig), Ok(()));
        assert!(verify_message(&to, &[n.wrapping_add(1); 3], &sig).is_err());
    }
}
