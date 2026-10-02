//! The control protocol against the independent Python reference's vectors (`tools/make_vectors_control.py`,
//! `tests/vectors/control.json`): every message must encode to exactly the reference's bytes, decode back to the same
//! message, and every malformed one must be refused with the error the reference gives.

use serde_json::Value;
use tenero_app::control::{
    frame, frame_len, ControlError, NodeInfo, NodeKind, Request, Response, Template,
    MAX_BLOCKS_PER_REQUEST, MAX_FRAME, MAX_NAME, MAX_TEXT,
};
use tenero_core::v2::{
    Block, BlockHeader, Coinbase, CoinbaseOutput, Input, Output, Prunable, Transaction, TxPrefix,
};
use tenero_node::Payout;
use tenero_store::StoredOutput;
use tenero_wallet::{Rules, ScanBlock};

fn vectors() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/vectors/control.json"
    );
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

fn output(v: &Value) -> Output {
    Output {
        onetime_address: arr(&v["onetime_address"]),
        amount_commitment: arr(&v["amount_commitment"]),
        amount_enc: arr(&v["amount_enc"]),
        view_tag: arr(&v["view_tag"]),
        ephemeral_pubkey: arr(&v["ephemeral_pubkey"]),
        anchor_enc: arr(&v["anchor_enc"]),
    }
}

fn prefix(v: &Value) -> TxPrefix {
    TxPrefix {
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
        fee: u(&v["fee"]),
        extra: unhex(v["extra"].as_str().unwrap()),
    }
}

fn tx(v: &Value) -> Transaction {
    Transaction {
        prefix: prefix(v),
        prunable: Prunable {
            rings: v["rings"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r.as_array().unwrap().iter().map(u).collect())
                .collect(),
            proof_data: unhex(v["proof_data"].as_str().unwrap()),
        },
    }
}

fn cb_output(v: &Value) -> CoinbaseOutput {
    CoinbaseOutput {
        onetime_address: arr(&v["onetime_address"]),
        amount: u(&v["amount"]),
        view_tag: arr(&v["view_tag"]),
        ephemeral_pubkey: arr(&v["ephemeral_pubkey"]),
        anchor_enc: arr(&v["anchor_enc"]),
    }
}

fn scan_block(v: &Value) -> ScanBlock {
    let cb = &v["coinbase"];
    ScanBlock {
        height: u(&v["height"]),
        id: arr(&v["id"]),
        first_output_index: u(&v["first_output_index"]),
        coinbase: Coinbase {
            version: u(&cb["version"]) as u16,
            height: u(&cb["height"]),
            outputs: cb["outputs"]
                .as_array()
                .unwrap()
                .iter()
                .map(cb_output)
                .collect(),
            extra: unhex(cb["extra"].as_str().unwrap()),
        },
        txs: v["txs"].as_array().unwrap().iter().map(prefix).collect(),
    }
}

fn block(v: &Value) -> Block {
    let h = &v["header"];
    let cb = &v["coinbase"];
    Block {
        header: BlockHeader {
            version: u(&h["version"]) as u16,
            prev_id: arr(&h["prev_id"]),
            timestamp: u(&h["timestamp"]),
            tx_root: arr(&h["tx_root"]),
            nonce: u(&h["nonce"]),
            mix: arr(&h["mix"]),
        },
        coinbase: Coinbase {
            version: u(&cb["version"]) as u16,
            height: u(&cb["height"]),
            outputs: cb["outputs"]
                .as_array()
                .unwrap()
                .iter()
                .map(cb_output)
                .collect(),
            extra: unhex(cb["extra"].as_str().unwrap()),
        },
        transactions: v["transactions"]
            .as_array()
            .unwrap()
            .iter()
            .map(tx)
            .collect(),
    }
}

fn request(m: &Value) -> Request {
    match m["type"].as_str().unwrap() {
        "auth" => Request::Auth {
            cookie: arr(&m["cookie"]),
        },
        "tip" => Request::Tip,
        "block" => Request::Block {
            height: u(&m["height"]),
        },
        "output" => Request::Output {
            index: u(&m["index"]),
        },
        "output_count" => Request::OutputCount,
        "key_image_spent" => Request::KeyImageSpent {
            key_image: arr(&m["key_image"]),
        },
        "rules" => Request::Rules,
        "submit_tx" => Request::SubmitTx(tx(&m["tx"])),
        "info" => Request::Info,
        "stop" => Request::Stop,
        "blocks" => Request::Blocks {
            from: u(&m["from"]),
            count: u(&m["count"]) as u16,
        },
        "block_template" => Request::BlockTemplate {
            payout: Payout {
                onetime_address: arr(&m["payout"]["onetime_address"]),
                view_tag: arr(&m["payout"]["view_tag"]),
                ephemeral_pubkey: arr(&m["payout"]["ephemeral_pubkey"]),
                anchor_enc: arr(&m["payout"]["anchor_enc"]),
            },
            max_body_bytes: u(&m["max_body_bytes"]) as u32,
        },
        "submit_block" => Request::SubmitBlock(block(&m["block"])),
        other => panic!("unknown request type {other}"),
    }
}

