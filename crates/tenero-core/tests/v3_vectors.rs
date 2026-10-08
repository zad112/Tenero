//! The version 3 data model against `tests/vectors/v3_*.json`, made by the separate Python reference
//! `reference/tools/make_vectors_v3.py`: the wire forms, the invalid encodings, the ids, the genesis, weight, the shape
//! rules, the reference heights and the curve tree's schedule.

use serde_json::{json, Value};
use tenero_core::hash::hex_lower;
use tenero_core::v2::ids::PowKind;
use tenero_core::v3::ids;
use tenero_core::v3::rules::{self, entering_from};
use tenero_core::v3::*;
use tenero_core::vectors::{hex, load};

fn hx(b: &[u8]) -> Value {
    Value::String(hex_lower(b))
}

fn bytes_of(v: &Value) -> Vec<u8> {
    hex(v.as_str().unwrap()).unwrap()
}

fn output_json(o: &Output) -> Value {
    json!({"onetime_address": hx(&o.onetime_address), "amount_commitment": hx(&o.amount_commitment),
           "amount_enc": hx(&o.amount_enc), "view_tag": hx(&o.view_tag), "anchor_enc": hx(&o.anchor_enc)})
}

fn prefix_fields(p: &TxPrefix) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert("version".into(), json!(p.version));
    m.insert(
        "inputs".into(),
        json!(p
            .inputs
            .iter()
            .map(|i| json!({"key_image": hx(&i.key_image)}))
            .collect::<Vec<_>>()),
    );
    m.insert(
        "outputs".into(),
        json!(p.outputs.iter().map(output_json).collect::<Vec<_>>()),
    );
    m.insert(
        "ephemeral_pubkeys".into(),
        json!(p
            .ephemeral_pubkeys
            .iter()
            .map(|k| hx(k))
            .collect::<Vec<_>>()),
    );
    m.insert("fee".into(), json!(p.fee));
    m.insert("encrypted_payment_id".into(), hx(&p.encrypted_payment_id));
    m
}

fn tx_json(t: &Transaction) -> Value {
    let mut m = prefix_fields(&t.prefix);
    m.insert(
        "reference_height".into(),
        json!(t.prunable.reference_height),
    );
    m.insert("proof_data".into(), hx(&t.prunable.proof_data));
    Value::Object(m)
}

fn pruned_json(t: &PrunedTransaction) -> Value {
    let mut m = prefix_fields(&t.prefix);
    m.insert("prunable_hash".into(), hx(&t.prunable_hash));
    Value::Object(m)
}

fn coinbase_json(c: &Coinbase) -> Value {
    json!({"version": c.version, "height": c.height, "extra": hx(&c.extra),
           "outputs": c.outputs.iter().map(|o| json!({"onetime_address": hx(&o.onetime_address), "amount": o.amount,
               "view_tag": hx(&o.view_tag), "ephemeral_pubkey": hx(&o.ephemeral_pubkey), "anchor_enc": hx(&o.anchor_enc)}))
               .collect::<Vec<_>>()})
}

fn header_json(h: &BlockHeader) -> Value {
    json!({"version": h.version, "prev_id": hx(&h.prev_id), "timestamp": h.timestamp, "tx_root": hx(&h.tx_root),
           "nonce": h.nonce, "mix": hx(&h.mix)})
}

/// Decodes `data` as `kind`, re-encodes it, and gives its JSON form.
fn round_trip(kind: &str, data: &[u8]) -> Result<Value, DecodeError> {
    fn rt<T: Wire>(data: &[u8], f: impl Fn(&T) -> Value) -> Result<Value, DecodeError> {
        let v = T::from_bytes(data)?;
        assert_eq!(v.to_bytes().unwrap(), data, "re-encodes to the same bytes");
        Ok(f(&v))
    }
    match kind {
        "output" => rt(data, output_json),
        "transaction" => rt(data, tx_json),
        "pruned_transaction" => rt(data, pruned_json),
        "coinbase" => rt(data, coinbase_json),
        "header" => rt(data, header_json),
        "block" => rt(data, |b: &Block| {
            json!({"header": header_json(&b.header), "coinbase": coinbase_json(&b.coinbase),
                   "transactions": b.transactions.iter().map(tx_json).collect::<Vec<_>>()})
        }),
        "pruned_block" => rt(data, |b: &PrunedBlock| {
            json!({"header": header_json(&b.header), "coinbase": coinbase_json(&b.coinbase),
                   "transactions": b.transactions.iter().map(pruned_json).collect::<Vec<_>>()})
        }),
        k => panic!("kind {k}"),
    }
}

