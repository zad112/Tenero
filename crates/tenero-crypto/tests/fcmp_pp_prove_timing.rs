//! How long it takes to MAKE an FCMP++ proof on this machine (`docs/FCMP_CARROT_PLAN.md`, milestone G1), with Monero's real
//! parameters and a tree of Monero's widths (38 outputs per leaf chunk, then 18 and 38 children per layer). The tree is a
//! random one, built as monero-oxide's own tests build theirs: only the branch the inputs are in is real, which is all a
//! proof sees. The inputs' keys are real (`O = x*G + y*T`), so the spend-authorisation proof and the key images are real.
//! Every proof made is then verified, so a timing of a broken proof cannot be printed.
//!
//! `cargo test --release -p tenero-crypto --test fcmp_pp_prove_timing -- --ignored --nocapture`
//!
//! Rule 5: these are numbers from the machine that ran them, one thread, nothing more. The pinned FCMP++ crates are only
//! partly audited.

use std::time::{Duration, Instant};

use ciphersuite::group::ff::Field;
use ciphersuite::group::Group;
use ciphersuite::Ciphersuite;
use dalek_ff_group::{Ed25519, EdwardsPoint, Scalar};
use ec_divisors::{DivisorCurve, ScalarDecomposition};
use full_chain_membership_proofs::tree::hash_grow;
use helioselene::{Helios, Selene};
use monero_ed25519::CompressedPoint;
use monero_fcmp_plus_plus::fcmps::{
    BranchBlind, Branches, CBlind, Fcmp, IBlind, IBlindBlind, OBlind, OutputBlinds, Path, TreeRoot,
    LAYER_ONE_LEN, LAYER_TWO_LEN,
};
use monero_fcmp_plus_plus::sal::{OpenedInputTuple, RerandomizedOutput, SpendAuthAndLinkability};
use monero_fcmp_plus_plus::{
    FcmpPlusPlus, Output, FCMP_PARAMS, HELIOS_FCMP_GENERATORS, SELENE_FCMP_GENERATORS,
};
use monero_fcmp_plus_plus_generators::{
    FCMP_PLUS_PLUS_U, FCMP_PLUS_PLUS_V, HELIOS_HASH_INIT, SELENE_HASH_INIT,
};
use rand_core::{OsRng, RngCore};

fn t() -> EdwardsPoint {
    EdwardsPoint(CompressedPoint::T.decompress().unwrap().into())
}

fn random_point() -> EdwardsPoint {
    EdwardsPoint::random(&mut OsRng)
}

/// An output we can spend: its secrets and its tuple.
struct Owned {
    x: Scalar,
    y: Scalar,
    output: Output,
}

fn owned() -> Owned {
    let x = Scalar::random(&mut OsRng);
    let y = Scalar::random(&mut OsRng);
    let o = EdwardsPoint::generator() * x + t() * y;
    Owned {
        x,
        y,
        output: Output::new(o, random_point(), random_point()).unwrap(),
    }
}

fn leaf_scalars(o: &Output) -> [<Selene as Ciphersuite>::F; 6] {
    let (ox, oy) = <Ed25519 as Ciphersuite>::G::to_xy(o.O()).unwrap();
    let (ix, iy) = <Ed25519 as Ciphersuite>::G::to_xy(o.I()).unwrap();
    let (cx, cy) = <Ed25519 as Ciphersuite>::G::to_xy(o.C()).unwrap();
    [ox, oy, ix, iy, cx, cy]
}

