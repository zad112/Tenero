//! The proofs of a version 2 transaction, and their verification.
//!
//! # What `proof_data` holds (exactly this, nothing after it)
//!
//! ```text
//! pseudo_outs   n_inputs * 32 bytes   Cp_i: a re-randomised commitment to input i's amount
//! range_proof   a Bulletproofs+ proof, Monero's standard encoding (varint L and R lengths)
//! clsags        n_inputs * (s: ring_len * 32, c1: 32, D: 32)   one per input, in input order
//! ```
//!
//! The Bulletproofs+ proof covers every output's `amount_commitment`, in output order, as it stands in
//! the prefix (not multiplied by 1/8; the library does that itself).
//!
//! # What the signatures cover
//!
//! CLSAG signs a 32-byte message, and hashes into its own challenge the ring, the key image and the
//! pseudo-output, so those need no place in the message. The message is
//!
//! ```text
//! SHA-256( "tenero ringct message v2" || chain_id || prefix || rings || range proof bytes )
//! ```
//!
//! with `prefix` and `rings` in their wire form: so the signatures bind the chain, every output, the fee,
//! `extra`, the key images, the ring indexes and the range proof. **This message is ours: the libraries'
//! audit says explicitly that what is bound in it is the integrator's responsibility.**
//!
//! # The balance
//!
//! `sum(Cp_i) - sum(Ca_j) - fee * H == 0`. Every ring member's commitment is its `amount_commitment`,
//! except a coinbase output's: its amount is public, so its commitment is the fixed `1*G + amount*H`
//! (Monero's "zero commitment", with mask 1). **Provisional: the Carrot specification will define the
//! commitment for a public amount, and this must match it when Carrot is added.**

use std::io::Cursor;

use curve25519_dalek::{edwards::EdwardsPoint, scalar::Scalar as DScalar, traits::IsIdentity};
use monero_bulletproofs::Bulletproof;
use monero_clsag::{Clsag, ClsagContext, Decoys};
use monero_ed25519::{Commitment, CompressedPoint, Point, Scalar};
use rand_core::{CryptoRng, OsRng, RngCore};
use tenero_chain::proofs::{ProofCheck, TxContext};
use tenero_core::hash::sha256;
use tenero_core::v2::codec::{Wire, Writer};
use tenero_core::v2::{Prunable, Transaction, TxPrefix};
use tenero_store::StoredOutput;
use zeroize::Zeroizing;

const MESSAGE_TAG: &[u8] = b"tenero ringct message v2";

#[derive(Debug, PartialEq, Eq)]
pub enum ProofError {
    /// `proof_data` is not exactly the layout above.
    Malformed(&'static str),
    /// The number of rings, or a ring's length, does not match the transaction or the chain lookup.
    RingShape,
    /// An output's one-time address or commitment is not a canonical, prime-order, non-identity point.
    BadOutputPoint(usize),
    /// A pseudo-output is not a canonical, prime-order point.
    BadPseudoOut(usize),
    /// `sum(pseudo_outs) != sum(output commitments) + fee * H`.
    Balance,
    /// The range proof does not verify.
    RangeProof,
    /// The CLSAG of this input does not verify.
    Clsag(usize),
}

impl std::fmt::Display for ProofError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProofError::Malformed(why) => write!(f, "proof data is malformed: {why}"),
            ProofError::RingShape => write!(f, "the rings do not match the transaction"),
            ProofError::BadOutputPoint(i) => write!(f, "output {i} has an invalid point"),
            ProofError::BadPseudoOut(i) => write!(f, "pseudo-output {i} is invalid"),
            ProofError::Balance => write!(f, "inputs and outputs do not balance"),
            ProofError::RangeProof => write!(f, "the range proof does not verify"),
            ProofError::Clsag(i) => write!(f, "the ring signature of input {i} does not verify"),
        }
    }
}

impl std::error::Error for ProofError {}

/// A point that must be canonical, of prime order and not the identity.
fn strict_point(bytes: &[u8; 32]) -> Option<EdwardsPoint> {
    let p: EdwardsPoint = CompressedPoint::from(*bytes).decompress()?.into();
    (p.is_torsion_free() && !p.is_identity()).then_some(p)
}

