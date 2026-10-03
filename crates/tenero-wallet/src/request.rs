//! Payment requests: a link (and a QR code of it) that says "pay this address this amount, for this", so that two people testing
//! the coin can ask each other for payments without copying three things by hand. Like Monero's `monero:` links and Bitcoin's BIP-21.
//!
//! ```text
//! tenero:tni1<136 hex digits>?amount=1.5&label=Rent&message=October%20rent
//! ```
//!
//! * the address is checked like any address (its checksum, its keys);
//! * `amount` is coins with up to 8 decimals, strictly parsed (`amount.rs`), and not zero; it may be left out (then the payer chooses);
//! * `label` (at most [`MAX_LABEL`] bytes) names what it is for or who asked; `message` (at most [`MAX_MESSAGE`]) says more;
//! * each is percent-encoded (everything but letters, digits and `-._~` as `%XX`) and may appear once; **an unknown parameter makes the
//!   link invalid** rather than being ignored (a link the wallet only half understands must not look like it is understood);
//! * control characters are refused (no line breaks in a label).
//!
//! A request is **not a promise or an invoice** and the interim scheme cannot tell which payment answered which request (one address
//! per account; the chain shows no payer): the wallet does not mark requests as paid. **A request shows an address and an amount to
//! whoever gets it.** Nothing here is private in Monero's sense.

use crate::amount::{format_coins, parse_coins};
use crate::interim::{Address, ADDRESS_PREFIX};

pub const URI_SCHEME: &str = "tenero";
pub const MAX_LABEL: usize = 64;
pub const MAX_MESSAGE: usize = 200;
/// The longest link read (a real one is under 600 bytes).
pub const MAX_URI: usize = 1_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentRequest {
    pub address: Address,
    pub amount: Option<u64>,
    pub label: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RequestError {
    /// Not a payment request at all (the reason).
    Format(&'static str),
    /// The address in it is not valid (why).
    Address(String),
    /// A parameter that is not understood, or given twice.
    Parameter(String),
    /// The amount is not an amount, or is zero.
    Amount,
    /// A label or message that is too long or holds a control character.
    Text(&'static str),
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestError::Format(w) => write!(f, "not a payment request: {w}"),
            RequestError::Address(w) => write!(f, "the address in the request: {w}"),
            RequestError::Parameter(p) => write!(f, "the request has a parameter that is not understood: {p}"),
            RequestError::Amount => write!(f, "the amount in the request is not a valid amount (digits with up to 8 decimals, not zero)"),
            RequestError::Text(w) => write!(f, "the request's text is not acceptable: {w}"),
        }
    }
}

impl std::error::Error for RequestError {}

/// A label or message the wallet would accept (checked when one is made, and again when a link is read).
pub fn check_text(text: &str, max: usize, what: &'static str) -> Result<(), RequestError> {
    if text.len() > max {
        return Err(RequestError::Text(match what {
            "label" => "the label is too long",
            _ => "the message is too long",
        }));
    }
    if text.chars().any(char::is_control) {
        return Err(RequestError::Text("no line breaks or control characters"));
    }
    Ok(())
}

fn encode(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let h = text.get(i + 1..i + 3)?;
                if !h.bytes().all(|c| c.is_ascii_hexdigit()) {
                    return None;
                }
                out.push(u8::from_str_radix(h, 16).ok()?);
                i += 3;
            }
            // a character that should have been encoded is not guessed at
            b if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') => {
                out.push(b);
                i += 1;
            }
            _ => return None,
        }
    }
    String::from_utf8(out).ok()
}

impl PaymentRequest {
    /// The link. Label and message are checked by whoever made the request ([`check_text`]); anything in them is encoded.
    pub fn to_uri(&self) -> String {
        let mut uri = format!("{URI_SCHEME}:{}", self.address.to_text());
        let mut params: Vec<String> = Vec::new();
        if let Some(a) = self.amount {
            params.push(format!("amount={}", format_coins(a)));
        }
        if let Some(l) = &self.label {
            params.push(format!("label={}", encode(l)));
        }
        if let Some(m) = &self.message {
            params.push(format!("message={}", encode(m)));
        }
        if !params.is_empty() {
            uri.push('?');
            uri.push_str(&params.join("&"));
        }
        uri
    }

    /// Reads a link strictly.
    pub fn from_uri(text: &str) -> Result<PaymentRequest, RequestError> {
        let t = text.trim();
        if t.len() > MAX_URI {
            return Err(RequestError::Format("too long"));
        }
        let rest = t
            .strip_prefix(URI_SCHEME)
            .and_then(|r| r.strip_prefix(':'))
            .ok_or(RequestError::Format("a request starts with tenero:"))?;
        let (addr, query) = match rest.split_once('?') {
            Some((a, q)) => (a, Some(q)),
            None => (rest, None),
        };
        if !addr.starts_with(ADDRESS_PREFIX) {
            return Err(RequestError::Format("no address after tenero:"));
        }
        let address = Address::from_text(addr).map_err(|e| RequestError::Address(e.to_string()))?;
        let (mut amount, mut label, mut message) = (None, None, None);
        if let Some(q) = query {
            if q.is_empty() {
                return Err(RequestError::Format("an empty parameter list"));
            }
            for pair in q.split('&') {
                let (k, v) = pair
                    .split_once('=')
                    .ok_or(RequestError::Format("a parameter without ="))?;
                let dup = |what: &str| RequestError::Parameter(format!("{what} given twice"));
                match k {
                    "amount" => {
                        if amount.is_some() {
                            return Err(dup("amount"));
                        }
                        let a = parse_coins(v)
                            .filter(|a| *a > 0)
                            .ok_or(RequestError::Amount)?;
                        amount = Some(a);
                    }
                    "label" => {
                        if label.is_some() {
                            return Err(dup("label"));
                        }
                        let d = decode(v).ok_or(RequestError::Format(
                            "a bad %-escape or an unencoded character in the label",
                        ))?;
                        check_text(&d, MAX_LABEL, "label")?;
                        label = Some(d);
                    }
                    "message" => {
                        if message.is_some() {
                            return Err(dup("message"));
                        }
                        let d = decode(v).ok_or(RequestError::Format(
                            "a bad %-escape or an unencoded character in the message",
                        ))?;
                        check_text(&d, MAX_MESSAGE, "message")?;
                        message = Some(d);
                    }
                    other => return Err(RequestError::Parameter(other.chars().take(30).collect())),
                }
            }
        }
        Ok(PaymentRequest {
            address,
            amount,
            label,
            message,
        })
    }
}

/// What the Send screen's "paste a request or an address" field makes of its text: a request, or a bare address.
pub fn parse_pay_text(text: &str) -> Result<PaymentRequest, RequestError> {
    let t = text.trim();
    if t.starts_with(URI_SCHEME) {
        return PaymentRequest::from_uri(t);
    }
    let address = Address::from_text(t).map_err(|e| RequestError::Address(e.to_string()))?;
    Ok(PaymentRequest {
        address,
        amount: None,
        label: None,
        message: None,
    })
}
