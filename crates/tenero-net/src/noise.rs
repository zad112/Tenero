//! The encrypted channel between two nodes: the Noise protocol (`Noise_XX_25519_ChaChaPoly_BLAKE2s`) over any
//! byte stream, through the `snow` crate. **Experimental and unaudited**, and `snow` itself has had no formal audit
//! (an exception to CLAUDE.md rule 3, recorded in `Cargo.toml`).
//!
//! **What it gives:** everything after the handshake is encrypted and integrity-protected, so a party that only
//! watches or alters the bytes on the wire can neither read them nor change them undetected, replay a chunk, drop
//! one from the middle, or reorder them (every chunk is authenticated under a counter). The handshake is bound to a
//! **prologue** (the chain id and the protocol version), so a node on another chain fails the handshake instead
//! of reaching the protocol.
//!
//! **What it does not give:** anonymity (who talks to whom is visible), and **no proof of who the peer is**: each
//! side learns the other's static public key, but nothing pins a key to a person or a node yet, so a man in the
//! middle who runs the handshake with both ends is not detected. Bans are therefore by IP address, not by key.
//!
//! **Framing.** A Noise message is at most 65,535 bytes, 16 of them the authentication tag, so the byte stream of
//! the protocol (`wire.rs`, frames up to 16 MiB) is cut into chunks of at most [`MAX_CHUNK`] bytes. On the stream,
//! every handshake message and every chunk is `length u16 (big-endian) | bytes`. A chunk is never empty.
//!
//! The reader and the writer of one connection are separate objects ([`SecureReader`], [`SecureWriter`]) so two
//! threads can own them: each keeps its own message counter (the Noise nonce) and shares only the cipher keys.

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;

use snow::resolvers::{CryptoResolver, DefaultResolver};
use snow::{Builder, HandshakeState, StatelessTransportState};

/// The Noise pattern and primitives.
pub const PATTERN: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
/// The same pattern with a pre-shared key mixed in at the end of the handshake (the standard `psk3` modifier): used by the
/// miner service (`docs/REMOTE_MINING_PLAN.md`), where an operator may allow only those who hold the key. A side that does
/// not hold the key fails the handshake; the key itself is never sent.
pub const PATTERN_PSK: &str = "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s";
/// The most plaintext bytes one encrypted chunk carries (65,535 minus the 16-byte tag).
pub const MAX_CHUNK: usize = 65_535 - TAG;
const TAG: usize = 16;
/// The handshake messages of XX are at most about 100 bytes; anything longer is refused.
const MAX_HANDSHAKE_MESSAGE: usize = 512;

#[derive(Debug)]
pub enum NoiseError {
    Io(io::Error),
    /// The handshake failed: a message that does not decrypt, is out of order or has the wrong length, or another
    /// prologue (another chain or protocol version).
    Handshake(String),
    /// A chunk that does not authenticate: damaged, replayed, dropped before it, or reordered.
    Decrypt,
    /// A chunk with a length the format forbids (shorter than a tag).
    BadLength(usize),
    /// 2^64 - 1 chunks have been sent: the connection must be closed rather than reuse a nonce.
    NonceExhausted,
}

impl std::fmt::Display for NoiseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoiseError::Io(e) => write!(f, "{e}"),
            NoiseError::Handshake(why) => write!(f, "handshake failed: {why}"),
            NoiseError::Decrypt => write!(f, "a chunk that does not authenticate"),
            NoiseError::BadLength(n) => write!(f, "a chunk of {n} bytes"),
            NoiseError::NonceExhausted => write!(f, "the message counter is exhausted"),
        }
    }
}

impl std::error::Error for NoiseError {}

impl From<io::Error> for NoiseError {
    fn from(e: io::Error) -> NoiseError {
        NoiseError::Io(e)
    }
}

/// A node's long-term Noise key. Its public half is what peers see; the private half never leaves this struct
/// except through [`NodeKey::to_bytes`] (for saving it) and is never printed.
#[derive(Clone)]
pub struct NodeKey {
    private: [u8; 32],
    public: [u8; 32],
}

impl std::fmt::Debug for NodeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "NodeKey(public {})", hex(&self.public))
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn builder() -> Builder<'static> {
    Builder::new(PATTERN.parse().expect("the pattern is valid"))
}

impl NodeKey {
    /// A fresh random key.
    pub fn generate() -> NodeKey {
        let kp = builder()
            .generate_keypair()
            .expect("the system random source works");
        NodeKey::from_parts(&kp.private, &kp.public)
    }

    fn from_parts(private: &[u8], public: &[u8]) -> NodeKey {
        let mut k = NodeKey {
            private: [0; 32],
            public: [0; 32],
        };
        k.private.copy_from_slice(private);
        k.public.copy_from_slice(public);
        k
    }

