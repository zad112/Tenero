//! Tests of the transaction proofs (`tenero_crypto::ringct`). Every transaction here carries REAL proofs,
//! made by the prover; each negative test breaks exactly one thing and expects that error.

use curve25519_dalek::{
    constants::{ED25519_BASEPOINT_POINT, EIGHT_TORSION},
    edwards::EdwardsPoint,
    scalar::Scalar as DScalar,
};
use monero_bulletproofs::Bulletproof;
use monero_ed25519::{Commitment, CompressedPoint, Scalar};
use rand_chacha::{rand_core::SeedableRng, ChaCha20Rng};
use tenero_chain::proofs::{ProofCheck, TxContext};
use tenero_core::v2::codec::Wire;
use tenero_core::v2::ids::tx_id;
use tenero_core::v2::{Input, Output, Prunable, Transaction, TxPrefix, MAX_PROOF};
use tenero_crypto::ringct::{
    commit, key_image, prove, public_amount_commitment, public_key, verify_tx, OutputSecret,
    ProofError, RingCtProofs, SpendInput,
};
use tenero_store::StoredOutput;

const CHAIN: [u8; 32] = [7; 32];

fn rand_scalar(rng: &mut ChaCha20Rng) -> [u8; 32] {
    <[u8; 32]>::from(Scalar::random(rng))
}

fn out(amount: u64, mask: &[u8; 32], rng: &mut ChaCha20Rng) -> (Output, OutputSecret) {
    let secret = rand_scalar(rng);
    (
        Output {
            onetime_address: public_key(&secret).unwrap(),
            amount_commitment: commit(mask, amount).unwrap(),
            amount_enc: [1; 8],
            view_tag: [2; 3],
            ephemeral_pubkey: public_key(&rand_scalar(rng)).unwrap(),
            anchor_enc: [3; 16],
        },
        OutputSecret {
            amount,
            mask: *mask,
        },
    )
}

/// A signed transaction and the chain's view of its rings.
struct Fixture {
    tx: Transaction,
    ring_members: Vec<Vec<StoredOutput>>,
    inputs: Vec<SpendInput>,
    outputs: Vec<OutputSecret>,
}

/// `input_amounts` are spent in rings of `ring_len`, the signer at position `signer(i)`. The outputs are
/// `output_amounts`; the fee is whatever is left over.
fn build(
    seed: u64,
    input_amounts: &[u64],
    output_amounts: &[u64],
    ring_len: usize,
    coinbase_inputs: bool,
) -> Fixture {
    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    let fee = input_amounts.iter().sum::<u64>() - output_amounts.iter().sum::<u64>();
    let mut inputs = Vec::new();
    let mut ring_members = Vec::new();
    for (n, &amount) in input_amounts.iter().enumerate() {
        let signer = (n * 5 + 3) % ring_len;
        let secret = rand_scalar(&mut rng);
        // a coinbase output's commitment has the public mask 1
        let mask = if coinbase_inputs {
            <[u8; 32]>::from(Scalar::ONE)
        } else {
            rand_scalar(&mut rng)
        };
        let mut members = Vec::new();
        let mut ring = Vec::new();
        let mut indexes = Vec::new();
        for pos in 0..ring_len {
            indexes.push(1000 * n as u64 + 10 * pos as u64 + 1);
            let (address, commitment, public_amount) = if pos == signer {
                let c = commit(&mask, amount).unwrap();
                (public_key(&secret).unwrap(), c, amount)
            } else {
                let a = 1 + (rng.next_u64_for_test() % 1000);
                (
                    public_key(&rand_scalar(&mut rng)).unwrap(),
                    commit(&rand_scalar(&mut rng), a).unwrap(),
                    a,
                )
            };
            let stored = if coinbase_inputs {
                StoredOutput {
                    onetime_address: address,
                    amount_commitment: [0; 32],
                    public_amount,
                    height: 5,
                    coinbase: true,
                }
            } else {
                StoredOutput {
                    onetime_address: address,
                    amount_commitment: commitment,
                    public_amount: 0,
                    height: 5,
                    coinbase: false,
                }
            };
            // what goes into the ring: the stored commitment, or the public-amount one for a coinbase
            let ring_commitment = if coinbase_inputs {
                public_amount_commitment(public_amount)
            } else {
                commitment
            };
            ring.push([address, ring_commitment]);
            members.push(stored);
        }
        ring_members.push(members);
        inputs.push(SpendInput {
            secret_key: secret,
            mask,
            amount,
            ring_indexes: indexes,
            ring,
            signer,
        });
    }
    // outputs, and the key images sorted ascending as the validator requires
    let mut out_pairs = Vec::new();
    for &a in output_amounts {
        let m = rand_scalar(&mut rng);
        out_pairs.push(out(a, &m, &mut rng));
    }
    let (outs, outputs): (Vec<Output>, Vec<OutputSecret>) = out_pairs.into_iter().unzip();
    let prefix = TxPrefix {
        version: 2,
        inputs: inputs
            .iter()
            .map(|i| Input {
                key_image: key_image(&i.secret_key).unwrap(),
            })
            .collect(),
        outputs: outs,
        fee,
        extra: vec![9, 9],
    };
    let prunable = prove(&mut rng, &CHAIN, &prefix, &inputs, &outputs).expect("proving");
    Fixture {
        tx: Transaction { prefix, prunable },
        ring_members,
        inputs,
        outputs,
    }
}

