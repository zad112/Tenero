//! The pool protocol's messages (a DRAFT) against the independent Python reference's vectors
//! (`reference/tools/make_vectors_pool.py`, `tests/vectors/pool.json`): every message must encode to exactly the
//! reference's bytes, decode back to the same message, and every malformed one must be refused with the error the
//! reference gives.

use serde_json::Value;
use tenero_app::pool::{
    frame, frame_len, DeclareJob, DeclareOutcome, Hello, HelloOk, Job, MinerMessage, PoolError,
    PoolMessage, HEADER_LEN, MAX_ADDRESS, MAX_AGENT, MAX_FRAME, MAX_NETWORK, MAX_POOL_NAME,
    MAX_PREFIX_BITS, MAX_PROVIDED_TXS, MAX_TEXT, MAX_TTL, MAX_TX_IDS, MAX_WORKER,
};
use tenero_core::v3::Wire;
use tenero_core::v3::{
    BlockHeader, Coinbase, CoinbaseOutput, Input, Output, Prunable, Transaction, TxPrefix,
};

fn vectors() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/vectors/pool.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

fn arr<const N: usize>(v: &Value) -> [u8; N] {
    unhex(v.as_str().unwrap()).try_into().unwrap()
}

fn u(v: &Value) -> u64 {
    v.as_u64().unwrap()
}

fn s(v: &Value) -> String {
    v.as_str().unwrap().to_string()
}

fn ids(v: &Value) -> Vec<[u8; 32]> {
    v.as_array().unwrap().iter().map(arr).collect()
}

fn output(v: &Value) -> Output {
    Output {
        onetime_address: arr(&v["onetime_address"]),
        amount_commitment: arr(&v["amount_commitment"]),
        amount_enc: arr(&v["amount_enc"]),
        view_tag: arr(&v["view_tag"]),
        anchor_enc: arr(&v["anchor_enc"]),
    }
}

fn tx(v: &Value) -> Transaction {
    Transaction {
        prefix: TxPrefix {
            version: u(&v["version"]) as u16,
            inputs: v["inputs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| Input {
                    key_image: arr(&i["key_image"]),
                })
                .collect(),
            outputs: v["outputs"]
                .as_array()
                .unwrap()
                .iter()
                .map(output)
                .collect(),
            ephemeral_pubkeys: v["ephemeral_pubkeys"]
                .as_array()
                .unwrap()
                .iter()
                .map(arr)
                .collect(),
            fee: u(&v["fee"]),
            encrypted_payment_id: arr(&v["encrypted_payment_id"]),
        },
        prunable: Prunable {
            reference_height: u(&v["reference_height"]),
            proof_data: unhex(v["proof_data"].as_str().unwrap()),
        },
    }
}

fn coinbase(v: &Value) -> Coinbase {
    Coinbase {
        version: u(&v["version"]) as u16,
        height: u(&v["height"]),
        outputs: v["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| CoinbaseOutput {
                onetime_address: arr(&o["onetime_address"]),
                amount: u(&o["amount"]),
                view_tag: arr(&o["view_tag"]),
                ephemeral_pubkey: arr(&o["ephemeral_pubkey"]),
                anchor_enc: arr(&o["anchor_enc"]),
            })
            .collect(),
        extra: unhex(v["extra"].as_str().unwrap()),
    }
}

fn header(v: &Value) -> BlockHeader {
    BlockHeader {
        version: u(&v["version"]) as u16,
        prev_id: arr(&v["prev_id"]),
        timestamp: u(&v["timestamp"]),
        tx_root: arr(&v["tx_root"]),
        nonce: u(&v["nonce"]),
        mix: arr(&v["mix"]),
    }
}