/// A random tree of `layers` layers whose first leaf chunk holds `ours` (and random outputs to fill it), and a path for
/// each of ours: all the paths share every layer, as they do when the spent outputs are in one chunk.
fn tree(
    ours: &[Owned],
    layers: usize,
) -> (
    Vec<Path<monero_fcmp_plus_plus::Curves>>,
    TreeRoot<Selene, Helios>,
) {
    assert!(layers >= 1 && ours.len() <= LAYER_ONE_LEN);
    let mut leaves: Vec<Output> = ours.iter().map(|o| o.output).collect();
    while leaves.len() < LAYER_ONE_LEN {
        leaves.push(Output::new(random_point(), random_point(), random_point()).unwrap());
    }
    let scalars: Vec<_> = leaves.iter().flat_map(leaf_scalars).collect();
    let selene_gens = &SELENE_FCMP_GENERATORS.generators;
    let helios_gens = &HELIOS_FCMP_GENERATORS.generators;
    let mut selene = Some(
        hash_grow(
            selene_gens,
            *SELENE_HASH_INIT,
            0,
            <Selene as Ciphersuite>::F::ZERO,
            &scalars,
        )
        .unwrap(),
    );
    let mut helios = None;
    let mut c2_layers = vec![];
    let mut c1_layers = vec![];
    while 1 + c1_layers.len() + c2_layers.len() < layers {
        if c2_layers.len() == c1_layers.len() {
            // a Helios-layer chunk: our Selene hash among random Selene points, by their x coordinates
            let mut chunk: Vec<_> = (0..LAYER_TWO_LEN)
                .map(|_| <Selene as Ciphersuite>::G::random(&mut OsRng))
                .collect();
            let at = (OsRng.next_u64() as usize) % LAYER_TWO_LEN;
            chunk[at] = selene.take().unwrap();
            let xs: Vec<_> = chunk
                .into_iter()
                .map(|p| <Selene as Ciphersuite>::G::to_xy(p).unwrap().0)
                .collect();
            helios = Some(
                hash_grow(
                    helios_gens,
                    *HELIOS_HASH_INIT,
                    0,
                    <Helios as Ciphersuite>::F::ZERO,
                    &xs,
                )
                .unwrap(),
            );
            c2_layers.push(xs);
        } else {
            let mut chunk: Vec<_> = (0..LAYER_ONE_LEN)
                .map(|_| <Helios as Ciphersuite>::G::random(&mut OsRng))
                .collect();
            let at = (OsRng.next_u64() as usize) % LAYER_ONE_LEN;
            chunk[at] = helios.take().unwrap();
            let xs: Vec<_> = chunk
                .into_iter()
                .map(|p| <Helios as Ciphersuite>::G::to_xy(p).unwrap().0)
                .collect();
            selene = Some(
                hash_grow(
                    selene_gens,
                    *SELENE_HASH_INIT,
                    0,
                    <Selene as Ciphersuite>::F::ZERO,
                    &xs,
                )
                .unwrap(),
            );
            c1_layers.push(xs);
        }
    }
    let root = match (selene, helios) {
        (Some(s), _) => TreeRoot::C1(s),
        (None, Some(h)) => TreeRoot::C2(h),
        (None, None) => unreachable!(),
    };
    let paths = ours
        .iter()
        .map(|o| Path {
            output: o.output,
            leaves: leaves.clone(),
            curve_2_layers: c2_layers.clone(),
            curve_1_layers: c1_layers.clone(),
        })
        .collect();
    (paths, root)
}

struct Timing {
    sal: Duration,
    blinds: Duration,
    membership: Duration,
    bytes: usize,
}