trait TestRng {
    fn next_u64_for_test(&mut self) -> u64;
}
impl TestRng for ChaCha20Rng {
    fn next_u64_for_test(&mut self) -> u64 {
        rand_chacha::rand_core::RngCore::next_u64(self)
    }
}

fn verify(f: &Fixture) -> Result<(), ProofError> {
    verify_tx(&CHAIN, &f.tx, &f.ring_members)
}

fn standard() -> Fixture {
    build(1, &[500, 300], &[400, 390], 16, false)
}

#[test]
fn a_transaction_with_real_proofs_verifies() {
    assert_eq!(verify(&standard()), Ok(()));
    assert_eq!(verify(&build(2, &[1_000], &[600, 399], 16, false)), Ok(()));
}

#[test]
fn a_zero_fee_and_a_zero_amount_output_are_fine() {
    assert_eq!(verify(&build(3, &[100], &[100, 0], 16, false)), Ok(()));
}

#[test]
fn the_real_proof_size_of_a_typical_transaction_is_within_the_limit() {
    let f = standard();
    let n = f.tx.prunable.proof_data.len();
    // 2 pseudo-outputs (64) + a 2-output Bulletproofs+ proof + 2 CLSAGs of 16 members (576 each)
    assert!(n > 1500 && n < 2500, "proof_data is {n} bytes");
    eprintln!("typical proof_data: {n} bytes");
    assert!(n <= MAX_PROOF);
}

#[test]
fn the_largest_allowed_transaction_fits_in_max_proof_and_verifies() {
    // 32 inputs, 16 outputs, all rings of 16: the limits of CONSENSUS_V2 section 6.2
    let inputs = vec![100u64; 32];
    let mut outputs = vec![200u64; 15];
    outputs.push(3200 - 15 * 200 - 7);
    let f = build(4, &inputs, &outputs, 16, false);
    let n = f.tx.prunable.proof_data.len();
    assert!(
        n <= MAX_PROOF,
        "the biggest proof_data is {n} bytes, limit {MAX_PROOF}"
    );
    assert_eq!(verify(&f), Ok(()));
    eprintln!("largest proof_data: {n} bytes of {MAX_PROOF}");
}

#[test]
fn a_transaction_survives_the_wire_and_keeps_its_id() {
    let f = standard();
    let bytes = f.tx.to_bytes().unwrap();
    let back = Transaction::from_bytes(&bytes).unwrap();
    assert_eq!(back, f.tx);
    assert_eq!(tx_id(&back).unwrap(), tx_id(&f.tx).unwrap());
    assert_eq!(verify_tx(&CHAIN, &back, &f.ring_members), Ok(()));
}

// ---- one thing broken at a time --------------------------------------------------------------

#[test]
fn another_chain_id_breaks_the_signatures() {
    let f = standard();
    assert_eq!(
        verify_tx(&[8; 32], &f.tx, &f.ring_members),
        Err(ProofError::Clsag(0))
    );
}

#[test]
fn a_changed_fee_breaks_the_balance() {
    let mut f = standard();
    f.tx.prefix.fee += 1;
    assert_eq!(verify(&f), Err(ProofError::Balance));
    f.tx.prefix.fee -= 2;
    assert_eq!(verify(&f), Err(ProofError::Balance));
}

#[test]
fn a_changed_extra_breaks_the_signatures() {
    let mut f = standard();
    f.tx.prefix.extra = vec![9, 8];
    assert_eq!(verify(&f), Err(ProofError::Clsag(0)));
}

