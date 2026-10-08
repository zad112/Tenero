//! Payment requests: the link format, strictly read and never half understood.

use tenero_wallet::request::{parse_pay_text, MAX_LABEL, MAX_MESSAGE};
use tenero_wallet::{Address, Network, PaymentRequest, RequestError, Wallet};

fn addr() -> Address {
    Wallet::from_seed(&[1; 32], Network::Test, 0).address()
}

fn uri(rest: &str) -> String {
    format!("tenero:{}{rest}", addr().to_text())
}

#[test]
fn a_request_is_written_and_read_back_in_every_combination() {
    let a = addr();
    for (amount, label, message) in [
        (None, None, None),
        (Some(150_000_000), None, None),
        (None, Some("Rent"), None),
        (None, None, Some("October rent")),
        (Some(1), Some("a"), Some("b")),
        (
            Some(u64::MAX),
            Some("x".repeat(MAX_LABEL).as_str()),
            Some("y".repeat(MAX_MESSAGE).as_str()),
        ),
    ] {
        let r = PaymentRequest {
            address: a,
            amount,
            label: label.map(str::to_string),
            message: message.map(str::to_string),
        };
        let text = r.to_uri();
        assert_eq!(
            PaymentRequest::from_uri(&text, Network::Test).unwrap(),
            r,
            "{text}"
        );
        assert_eq!(
            PaymentRequest::from_uri(&format!("  {text}\n"), Network::Test).unwrap(),
            r,
            "spaces around it do not matter"
        );
    }
    // the exact form
    let r = PaymentRequest {
        address: a,
        amount: Some(150_000_000),
        label: Some("Rent".into()),
        message: Some("October rent".into()),
    };
    assert_eq!(
        r.to_uri(),
        uri("?amount=1.5&label=Rent&message=October%20rent")
    );
    assert_eq!(
        PaymentRequest {
            amount: None,
            label: None,
            message: None,
            ..r
        }
        .to_uri(),
        uri("")
    );
}

#[test]
fn awkward_text_survives_the_encoding_and_cannot_add_a_parameter() {
    for text in [
        "a&b=c",
        "100% sure",
        "plus+sign",
        "ünïcödé 試験 ✓",
        "slash/and?question#hash",
        "~-._ok",
        "  inner  spaces ",
    ] {
        let r = PaymentRequest {
            address: addr(),
            amount: None,
            label: Some(text.trim().to_string()),
            message: Some(text.to_string()),
        };
        let back = PaymentRequest::from_uri(&r.to_uri(), Network::Test).unwrap();
        assert_eq!(back.label.as_deref(), Some(text.trim()));
        assert_eq!(back.message.as_deref(), Some(text));
        assert!(back.amount.is_none(), "`{text}` added a parameter");
    }
    // a label that tries to add an amount is just text
    let r = PaymentRequest {
        address: addr(),
        amount: None,
        label: Some("x&amount=999".into()),
        message: None,
    };
    let back = PaymentRequest::from_uri(&r.to_uri(), Network::Test).unwrap();
    assert_eq!(
        (back.amount, back.label.as_deref()),
        (None, Some("x&amount=999"))
    );
}