/// Makes one FCMP++ proof spending `ours` against a `layers`-layer tree, verifies it, and says how long each part took.
fn prove_once(ours: &[Owned], layers: usize) -> Timing {
    let (paths, root) = tree(ours, layers);
    let signable_tx_hash = [7u8; 32];

    // the spend-authorisation and linkability proof of each input (re-randomising the output it spends)
    let start = Instant::now();
    let mut inputs = vec![];
    let mut rerandomized = vec![];
    let mut key_images = vec![];
    for o in ours {
        let r = RerandomizedOutput::new(&mut OsRng, o.output);
        let opening = OpenedInputTuple::open(&r, &o.x, &o.y).unwrap();
        let (key_image, sal) =
            SpendAuthAndLinkability::prove(&mut OsRng, signable_tx_hash, &opening);
        assert_eq!(key_image, o.output.I() * o.x);
        inputs.push((r.input(), sal));
        key_images.push(key_image);
        rerandomized.push(r);
    }
    let sal = start.elapsed();

    // the blinds: one set per input, and one per branch layer
    let start = Instant::now();
    let output_blinds: Vec<_> = rerandomized
        .iter()
        .map(|r| {
            OutputBlinds::new(
                OBlind::new(t(), ScalarDecomposition::new(r.o_blind()).unwrap()),
                IBlind::new(
                    EdwardsPoint((*FCMP_PLUS_PLUS_U).into()),
                    EdwardsPoint((*FCMP_PLUS_PLUS_V).into()),
                    ScalarDecomposition::new(r.i_blind()).unwrap(),
                ),
                IBlindBlind::new(t(), ScalarDecomposition::new(r.i_blind_blind()).unwrap()),
                CBlind::new(
                    EdwardsPoint::generator(),
                    ScalarDecomposition::new(r.c_blind()).unwrap(),
                ),
            )
        })
        .collect();
    let branches = Branches::new(paths).unwrap();
    let c1_blinds = (0..branches.necessary_c1_blinds())
        .map(|_| {
            BranchBlind::new(
                SELENE_FCMP_GENERATORS.generators.h(),
                ScalarDecomposition::new(<Selene as Ciphersuite>::F::random(&mut OsRng)).unwrap(),
            )
        })
        .collect();
    let c2_blinds = (0..branches.necessary_c2_blinds())
        .map(|_| {
            BranchBlind::new(
                HELIOS_FCMP_GENERATORS.generators.h(),
                ScalarDecomposition::new(<Helios as Ciphersuite>::F::random(&mut OsRng)).unwrap(),
            )
        })
        .collect();
    let blinded = branches.blind(output_blinds, c1_blinds, c2_blinds).unwrap();
    let blinds = start.elapsed();

    // the membership proof
    let start = Instant::now();
    let fcmp = Fcmp::prove(&mut OsRng, &*FCMP_PARAMS, blinded).unwrap();
    let membership = start.elapsed();

    let proof = FcmpPlusPlus::new(inputs, fcmp);
    let mut bytes = vec![];
    proof.write(&mut bytes).unwrap();
    assert_eq!(bytes.len(), FcmpPlusPlus::proof_size(ours.len(), layers));

    // and it must verify, read back from its bytes as a node reads it
    let pseudo_outs: Vec<[u8; 32]> = rerandomized.iter().map(|r| r.input().C_tilde()).collect();
    let read = FcmpPlusPlus::read(&pseudo_outs, layers, &mut bytes.as_slice()).unwrap();
    let mut ed = multiexp::BatchVerifier::new(ours.len());
    let mut c1 = generalized_bulletproofs::Generators::batch_verifier();
    let mut c2 = generalized_bulletproofs::Generators::batch_verifier();
    read.verify(
        &mut OsRng,
        &mut ed,
        &mut c1,
        &mut c2,
        root,
        layers,
        signable_tx_hash,
        key_images,
    )
    .unwrap();
    assert!(ed.verify_vartime());
    assert!(SELENE_FCMP_GENERATORS.generators.verify(c1));
    assert!(HELIOS_FCMP_GENERATORS.generators.verify(c2));

    Timing {
        sal,
        blinds,
        membership,
        bytes: bytes.len(),
    }
}

/// How many outputs a tree of `layers` layers holds at most (38, then times 18, times 38, ...).
fn capacity(layers: usize) -> u128 {
    (1..layers).fold(LAYER_ONE_LEN as u128, |n, i| {
        n * if i % 2 == 1 {
            LAYER_TWO_LEN
        } else {
            LAYER_ONE_LEN
        } as u128
    })
}

#[test]
#[ignore]
fn time_proving() {
    let start = Instant::now();
    let _ = &*FCMP_PARAMS;
    let _ = &SELENE_FCMP_GENERATORS.generators;
    let _ = &HELIOS_FCMP_GENERATORS.generators;
    println!(
        "building the parameters and generators (once per process): {:.2?}",
        start.elapsed()
    );

    for n in [1usize, 2, 4] {
        for layers in 1..=7 {
            let ours: Vec<Owned> = (0..n).map(|_| owned()).collect();
            let runs = if n == 1 { 3 } else { 2 };
            let mut total = Timing {
                sal: Duration::ZERO,
                blinds: Duration::ZERO,
                membership: Duration::ZERO,
                bytes: 0,
            };
            for _ in 0..runs {
                let one = prove_once(&ours, layers);
                total.sal += one.sal;
                total.blinds += one.blinds;
                total.membership += one.membership;
                total.bytes = one.bytes;
            }
            let r = runs as u32;
            let all = (total.sal + total.blinds + total.membership) / r;
            println!(
                "FCMP++ prove, {n} input(s), {layers} layers (up to {:>13} outputs), {:>6} bytes: {:>8.2?} in all \
                 (spend-auth {:>7.2?}, blinds {:>8.2?}, membership {:>8.2?})",
                capacity(layers),
                total.bytes,
                all,
                total.sal / r,
                total.blinds / r,
                total.membership / r
            );
        }
    }
    println!("(one thread; each proof was verified after it was made)");
}