#[test]
fn a_changed_ephemeral_key_or_view_tag_breaks_the_signatures() {
    let mut f = standard();
    f.tx.prefix.outputs[0].view_tag = [0, 0, 0];
    assert_eq!(verify(&f), Err(ProofError::Clsag(0)));
    let mut f = standard();
    f.tx.prefix.outputs[1].anchor_enc[0] ^= 1;
    assert_eq!(verify(&f), Err(ProofError::Clsag(0)));
}

#[test]
fn a_changed_ring_index_breaks_the_signatures() {
    let mut f = standard();
    f.tx.prunable.rings[0][4] += 1;
    assert_eq!(verify(&f), Err(ProofError::Clsag(0)));
}

#[test]
fn a_swapped_key_image_is_refused() {
    let mut f = standard();
    // another valid image, of a key that is not the signer's
    let other = key_image(&rand_scalar(&mut ChaCha20Rng::seed_from_u64(99))).unwrap();
    f.tx.prefix.inputs[0].key_image = other;
    assert_eq!(verify(&f), Err(ProofError::Clsag(0)));
}

#[test]
fn moving_a_key_image_to_the_other_input_is_refused() {
    let mut f = standard();
    let (a, b) = (
        f.tx.prefix.inputs[0].key_image,
        f.tx.prefix.inputs[1].key_image,
    );
    f.tx.prefix.inputs[0].key_image = b;
    f.tx.prefix.inputs[1].key_image = a;
    assert_eq!(verify(&f), Err(ProofError::Clsag(0)));
}

#[test]
fn a_wrong_ring_member_on_the_chain_breaks_the_signature() {
    let mut f = standard();
    // the chain says decoy 2 of input 1 is another output than the signature was made for
    f.ring_members[1][2].onetime_address = public_key(&[5; 32]).unwrap();
    assert_eq!(verify(&f), Err(ProofError::Clsag(1)));
    let mut f = standard();
    f.ring_members[0][7].amount_commitment = commit(&[6; 32], 77).unwrap();
    assert_eq!(verify(&f), Err(ProofError::Clsag(0)));
}

#[test]
fn a_ring_shorter_than_its_lookup_is_a_shape_error() {
    let mut f = standard();
    f.ring_members[0].pop();
    assert_eq!(verify(&f), Err(ProofError::RingShape));
    let mut f = standard();
    f.ring_members.pop();
    assert_eq!(verify(&f), Err(ProofError::RingShape));
}

#[test]
fn a_changed_pseudo_output_breaks_the_balance() {
    let mut f = standard();
    // replace pseudo-output 0 by another prime-order point
    let other = public_key(&[3; 32]).unwrap();
    f.tx.prunable.proof_data[..32].copy_from_slice(&other);
    assert_eq!(verify(&f), Err(ProofError::Balance));
}

#[test]
fn a_pseudo_output_with_a_torsion_component_is_refused() {
    let mut f = standard();
    let p = CompressedPoint::from(<[u8; 32]>::try_from(&f.tx.prunable.proof_data[..32]).unwrap())
        .decompress()
        .unwrap()
        .into();
    let bad: EdwardsPoint = p + EIGHT_TORSION[1];
    f.tx.prunable.proof_data[..32].copy_from_slice(&bad.compress().to_bytes());
    assert_eq!(verify(&f), Err(ProofError::BadPseudoOut(0)));
}

#[test]
fn shifting_value_between_outputs_passes_the_balance_but_not_the_range_proof() {
    // C1 + H and C2 - H: the sum is unchanged, so the balance holds, but the range proof was made for
    // the old commitments
    let mut f = standard();
    let h: EdwardsPoint = CompressedPoint::H.decompress().unwrap().into();
    let bump = |bytes: &[u8; 32], up: bool| -> [u8; 32] {
        let p: EdwardsPoint = CompressedPoint::from(*bytes).decompress().unwrap().into();
        let q = if up { p + h } else { p - h };
        q.compress().to_bytes()
    };
    let c0 = f.tx.prefix.outputs[0].amount_commitment;
    let c1 = f.tx.prefix.outputs[1].amount_commitment;
    f.tx.prefix.outputs[0].amount_commitment = bump(&c0, true);
    f.tx.prefix.outputs[1].amount_commitment = bump(&c1, false);
    assert_eq!(verify(&f), Err(ProofError::RangeProof));
}

