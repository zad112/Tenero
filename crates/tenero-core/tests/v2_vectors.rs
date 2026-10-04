//! The version 2 data model against `tests/vectors/v2_*.json`, which a separate Python reference made
//! (`reference/tools/make_vectors_v2.py`): the wire forms, the invalid encodings, the ids, the Merkle root and the genesis.

use serde_json::{json, Value};
use tenero_core::hash::{hex_lower, sha256};
use tenero_core::v2::ids::{self, PowKind};
use tenero_core::v2::*;
use tenero_core::vectors::{hex, load};

fn hx(b: &[u8]) -> Value {
    Value::String(hex_lower(b))
}

fn bytes_of(v: &Value) -> Vec<u8> {
    hex(v.as_str().unwrap()).unwrap()
}

fn arr32(v: &Value) -> [u8; 32] {
    bytes_of(v).try_into().unwrap()
}

// ---- each type as the JSON object the vectors use, so a decoded value can be compared field by field

fn output_json(o: &Output) -> Value {
    json!({"onetime_address": hx(&o.onetime_address), "amount_commitment": hx(&o.amount_commitment),
           "amount_enc": hx(&o.amount_enc), "view_tag": hx(&o.view_tag),
           "ephemeral_pubkey": hx(&o.ephemeral_pubkey), "anchor_enc": hx(&o.anchor_enc)})
}

fn input_json(i: &Input) -> Value {
    json!({"key_image": hx(&i.key_image)})
}

fn prefix_fields(p: &TxPrefix) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert("version".into(), json!(p.version));
    m.insert(
        "inputs".into(),
        Value::Array(p.inputs.iter().map(input_json).collect()),
    );
    m.insert(
        "outputs".into(),
        Value::Array(p.outputs.iter().map(output_json).collect()),
    );
    m.insert("fee".into(), json!(p.fee));
    m.insert("extra".into(), hx(&p.extra));
    m
}

fn tx_json(t: &Transaction) -> Value {
    let mut m = prefix_fields(&t.prefix);
    m.insert("rings".into(), json!(t.prunable.rings));
    m.insert("proof_data".into(), hx(&t.prunable.proof_data));
    Value::Object(m)
}

fn pruned_json(t: &PrunedTransaction) -> Value {
    let mut m = prefix_fields(&t.prefix);
    m.insert("prunable_hash".into(), hx(&t.prunable_hash));
    Value::Object(m)
}

fn cb_output_json(o: &CoinbaseOutput) -> Value {
    json!({"onetime_address": hx(&o.onetime_address), "amount": o.amount, "view_tag": hx(&o.view_tag),
           "ephemeral_pubkey": hx(&o.ephemeral_pubkey), "anchor_enc": hx(&o.anchor_enc)})
}

fn coinbase_json(c: &Coinbase) -> Value {
    json!({"version": c.version, "height": c.height, "extra": hx(&c.extra),
           "outputs": c.outputs.iter().map(cb_output_json).collect::<Vec<_>>()})
}

fn header_json(h: &BlockHeader) -> Value {
    json!({"version": h.version, "prev_id": hx(&h.prev_id), "timestamp": h.timestamp,
           "tx_root": hx(&h.tx_root), "nonce": h.nonce, "mix": hx(&h.mix)})
}

fn block_json(b: &Block) -> Value {
    json!({"header": header_json(&b.header), "coinbase": coinbase_json(&b.coinbase),
           "transactions": b.transactions.iter().map(tx_json).collect::<Vec<_>>()})
}

fn pruned_block_json(b: &PrunedBlock) -> Value {
    json!({"header": header_json(&b.header), "coinbase": coinbase_json(&b.coinbase),
           "transactions": b.transactions.iter().map(pruned_json).collect::<Vec<_>>()})
}

/// Decodes `bytes` as `T`, checks it equals the vector's object field by field, and that encoding it
/// again gives back exactly the bytes.
fn round_trip<T: Wire + std::fmt::Debug>(
    bytes: &[u8],
    object: &Value,
    to_json: fn(&T) -> Value,
    what: &str,
) {
    let x = T::from_bytes(bytes).unwrap_or_else(|e| panic!("{what}: {e}"));
    assert_eq!(to_json(&x), *object, "{what}: decoded fields");
    assert_eq!(x.to_bytes().unwrap(), bytes, "{what}: re-encoded bytes");
}

