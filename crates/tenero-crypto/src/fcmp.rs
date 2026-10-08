//! The proofs of a version 3 (`gamma`) transaction (`docs/CONSENSUS_V2.md` 15.7): checking them, a block's at once,
//! and making them (for the wallet and the tests).
//!
//! ```text
//! proof_data = pseudo_outs (n_inputs * 32) | Bulletproofs+ (Monero's encoding) | FCMP++ (n_inputs, the tree's layers)
//! ```
//!
//! A pseudo-output is the re-randomised commitment `C~ = C + r_c G` of the output being spent, so the balance
//! `sum(C~) = sum(output commitments) + fee * H` holds when the prover picks the last input's `r_c` to make it hold. The
//! FCMP++ spend-authorisation proofs sign `tenero_core::v3::ids::proof_message`. The FCMP++ crates are **only partly
//! audited**; the layout, the message and the checks around them are ours and **unaudited**.

use std::collections::{HashSet, VecDeque};
use std::io::Cursor;
use std::sync::Mutex;

use ciphersuite::group::ff::{Field, PrimeField};
use ciphersuite::group::{Group, GroupEncoding};
use ciphersuite::Ciphersuite;
use curve25519_dalek::edwards::EdwardsPoint as DPoint;
use curve25519_dalek::scalar::Scalar as DScalar;
use curve25519_dalek::traits::IsIdentity;
use dalek_ff_group::{Ed25519, EdwardsPoint, Scalar};
use ec_divisors::ScalarDecomposition;
use helioselene::{Helios, Selene};
use monero_bulletproofs::Bulletproof;
use monero_ed25519::{Commitment, CompressedPoint};
use monero_fcmp_plus_plus::fcmps::{
    BranchBlind, Branches, CBlind, Fcmp, IBlind, IBlindBlind, OBlind, OutputBlinds, Path, TreeRoot,
};
use monero_fcmp_plus_plus::sal::{OpenedInputTuple, RerandomizedOutput, SpendAuthAndLinkability};
use monero_fcmp_plus_plus::{
    Curves, FcmpPlusPlus, FCMP_PARAMS, HELIOS_FCMP_GENERATORS, SELENE_FCMP_GENERATORS,
};
use monero_fcmp_plus_plus_generators::{FCMP_PLUS_PLUS_U, FCMP_PLUS_PLUS_V};
use rand_core::{CryptoRng, OsRng, RngCore};
use tenero_chain::proofs::{ProofCheck, TxContext};
use tenero_core::v3::ids::proof_message;
use tenero_core::v3::Transaction;
use tenero_store::TreeState;

use crate::curve_tree::Leaf;

/// Why a version 3 transaction's proofs are refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProofError {
    /// `proof_data` is not exactly the layout above.
    Malformed(&'static str),
    BadOutputPoint(usize),
    BadKeyImage(usize),
    BadPseudoOut(usize),
    /// `sum(pseudo_outs) != sum(output commitments) + fee * H`.
    Balance,
    RangeProof,
    /// The FCMP++ proof (spend authorisation or membership) does not verify.
    Membership,
}

impl std::fmt::Display for ProofError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProofError::Malformed(why) => write!(f, "proof data is malformed: {why}"),
            ProofError::BadOutputPoint(i) => write!(f, "output {i} has an invalid point"),
            ProofError::BadKeyImage(i) => {
                write!(f, "key image {i} is not a valid prime-order point")
            }
            ProofError::BadPseudoOut(i) => write!(f, "pseudo-output {i} is invalid"),
            ProofError::Balance => write!(f, "inputs and outputs do not balance"),
            ProofError::RangeProof => write!(f, "the range proof does not verify"),
            ProofError::Membership => write!(f, "the FCMP++ proof does not verify"),
        }
    }
}

impl std::error::Error for ProofError {}

pub use crate::curve_tree::strict_point;

fn h() -> DPoint {
    CompressedPoint::H
        .decompress()
        .expect("H is a point")
        .into()
}

/// `proof_data`, parsed strictly.
pub struct Parsed {
    pub pseudo_outs: Vec<[u8; 32]>,
    pub range_proof: Bulletproof,
    pub range_proof_bytes: Vec<u8>,
    pub fcmp: FcmpPlusPlus,
}

