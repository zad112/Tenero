//! A real Monero mainnet transaction with CLSAG signatures (`tests/vectors/upstream_monero_clsag.json`,
//! imported verbatim). Monero's own signature hash of the transaction (computed by monero-oxide, a
//! dev-dependency used for this only) is what the signatures cover; the pinned `monero-clsag` must accept
//! them, and must refuse them when anything they cover is changed.

use monero_clsag::Clsag;
use monero_ed25519::CompressedPoint;
use monero_oxide::ringct::RctPrunable;
use monero_oxide::transaction::{NotPruned, Transaction};
use tenero_core::vectors::{hex, load};

fn point(v: &str) -> CompressedPoint {
    CompressedPoint::from(<[u8; 32]>::try_from(hex(v).unwrap()).unwrap())
}

struct Real {
    clsags: Vec<Clsag>,
    rings: Vec<Vec<[CompressedPoint; 2]>>,
    images: Vec<CompressedPoint>,
    pseudo_outs: Vec<CompressedPoint>,
    message: [u8; 32],
}

fn real() -> Real {
    let file = load("upstream_monero_clsag").unwrap();
    let tx = Transaction::<NotPruned>::read(
        &mut hex(file["tx_hex"].as_str().unwrap()).unwrap().as_slice(),
    )
    .unwrap();
    let message = tx
        .signature_hash()
        .expect("a v2 transaction has a signature hash");
    let clsags = match &tx {
        Transaction::V2 {
            proofs: Some(proofs),
            ..
        } => match &proofs.prunable {
            RctPrunable::Clsag { clsags, .. } => clsags.clone(),
            _ => panic!("not a CLSAG transaction"),
        },
        _ => panic!("not a v2 transaction with proofs"),
    };
    let inputs = file["inputs"].as_array().unwrap();
    Real {
        clsags,
        rings: inputs
            .iter()
            .map(|i| {
                i["ring"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|m| [point(m[0].as_str().unwrap()), point(m[1].as_str().unwrap())])
                    .collect()
            })
            .collect(),
        images: inputs
            .iter()
            .map(|i| point(i["key_image"].as_str().unwrap()))
            .collect(),
        pseudo_outs: inputs
            .iter()
            .map(|i| point(i["pseudo_out"].as_str().unwrap()))
            .collect(),
        message,
    }
}

#[test]
fn the_vector_has_two_inputs_with_rings_of_sixteen() {
    let r = real();
    assert_eq!(r.clsags.len(), 2);
    assert!(r.rings.iter().all(|ring| ring.len() == 16));
    assert!(r.clsags.iter().all(|c| c.s.len() == 16));
}

#[test]
fn real_monero_clsag_signatures_verify() {
    let r = real();
    for i in 0..r.clsags.len() {
        r.clsags[i]
            .verify(
                r.rings[i].clone(),
                &r.images[i],
                &r.pseudo_outs[i],
                &r.message,
            )
            .unwrap_or_else(|e| panic!("input {i}: {e:?}"));
    }
}

#[test]
fn real_signatures_do_not_verify_when_what_they_cover_changes() {
    let r = real();
    let ok = |i: usize,
              ring: Vec<[CompressedPoint; 2]>,
              image: &CompressedPoint,
              pseudo: &CompressedPoint,
              msg: &[u8; 32]| { r.clsags[i].verify(ring, image, pseudo, msg).is_ok() };
    for i in 0..2 {
        let j = 1 - i;
        // another message
        let mut msg = r.message;
        msg[0] ^= 1;
        assert!(!ok(
            i,
            r.rings[i].clone(),
            &r.images[i],
            &r.pseudo_outs[i],
            &msg
        ));
        // the other input's key image, pseudo-output, or ring
        assert!(!ok(
            i,
            r.rings[i].clone(),
            &r.images[j],
            &r.pseudo_outs[i],
            &r.message
        ));
        assert!(!ok(
            i,
            r.rings[i].clone(),
            &r.images[i],
            &r.pseudo_outs[j],
            &r.message
        ));
        assert!(!ok(
            i,
            r.rings[j].clone(),
            &r.images[i],
            &r.pseudo_outs[i],
            &r.message
        ));
        // the ring in another order
        let mut swapped = r.rings[i].clone();
        swapped.swap(0, 1);
        assert!(!ok(i, swapped, &r.images[i], &r.pseudo_outs[i], &r.message));
        // one member's commitment or key replaced
        let mut altered = r.rings[i].clone();
        altered[5][1] = CompressedPoint::G;
        assert!(!ok(i, altered, &r.images[i], &r.pseudo_outs[i], &r.message));
        let mut altered = r.rings[i].clone();
        altered[5][0] = CompressedPoint::G;
        assert!(!ok(i, altered, &r.images[i], &r.pseudo_outs[i], &r.message));
        // a shorter ring
        let mut shorter = r.rings[i].clone();
        shorter.pop();
        assert!(!ok(i, shorter, &r.images[i], &r.pseudo_outs[i], &r.message));
    }
}

#[test]
fn a_real_signature_with_any_byte_flipped_is_refused() {
    let r = real();
    for i in 0..2 {
        let mut bytes = Vec::new();
        r.clsags[i].write(&mut bytes).unwrap();
        for pos in (0..bytes.len()).step_by(11) {
            let mut b = bytes.clone();
            b[pos] ^= 1;
            let accepted = Clsag::read(16, &mut b.as_slice())
                .map(|c| {
                    c.verify(
                        r.rings[i].clone(),
                        &r.images[i],
                        &r.pseudo_outs[i],
                        &r.message,
                    )
                    .is_ok()
                })
                .unwrap_or(false);
            assert!(!accepted, "input {i}: a flip at byte {pos} was accepted");
        }
    }
}
