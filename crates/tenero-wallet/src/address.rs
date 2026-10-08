//! Version 3 (`gamma`) addresses as text (`docs/CONSENSUS_V2.md` 15.9; vectors `tests/vectors/v3_address.json` from
//! `reference/tools/make_vectors_address.py`).
//!
//! `varint(tag) || spend key || view key || [payment ID, integrated only] || checksum` in Monero's block base58, the
//! checksum the first 4 bytes of `SHA-256("tenero address v3" || everything before it)`. The tags make every address of a
//! network start with its four letters: `TENg` (gamma), `TENd` (dev), `TENt` (test). Not consensus: a node never sees an
//! address. **Unaudited**, like everything Carrot here.

use tenero_carrot::account::Destination;
use tenero_carrot::{PaymentId, NULL_PAYMENT_ID};
use tenero_core::hash::sha256;

/// The networks a 0.3.0 wallet knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Network {
    Gamma,
    Dev,
    Test,
}

impl Network {
    pub const ALL: [Network; 3] = [Network::Gamma, Network::Dev, Network::Test];

    /// The four letters every address of the network starts with.
    pub fn prefix(self) -> &'static str {
        match self {
            Network::Gamma => "TENg",
            Network::Dev => "TENd",
            Network::Test => "TENt",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Network::Gamma => "gamma",
            Network::Dev => "dev",
            Network::Test => "test",
        }
    }

    /// The tag of each kind of address on this network (4-byte varints; see the module documentation).
    fn tag(self, kind: Kind) -> u64 {
        match (self, kind) {
            (Network::Gamma, Kind::Main) => 2_255_132,
            (Network::Gamma, Kind::Subaddress) => 2_271_516,
            (Network::Gamma, Kind::Integrated) => 4_352_284,
            (Network::Dev, Kind::Main) => 2_156_828,
            (Network::Dev, Kind::Subaddress) => 2_173_212,
            (Network::Dev, Kind::Integrated) => 4_253_980,
            (Network::Test, Kind::Main) => 2_648_348,
            (Network::Test, Kind::Subaddress) => 2_664_732,
            (Network::Test, Kind::Integrated) => 4_745_500,
        }
    }
}

/// Which kind of address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Main,
    Subaddress,
    /// A main address with an 8-byte payment ID.
    Integrated,
}

/// A Carrot address on one network.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Address {
    pub network: Network,
    pub kind: Kind,
    pub spend_pubkey: [u8; 32],
    pub view_pubkey: [u8; 32],
    /// All zero unless `kind` is `Integrated`.
    pub payment_id: PaymentId,
}

/// Why a text is not an address of the expected network. [`AddressError::as_str`] gives the names the vectors use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressError {
    BadLength,
    BadCharacter,
    BadBlock,
    BadChecksum,
    BadTag,
    UnknownNetwork,
    AnotherNetwork,
    /// A `tni1` address of the interim scheme (the `beta` network, 0.2 programs).
    Interim,
}

impl AddressError {
    pub fn as_str(self) -> &'static str {
        match self {
            AddressError::BadLength => "bad length",
            AddressError::BadCharacter => "bad character",
            AddressError::BadBlock => "bad block",
            AddressError::BadChecksum => "bad checksum",
            AddressError::BadTag => "bad tag",
            AddressError::UnknownNetwork => "unknown network",
            AddressError::AnotherNetwork => "another network",
            AddressError::Interim => "an interim address",
        }
    }
}

impl std::fmt::Display for AddressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AddressError::AnotherNetwork => write!(f, "that address is for another network"),
            AddressError::Interim => write!(
                f,
                "that is a beta (tni1) address: this wallet is for the gamma network"
            ),
            AddressError::UnknownNetwork => write!(f, "that is not a Tenero address"),
            other => write!(f, "invalid address: {}", other.as_str()),
        }
    }
}

impl std::error::Error for AddressError {}

const ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
const ENCODED_SIZES: [usize; 9] = [0, 2, 3, 5, 6, 7, 9, 10, 11];
const CHECKSUM_TAG: &[u8] = b"tenero address v3";

fn varint(mut n: u64, out: &mut Vec<u8>) {
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// The canonical varint at the start of `data`: its value and length.
fn read_varint(data: &[u8]) -> Result<(u64, usize), AddressError> {
    let mut n = 0u64;
    for (i, &b) in data.iter().take(10).enumerate() {
        n |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            if b == 0 && i > 0 {
                return Err(AddressError::BadTag);
            }
            return Ok((n, i + 1));
        }
    }
    Err(AddressError::BadTag)
}

/// Monero's block base58: 8-byte blocks as 11 characters, a last block of n bytes as `ENCODED_SIZES[n]` characters.
pub(crate) fn b58encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 11 / 8 + 11);
    for block in data.chunks(8) {
        let mut v: u128 = 0;
        for &b in block {
            v = (v << 8) | u128::from(b);
        }
        let size = ENCODED_SIZES[block.len()];
        let mut chars = vec![b'1'; size];
        for c in chars.iter_mut().rev() {
            *c = ALPHABET[(v % 58) as usize];
            v /= 58;
        }
        out.push_str(std::str::from_utf8(&chars).expect("ASCII"));
    }
    out
}