#[test]
fn a_range_proof_for_other_outputs_is_refused() {
    // splice the range proof of a different transaction (same size) into this one
    let a = standard();
    let b = build(11, &[500, 300], &[400, 390], 16, false);
    let bp_a = tenero_crypto::ringct::parse(&a.tx.prunable.proof_data, &[16, 16]).unwrap();
    let bp_b = tenero_crypto::ringct::parse(&b.tx.prunable.proof_data, &[16, 16]).unwrap();
    assert_eq!(bp_a.range_proof_bytes.len(), bp_b.range_proof_bytes.len());
    let mut f = standard();
    let start = 64;
    f.tx.prunable.proof_data[start..start + bp_a.range_proof_bytes.len()]
        .copy_from_slice(&bp_b.range_proof_bytes);
    assert_eq!(verify(&f), Err(ProofError::RangeProof));
}

#[test]
fn flipping_any_byte_of_proof_data_is_refused() {
    // a coarse sweep: one byte in every 37, so every part of the layout is hit
    let f = standard();
    let n = f.tx.prunable.proof_data.len();
    let mut checked = 0;
    for pos in (0..n).step_by(37) {
        let mut g = standard();
        g.tx.prunable.proof_data[pos] ^= 0x01;
        assert!(
            verify(&g).is_err(),
            "a flip at byte {pos} of {n} was accepted"
        );
        checked += 1;
    }
    assert!(checked > 40);
}

#[test]
fn trailing_or_missing_bytes_are_malformed() {
    let mut f = standard();
    f.tx.prunable.proof_data.push(0);
    assert!(matches!(verify(&f), Err(ProofError::Malformed(_))));
    let mut f = standard();
    f.tx.prunable.proof_data.pop();
    assert!(matches!(verify(&f), Err(ProofError::Malformed(_))));
    let mut f = standard();
    f.tx.prunable.proof_data.truncate(20);
    assert!(matches!(verify(&f), Err(ProofError::Malformed(_))));
    let mut f = standard();
    f.tx.prunable.proof_data.clear();
    assert!(matches!(verify(&f), Err(ProofError::Malformed(_))));
}

#[test]
fn proof_data_made_for_another_input_count_is_malformed() {
    let one = build(5, &[800], &[400, 390], 16, false);
    let mut f = standard();
    f.tx.prunable = Prunable {
        rings: f.tx.prunable.rings.clone(),
        proof_data: one.tx.prunable.proof_data.clone(),
    };
    assert!(verify(&f).is_err());
}

#[test]
fn an_identity_or_torsioned_or_noncanonical_output_point_is_refused() {
    // the one-time address
    let mut f = standard();
    f.tx.prefix.outputs[0].onetime_address = CompressedPoint::IDENTITY.to_bytes();
    assert_eq!(verify(&f), Err(ProofError::BadOutputPoint(0)));

    let mut f = standard();
    let p: EdwardsPoint = CompressedPoint::from(f.tx.prefix.outputs[1].onetime_address)
        .decompress()
        .unwrap()
        .into();
    f.tx.prefix.outputs[1].onetime_address = (p + EIGHT_TORSION[1]).compress().to_bytes();
    assert_eq!(verify(&f), Err(ProofError::BadOutputPoint(1)));

    // y = p (2^255 - 19) is not a canonical encoding of anything
    let mut f = standard();
    let mut y = [0xffu8; 32];
    y[0] = 0xed;
    y[31] = 0x7f;
    f.tx.prefix.outputs[0].onetime_address = y;
    assert_eq!(verify(&f), Err(ProofError::BadOutputPoint(0)));

    // the commitment
    let mut f = standard();
    f.tx.prefix.outputs[0].amount_commitment = CompressedPoint::IDENTITY.to_bytes();
    assert_eq!(verify(&f), Err(ProofError::BadOutputPoint(0)));
    let mut f = standard();
    let c: EdwardsPoint = CompressedPoint::from(f.tx.prefix.outputs[0].amount_commitment)
        .decompress()
        .unwrap()
        .into();
    f.tx.prefix.outputs[0].amount_commitment = (c + EIGHT_TORSION[1]).compress().to_bytes();
    assert_eq!(verify(&f), Err(ProofError::BadOutputPoint(0)));
}

// ---- coinbase outputs as ring members --------------------------------------------------------

#[test]
fn a_coinbase_output_can_be_spent_with_its_public_amount_commitment() {
    let f = build(6, &[700], &[400, 290], 16, true);
    assert!(f.ring_members[0].iter().all(|o| o.coinbase));
    assert_eq!(verify(&f), Ok(()));
}