fn decode_error(kind: &str, bytes: &[u8]) -> DecodeError {
    let e = match kind {
        "output" => Output::from_bytes(bytes).err(),
        "input" => Input::from_bytes(bytes).err(),
        "transaction" => Transaction::from_bytes(bytes).err(),
        "pruned_transaction" => PrunedTransaction::from_bytes(bytes).err(),
        "coinbase_output" => CoinbaseOutput::from_bytes(bytes).err(),
        "coinbase" => Coinbase::from_bytes(bytes).err(),
        "header" => BlockHeader::from_bytes(bytes).err(),
        "block" => Block::from_bytes(bytes).err(),
        "pruned_block" => PrunedBlock::from_bytes(bytes).err(),
        other => panic!("unknown kind {other}"),
    };
    e.unwrap_or_else(|| panic!("{kind}: the decoder accepted an invalid encoding"))
}

// ------------------------------------------------------------------ v2_serialization.json

#[test]
fn the_limits_are_the_ones_the_vectors_were_made_with() {
    let l = &load("v2_serialization").unwrap()["limits"];
    let g = |k: &str| usize::try_from(l[k].as_u64().unwrap()).unwrap();
    assert_eq!(g("MAX_INPUTS"), MAX_INPUTS);
    assert_eq!(g("MIN_INPUTS"), MIN_INPUTS);
    assert_eq!(g("MAX_OUTPUTS"), MAX_OUTPUTS);
    assert_eq!(g("MIN_OUTPUTS"), MIN_OUTPUTS);
    assert_eq!(g("MAX_COINBASE_OUTPUTS"), MAX_COINBASE_OUTPUTS);
    assert_eq!(g("MIN_COINBASE_OUTPUTS"), MIN_COINBASE_OUTPUTS);
    assert_eq!(g("MAX_EXTRA"), MAX_EXTRA);
    assert_eq!(g("MAX_PROOF"), MAX_PROOF);
    assert_eq!(g("MAX_RING"), MAX_RING);
    assert_eq!(g("MAX_BLOCK_TXS"), MAX_BLOCK_TXS);
}

#[test]
fn the_primitive_encodings() {
    for p in load("v2_serialization").unwrap()["primitives"]
        .as_array()
        .unwrap()
    {
        let (ty, want) = (p["type"].as_str().unwrap(), bytes_of(&p["hex"]));
        let value = p["value"].as_u64().unwrap();
        let mut w = Writer::new();
        match ty {
            "u8" => w.raw(&[u8::try_from(value).unwrap()]),
            "u16" => w.u16(u16::try_from(value).unwrap()),
            "u32" => w.u32(u32::try_from(value).unwrap()),
            "u64" => w.u64(value),
            other => panic!("unknown type {other}"),
        }
        assert_eq!(w.into_bytes(), want, "{ty} {value}");
        let mut r = Reader::new(&want);
        let got = match ty {
            "u8" => u64::from(r.take(1).unwrap()[0]),
            "u16" => u64::from(r.u16().unwrap()),
            "u32" => u64::from(r.u32().unwrap()),
            _ => r.u64().unwrap(),
        };
        assert_eq!(got, value, "{ty} read back");
        r.finish().unwrap();
    }
}