/// Parses `proof_data` for `n_inputs` inputs and a tree of `layers` layers: nothing short, nothing after, and a range
/// proof that re-encodes to its own bytes.
pub fn parse(data: &[u8], n_inputs: usize, layers: usize) -> Result<Parsed, ProofError> {
    let mut pseudo_outs = Vec::with_capacity(n_inputs);
    for i in 0..n_inputs {
        let b = data
            .get(i * 32..i * 32 + 32)
            .ok_or(ProofError::Malformed("short pseudo-outputs"))?;
        pseudo_outs.push(<[u8; 32]>::try_from(b).expect("32 bytes"));
    }
    let bp_start = n_inputs * 32;
    let mut cur = Cursor::new(&data[bp_start..]);
    let range_proof = Bulletproof::read_plus(&mut cur)
        .map_err(|_| ProofError::Malformed("range proof does not decode"))?;
    let bp_end = bp_start + cur.position() as usize;
    let range_proof_bytes = data[bp_start..bp_end].to_vec();
    if range_proof.serialize() != range_proof_bytes {
        return Err(ProofError::Malformed("range proof is not canonical"));
    }
    let rest = &data[bp_end..];
    if rest.len() != FcmpPlusPlus::proof_size(n_inputs, layers) {
        return Err(ProofError::Malformed(
            "the FCMP++ proof is not its exact size",
        ));
    }
    let fcmp = FcmpPlusPlus::read(&pseudo_outs, layers, &mut &rest[..])
        .map_err(|_| ProofError::Malformed("the FCMP++ proof does not decode"))?;
    Ok(Parsed {
        pseudo_outs,
        range_proof,
        range_proof_bytes,
        fcmp,
    })
}

/// Checks the proofs of one or more transactions as one batch, as a node checks a block. Each [`Batch::add`] checks
/// everything but the FCMP++ batch equations and queues those; [`Batch::finish`] checks them all.
pub struct Batch {
    ed: multiexp::BatchVerifier<(), <Ed25519 as Ciphersuite>::G>,
    c1: generalized_bulletproofs::BatchVerifier<Selene>,
    c2: generalized_bulletproofs::BatchVerifier<Helios>,
    /// An FCMP++ `verify` that returned an error leaves the verifiers corrupt: the whole batch then fails.
    poisoned: bool,
}

impl Default for Batch {
    fn default() -> Self {
        Batch::new()
    }
}

impl Batch {
    pub fn new() -> Batch {
        Batch {
            ed: multiexp::BatchVerifier::new(1),
            c1: generalized_bulletproofs::Generators::batch_verifier(),
            c2: generalized_bulletproofs::Generators::batch_verifier(),
            poisoned: false,
        }
    }

    /// Checks `tx`'s proofs against the tree root and layer count of its reference block.
    pub fn add(
        &mut self,
        chain_id: &[u8; 32],
        tx: &Transaction,
        root: TreeRoot<Selene, Helios>,
        layers: usize,
    ) -> Result<(), ProofError> {
        let n_in = tx.prefix.inputs.len();
        let parsed = parse(&tx.prunable.proof_data, n_in, layers)?;

        let mut out_commitments = Vec::with_capacity(tx.prefix.outputs.len());
        for (j, o) in tx.prefix.outputs.iter().enumerate() {
            strict_point(&o.onetime_address).ok_or(ProofError::BadOutputPoint(j))?;
            out_commitments
                .push(strict_point(&o.amount_commitment).ok_or(ProofError::BadOutputPoint(j))?);
        }
        let mut key_images = Vec::with_capacity(n_in);
        for (i, input) in tx.prefix.inputs.iter().enumerate() {
            key_images.push(EdwardsPoint(
                strict_point(&input.key_image).ok_or(ProofError::BadKeyImage(i))?,
            ));
        }
        let mut pseudo = Vec::with_capacity(n_in);
        for (i, p) in parsed.pseudo_outs.iter().enumerate() {
            pseudo.push(strict_point(p).ok_or(ProofError::BadPseudoOut(i))?);
        }

        let sum_in: DPoint = pseudo.iter().sum();
        let sum_out: DPoint = out_commitments.iter().sum();
        if !IsIdentity::is_identity(&(sum_in - sum_out - h() * DScalar::from(tx.prefix.fee))) {
            return Err(ProofError::Balance);
        }

        let commitments: Vec<CompressedPoint> = tx
            .prefix
            .outputs
            .iter()
            .map(|o| CompressedPoint::from(o.amount_commitment))
            .collect();
        if !parsed.range_proof.verify(&mut OsRng, &commitments) {
            return Err(ProofError::RangeProof);
        }

        let msg = proof_message(chain_id, tx, &parsed.pseudo_outs, &parsed.range_proof_bytes)
            .map_err(|_| ProofError::Malformed("the prefix does not encode"))?;
        if parsed
            .fcmp
            .verify(
                &mut OsRng,
                &mut self.ed,
                &mut self.c1,
                &mut self.c2,
                root,
                layers,
                msg,
                key_images,
            )
            .is_err()
        {
            self.poisoned = true;
            return Err(ProofError::Membership);
        }
        Ok(())
    }