#[test]
fn every_valid_object_decodes_to_the_reference_s_object_and_back() {
    let f = load("v3_serialization").unwrap();
    assert_eq!(f["limits"]["OUTPUT_SIZE"], OUTPUT_SIZE);
    for c in f["valid"].as_array().unwrap() {
        let got = round_trip(c["kind"].as_str().unwrap(), &bytes_of(&c["hex"])).unwrap();
        assert_eq!(got, c["object"], "{}", c["note"]);
    }
}

#[test]
fn every_invalid_encoding_fails_as_the_reference_says() {
    for c in load("v3_serialization").unwrap()["invalid"]
        .as_array()
        .unwrap()
    {
        let err = round_trip(c["kind"].as_str().unwrap(), &bytes_of(&c["hex"])).unwrap_err();
        assert_eq!(err.as_str(), c["error"].as_str().unwrap(), "{}", c["note"]);
    }
}

#[test]
fn a_prefix_with_the_wrong_number_of_ephemeral_keys_cannot_be_encoded() {
    let f = load("v3_serialization").unwrap();
    let two = f["valid"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == "transaction")
        .unwrap();
    let mut t = Transaction::from_bytes(&bytes_of(&two["hex"])).unwrap();
    t.prefix.ephemeral_pubkeys.push([7; 32]);
    assert_eq!(t.to_bytes(), Err(EncodeError::CountOutOfRange));
}

#[test]
fn the_ids_and_the_proof_message_are_the_reference_s() {
    let f = load("v3_ids").unwrap();
    for c in f["transactions"].as_array().unwrap() {
        let t = Transaction::from_bytes(&round_trip_bytes(&c["transaction"])).unwrap();
        assert_eq!(
            hex_lower(&ids::prunable_hash(&t.prunable).unwrap()),
            c["prunable_hash"],
            "{}",
            c["note"]
        );
        assert_eq!(hex_lower(&ids::tx_id(&t).unwrap()), c["tx_id"]);
        assert_eq!(
            hex_lower(&ids::pruned_tx_id(&t.prune().unwrap()).unwrap()),
            c["pruned_tx_id"]
        );
        let m = &c["proof_message"];
        let pseudo: Vec<[u8; 32]> = m["pseudo_outs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| bytes_of(p).try_into().unwrap())
            .collect();
        let chain: [u8; 32] = bytes_of(&m["chain_id"]).try_into().unwrap();
        let msg = ids::proof_message(&chain, &t, &pseudo, &bytes_of(&m["range_proof"])).unwrap();
        assert_eq!(hex_lower(&msg), m["message"]);
    }
    let cb: Coinbase = from_json_coinbase(&f["coinbase"]["coinbase"]);
    assert_eq!(
        hex_lower(&ids::coinbase_id(&cb).unwrap()),
        f["coinbase"]["coinbase_id"]
    );
    let h = &f["header"];
    let hdr = BlockHeader::from_bytes(&header_bytes(&h["header"])).unwrap();
    assert_eq!(hex_lower(&ids::header_hash(&hdr)), h["header_hash"]);
    assert_eq!(
        hex_lower(&ids::block_id(&hdr, PowKind::Matmul)),
        h["block_id_matmul"]
    );
    assert_eq!(
        hex_lower(&ids::block_id(&hdr, PowKind::Sha256)),
        h["block_id_sha256"]
    );
}

/// A transaction from the JSON object form (by re-encoding it the way the reference does).
fn round_trip_bytes(t: &Value) -> Vec<u8> {
    let p = TxPrefix {
        version: t["version"].as_u64().unwrap() as u16,
        inputs: t["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| Input {
                key_image: bytes_of(&i["key_image"]).try_into().unwrap(),
            })
            .collect(),
        outputs: t["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| Output {
                onetime_address: bytes_of(&o["onetime_address"]).try_into().unwrap(),
                amount_commitment: bytes_of(&o["amount_commitment"]).try_into().unwrap(),
                amount_enc: bytes_of(&o["amount_enc"]).try_into().unwrap(),
                view_tag: bytes_of(&o["view_tag"]).try_into().unwrap(),
                anchor_enc: bytes_of(&o["anchor_enc"]).try_into().unwrap(),
            })
            .collect(),
        ephemeral_pubkeys: t["ephemeral_pubkeys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| bytes_of(k).try_into().unwrap())
            .collect(),
        fee: t["fee"].as_u64().unwrap(),
        encrypted_payment_id: bytes_of(&t["encrypted_payment_id"]).try_into().unwrap(),
    };
    Transaction {
        prefix: p,
        prunable: Prunable {
            reference_height: t["reference_height"].as_u64().unwrap(),
            proof_data: bytes_of(&t["proof_data"]),
        },
    }
    .to_bytes()
    .unwrap()
}