#[test]
fn a_wrong_public_amount_for_a_coinbase_ring_member_breaks_the_signature() {
    let mut f = build(6, &[700], &[400, 290], 16, true);
    let signer = f.inputs[0].signer;
    f.ring_members[0][signer].public_amount += 1;
    assert_eq!(verify(&f), Err(ProofError::Clsag(0)));
}

// ---- the hook --------------------------------------------------------------------------------

#[test]
fn the_validator_hook_says_it_checks_and_rejects_a_bad_transaction() {
    let f = standard();
    let ctx = TxContext {
        chain_id: CHAIN,
        height: 100,
        tx: &f.tx,
        ring_members: f.ring_members.clone(),
    };
    assert!(RingCtProofs.checks_proofs());
    assert_eq!(RingCtProofs.check_tx(&ctx), Ok(()));

    let mut bad = f.tx.clone();
    bad.prefix.fee += 1;
    let ctx = TxContext {
        chain_id: CHAIN,
        height: 100,
        tx: &bad,
        ring_members: f.ring_members.clone(),
    };
    assert!(RingCtProofs.check_tx(&ctx).unwrap_err().contains("balance"));
}

#[test]
fn the_prover_refuses_a_non_canonical_secret() {
    let f = standard();
    let mut rng = ChaCha20Rng::seed_from_u64(1);
    let mut inputs: Vec<SpendInput> = f
        .inputs
        .iter()
        .map(|i| SpendInput {
            secret_key: i.secret_key,
            mask: i.mask,
            amount: i.amount,
            ring_indexes: i.ring_indexes.clone(),
            ring: i.ring.clone(),
            signer: i.signer,
        })
        .collect();
    inputs[0].secret_key = [0xff; 32];
    assert!(prove(&mut rng, &CHAIN, &f.tx.prefix, &inputs, &f.outputs).is_none());
}

#[test]
fn keys_and_images_are_what_the_math_says() {
    let x = rand_scalar(&mut ChaCha20Rng::seed_from_u64(3));
    let d = DScalar::from_canonical_bytes(x).unwrap();
    assert_eq!(
        public_key(&x).unwrap(),
        (ED25519_BASEPOINT_POINT * d).compress().to_bytes()
    );
    assert_ne!(key_image(&x).unwrap(), public_key(&x).unwrap());
}

#[test]
fn another_valid_range_proof_for_the_same_outputs_breaks_the_signatures() {
    // The range proof is valid and the balance holds, but the signatures covered the OTHER proof's bytes:
    // so the proof is bound into the message, not only checked on its own.
    let f = standard();
    let masks: Vec<Commitment> = {
        // the outputs' secrets are in `f.outputs`
        f.outputs
            .iter()
            .map(|o| {
                Commitment::new(
                    Scalar::from(DScalar::from_canonical_bytes(o.mask).unwrap()),
                    o.amount,
                )
            })
            .collect()
    };
    let mut rng = ChaCha20Rng::seed_from_u64(1234);
    let second = Bulletproof::prove_plus(&mut rng, masks)
        .unwrap()
        .serialize();
    let first = tenero_crypto::ringct::parse(&f.tx.prunable.proof_data, &[16, 16]).unwrap();
    assert_eq!(second.len(), first.range_proof_bytes.len());
    assert_ne!(second, first.range_proof_bytes);
    let mut g = standard();
    g.tx.prunable.proof_data[64..64 + second.len()].copy_from_slice(&second);
    assert_eq!(verify(&g), Err(ProofError::Clsag(0)));
}

#[test]
fn a_range_proof_with_a_padded_length_prefix_is_not_accepted() {
    // A non-minimal varint (0x87 0x00 for 7) is the same proof in different bytes: a malleability that
    // would change the transaction id. The 2-output proof has 7 rounds; its length prefix sits after the
    // 6 fixed 32-byte fields.
    let f = standard();
    let parsed = tenero_crypto::ringct::parse(&f.tx.prunable.proof_data, &[16, 16]).unwrap();
    assert_eq!(parsed.range_proof_bytes[192], 7);
    let mut data = f.tx.prunable.proof_data.clone();
    data[64 + 192] = 0x87;
    data.insert(64 + 193, 0x00);
    assert!(matches!(
        tenero_crypto::ringct::parse(&data, &[16, 16]),
        Err(ProofError::Malformed(_))
    ));
}

#[test]
fn an_empty_ring_is_a_shape_error() {
    let mut f = standard();
    f.tx.prunable.rings = vec![vec![], vec![]];
    f.ring_members = vec![vec![], vec![]];
    assert_eq!(verify(&f), Err(ProofError::RingShape));
}