    /// Whether every queued FCMP++ proof verifies.
    pub fn finish(self) -> bool {
        !self.poisoned
            && self.ed.verify_vartime()
            && SELENE_FCMP_GENERATORS.generators.verify(self.c1)
            && HELIOS_FCMP_GENERATORS.generators.verify(self.c2)
    }
}

/// The exact length of `proof_data` for `n_inputs` inputs, `n_outputs` outputs and a tree of `layers` layers: the
/// pseudo-outputs, a Bulletproofs+ proof of the outputs (padded to a power of two: six 32-byte values, then L and R, each a
/// one-byte count and `log2(64 * padded)` points), and the FCMP++ proof. A wallet knows its fee before it proves.
pub fn proof_data_size(n_inputs: usize, n_outputs: usize, layers: usize) -> usize {
    let rounds = (64 * n_outputs.next_power_of_two()).ilog2() as usize;
    let range_proof = 32 * 6 + 2 * (1 + 32 * rounds);
    32 * n_inputs + range_proof + FcmpPlusPlus::proof_size(n_inputs, layers)
}

/// The stack the FCMP++ prover and verifier run with. They recurse deeply and keep large values on the stack: a build
/// without optimisation overflowed the 1 MiB stack of a Windows main thread while proving (2026-10-08), and the 2 MiB of
/// an ordinary spawned thread is no margin either. The memory is reserved, not used, until it is needed.
pub const PROOF_STACK: usize = 64 << 20;

/// Runs `f` on a thread of its own with [`PROOF_STACK`] of stack, and gives back what it returns (a panic in it goes on in
/// the caller, as if `f` had run here).
pub fn on_proof_stack<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|s| {
        let h = std::thread::Builder::new()
            .name("fcmp".into())
            .stack_size(PROOF_STACK)
            .spawn_scoped(s, f)
            .expect("a thread for the proof");
        h.join().unwrap_or_else(|p| std::panic::resume_unwind(p))
    })
}

/// Checks one transaction's proofs on their own (the mempool's case).
pub fn verify_tx(
    chain_id: &[u8; 32],
    tx: &Transaction,
    root: TreeRoot<Selene, Helios>,
    layers: usize,
) -> Result<(), ProofError> {
    on_proof_stack(|| {
        let mut b = Batch::new();
        b.add(chain_id, tx, root, layers)?;
        if b.finish() {
            Ok(())
        } else {
            Err(ProofError::Membership)
        }
    })
}

/// The root and layer count of a stored tree state, as the verifier takes them.
fn root_of(tree: &TreeState) -> Result<(TreeRoot<Selene, Helios>, usize), String> {
    let layers = usize::from(tree.n_layers);
    let root = crate::curve_tree::root_from_bytes(layers, &tree.root)
        .ok_or("the reference block's tree root is not a point")?;
    Ok((root, layers))
}

/// The block validator's proof check for version 3 (`tenero_chain::ProofCheck`): every transaction's proofs against
/// the tree of its reference block. A block's transactions are verified as batches, one per thread.
pub struct FcmpProofs {
    threads: usize,
    /// The transactions whose proofs verified, each against one tree: a transaction checked when it reached the mempool
    /// is not checked again in its block (`docs/FCMP_CARROT_PLAN.md` F14).
    verified: Mutex<Verified>,
}

/// How many verdicts are remembered: about ten blocks at the ceiling (32 bytes each).
pub const VERIFIED_CAPACITY: usize = 65_536;