fn from_json_coinbase(c: &Value) -> Coinbase {
    Coinbase {
        version: c["version"].as_u64().unwrap() as u16,
        height: c["height"].as_u64().unwrap(),
        outputs: c["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| CoinbaseOutput {
                onetime_address: bytes_of(&o["onetime_address"]).try_into().unwrap(),
                amount: o["amount"].as_u64().unwrap(),
                view_tag: bytes_of(&o["view_tag"]).try_into().unwrap(),
                ephemeral_pubkey: bytes_of(&o["ephemeral_pubkey"]).try_into().unwrap(),
                anchor_enc: bytes_of(&o["anchor_enc"]).try_into().unwrap(),
            })
            .collect(),
        extra: bytes_of(&c["extra"]),
    }
}

fn header_bytes(h: &Value) -> Vec<u8> {
    BlockHeader {
        version: h["version"].as_u64().unwrap() as u16,
        prev_id: bytes_of(&h["prev_id"]).try_into().unwrap(),
        timestamp: h["timestamp"].as_u64().unwrap(),
        tx_root: bytes_of(&h["tx_root"]).try_into().unwrap(),
        nonce: h["nonce"].as_u64().unwrap(),
        mix: bytes_of(&h["mix"]).try_into().unwrap(),
    }
    .to_bytes()
    .unwrap()
}

#[test]
fn the_genesis_of_gamma_dev_and_test() {
    let cases = load("v3_genesis").unwrap();
    let labels = [ids::GAMMA_LABEL, ids::DEV_LABEL, ids::TEST_LABEL];
    for (c, label) in cases["cases"].as_array().unwrap().iter().zip(labels) {
        assert_eq!(c["label"], label);
        assert_eq!(
            hex_lower(&ids::genesis_header(label).to_bytes().unwrap()),
            c["header_hex"]
        );
        assert_eq!(hex_lower(&ids::genesis_id(label)), c["chain_id"]);
    }
}

#[test]
fn weight_is_the_reference_s() {
    let f = load("v3_weight").unwrap();
    assert_eq!(f["proof_weight_divisor"], rules::PROOF_WEIGHT_DIVISOR);
    assert_eq!(f["fee_reference_weight"], rules::FEE_REFERENCE_WEIGHT);
    for c in f["transactions"].as_array().unwrap() {
        let t = Transaction::from_bytes(&bytes_of(&c["hex"])).unwrap();
        assert_eq!(
            rules::tx_weight(&t).unwrap(),
            c["weight"].as_u64().unwrap(),
            "{}",
            c["note"]
        );
    }
    for c in f["fees"].as_array().unwrap() {
        let fee = rules::min_fee(
            c["size"].as_u64().unwrap(),
            c["base_reward"].as_u64().unwrap(),
            c["median"].as_u64().unwrap(),
        )
        .unwrap();
        assert_eq!(fee, c["fee"].as_u64().unwrap());
    }
    assert_eq!(f["max_block_weight"], rules::MAX_BLOCK_WEIGHT);
    assert_eq!(f["max_block_bytes"], rules::MAX_BLOCK_BYTES);
    for c in f["limits"].as_array().unwrap() {
        let n = |k: &str| c[k].as_u64().unwrap();
        assert_eq!(
            rules::block_too_large(n("weight"), n("size"), n("median")),
            c["too_large"].as_bool().unwrap(),
            "{c}"
        );
    }
}