fn miner(m: &Value) -> MinerMessage {
    match m["type"].as_str().unwrap() {
        "hello" => MinerMessage::Hello(Hello {
            min_version: u(&m["min_version"]) as u16,
            max_version: u(&m["max_version"]) as u16,
            capabilities: u(&m["capabilities"]) as u32,
            network: s(&m["network"]),
            address: s(&m["address"]),
            worker: s(&m["worker"]),
            agent: s(&m["agent"]),
        }),
        "submit_share" => MinerMessage::SubmitShare {
            job_id: u(&m["job_id"]),
            nonce: u(&m["nonce"]),
            mix: arr(&m["mix"]),
        },
        "ping" => MinerMessage::Ping {
            token: u(&m["token"]),
        },
        "declare_job" => MinerMessage::DeclareJob(DeclareJob {
            decl_id: u(&m["decl_id"]),
            height: u(&m["height"]),
            prev_id: arr(&m["prev_id"]),
            timestamp: u(&m["timestamp"]),
            coinbase: coinbase(&m["coinbase"]),
            tx_ids: ids(&m["tx_ids"]),
        }),
        "provide_txs" => MinerMessage::ProvideTxs {
            decl_id: u(&m["decl_id"]),
            txs: m["txs"].as_array().unwrap().iter().map(tx).collect(),
        },
        other => panic!("unknown miner message {other}"),
    }
}

fn pool(m: &Value) -> PoolMessage {
    match m["type"].as_str().unwrap() {
        "hello_ok" => PoolMessage::HelloOk(HelloOk {
            version: u(&m["version"]) as u16,
            capabilities: u(&m["capabilities"]) as u32,
            session: u(&m["session"]),
            prefix_bits: u(&m["prefix_bits"]) as u8,
            prefix: u(&m["prefix"]),
            share_target: arr(&m["share_target"]),
            pool_name: s(&m["pool_name"]),
            pays_pool: m["pays_pool"].as_bool().unwrap(),
        }),
        "share_result" => PoolMessage::ShareResult {
            job_id: u(&m["job_id"]),
            accepted: m["accepted"].as_bool().unwrap(),
            reason: u(&m["reason"]) as u8,
            text: s(&m["text"]),
        },
        "pong" => PoolMessage::Pong {
            token: u(&m["token"]),
        },
        "declare_result" => PoolMessage::DeclareResult {
            decl_id: u(&m["decl_id"]),
            outcome: match m["status"].as_str().unwrap() {
                "accepted" => DeclareOutcome::Accepted {
                    job_id: u(&m["job_id"]),
                    header: header(&m["header"]),
                },
                "refused" => DeclareOutcome::Refused {
                    reason: u(&m["reason"]) as u8,
                    text: s(&m["text"]),
                },
                "missing" => DeclareOutcome::Missing(ids(&m["missing"])),
                other => panic!("unknown declare status {other}"),
            },
        },
        "job" => PoolMessage::Job(Job {
            job_id: u(&m["job_id"]),
            height: u(&m["height"]),
            clean: m["clean"].as_bool().unwrap(),
            header: header(&m["header"]),
            block_target: arr(&m["block_target"]),
            ttl: u(&m["ttl"]) as u32,
        }),
        "set_share_target" => PoolMessage::SetShareTarget {
            share_target: arr(&m["share_target"]),
        },
        "set_payout" => PoolMessage::SetPayout {
            height: u(&m["height"]),
            spend_pubkey: arr(&m["spend_pubkey"]),
            view_pubkey: arr(&m["view_pubkey"]),
            anchor: arr(&m["anchor"]),
        },
        "error" => PoolMessage::Error(s(&m["message"])),
        other => panic!("unknown pool message {other}"),
    }
}

#[test]
fn the_limits_are_the_references() {
    let v = vectors();
    let l = &v["limits"];
    let want = [
        ("max_frame", MAX_FRAME),
        ("max_network", MAX_NETWORK),
        ("max_address", MAX_ADDRESS),
        ("max_worker", MAX_WORKER),
        ("max_agent", MAX_AGENT),
        ("max_pool_name", MAX_POOL_NAME),
        ("max_text", MAX_TEXT),
        ("max_tx_ids", MAX_TX_IDS),
        ("max_provided_txs", MAX_PROVIDED_TXS),
        ("max_ttl", MAX_TTL as usize),
        ("max_prefix_bits", MAX_PREFIX_BITS as usize),
        ("header_len", HEADER_LEN),
    ];
    for (name, ours) in want {
        assert_eq!(u(&l[name]) as usize, ours, "{name}");
    }
    // and the header really is that long
    let h = BlockHeader {
        version: 2,
        prev_id: [0; 32],
        timestamp: 0,
        tx_root: [0; 32],
        nonce: 0,
        mix: [0; 64],
    };
    assert_eq!(h.to_bytes().unwrap().len(), HEADER_LEN);
}