fn response(m: &Value) -> Response {
    match m["type"].as_str().unwrap() {
        "authed" => Response::Authed,
        "tip" => Response::Tip {
            height: u(&m["height"]),
            id: arr(&m["id"]),
        },
        "block" => Response::Block(if m["block"].is_null() {
            None
        } else {
            Some(scan_block(&m["block"]))
        }),
        "output" => Response::Output(if m["output"].is_null() {
            None
        } else {
            let o = &m["output"];
            Some(StoredOutput {
                onetime_address: arr(&o["onetime_address"]),
                amount_commitment: arr(&o["amount_commitment"]),
                public_amount: u(&o["public_amount"]),
                height: u(&o["height"]),
                coinbase: o["coinbase"].as_bool().unwrap(),
            })
        }),
        "output_count" => Response::OutputCount(u(&m["count"])),
        "spent" => Response::Spent(m["spent"].as_bool().unwrap()),
        "rules" => Response::Rules(Rules {
            chain_id: arr(&m["chain_id"]),
            ring_size: u(&m["ring_size"]) as usize,
            coinbase_maturity: u(&m["coinbase_maturity"]),
            spend_maturity: u(&m["spend_maturity"]),
            next_height: u(&m["next_height"]),
            reward: u(&m["reward"]),
            median: u(&m["median"]),
        }),
        "tx_accepted" => Response::TxAccepted { id: arr(&m["id"]) },
        "info" => Response::Info(NodeInfo {
            height: u(&m["height"]),
            tip_id: arr(&m["tip_id"]),
            peers: u(&m["peers"]) as u32,
            inbound: u(&m["inbound"]) as u32,
            pruned_below: u(&m["pruned_below"]),
            mempool_txs: u(&m["mempool_txs"]) as u32,
            syncing: m["syncing"].as_bool().unwrap(),
            kind: match m["kind"].as_str().unwrap() {
                "archive" => NodeKind::Archive,
                _ => NodeKind::Pruned,
            },
            network: m["network"].as_str().unwrap().to_string(),
            version: m["version"].as_str().unwrap().to_string(),
        }),
        "stopping" => Response::Stopping,
        "blocks" => Response::Blocks(
            m["blocks"]
                .as_array()
                .unwrap()
                .iter()
                .map(scan_block)
                .collect(),
        ),
        "template" => Response::Template(Template {
            block: block(&m["block"]),
            height: u(&m["height"]),
            target: arr(&m["target"]),
        }),
        "block_submitted" => Response::BlockSubmitted {
            id: arr(&m["id"]),
            in_chain: m["in_chain"].as_bool().unwrap(),
        },
        "error" => Response::Error(m["message"].as_str().unwrap().to_string()),
        other => panic!("unknown response type {other}"),
    }
}

#[test]
fn the_limits_are_the_references() {
    let v = vectors();
    let l = &v["limits"];
    assert_eq!(u(&l["max_frame"]) as usize, MAX_FRAME);
    assert_eq!(u(&l["max_text"]) as usize, MAX_TEXT);
    assert_eq!(u(&l["max_name"]) as usize, MAX_NAME);
    assert_eq!(
        u(&l["max_blocks_per_request"]) as u16,
        MAX_BLOCKS_PER_REQUEST
    );
}

#[test]
fn every_valid_message_encodes_to_the_references_bytes_and_decodes_back() {
    let v = vectors();
    let cases = v["valid"].as_array().unwrap();
    assert!(cases.len() >= 35);
    for c in cases {
        let note = c["note"].as_str().unwrap();
        let body = unhex(c["body"].as_str().unwrap());
        let framed = unhex(c["frame"].as_str().unwrap());
        if c["direction"] == "request" {
            let m = request(&c["message"]);
            assert_eq!(m.to_body().unwrap(), body, "request: {note}");
            assert_eq!(Request::from_body(&body).unwrap(), m, "request: {note}");
        } else {
            let m = response(&c["message"]);
            assert_eq!(m.to_body().unwrap(), body, "response: {note}");
            assert_eq!(Response::from_body(&body).unwrap(), m, "response: {note}");
        }
        assert_eq!(frame(&body).unwrap(), framed, "{note}");
    }
}

#[test]
fn every_kind_number_is_the_references() {
    let v = vectors();
    for (name, kind) in v["kinds"]["requests"].as_object().unwrap() {
        let c = v["valid"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["direction"] == "request" && c["message"]["type"] == name.as_str())
            .unwrap();
        assert_eq!(
            unhex(c["body"].as_str().unwrap())[0] as u64,
            u(kind),
            "{name}"
        );
    }
    for (name, kind) in v["kinds"]["responses"].as_object().unwrap() {
        let c = v["valid"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["direction"] == "response" && c["message"]["type"] == name.as_str())
            .unwrap();
        assert_eq!(
            unhex(c["body"].as_str().unwrap())[0] as u64,
            u(kind),
            "{name}"
        );
    }
    assert_eq!(u(&v["kinds"]["error"]), 0xFF);
}

#[test]
fn every_malformed_message_is_refused_with_the_references_error() {
    let v = vectors();
    let cases = v["invalid"].as_array().unwrap();
    assert!(cases.len() >= 60);
    for c in cases {
        let note = c["note"].as_str().unwrap();
        let body = unhex(c["body"].as_str().unwrap());
        let want = c["error"].as_str().unwrap();
        let got = if c["direction"] == "request" {
            Request::from_body(&body).err()
        } else {
            Response::from_body(&body).err()
        };
        let got = got.unwrap_or_else(|| panic!("accepted: {note}"));
        let ok = match want {
            "length" => matches!(got, ControlError::BadLength(_)),
            "kind" => matches!(got, ControlError::UnknownKind(_)),
            "trailing" => matches!(got, ControlError::Trailing),
            "malformed" => matches!(got, ControlError::Decode(_)),
            other => panic!("unknown error class {other}"),
        };
        assert!(ok, "{note}: wanted {want}, got {got:?}");
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