#[test]
fn the_shape_rules_and_the_reference_heights_are_the_reference_s() {
    let f = load("v3_shape").unwrap();
    assert_eq!(f["max_reference_age"], rules::MAX_REFERENCE_AGE);
    for c in f["transactions"].as_array().unwrap() {
        let t = Transaction::from_bytes(&bytes_of(&c["hex"])).unwrap();
        let got = rules::shape(&t.prefix).err().map(|e| e.as_str());
        assert_eq!(got, c["error"].as_str(), "{}", c["note"]);
    }
    for c in f["coinbases"].as_array().unwrap() {
        let cb = Coinbase::from_bytes(&bytes_of(&c["hex"])).unwrap();
        assert_eq!(
            rules::coinbase_shape(&cb).err().map(|e| e.as_str()),
            c["error"].as_str(),
            "{}",
            c["note"]
        );
    }
    for c in f["reference"].as_array().unwrap() {
        let ok = rules::reference_ok(
            c["reference_height"].as_u64().unwrap(),
            c["block_height"].as_u64().unwrap(),
            c["tree_leaves"].as_u64().unwrap(),
        );
        assert_eq!(ok, c["ok"].as_bool().unwrap(), "{c}");
    }
}

#[test]
fn the_tree_schedule_is_the_reference_s() {
    let f = load("v3_tree_schedule").unwrap();
    let blocks: Vec<(u64, u64)> = f["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            (
                b["coinbase_outputs"].as_u64().unwrap(),
                b["other_outputs"].as_u64().unwrap(),
            )
        })
        .collect();
    let mut first = vec![];
    let mut n = 0;
    for (cb, other) in &blocks {
        first.push(n);
        n += cb + other;
    }
    for (height, want) in f["entering"].as_array().unwrap().iter().enumerate() {
        let (cb_from, other_from) = entering_from(height as u64);
        let mut got: Vec<u64> = vec![];
        if let Some(h) = cb_from {
            let h = h as usize;
            got.extend(first[h]..first[h] + blocks[h].0);
        }
        if let Some(h) = other_from {
            let h = h as usize;
            got.extend(first[h] + blocks[h].0..first[h] + blocks[h].0 + blocks[h].1);
        }
        got.sort_unstable();
        let want: Vec<u64> = want
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_u64().unwrap())
            .collect();
        assert_eq!(got, want, "height {height}");
    }
}

#[test]
fn the_long_term_median_is_the_reference_s() {
    let f = load("v3_median").unwrap();
    assert_eq!(f["min_block_median"], rules::MIN_BLOCK_MEDIAN);
    assert_eq!(f["median_window"], tenero_core::fees::MEDIAN_WINDOW);
    assert_eq!(f["long_term_window"], rules::LONG_TERM_WINDOW);
    assert_eq!(f["short_term_multiple"], rules::SHORT_TERM_MULTIPLE);
    assert_eq!(
        f["long_term_growth"],
        serde_json::json!([rules::LONG_TERM_GROWTH_NUM, rules::LONG_TERM_GROWTH_DEN])
    );
    for c in f["cases"].as_array().unwrap() {
        let window = c["window"].as_u64().unwrap() as usize;
        let (mut weights, mut lts) = (Vec::new(), Vec::new());
        for (d, b) in c["demand"]
            .as_array()
            .unwrap()
            .iter()
            .zip(c["blocks"].as_array().unwrap())
        {
            let ltm = rules::long_term_median(&lts, window);
            let median = rules::effective_median(&weights, ltm);
            let weight = d.as_u64().unwrap().min(rules::block_limit(median));
            let lt = rules::long_term_weight(weight, ltm);
            let n = |k: &str| b[k].as_u64().unwrap();
            assert_eq!(
                (median, ltm, weight, lt),
                (
                    n("median"),
                    n("long_term_median"),
                    n("weight"),
                    n("long_term_weight")
                ),
                "{}",
                c["note"]
            );
            weights.push(weight);
            lts.push(lt);
        }
    }
}
