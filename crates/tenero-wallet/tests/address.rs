//! Version 3 addresses and keys against the independent Python reference (`tests/vectors/v3_address.json`).

use tenero_core::vectors::{hex, load};
use tenero_wallet::purse::account_seed;
use tenero_wallet::{carrot_master, Address, Kind, Network};

fn network(name: &str) -> Network {
    match name {
        "gamma" => Network::Gamma,
        "dev" => Network::Dev,
        "test" => Network::Test,
        other => panic!("{other}"),
    }
}

fn b32(v: &serde_json::Value) -> [u8; 32] {
    hex(v.as_str().unwrap()).unwrap().try_into().unwrap()
}

#[test]
fn addresses_encode_and_decode_as_the_reference_says() {
    let v = load("v3_address").unwrap();
    for (net, prefix) in v["prefixes"].as_object().unwrap() {
        assert_eq!(network(net).prefix(), prefix.as_str().unwrap());
    }
    for c in v["valid"].as_array().unwrap() {
        let net = network(c["network"].as_str().unwrap());
        let kind = match c["kind"].as_str().unwrap() {
            "main" => Kind::Main,
            "subaddress" => Kind::Subaddress,
            _ => Kind::Integrated,
        };
        let a = Address {
            network: net,
            kind,
            spend_pubkey: b32(&c["spend"]),
            view_pubkey: b32(&c["view"]),
            payment_id: c["payment_id"]
                .as_str()
                .map_or([0; 8], |p| hex(p).unwrap().try_into().unwrap()),
        };
        let text = c["text"].as_str().unwrap();
        assert_eq!(a.to_text(), text);
        assert_eq!(Address::parse(text, net), Ok(a));
        assert!(text.starts_with(net.prefix()));
    }
    for c in v["invalid"].as_array().unwrap() {
        let got = Address::parse(
            c["text"].as_str().unwrap(),
            network(c["network"].as_str().unwrap()),
        );
        assert_eq!(
            got.map_err(|e| e.as_str()),
            Err(c["error"].as_str().unwrap()),
            "{}",
            c["note"]
        );
    }
}

#[test]
fn the_carrot_master_secret_is_the_reference_s() {
    let v = load("v3_address").unwrap();
    for k in v["keys"].as_array().unwrap() {
        let seed = b32(&k["seed"]);
        let acct = account_seed(&seed, k["account"].as_u64().unwrap() as u32);
        assert_eq!(*acct, b32(&k["account_seed"]));
        assert_eq!(*carrot_master(&acct), b32(&k["carrot_master"]));
    }
}