/// Remembered good verdicts, oldest dropped first. Only a success is remembered, and its key binds everything checked:
/// the whole transaction (its id commits to the prefix and the proofs), the chain and the tree it was checked against.
#[derive(Default)]
struct Verified {
    set: HashSet<[u8; 32]>,
    order: VecDeque<[u8; 32]>,
}

impl Verified {
    fn insert(&mut self, key: [u8; 32]) {
        if self.set.insert(key) {
            self.order.push_back(key);
            while self.order.len() > VERIFIED_CAPACITY {
                if let Some(old) = self.order.pop_front() {
                    self.set.remove(&old);
                }
            }
        }
    }
}

/// The key a verdict is remembered by: the transaction, the chain and the tree (root and layers). `None` if the
/// transaction does not encode (it is then checked, and fails, the usual way).
fn verdict_key(ctx: &TxContext<'_>) -> Option<[u8; 32]> {
    let id = tenero_core::v3::ids::tx_id(ctx.tx).ok()?;
    Some(tenero_core::hash::sha256(&[
        b"tenero verified proofs v3",
        &id,
        &ctx.chain_id,
        &ctx.tree.root,
        &[ctx.tree.n_layers],
        &ctx.tree.n_leaves.to_le_bytes(),
    ]))
}

impl Default for FcmpProofs {
    fn default() -> Self {
        FcmpProofs::new()
    }
}

/// The node's proof check (one per process, so its memory of verdicts is shared).
pub static FCMP: std::sync::LazyLock<FcmpProofs> = std::sync::LazyLock::new(FcmpProofs::new);

impl FcmpProofs {
    /// Up to 4 threads (fewer if the machine has fewer cores). A block at the ceiling (about 6,300 typical transactions)
    /// takes about 2 minutes of one 5900X core, so about 30 s on 4 (`docs/FCMP_CARROT_PLAN.md` F14); a quiet block, a few
    /// hundred milliseconds; a block whose transactions all came through the mempool, almost nothing.
    pub fn new() -> FcmpProofs {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        FcmpProofs::with_threads(cores.min(4))
    }

    pub fn with_threads(threads: usize) -> FcmpProofs {
        FcmpProofs {
            threads: threads.max(1),
            verified: Mutex::new(Verified::default()),
        }
    }