#[test]
fn a_link_that_is_not_exactly_a_request_is_refused_and_says_why() {
    let bad = |text: String, pattern: fn(&RequestError) -> bool| {
        let e = PaymentRequest::from_uri(&text, Network::Test).unwrap_err();
        assert!(pattern(&e), "`{text}` gave {e:?}");
    };
    let format = |e: &RequestError| matches!(e, RequestError::Format(_));
    let param = |e: &RequestError| matches!(e, RequestError::Parameter(_));
    bad(String::new(), format);
    bad("hello".into(), format);
    bad(addr().to_text(), format); // no scheme
    bad(format!("TENERO:{}", addr().to_text()), format); // the scheme is lower case
    bad(format!("tenero://{}", addr().to_text()), format);
    bad("bitcoin:1abc".into(), format);
    bad("tenero:".into(), format);
    bad(uri("?"), format);
    bad(uri("?amount"), format);
    bad(uri("?amount=1&"), format);
    bad(uri("?amount=1&amount=2"), param);
    bad(uri("?label=a&label=b"), param);
    bad(uri("?message=a&message=b"), param);
    bad(uri("?colour=red"), param);
    bad(uri("?req-thing=1"), param);
    bad(uri("?AMOUNT=1"), param);
    bad(uri("?amount=0"), |e| matches!(e, RequestError::Amount));
    bad(uri("?amount=0.0"), |e| matches!(e, RequestError::Amount));
    bad(uri("?amount=1.123456789"), |e| {
        matches!(e, RequestError::Amount)
    });
    bad(uri("?amount=-1"), |e| matches!(e, RequestError::Amount));
    bad(uri("?amount=1e3"), |e| matches!(e, RequestError::Amount));
    bad(uri("?amount=%31"), |e| matches!(e, RequestError::Amount));
    bad(uri("?amount=18446744073709551616"), |e| {
        matches!(e, RequestError::Amount)
    });
    bad(uri("?label=%zz"), format);
    bad(uri("?label=%4"), format);
    bad(uri("?label=a b"), format); // a space must be %20
    bad(uri("?label=a+b"), format); // and + is not a space
    bad(uri("?label=%FF"), format); // not UTF-8
    bad(uri("?label=a%0Ab"), |e| matches!(e, RequestError::Text(_)));
    bad(uri("?label=a%00b"), |e| matches!(e, RequestError::Text(_)));
    bad(uri(&format!("?label={}", "x".repeat(MAX_LABEL + 1))), |e| {
        matches!(e, RequestError::Text(_))
    });
    bad(
        uri(&format!("?message={}", "x".repeat(MAX_MESSAGE + 1))),
        |e| matches!(e, RequestError::Text(_)),
    );
    bad(format!("tenero:tni1{}", "0".repeat(136)), |e| {
        matches!(e, RequestError::Address(_))
    });
    let good = addr().to_text();
    bad(format!("tenero:{}", &good[..good.len() - 2]), |e| {
        matches!(e, RequestError::Address(_))
    });
    bad(format!("tenero:{good}x"), |e| {
        matches!(e, RequestError::Address(_))
    });
    // an address of another network is refused, not paid
    let gamma = Wallet::from_seed(&[1; 32], Network::Gamma, 0)
        .address()
        .to_text();
    bad(format!("tenero:{gamma}?amount=1"), |e| {
        matches!(e, RequestError::Address(_))
    });
    bad("x".repeat(2000), format);
    bad(
        format!("tenero:{}?label={}", good, "a".repeat(2000)),
        format,
    );
}

#[test]
fn the_pay_field_takes_a_request_or_a_bare_address_and_nothing_else() {
    let a = addr();
    let bare = parse_pay_text(&format!("  {}\n", a.to_text()), Network::Test).unwrap();
    assert_eq!((bare.address, bare.amount, bare.label), (a, None, None));
    let r = parse_pay_text(&uri("?amount=2&label=Tea"), Network::Test).unwrap();
    assert_eq!(
        (r.amount, r.label.as_deref()),
        (Some(200_000_000), Some("Tea"))
    );
    assert!(parse_pay_text("", Network::Test).is_err());
    assert!(parse_pay_text("tenero:nonsense", Network::Test).is_err());
    assert!(parse_pay_text("tni1abc", Network::Test).is_err());
}

#[test]
fn nothing_panics_on_truncated_or_random_links() {
    let full = PaymentRequest {
        address: addr(),
        amount: Some(150_000_000),
        label: Some("Rent ünï".into()),
        message: Some("October rent".into()),
    }
    .to_uri();
    for n in 0..=full.len() {
        if full.is_char_boundary(n) {
            let _ = PaymentRequest::from_uri(&full[..n], Network::Test);
            let _ = parse_pay_text(&full[..n], Network::Test);
        }
    }
    let mut x = 0x1234_5678_9abc_def1u64;
    for _ in 0..2000 {
        let n = (x % 300) as usize;
        let s: String = (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                char::from_u32((x % 0x300) as u32).unwrap_or('?')
            })
            .collect();
        let _ = PaymentRequest::from_uri(&s, Network::Test);
        let _ = PaymentRequest::from_uri(&format!("tenero:tni1{s}"), Network::Test);
        let _ = PaymentRequest::from_uri(&format!("{}?{s}", uri("")), Network::Test);
    }
}