#[test]
fn every_valid_object_decodes_to_the_stated_fields_and_encodes_to_the_same_bytes() {
    let cases = load("v2_serialization").unwrap();
    let cases = cases["valid"].as_array().unwrap();
    assert!(cases.len() >= 17);
    for c in cases {
        let (kind, bytes, object) = (
            c["kind"].as_str().unwrap(),
            bytes_of(&c["hex"]),
            &c["object"],
        );
        let what = format!("{kind}: {}", c["note"].as_str().unwrap());
        match kind {
            "output" => round_trip::<Output>(&bytes, object, output_json, &what),
            "input" => round_trip::<Input>(&bytes, object, input_json, &what),
            "transaction" => round_trip::<Transaction>(&bytes, object, tx_json, &what),
            "pruned_transaction" => {
                round_trip::<PrunedTransaction>(&bytes, object, pruned_json, &what)
            }
            "coinbase_output" => {
                round_trip::<CoinbaseOutput>(&bytes, object, cb_output_json, &what)
            }
            "coinbase" => round_trip::<Coinbase>(&bytes, object, coinbase_json, &what),
            "header" => round_trip::<BlockHeader>(&bytes, object, header_json, &what),
            "block" => round_trip::<Block>(&bytes, object, block_json, &what),
            "pruned_block" => round_trip::<PrunedBlock>(&bytes, object, pruned_block_json, &what),
            other => panic!("unknown kind {other}"),
        }
    }
}

#[test]
fn every_invalid_encoding_is_refused_in_the_stated_way() {
    let v = load("v2_serialization").unwrap();
    let cases = v["invalid"].as_array().unwrap();
    assert!(cases.len() >= 25);
    let mut kinds = std::collections::BTreeSet::new();
    for c in cases {
        let (kind, note) = (c["kind"].as_str().unwrap(), c["note"].as_str().unwrap());
        let got = decode_error(kind, &bytes_of(&c["hex"]));
        assert_eq!(got.as_str(), c["error"].as_str().unwrap(), "{kind}: {note}");
        kinds.insert(got.as_str());
    }
    assert_eq!(kinds.len(), 4, "all four failure kinds are exercised");
}

/// The property that makes an encoding canonical: whatever the decoder accepts, encoding it again gives
/// the same bytes; and it never panics, whatever the bytes. Every byte of several objects is flipped.
#[test]
fn whatever_the_decoder_accepts_it_re_encodes_identically_and_it_never_panics() {
    let v = load("v2_serialization").unwrap();
    let mut checked = 0usize;
    let mut accepted = 0usize;
    for c in v["valid"].as_array().unwrap() {
        let kind = c["kind"].as_str().unwrap();
        let original = bytes_of(&c["hex"]);
        if original.len() > 4_000 {
            continue; // the two very large cases would only repeat the same paths
        }
        let mut variants: Vec<Vec<u8>> = Vec::new();
        for cut in 0..original.len() {
            variants.push(original[..cut].to_vec());
        }
        for i in 0..original.len() {
            for f in [0x00u8, 0xff, 0x01, 0x80] {
                let mut m = original.clone();
                m[i] ^= f;
                variants.push(m);
            }
        }
        let mut with_extra = original.clone();
        with_extra.push(0);
        variants.push(with_extra);
        for bytes in variants {
            checked += 1;
            macro_rules! check {
                ($t:ty) => {
                    if let Ok(x) = <$t>::from_bytes(&bytes) {
                        accepted += 1;
                        assert_eq!(
                            x.to_bytes().unwrap(),
                            bytes,
                            "{kind}: a non-canonical encoding was accepted"
                        );
                    }
                };
            }
            match kind {
                "output" => check!(Output),
                "input" => check!(Input),
                "transaction" => check!(Transaction),
                "pruned_transaction" => check!(PrunedTransaction),
                "coinbase_output" => check!(CoinbaseOutput),
                "coinbase" => check!(Coinbase),
                "header" => check!(BlockHeader),
                "block" => check!(Block),
                "pruned_block" => check!(PrunedBlock),
                other => panic!("unknown kind {other}"),
            }
        }
    }
    assert!(checked > 10_000, "only {checked} variants were tried");
    assert!(
        accepted > 100,
        "only {accepted} mutated inputs were accepted, so the check would prove little"
    );
}