    /// How many verdicts are remembered.
    pub fn remembered(&self) -> usize {
        self.lock().set.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Verified> {
        // a panic while holding it leaves only a set of good verdicts: still valid
        self.verified.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn known(&self, ctx: &TxContext<'_>) -> bool {
        verdict_key(ctx).is_some_and(|k| self.lock().set.contains(&k))
    }

    fn remember(&self, ctxs: &[&TxContext<'_>]) {
        let keys: Vec<[u8; 32]> = ctxs.iter().filter_map(|c| verdict_key(c)).collect();
        let mut v = self.lock();
        for k in keys {
            v.insert(k);
        }
    }

    /// One batch over `txs` (each with its index in the block), and if it fails, which transaction.
    fn check_chunk(txs: &[(usize, &TxContext<'_>)]) -> Result<(), (usize, String)> {
        let mut batch = Batch::new();
        for (i, ctx) in txs {
            let (root, layers) = root_of(&ctx.tree).map_err(|e| (*i, e))?;
            batch
                .add(&ctx.chain_id, ctx.tx, root, layers)
                .map_err(|e| (*i, e.to_string()))?;
        }
        if batch.finish() {
            return Ok(());
        }
        // a batch says only that something failed: find what, one by one (only an invalid block pays this)
        for (i, ctx) in txs {
            let (root, layers) = root_of(&ctx.tree).map_err(|e| (*i, e))?;
            verify_tx(&ctx.chain_id, ctx.tx, root, layers).map_err(|e| (*i, e.to_string()))?;
        }
        Err((
            txs.first().map_or(0, |t| t.0),
            "the batch does not verify".into(),
        ))
    }
}

impl ProofCheck for FcmpProofs {
    fn check_tx(&self, ctx: &TxContext<'_>) -> Result<(), String> {
        if self.known(ctx) {
            return Ok(());
        }
        let (root, layers) = root_of(&ctx.tree)?;
        verify_tx(&ctx.chain_id, ctx.tx, root, layers).map_err(|e| e.to_string())?;
        self.remember(&[ctx]);
        Ok(())
    }

    fn check_block(&self, txs: &[TxContext<'_>]) -> Result<(), (usize, String)> {
        let todo: Vec<(usize, &TxContext<'_>)> = txs
            .iter()
            .enumerate()
            .filter(|(_, c)| !self.known(c))
            .collect();
        if todo.is_empty() {
            return Ok(());
        }
        let result = if todo.len() == 1 || self.threads == 1 {
            on_proof_stack(|| FcmpProofs::check_chunk(&todo))
        } else {
            let per = todo.len().div_ceil(self.threads.min(todo.len()));
            let results: Vec<Result<(), (usize, String)>> = std::thread::scope(|s| {
                let handles: Vec<_> = todo
                    .chunks(per)
                    .map(|chunk| {
                        std::thread::Builder::new()
                            .name("fcmp".into())
                            .stack_size(PROOF_STACK)
                            .spawn_scoped(s, move || FcmpProofs::check_chunk(chunk))
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| match h {
                        Ok(h) => h
                            .join()
                            .unwrap_or_else(|_| Err((0, "a proof check panicked".into()))),
                        Err(e) => Err((0, format!("no thread for a proof check: {e}"))),
                    })
                    .collect()
            });
            // the first failing transaction in block order (the chunks are in block order)
            results.into_iter().collect()
        };
        result?;
        let done: Vec<&TxContext<'_>> = todo.iter().map(|(_, c)| *c).collect();
        self.remember(&done);
        Ok(())
    }

    fn checks_proofs(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------------------------
// The prover: what a wallet does.
// ---------------------------------------------------------------------------------------------

/// An output being spent: its key's two secrets (`O = x G + y T`), its commitment's mask and amount (`C = z G + a H`;
/// a coinbase output's mask is one), its leaf, and its path in the reference block's tree.
pub struct Spend {
    pub x: DScalar,
    pub y: DScalar,
    pub mask: DScalar,
    pub amount: u64,
    pub leaf: Leaf,
    pub path: Path<Curves>,
}

/// An output being made: its amount and its commitment's mask.
pub struct OutputSecret {
    pub amount: u64,
    pub mask: DScalar,
}

fn s(x: &DScalar) -> Scalar {
    *x
}

/// Makes `proof_data` for `tx`, whose prefix (key images in the order of `spends`, outputs, fee, payment ID) and
/// reference height are already set, against a tree of `layers` layers. `None` if anything does not fit (a key image
/// that is not `x * I`, an amount that does not balance, a path not in one tree).
pub fn prove(
    rng: &mut (impl RngCore + CryptoRng + Send),
    chain_id: &[u8; 32],
    tx: &Transaction,
    spends: &[Spend],
    outputs: &[OutputSecret],
    layers: usize,
) -> Option<Vec<u8>> {
    on_proof_stack(|| prove_here(rng, chain_id, tx, spends, outputs, layers))
}

/// [`prove`], on the calling thread's stack.
fn prove_here(
    rng: &mut (impl RngCore + CryptoRng),
    chain_id: &[u8; 32],
    tx: &Transaction,
    spends: &[Spend],
    outputs: &[OutputSecret],
    layers: usize,
) -> Option<Vec<u8>> {
    if spends.len() != tx.prefix.inputs.len()
        || outputs.len() != tx.prefix.outputs.len()
        || spends.is_empty()
    {
        return None;
    }
    // amounts must balance
    let total_in = spends
        .iter()
        .try_fold(0u64, |a, s| a.checked_add(s.amount))?;
    let total_out = outputs
        .iter()
        .try_fold(tx.prefix.fee, |a, o| a.checked_add(o.amount))?;
    if total_in != total_out {
        return None;
    }

    // 1. the range proof
    let bp = Bulletproof::prove_plus(
        rng,
        outputs
            .iter()
            .map(|o| Commitment::new(monero_ed25519::Scalar::from(o.mask), o.amount))
            .collect(),
    )
    .ok()?;
    let bp_bytes = bp.serialize();

    // 2. re-randomise every input; the last input's r_c makes the pseudo-outputs balance the outputs
    let out_masks: DScalar = outputs.iter().map(|o| o.mask).sum();
    let mut rerandomized = Vec::with_capacity(spends.len());
    let mut sum = DScalar::ZERO; // sum of (z_i + r_c_i) so far
    for (i, sp) in spends.iter().enumerate() {
        let r = RerandomizedOutput::new(rng, sp.leaf.output);
        if i + 1 < spends.len() {
            // c_blind() is -r_c
            sum += sp.mask - r.c_blind();
            rerandomized.push(r);
        } else {
            let r_c = out_masks - sum - sp.mask;
            let mut bytes = vec![];
            r.write(&mut bytes).ok()?;
            let c_tilde = sp.leaf.output.C() + EdwardsPoint::generator() * s(&r_c);
            bytes[96..128].copy_from_slice(&c_tilde.to_bytes());
            bytes[224..256].copy_from_slice(&s(&r_c).to_repr());
            rerandomized.push(RerandomizedOutput::read(&mut bytes.as_slice()).ok()?);
        }
    }
    let pseudo_outs: Vec<[u8; 32]> = rerandomized.iter().map(|r| r.input().C_tilde()).collect();

    // 3. the message, and each input's spend-authorisation proof over it
    let msg = proof_message(chain_id, tx, &pseudo_outs, &bp_bytes).ok()?;
    let mut inputs = Vec::with_capacity(spends.len());
    for ((sp, r), input) in spends.iter().zip(&rerandomized).zip(&tx.prefix.inputs) {
        let opening = OpenedInputTuple::open(r, &s(&sp.x), &s(&sp.y))?;
        let (ki, sal) = SpendAuthAndLinkability::prove(rng, msg, &opening);
        if ki.to_bytes() != input.key_image {
            return None;
        }
        inputs.push((r.input(), sal));
    }

    // 4. the membership proof
    let branches = Branches::new(spends.iter().map(|sp| sp.path.clone()).collect())?;
    let t = EdwardsPoint(CompressedPoint::T.decompress()?.into());
    let output_blinds = rerandomized
        .iter()
        .map(|r| {
            Some(OutputBlinds::new(
                OBlind::new(t, ScalarDecomposition::new(r.o_blind())?),
                IBlind::new(
                    EdwardsPoint((*FCMP_PLUS_PLUS_U).into()),
                    EdwardsPoint((*FCMP_PLUS_PLUS_V).into()),
                    ScalarDecomposition::new(r.i_blind())?,
                ),
                IBlindBlind::new(t, ScalarDecomposition::new(r.i_blind_blind())?),
                CBlind::new(
                    EdwardsPoint::generator(),
                    ScalarDecomposition::new(r.c_blind())?,
                ),
            ))
        })
        .collect::<Option<Vec<_>>>()?;
    let c1 = (0..branches.necessary_c1_blinds())
        .map(|_| {
            Some(BranchBlind::new(
                SELENE_FCMP_GENERATORS.generators.h(),
                ScalarDecomposition::new(<Selene as Ciphersuite>::F::random(&mut *rng))?,
            ))
        })
        .collect::<Option<Vec<_>>>()?;
    let c2 = (0..branches.necessary_c2_blinds())
        .map(|_| {
            Some(BranchBlind::new(
                HELIOS_FCMP_GENERATORS.generators.h(),
                ScalarDecomposition::new(<Helios as Ciphersuite>::F::random(&mut *rng))?,
            ))
        })
        .collect::<Option<Vec<_>>>()?;
    let blinded = branches.blind(output_blinds, c1, c2).ok()?;
    let fcmp = Fcmp::prove(rng, &*FCMP_PARAMS, blinded).ok()?;

    // 5. lay it out
    let mut proof_data = Vec::new();
    for p in &pseudo_outs {
        proof_data.extend_from_slice(p);
    }
    proof_data.extend_from_slice(&bp_bytes);
    FcmpPlusPlus::new(inputs, fcmp)
        .write(&mut proof_data)
        .ok()?;
    if proof_data.len()
        != pseudo_outs.len() * 32 + bp_bytes.len() + FcmpPlusPlus::proof_size(spends.len(), layers)
    {
        return None;
    }
    Some(proof_data)
}

/// The key image of an output whose key is `x G + y T`: `x * Hp²(O)`.
pub fn key_image(x: &DScalar, onetime_address: &[u8; 32]) -> [u8; 32] {
    let i: DPoint = monero_ed25519::Point::hash(*onetime_address).into();
    (i * x).compress().to_bytes()
}