#[test]
fn every_valid_message_encodes_to_the_references_bytes_and_decodes_back() {
    let v = vectors();
    let cases = v["valid"].as_array().unwrap();
    assert!(cases.len() >= 30);
    for c in cases {
        let note = c["note"].as_str().unwrap();
        let body = unhex(c["body"].as_str().unwrap());
        let framed = unhex(c["frame"].as_str().unwrap());
        if c["direction"] == "miner" {
            let m = miner(&c["message"]);
            assert_eq!(m.to_body().unwrap(), body, "miner: {note}");
            assert_eq!(MinerMessage::from_body(&body).unwrap(), m, "miner: {note}");
        } else {
            let m = pool(&c["message"]);
            assert_eq!(m.to_body().unwrap(), body, "pool: {note}");
            assert_eq!(PoolMessage::from_body(&body).unwrap(), m, "pool: {note}");
        }
        assert_eq!(frame(&body).unwrap(), framed, "{note}");
    }
}

#[test]
fn every_kind_number_is_the_references() {
    let v = vectors();
    for (dir, key) in [("miner", "miner"), ("pool", "pool")] {
        for (name, kind) in v["kinds"][key].as_object().unwrap() {
            let c = v["valid"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["direction"] == dir && c["message"]["type"] == name.as_str())
                .unwrap();
            assert_eq!(
                unhex(c["body"].as_str().unwrap())[0] as u64,
                u(kind),
                "{name}"
            );
        }
    }
    assert_eq!(u(&v["kinds"]["error"]), 0xFF);
}

#[test]
fn every_malformed_message_is_refused_with_the_references_error() {
    let v = vectors();
    let cases = v["invalid"].as_array().unwrap();
    assert!(cases.len() >= 100);
    for c in cases {
        let note = c["note"].as_str().unwrap();
        let body = unhex(c["body"].as_str().unwrap());
        let want = c["error"].as_str().unwrap();
        let got = if c["direction"] == "miner" {
            MinerMessage::from_body(&body).err()
        } else {
            PoolMessage::from_body(&body).err()
        };
        let got = got.unwrap_or_else(|| panic!("accepted: {note}"));
        let ok = match want {
            "length" => matches!(got, PoolError::BadLength(_)),
            "kind" => matches!(got, PoolError::UnknownKind(_)),
            "trailing" => matches!(got, PoolError::Trailing),
            "malformed" => matches!(got, PoolError::Decode(_)),
            other => panic!("unknown error class {other}"),
        };
        assert!(ok, "{note}: wanted {want}, got {got:?}");
    }
}

#[test]
fn a_message_of_one_direction_is_an_unknown_kind_in_the_other() {
    let v = vectors();
    for c in v["valid"].as_array().unwrap() {
        let note = c["note"].as_str().unwrap();
        let body = unhex(c["body"].as_str().unwrap());
        if c["direction"] == "miner" {
            assert!(
                matches!(
                    PoolMessage::from_body(&body),
                    Err(PoolError::UnknownKind(_))
                ),
                "{note}"
            );
        } else {
            assert!(
                matches!(
                    MinerMessage::from_body(&body),
                    Err(PoolError::UnknownKind(_))
                ),
                "{note}"
            );
        }
    }
}

#[test]
fn the_frame_length_rule_is_the_references() {
    let v = vectors();
    for c in v["frames"].as_array().unwrap() {
        let n = u(&c["length"]) as u32;
        let got = frame_len(n.to_le_bytes());
        assert_eq!(got.is_ok(), c["ok"].as_bool().unwrap(), "{}", c["note"]);
        if let Ok(len) = got {
            assert_eq!(len, n as usize);
        }
    }
}