/// The commitment of a ring member: its own, or the fixed public-amount commitment for a coinbase output.
fn ring_commitment(o: &StoredOutput) -> [u8; 32] {
    if o.coinbase {
        public_amount_commitment(o.public_amount)
    } else {
        o.amount_commitment
    }
}

/// `1*G + amount*H`. **Provisional** until the Carrot specification's value is imported.
pub fn public_amount_commitment(amount: u64) -> [u8; 32] {
    Commitment::new(Scalar::ONE, amount)
        .commit()
        .compress()
        .to_bytes()
}

/// The message the signatures cover (module docs).
pub fn message(
    chain_id: &[u8; 32],
    prefix: &TxPrefix,
    rings: &[Vec<u64>],
    range_proof: &[u8],
) -> Result<[u8; 32], ProofError> {
    let mut w = Writer::new();
    w.raw(MESSAGE_TAG);
    w.raw(chain_id);
    prefix
        .write(&mut w)
        .map_err(|_| ProofError::Malformed("prefix"))?;
    // the rings in their wire form, with the range proof standing in for the proof bytes
    let rings_and_bp = Prunable {
        rings: rings.to_vec(),
        proof_data: range_proof.to_vec(),
    };
    rings_and_bp
        .write(&mut w, rings.len())
        .map_err(|_| ProofError::Malformed("rings"))?;
    Ok(sha256(&[&w.into_bytes()]))
}

/// `proof_data`, parsed strictly.
pub struct ParsedProofs {
    pub pseudo_outs: Vec<[u8; 32]>,
    pub range_proof: Bulletproof,
    pub range_proof_bytes: Vec<u8>,
    pub clsags: Vec<Clsag>,
}

/// Parses `proof_data` for a transaction whose input i has a ring of `ring_lens[i]` members. Refuses
/// anything that is not exactly the layout: short, trailing bytes, or a range proof that would not
/// re-encode to the same bytes.
pub fn parse(data: &[u8], ring_lens: &[usize]) -> Result<ParsedProofs, ProofError> {
    let mut pseudo_outs = Vec::with_capacity(ring_lens.len());
    for i in 0..ring_lens.len() {
        let bytes = data
            .get(i * 32..i * 32 + 32)
            .ok_or(ProofError::Malformed("short pseudo-outputs"))?;
        pseudo_outs.push(<[u8; 32]>::try_from(bytes).expect("32 bytes"));
    }
    let bp_start = ring_lens.len() * 32;
    let mut cur = Cursor::new(&data[bp_start..]);
    let range_proof = Bulletproof::read_plus(&mut cur)
        .map_err(|_| ProofError::Malformed("range proof does not decode"))?;
    let bp_end = bp_start + cur.position() as usize;
    let range_proof_bytes = data[bp_start..bp_end].to_vec();
    // Belt and braces: today the library already refuses a padded length prefix (a test shows it), so
    // mutation testing cannot reach this line. It stays so that a library update that relaxed the decoder
    // could not make a transaction's id malleable.
    if range_proof.serialize() != range_proof_bytes {
        return Err(ProofError::Malformed("range proof is not canonical"));
    }
    let mut cur = Cursor::new(&data[bp_end..]);
    let mut clsags = Vec::with_capacity(ring_lens.len());
    for &len in ring_lens {
        clsags.push(Clsag::read(len, &mut cur).map_err(|_| ProofError::Malformed("signature"))?);
    }
    if bp_end + cur.position() as usize != data.len() {
        return Err(ProofError::Malformed("trailing bytes"));
    }
    Ok(ParsedProofs {
        pseudo_outs,
        range_proof,
        range_proof_bytes,
        clsags,
    })
}