#[test]
fn encoding_refuses_what_no_decoder_would_accept() {
    let v = load("v2_serialization").unwrap();
    let case = v["valid"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["note"] == "2 inputs, 2 outputs")
        .unwrap();
    let tx = Transaction::from_bytes(&bytes_of(&case["hex"])).unwrap();

    let mut t = tx.clone();
    t.prefix.inputs.clear();
    assert_eq!(t.to_bytes(), Err(EncodeError::CountOutOfRange));
    let mut t = tx.clone();
    t.prefix.outputs.truncate(1);
    assert_eq!(t.to_bytes(), Err(EncodeError::CountOutOfRange));
    let mut t = tx.clone();
    t.prefix.extra = vec![0; MAX_EXTRA + 1];
    assert_eq!(t.to_bytes(), Err(EncodeError::LengthOverMaximum));
    let mut t = tx.clone();
    t.prunable.proof_data = vec![0; MAX_PROOF + 1];
    assert_eq!(t.to_bytes(), Err(EncodeError::LengthOverMaximum));
    let mut t = tx.clone();
    t.prunable.rings[0] = (0..=MAX_RING as u64).collect();
    assert_eq!(t.to_bytes(), Err(EncodeError::CountOutOfRange));
    // exactly one ring per input: fewer or more can never be read back
    let mut t = tx.clone();
    t.prunable.rings.pop();
    assert_eq!(t.to_bytes(), Err(EncodeError::CountOutOfRange));
    let mut t = tx.clone();
    t.prunable.rings.push(vec![1, 2, 3]);
    assert_eq!(t.to_bytes(), Err(EncodeError::CountOutOfRange));
    // the id of an object that cannot be encoded is an error too, not a hash of something else
    assert!(ids::tx_id(&t).is_err());
    assert!(t.prune().is_err());
    // and the limits themselves are accepted
    let mut t = tx.clone();
    t.prefix.extra = vec![0; MAX_EXTRA];
    t.prunable.proof_data = vec![0; MAX_PROOF];
    assert!(t.to_bytes().is_ok());
}

// ------------------------------------------------------------------ v2_ids.json

#[test]
fn header_hashes_seeds_and_block_ids() {
    let v = load("v2_ids").unwrap();
    let t = &v["tags"];
    assert_eq!(t["header"].as_str().unwrap().as_bytes(), ids::HEADER_TAG);
    assert_eq!(t["transaction"].as_str().unwrap().as_bytes(), ids::TX_TAG);
    assert_eq!(
        t["prunable"].as_str().unwrap().as_bytes(),
        ids::PRUNABLE_TAG
    );
    assert_eq!(
        t["coinbase"].as_str().unwrap().as_bytes(),
        ids::COINBASE_TAG
    );
    for c in v["headers"].as_array().unwrap() {
        let h = BlockHeader::from_bytes(&bytes_of(&c["header_bytes"])).unwrap();
        assert_eq!(header_json(&h), c["header"]);
        let hh = ids::header_hash(&h);
        assert_eq!(hex_lower(&hh), c["header_hash"].as_str().unwrap());
        assert_eq!(
            hex_lower(&ids::pow_seed(&hh, h.nonce)),
            c["seed"].as_str().unwrap()
        );
        assert_eq!(
            hex_lower(&ids::block_id(&h, PowKind::Matmul)),
            c["block_id_matmul"].as_str().unwrap()
        );
        let zero_mix = BlockHeader {
            mix: [0; 64],
            ..h.clone()
        };
        assert_eq!(
            hex_lower(&ids::block_id(&zero_mix, PowKind::Sha256)),
            c["block_id_sha256"].as_str().unwrap()
        );
    }
}