pub(crate) fn b58decode(text: &str) -> Result<Vec<u8>, AddressError> {
    let bytes = text.as_bytes();
    if !ENCODED_SIZES.contains(&(bytes.len() % 11)) {
        return Err(AddressError::BadLength);
    }
    let mut out = Vec::with_capacity(bytes.len() * 8 / 11 + 8);
    for chunk in bytes.chunks(11) {
        let size = ENCODED_SIZES
            .iter()
            .position(|&s| s == chunk.len())
            .ok_or(AddressError::BadLength)?;
        let mut v: u128 = 0;
        for &c in chunk {
            let d = ALPHABET
                .iter()
                .position(|&a| a == c)
                .ok_or(AddressError::BadCharacter)?;
            v = v * 58 + d as u128;
        }
        if v >> (8 * size) != 0 {
            return Err(AddressError::BadBlock);
        }
        out.extend_from_slice(&v.to_be_bytes()[16 - size..]);
    }
    Ok(out)
}

impl Address {
    /// The address of a Carrot destination on `network`.
    pub fn of(network: Network, d: &Destination) -> Address {
        let kind = if d.is_subaddress {
            Kind::Subaddress
        } else if d.payment_id != NULL_PAYMENT_ID {
            Kind::Integrated
        } else {
            Kind::Main
        };
        Address {
            network,
            kind,
            spend_pubkey: d.spend_pubkey,
            view_pubkey: d.view_pubkey,
            payment_id: d.payment_id,
        }
    }

    /// The same main address with a payment ID: an integrated address. `None` for a subaddress (Carrot has no integrated
    /// subaddresses) or a zero payment ID.
    pub fn with_payment_id(&self, payment_id: PaymentId) -> Option<Address> {
        (self.kind != Kind::Subaddress && payment_id != NULL_PAYMENT_ID).then_some(Address {
            kind: Kind::Integrated,
            payment_id,
            ..*self
        })
    }

    /// Where an output to this address goes.
    pub fn destination(&self) -> Destination {
        Destination {
            spend_pubkey: self.spend_pubkey,
            view_pubkey: self.view_pubkey,
            is_subaddress: self.kind == Kind::Subaddress,
            payment_id: if self.kind == Kind::Integrated {
                self.payment_id
            } else {
                NULL_PAYMENT_ID
            },
        }
    }

    pub fn to_text(&self) -> String {
        let mut body = Vec::with_capacity(84);
        varint(self.network.tag(self.kind), &mut body);
        body.extend_from_slice(&self.spend_pubkey);
        body.extend_from_slice(&self.view_pubkey);
        if self.kind == Kind::Integrated {
            body.extend_from_slice(&self.payment_id);
        }
        let check = sha256(&[CHECKSUM_TAG, &body]);
        body.extend_from_slice(&check[..4]);
        b58encode(&body)
    }

    /// The address `text`, of whichever network its tag names (a program that learns its network from the address, as
    /// a miner pointed at a pool does). Otherwise as strict as [`Address::parse`].
    pub fn parse_any(text: &str) -> Result<Address, AddressError> {
        Network::ALL
            .iter()
            .find_map(|n| Address::parse(text, *n).ok())
            .ok_or_else(|| Address::parse(text, Network::Gamma).expect_err("no network took it"))
    }

    /// The address `text` on `network`. Surrounding spaces are ignored; nothing else is forgiven.
    pub fn parse(text: &str, network: Network) -> Result<Address, AddressError> {
        let text = text.trim();
        if text.starts_with("tni1") {
            return Err(AddressError::Interim);
        }
        let data = b58decode(text)?;
        if data.len() < 5 {
            return Err(AddressError::BadLength);
        }
        let (body, check) = data.split_at(data.len() - 4);
        if sha256(&[CHECKSUM_TAG, body])[..4] != *check {
            return Err(AddressError::BadChecksum);
        }
        let (tag, used) = read_varint(body)?;
        let (net, kind) = Network::ALL
            .iter()
            .flat_map(|n| {
                [Kind::Main, Kind::Subaddress, Kind::Integrated]
                    .into_iter()
                    .map(move |k| (*n, k))
            })
            .find(|(n, k)| n.tag(*k) == tag)
            .ok_or(AddressError::UnknownNetwork)?;
        if net != network {
            return Err(AddressError::AnotherNetwork);
        }
        let keys = &body[used..];
        let want = if kind == Kind::Integrated { 72 } else { 64 };
        if keys.len() != want {
            return Err(AddressError::BadLength);
        }
        let mut a = Address {
            network,
            kind,
            spend_pubkey: keys[..32].try_into().expect("32"),
            view_pubkey: keys[32..64].try_into().expect("32"),
            payment_id: NULL_PAYMENT_ID,
        };
        if kind == Kind::Integrated {
            a.payment_id = keys[64..72].try_into().expect("8");
        }
        Ok(a)
    }
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_text())
    }
}

/// An account's Carrot master secret from its 32-byte seed: `SHA-256("tenero carrot master v1" || seed)`
/// (`docs/CONSENSUS_V2.md` 15.9). A `beta` wallet of the same seed uses the seed differently, so the two never share keys.
pub fn carrot_master(seed: &[u8; 32]) -> zeroize::Zeroizing<[u8; 32]> {
    zeroize::Zeroizing::new(sha256(&[b"tenero carrot master v1", seed]))
}