    /// The key whose private half is `bytes` (32 bytes). The public half is derived from it.
    pub fn from_bytes(bytes: &[u8]) -> Result<NodeKey, String> {
        if bytes.len() != 32 {
            return Err(format!("a node key is 32 bytes, not {}", bytes.len()));
        }
        let params: snow::params::NoiseParams =
            PATTERN.parse().map_err(|_| "bad pattern".to_string())?;
        let mut dh = DefaultResolver
            .resolve_dh(&params.dh)
            .ok_or_else(|| "no Curve25519".to_string())?;
        dh.set(bytes);
        Ok(NodeKey::from_parts(bytes, dh.pubkey()))
    }

    pub fn public(&self) -> [u8; 32] {
        self.public
    }

    /// The private half, for saving. Handle with care: never log it.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.private
    }

    /// Reads the key at `path`, or makes one and saves it there (written aside, then renamed).
    pub fn load_or_create(path: &Path) -> Result<NodeKey, String> {
        match std::fs::read(path) {
            Ok(bytes) => NodeKey::from_bytes(&bytes),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let key = NodeKey::generate();
                let mut tmp = path.as_os_str().to_owned();
                tmp.push(".tmp");
                let tmp = std::path::PathBuf::from(tmp);
                std::fs::write(&tmp, key.private).map_err(|e| e.to_string())?;
                std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
                Ok(key)
            }
            Err(e) => Err(e.to_string()),
        }
    }
}

/// The bytes the handshake is bound to: a label, the protocol version and the chain id. Both ends must use the same.
pub fn prologue(protocol_version: u32, chain_id: &[u8; 32]) -> Vec<u8> {
    let mut p = b"tenero p2p".to_vec();
    p.extend_from_slice(&protocol_version.to_le_bytes());
    p.extend_from_slice(chain_id);
    p
}

fn write_handshake_message<S: Write>(
    stream: &mut S,
    hs: &mut HandshakeState,
    payload: &[u8],
) -> Result<(), NoiseError> {
    let mut buf = [0u8; MAX_HANDSHAKE_MESSAGE];
    let n = hs
        .write_message(payload, &mut buf)
        .map_err(|e| NoiseError::Handshake(e.to_string()))?;
    let mut framed = Vec::with_capacity(2 + n);
    framed.extend_from_slice(&(n as u16).to_be_bytes());
    framed.extend_from_slice(&buf[..n]);
    stream.write_all(&framed)?;
    stream.flush()?;
    Ok(())
}

fn read_handshake_message<S: Read>(
    stream: &mut S,
    hs: &mut HandshakeState,
) -> Result<(), NoiseError> {
    let mut len = [0u8; 2];
    stream.read_exact(&mut len)?;
    let len = u16::from_be_bytes(len) as usize;
    // (a zero length fails to decrypt below anyway; the limit keeps a peer from making us reserve 64 KiB for a
    // message XX can never need)
    if len > MAX_HANDSHAKE_MESSAGE {
        return Err(NoiseError::Handshake(format!(
            "a handshake message of {len} bytes"
        )));
    }
    let mut msg = vec![0u8; len];
    stream.read_exact(&mut msg)?;
    let mut sink = [0u8; MAX_HANDSHAKE_MESSAGE];
    hs.read_message(&msg, &mut sink)
        .map_err(|e| NoiseError::Handshake(e.to_string()))?;
    Ok(())
}

/// A finished handshake: the two halves of the channel and the peer's static public key.
pub struct Secured {
    pub reader: SecureReader,
    pub writer: SecureWriter,
    /// The peer's static public key, as it proved possession of it in the handshake.
    pub remote_public: [u8; 32],
}

fn finish(hs: HandshakeState) -> Result<Secured, NoiseError> {
    let remote = hs
        .get_remote_static()
        .ok_or_else(|| NoiseError::Handshake("the peer sent no static key".into()))?;
    let mut remote_public = [0u8; 32];
    if remote.len() != 32 {
        return Err(NoiseError::Handshake(
            "a static key of the wrong length".into(),
        ));
    }
    remote_public.copy_from_slice(remote);
    let state = hs
        .into_stateless_transport_mode()
        .map_err(|e| NoiseError::Handshake(e.to_string()))?;
    let state = Arc::new(state);
    Ok(Secured {
        reader: SecureReader {
            state: Arc::clone(&state),
            nonce: 0,
        },
        writer: SecureWriter { state, nonce: 0 },
        remote_public,
    })
}

fn configured<'a>(
    key: &'a NodeKey,
    prologue: &'a [u8],
    psk: Option<&'a [u8; 32]>,
) -> Result<Builder<'a>, NoiseError> {
    let bad = |e: snow::Error| NoiseError::Handshake(e.to_string());
    let pattern = if psk.is_some() { PATTERN_PSK } else { PATTERN };
    let mut b = Builder::new(pattern.parse().expect("the pattern is valid"))
        .local_private_key(&key.private)
        .map_err(bad)?
        .prologue(prologue)
        .map_err(bad)?;
    if let Some(k) = psk {
        b = b.psk(3, k).map_err(bad)?;
    }
    Ok(b)
}

/// The dialling side of the handshake over `stream`. `prologue` must equal the other side's.
pub fn handshake_initiator<S: Read + Write>(
    stream: &mut S,
    key: &NodeKey,
    prologue: &[u8],
) -> Result<Secured, NoiseError> {
    handshake_initiator_psk(stream, key, prologue, None)
}