#[test]
fn transaction_ids_full_and_pruned() {
    let v = load("v2_ids").unwrap();
    for c in v["transactions"].as_array().unwrap() {
        let bytes = bytes_of(&c["bytes"]);
        let tx = Transaction::from_bytes(&bytes).unwrap();
        assert_eq!(tx_json(&tx), c["transaction"]);
        let n = tx.prefix.inputs.len();
        assert_eq!(tx.prefix.to_bytes().unwrap(), bytes_of(&c["prefix_bytes"]));
        assert_eq!(
            tx.prunable.to_bytes(n).unwrap(),
            bytes_of(&c["prunable_bytes"])
        );
        assert_eq!(
            Prunable::from_bytes(&bytes_of(&c["prunable_bytes"]), n).unwrap(),
            tx.prunable
        );
        assert_eq!(
            hex_lower(&ids::prunable_hash(&tx.prunable, n).unwrap()),
            c["prunable_hash"].as_str().unwrap()
        );
        assert_eq!(
            hex_lower(&ids::tx_id(&tx).unwrap()),
            c["id"].as_str().unwrap()
        );

        let pruned = tx.prune().unwrap();
        assert_eq!(pruned.to_bytes().unwrap(), bytes_of(&c["pruned_bytes"]));
        assert_eq!(
            PrunedTransaction::from_bytes(&bytes_of(&c["pruned_bytes"])).unwrap(),
            pruned
        );
        assert_eq!(
            hex_lower(&ids::pruned_tx_id(&pruned).unwrap()),
            c["pruned_id"].as_str().unwrap()
        );
        assert_eq!(
            ids::pruned_tx_id(&pruned).unwrap(),
            ids::tx_id(&tx).unwrap(),
            "pruning must not change the id"
        );
    }
}

/// The reason the rings can leave the prefix: the id still covers every ring index, so a ring cannot be
/// changed, swapped between inputs, or shortened under an existing id.
#[test]
fn the_prefix_holds_no_ring_and_the_id_covers_every_ring_index() {
    let v = load("v2_ids").unwrap();
    let c = &v["transactions"].as_array().unwrap()[0];
    let tx = Transaction::from_bytes(&bytes_of(&c["bytes"])).unwrap();
    let id = ids::tx_id(&tx).unwrap();
    // 2 (version) + 4 + 2 * 32 (key images) + 4 + 2 * 123 + 8 + 4 + 24 (extra)
    assert_eq!(tx.prefix.to_bytes().unwrap().len(), 356);
    assert_eq!(tx.prefix.inputs.len(), 2);

    for input in 0..2 {
        for member in 0..tx.prunable.rings[input].len() {
            let mut t = tx.clone();
            t.prunable.rings[input][member] ^= 1;
            assert_ne!(ids::tx_id(&t).unwrap(), id, "ring {input} member {member}");
        }
    }
    let mut swapped = tx.clone();
    swapped.prunable.rings.swap(0, 1);
    assert_ne!(
        ids::tx_id(&swapped).unwrap(),
        id,
        "rings exchanged between inputs"
    );
    let mut shorter = tx.clone();
    shorter.prunable.rings[0].pop();
    assert_ne!(
        ids::tx_id(&shorter).unwrap(),
        id,
        "a ring one member shorter"
    );
    // and the pruned form, which has neither rings nor proofs, still has the same id
    assert_eq!(ids::pruned_tx_id(&tx.prune().unwrap()).unwrap(), id);
}

#[test]
fn coinbase_ids_and_domain_separation() {
    let v = load("v2_ids").unwrap();
    for c in v["coinbases"].as_array().unwrap() {
        let cb = Coinbase::from_bytes(&bytes_of(&c["bytes"])).unwrap();
        assert_eq!(coinbase_json(&cb), c["coinbase"]);
        assert_eq!(
            hex_lower(&ids::coinbase_id(&cb).unwrap()),
            c["id"].as_str().unwrap()
        );
    }
    let same = &v["domain_separation"];
    let bytes = bytes_of(&same["bytes"]);
    let tx = Transaction::from_bytes(&bytes).unwrap();
    assert_eq!(
        hex_lower(&ids::tx_id(&tx).unwrap()),
        same["as_transaction"].as_str().unwrap()
    );
    assert_eq!(
        hex_lower(&sha256(&[ids::COINBASE_TAG, &bytes])),
        same["as_coinbase"].as_str().unwrap()
    );
    assert_ne!(same["as_transaction"], same["as_coinbase"]);
}