/// Verifies every proof of one transaction. `ring_members[i]` are the chain's outputs for input i's ring,
/// in the ring's order.
pub fn verify_tx(
    chain_id: &[u8; 32],
    tx: &Transaction,
    ring_members: &[Vec<StoredOutput>],
) -> Result<(), ProofError> {
    let n_in = tx.prefix.inputs.len();
    if tx.prunable.rings.len() != n_in || ring_members.len() != n_in {
        return Err(ProofError::RingShape);
    }
    let mut ring_lens = Vec::with_capacity(n_in);
    for (ring, members) in tx.prunable.rings.iter().zip(ring_members) {
        if ring.len() != members.len() || ring.is_empty() {
            return Err(ProofError::RingShape);
        }
        ring_lens.push(ring.len());
    }
    let proofs = parse(&tx.prunable.proof_data, &ring_lens)?;

    // Outputs: keys and commitments must be valid prime-order points (CONSENSUS_V2 section 7.1).
    let mut out_commitments = Vec::with_capacity(tx.prefix.outputs.len());
    for (j, o) in tx.prefix.outputs.iter().enumerate() {
        if strict_point(&o.onetime_address).is_none() {
            return Err(ProofError::BadOutputPoint(j));
        }
        let c = strict_point(&o.amount_commitment).ok_or(ProofError::BadOutputPoint(j))?;
        out_commitments.push(c);
    }
    let mut pseudo = Vec::with_capacity(n_in);
    for (i, p) in proofs.pseudo_outs.iter().enumerate() {
        pseudo.push(strict_point(p).ok_or(ProofError::BadPseudoOut(i))?);
    }

    // Balance: sum(pseudo) - sum(outputs) - fee*H == identity.
    let h: EdwardsPoint = CompressedPoint::H
        .decompress()
        .expect("H is a valid point")
        .into();
    let sum_in: EdwardsPoint = pseudo.iter().sum();
    let sum_out: EdwardsPoint = out_commitments.iter().sum();
    if !(sum_in - sum_out - h * DScalar::from(tx.prefix.fee)).is_identity() {
        return Err(ProofError::Balance);
    }

    // Range proof over the output commitments.
    let commitments: Vec<CompressedPoint> = tx
        .prefix
        .outputs
        .iter()
        .map(|o| CompressedPoint::from(o.amount_commitment))
        .collect();
    if !proofs.range_proof.verify(&mut OsRng, &commitments) {
        return Err(ProofError::RangeProof);
    }

    // One CLSAG per input, over the message.
    let msg = message(
        chain_id,
        &tx.prefix,
        &tx.prunable.rings,
        &proofs.range_proof_bytes,
    )?;
    for (i, members) in ring_members.iter().enumerate() {
        let ring: Vec<[CompressedPoint; 2]> = members
            .iter()
            .map(|o| {
                [
                    CompressedPoint::from(o.onetime_address),
                    CompressedPoint::from(ring_commitment(o)),
                ]
            })
            .collect();
        let image = CompressedPoint::from(tx.prefix.inputs[i].key_image);
        let pseudo_out = CompressedPoint::from(proofs.pseudo_outs[i]);
        proofs.clsags[i]
            .verify(ring, &image, &pseudo_out, &msg)
            .map_err(|_| ProofError::Clsag(i))?;
    }
    Ok(())
}

/// The block validator's proof check: real CLSAG, Bulletproofs+ and balance verification.
pub struct RingCtProofs;

impl ProofCheck for RingCtProofs {
    fn check_tx(&self, ctx: &TxContext<'_>) -> Result<(), String> {
        verify_tx(&ctx.chain_id, ctx.tx, &ctx.ring_members).map_err(|e| e.to_string())
    }