/// The accepting side of the handshake over `stream`.
pub fn handshake_responder<S: Read + Write>(
    stream: &mut S,
    key: &NodeKey,
    prologue: &[u8],
) -> Result<Secured, NoiseError> {
    handshake_responder_psk(stream, key, prologue, None)
}

/// [`handshake_initiator`] with an optional pre-shared key: both sides must use the same pattern, so a side with a key
/// cannot talk to a side without one.
pub fn handshake_initiator_psk<S: Read + Write>(
    stream: &mut S,
    key: &NodeKey,
    prologue: &[u8],
    psk: Option<&[u8; 32]>,
) -> Result<Secured, NoiseError> {
    let mut hs = configured(key, prologue, psk)?
        .build_initiator()
        .map_err(|e| NoiseError::Handshake(e.to_string()))?;
    write_handshake_message(stream, &mut hs, &[])?; // -> e
    read_handshake_message(stream, &mut hs)?; // <- e, ee, s, es
    write_handshake_message(stream, &mut hs, &[])?; // -> s, se
    finish(hs)
}

/// [`handshake_responder`] with an optional pre-shared key.
pub fn handshake_responder_psk<S: Read + Write>(
    stream: &mut S,
    key: &NodeKey,
    prologue: &[u8],
    psk: Option<&[u8; 32]>,
) -> Result<Secured, NoiseError> {
    let mut hs = configured(key, prologue, psk)?
        .build_responder()
        .map_err(|e| NoiseError::Handshake(e.to_string()))?;
    read_handshake_message(stream, &mut hs)?; // <- e
    write_handshake_message(stream, &mut hs, &[])?; // -> e, ee, s, es
    read_handshake_message(stream, &mut hs)?; // <- s, se
    finish(hs)
}

/// The sending half: encrypts bytes into chunks.
pub struct SecureWriter {
    state: Arc<StatelessTransportState>,
    nonce: u64,
}

impl SecureWriter {
    /// Encrypts `data` into `length u16 | ciphertext` chunks of at most [`MAX_CHUNK`] plaintext bytes each and
    /// returns the bytes to put on the stream. Empty `data` gives no bytes. Fails instead of reusing a nonce.
    pub fn seal(&mut self, data: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let chunks = data.len().div_ceil(MAX_CHUNK);
        let mut out = Vec::with_capacity(data.len() + chunks * (2 + TAG));
        for chunk in data.chunks(MAX_CHUNK) {
            if self.nonce == u64::MAX {
                return Err(NoiseError::NonceExhausted);
            }
            let mut buf = vec![0u8; chunk.len() + TAG];
            let n = self
                .state
                .write_message(self.nonce, chunk, &mut buf)
                .map_err(|_| NoiseError::Decrypt)?;
            self.nonce += 1;
            out.extend_from_slice(&(n as u16).to_be_bytes());
            out.extend_from_slice(&buf[..n]);
        }
        Ok(out)
    }

    /// Seals `data` and writes it.
    pub fn write_all<W: Write>(&mut self, w: &mut W, data: &[u8]) -> Result<(), NoiseError> {
        let bytes = self.seal(data)?;
        w.write_all(&bytes)?;
        Ok(())
    }

    #[doc(hidden)]
    pub fn set_nonce_for_tests(&mut self, nonce: u64) {
        self.nonce = nonce;
    }
}

/// The receiving half: reads and decrypts chunks.
pub struct SecureReader {
    state: Arc<StatelessTransportState>,
    nonce: u64,
}

impl SecureReader {
    /// Reads one chunk from `r` and returns its plaintext (never empty). A chunk that does not authenticate under
    /// the next expected counter (damaged, replayed, reordered, or one before it was dropped) is an error, and after
    /// any error the connection must be closed.
    pub fn read_chunk<R: Read>(&mut self, r: &mut R) -> Result<Vec<u8>, NoiseError> {
        let mut len = [0u8; 2];
        r.read_exact(&mut len)?;
        let len = u16::from_be_bytes(len) as usize;
        if len <= TAG {
            return Err(NoiseError::BadLength(len));
        }
        let mut ct = vec![0u8; len];
        r.read_exact(&mut ct)?;
        self.open(&ct)
    }

    /// Decrypts one chunk's ciphertext (without its length prefix).
    pub fn open(&mut self, ct: &[u8]) -> Result<Vec<u8>, NoiseError> {
        if ct.len() <= TAG {
            return Err(NoiseError::BadLength(ct.len()));
        }
        if self.nonce == u64::MAX {
            return Err(NoiseError::NonceExhausted);
        }
        let mut out = vec![0u8; ct.len()];
        let n = self
            .state
            .read_message(self.nonce, ct, &mut out)
            .map_err(|_| NoiseError::Decrypt)?;
        self.nonce += 1;
        out.truncate(n);
        Ok(out)
    }

    #[doc(hidden)]
    pub fn set_nonce_for_tests(&mut self, nonce: u64) {
        self.nonce = nonce;
    }
}