#[test]
fn a_blocks_tx_root_commits_to_its_coinbase_and_transactions() {
    let v = load("v2_serialization").unwrap();
    let block = v["valid"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["note"] == "a header, a coinbase and two transactions")
        .unwrap();
    let b = Block::from_bytes(&bytes_of(&block["hex"])).unwrap();
    assert_eq!(
        ids::block_tx_root(&b.coinbase, &b.transactions).unwrap(),
        b.header.tx_root
    );
    // change one byte of the coinbase, of a transaction, or drop a transaction: the root changes
    let root = |cb: &Coinbase, txs: &[Transaction]| ids::block_tx_root(cb, txs).unwrap();
    let mut cb = b.coinbase.clone();
    cb.extra[0] ^= 1;
    assert_ne!(root(&cb, &b.transactions), b.header.tx_root);
    let mut txs = b.transactions.clone();
    txs[1].prunable.proof_data[0] ^= 1;
    assert_ne!(root(&b.coinbase, &txs), b.header.tx_root);
    assert_ne!(root(&b.coinbase, &b.transactions[..1]), b.header.tx_root);
    // and the pruned block has the same root, from prefixes and prunable hashes alone
    let ids_pruned: Vec<[u8; 32]> = std::iter::once(ids::coinbase_id(&b.coinbase).unwrap())
        .chain(
            b.transactions
                .iter()
                .map(|t| ids::pruned_tx_id(&t.prune().unwrap()).unwrap()),
        )
        .collect();
    assert_eq!(ids::merkle_root(&ids_pruned), b.header.tx_root);
}

// ------------------------------------------------------------------ v2_merkle.json, v2_genesis.json

#[test]
fn merkle_roots() {
    let v = load("v2_merkle").unwrap();
    let cases = v["cases"].as_array().unwrap();
    assert!(cases.len() >= 20);
    for c in cases {
        let leaves: Vec<[u8; 32]> = c["leaves"].as_array().unwrap().iter().map(arr32).collect();
        assert_eq!(leaves.len() as u64, c["n"].as_u64().unwrap());
        assert_eq!(
            hex_lower(&ids::merkle_root(&leaves)),
            c["root"].as_str().unwrap(),
            "n = {}",
            leaves.len()
        );
    }
    let p = &v["properties"];
    assert_eq!(
        hex_lower(&ids::merkle_root(&[])),
        p["empty_root"].as_str().unwrap()
    );
    let (a, b, c) = (sha256(&[b"a"]), sha256(&[b"b"]), sha256(&[b"c"]));
    assert_eq!(
        hex_lower(&ids::merkle_root(&[a, b, c])),
        p["not_padded"]["root_of_a_b_c"].as_str().unwrap()
    );
    assert_eq!(
        hex_lower(&ids::merkle_root(&[a, b, c, c])),
        p["not_padded"]["root_of_a_b_c_c"].as_str().unwrap()
    );
    assert_ne!(
        ids::merkle_root(&[a, b, c]),
        ids::merkle_root(&[a, b, c, c]),
        "no duplicated last leaf"
    );
    assert_ne!(ids::merkle_root(&[a, b]), ids::merkle_root(&[a, b, c]));
}

#[test]
fn genesis_and_the_chain_id() {
    let v = load("v2_genesis").unwrap();
    let cases = v["cases"].as_array().unwrap();
    assert_eq!(cases[0]["label"].as_str().unwrap(), ids::NETWORK_LABEL);
    // the release network's label is in the vectors, so that its chain id is checked against the reference
    assert!(
        cases.iter().any(|c| c["label"] == "tenero alpha network 1"),
        "the alpha label is vectored"
    );
    let mut seen = std::collections::BTreeSet::new();
    for c in cases {
        let label = c["label"].as_str().unwrap();
        let h = ids::genesis_header(label);
        assert_eq!(header_json(&h), c["header"], "{label:?}");
        assert_eq!(
            h.to_bytes().unwrap(),
            bytes_of(&c["header_bytes"]),
            "{label:?}"
        );
        assert_eq!(
            hex_lower(&ids::genesis_id(label)),
            c["genesis_id"].as_str().unwrap(),
            "{label:?}"
        );
        assert_eq!(c["genesis_id"], c["chain_id"]);
        // no premine: the genesis block is a header and nothing else
        assert_eq!(c["transactions"], 0, "{label:?}");
        assert_eq!(c["coinbase_outputs"], 0, "{label:?}");
        assert!(
            seen.insert(c["chain_id"].as_str().unwrap().to_string()),
            "two labels gave one chain id"
        );
    }
}