    fn checks_proofs(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------------------------
// The prover: what a wallet does. Used by the tests here and by the wallet later.
// ---------------------------------------------------------------------------------------------

/// An input to spend: the secrets of the output being spent, and the ring it hides in.
pub struct SpendInput {
    /// The discrete log of the output's one-time address (`address = secret * G`).
    pub secret_key: [u8; 32],
    /// The mask of the output's commitment (`commitment = mask*G + amount*H`).
    pub mask: [u8; 32],
    pub amount: u64,
    /// The ring's global output indexes, ascending.
    pub ring_indexes: Vec<u64>,
    /// For each ring member `[one-time address, commitment]`, in the same order.
    pub ring: Vec<[[u8; 32]; 2]>,
    /// The position of the output being spent within the ring.
    pub signer: usize,
}

/// An output being created: its amount and the mask of its commitment.
pub struct OutputSecret {
    pub amount: u64,
    pub mask: [u8; 32],
}

fn canonical_scalar(bytes: &[u8; 32]) -> Option<DScalar> {
    Option::<DScalar>::from(DScalar::from_canonical_bytes(*bytes))
}

/// The commitment `mask*G + amount*H` as bytes.
pub fn commit(mask: &[u8; 32], amount: u64) -> Option<[u8; 32]> {
    let mask = canonical_scalar(mask)?;
    Some(
        Commitment::new(Scalar::from(mask), amount)
            .commit()
            .compress()
            .to_bytes(),
    )
}

/// The one-time address `secret * G` as bytes.
pub fn public_key(secret_key: &[u8; 32]) -> Option<[u8; 32]> {
    let x = canonical_scalar(secret_key)?;
    Some(
        Point::from(curve25519_dalek::constants::ED25519_BASEPOINT_POINT * x)
            .compress()
            .to_bytes(),
    )
}

/// The key image of an output: `secret * Hp(address)`.
pub fn key_image(secret_key: &[u8; 32]) -> Option<[u8; 32]> {
    let x = canonical_scalar(secret_key)?;
    let address = public_key(secret_key)?;
    let image = Point::biased_hash(address).into() * x;
    Some(Point::from(image).compress().to_bytes())
}

/// Builds the prunable part (rings and `proof_data`) of a transaction whose prefix is already fixed: the
/// prefix's key images and output commitments must be those of `inputs` and `outputs`, or the result will
/// not verify. Returns `None` if a secret is not a canonical scalar or a ring is malformed.
pub fn prove(
    rng: &mut (impl RngCore + CryptoRng),
    chain_id: &[u8; 32],
    prefix: &TxPrefix,
    inputs: &[SpendInput],
    outputs: &[OutputSecret],
) -> Option<Prunable> {
    // 1. the range proof over the outputs
    let mut out_commitments = Vec::new();
    let mut sum_masks = DScalar::ZERO;
    for o in outputs {
        let mask = canonical_scalar(&o.mask)?;
        sum_masks += mask;
        out_commitments.push(Commitment::new(Scalar::from(mask), o.amount));
    }
    let bp = Bulletproof::prove_plus(rng, out_commitments).ok()?;
    let bp_bytes = bp.serialize();

    // 2. the message, then one CLSAG per input
    let rings: Vec<Vec<u64>> = inputs.iter().map(|i| i.ring_indexes.clone()).collect();
    let msg = message(chain_id, prefix, &rings, &bp_bytes).ok()?;
    let mut contexts = Vec::new();
    for i in inputs {
        let mut offsets = Vec::new();
        let mut previous = 0u64;
        for &index in &i.ring_indexes {
            offsets.push(index.checked_sub(previous)?);
            previous = index;
        }
        let ring: Vec<[Point; 2]> = i
            .ring
            .iter()
            .map(|m| {
                Some([
                    CompressedPoint::from(m[0]).decompress()?,
                    CompressedPoint::from(m[1]).decompress()?,
                ])
            })
            .collect::<Option<_>>()?;
        let decoys = Decoys::new(offsets, u8::try_from(i.signer).ok()?, ring)?;
        let mask = canonical_scalar(&i.mask)?;
        let context =
            ClsagContext::new(decoys, Commitment::new(Scalar::from(mask), i.amount)).ok()?;
        let key = canonical_scalar(&i.secret_key)?;
        contexts.push((Zeroizing::new(Scalar::from(key)), context));
    }
    let signed = Clsag::sign(rng, contexts, Scalar::from(sum_masks), msg).ok()?;

    // 3. lay it out
    let mut proof_data = Vec::new();
    for (_, pseudo_out) in &signed {
        proof_data.extend_from_slice(&pseudo_out.compress().to_bytes());
    }
    proof_data.extend_from_slice(&bp_bytes);
    for (clsag, _) in &signed {
        clsag.write(&mut proof_data).ok()?;
    }
    Some(Prunable { rings, proof_data })
}
