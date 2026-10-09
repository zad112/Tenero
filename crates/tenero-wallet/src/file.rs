//! The wallet file: the seed and the wallet's records, encrypted with a passphrase.
//!
//! ```text
//! "TWL1" | m_kib u32 | t u32 | p u32 | salt 16 | nonce 12 | ciphertext + 16-byte tag
//! ```
//!
//! The key is Argon2id (RustCrypto `argon2`) of the passphrase with that salt and those cost parameters; the cipher
//! is ChaCha20-Poly1305 (RustCrypto `chacha20poly1305`). The first 32 bytes (the magic, the cost parameters and
//! the salt) are the associated data, so a changed cost parameter fails to decrypt rather than weakening the key.
//! A fresh salt and nonce are drawn at every save. The plaintext is the seed and the scan state (the outputs the
//! wallet owns and where it has scanned to), because that state says what the wallet has received and is private too.
//!
//! **What this does not do:** it does not hide that a file exists or its size; it does not protect against malware
//! on the machine (which can read the passphrase as it is typed, or the seed from memory); a weak passphrase is
//! guessable offline, only slowed by Argon2. Nothing here is audited as used.

use std::io::Write;
use std::path::Path;

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand_core::{CryptoRng, RngCore};
use tenero_carrot::account::AddressIndex;
use tenero_core::v3::{DecodeError, EncodeError, Reader, Writer};
use zeroize::Zeroizing;

use crate::address::Network;
use crate::wallet::{Outgoing, Owned, Reserved, Wallet, RECENT_BLOCKS};

/// One wallet (one account), the first format.
const MAGIC: &[u8; 4] = b"TWL1";
/// A purse: several accounts of one master seed (`purse.rs`).
pub(crate) const MAGIC_PURSE: &[u8; 4] = b"TWL2";
const HEADER: usize = 4 + 12 + 16;
/// Version 2: the `gamma` network (Carrot outputs, the network, the subaddresses watched). Version 1 was the interim
/// scheme of `beta`, which a 0.3.0 wallet does not read.
/// 3 (0.3.0): the state says how much of the account the wallet holds (its seed, or a view-only tier's keys). 2 (an
/// earlier 0.3.0 build) held the seed only, and is still read.
/// 4: also the transactions found spending the wallet's coins ([`Outgoing`]), and the time of each coin and of each of those
/// (its block's timestamp). A state of 3 or 2 has neither: it is read, and its scanning starts again from its birth height so
/// that they are found.
const STATE_VERSION: u16 = 4;
const NO_OUTGOING_STATE_VERSION: u16 = 3;
const SEED_ONLY_STATE_VERSION: u16 = 2;
const MAX_OUTGOING: usize = 1_000_000;
const MAX_OUTGOING_SPENDS: usize = 4_096;
const BETA_STATE_VERSION: u16 = 1;
const MAX_OWNED: usize = 1_000_000;
const MAX_RESERVED: usize = 4_096;

/// The Argon2id cost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KdfParams {
    pub memory_kib: u32,
    pub iterations: u32,
    pub lanes: u32,
}

impl KdfParams {
    /// 64 MiB, 3 passes, one lane: the RFC 9106 "low memory" recommendation. (It is a recommendation for
    /// interactive use; a wallet that holds value should use more.)
    pub const DEFAULT: KdfParams = KdfParams {
        memory_kib: 64 * 1024,
        iterations: 3,
        lanes: 1,
    };

    /// The cheapest Argon2 allows. **For tests only**: it makes a guess nearly free.
    pub const TEST_ONLY_WEAK: KdfParams = KdfParams {
        memory_kib: 8,
        iterations: 1,
        lanes: 1,
    };

    /// What a file may ask a reader to spend: more than this is refused (a hostile file must not be able to
    /// make the wallet allocate gigabytes).
    const MAX_MEMORY_KIB: u32 = 1024 * 1024;
    const MAX_ITERATIONS: u32 = 64;
    const MAX_LANES: u32 = 16;
}

#[derive(Debug, PartialEq, Eq)]
pub enum FileError {
    Io(String),
    /// Not a wallet file (wrong magic, or too short).
    NotAWalletFile,
    /// The cost parameters are not acceptable.
    BadParams,
    /// Wrong passphrase, or the file has been changed.
    WrongPassphraseOrCorrupt,
    /// Decrypted, but the contents are not a valid wallet.
    Corrupt(String),
}

impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileError::Io(e) => write!(f, "file error: {e}"),
            FileError::NotAWalletFile => write!(f, "not a wallet file"),
            FileError::BadParams => {
                write!(f, "the file asks for unacceptable key-derivation settings")
            }
            FileError::WrongPassphraseOrCorrupt => {
                write!(f, "wrong passphrase, or the file has been changed")
            }
            FileError::Corrupt(e) => write!(f, "the wallet file is damaged: {e}"),
        }
    }
}

impl std::error::Error for FileError {}

fn derive(
    passphrase: &[u8],
    salt: &[u8; 16],
    p: &KdfParams,
) -> Result<Zeroizing<[u8; 32]>, FileError> {
    let params = Params::new(p.memory_kib, p.iterations, p.lanes, Some(32))
        .map_err(|_| FileError::BadParams)?;
    let mut key = Zeroizing::new([0u8; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase, salt, &mut *key)
        .map_err(|_| FileError::BadParams)?;
    Ok(key)
}

fn enc_err(e: EncodeError) -> FileError {
    FileError::Corrupt(e.to_string())
}

fn dec_err(e: DecodeError) -> FileError {
    FileError::Corrupt(e.as_str().to_string())
}

impl Wallet {
    fn state_bytes(&self) -> Result<Zeroizing<Vec<u8>>, FileError> {
        let mut w = Writer::new();
        self.write_state(&mut w)?;
        Ok(Zeroizing::new(w.into_bytes()))
    }

    pub(crate) fn write_state(&self, w: &mut Writer) -> Result<(), FileError> {
        self.write_state_as(w, STATE_VERSION)
    }

    /// The state as `version` writes it (only the tests write an older one, to check it is still read).
    fn write_state_as(&self, w: &mut Writer, version: u16) -> Result<(), FileError> {
        w.u16(version);
        match (self.seed(), self.access()) {
            (Some(seed), _) => {
                w.raw(&[0]);
                w.raw(seed);
            }
            (None, crate::wallet::Access::ViewAll(v)) => {
                w.raw(&[1]);
                w.raw(&v.s_view_balance);
                w.raw(&v.partial_spend_pubkey);
            }
            (None, crate::wallet::Access::ViewReceived(v)) => {
                w.raw(&[2]);
                w.raw(v.k_view_incoming.as_bytes());
                w.raw(&v.s_generate_address);
                w.raw(&v.public.spend_pubkey);
            }
            (None, crate::wallet::Access::Full(_)) => {
                return Err(FileError::Corrupt("a full wallet with no seed".into()))
            }
        }
        w.raw(&[network_byte(self.network())]);
        w.u32(self.watched_subaddresses());
        w.u64(self.birth_height);
        match self.scanned {
            Some(h) => {
                w.raw(&[1]);
                w.u64(h);
            }
            None => {
                w.raw(&[0]);
                w.u64(0);
            }
        }
        w.count(self.recent.len(), 0, RECENT_BLOCKS)
            .map_err(enc_err)?;
        for (h, id) in &self.recent {
            w.u64(*h);
            w.raw(id);
        }
        w.count(self.owned.len(), 0, MAX_OWNED).map_err(enc_err)?;
        for o in &self.owned {
            w.u64(o.global_index);
            w.u64(o.height);
            if version == STATE_VERSION {
                w.u64(o.time);
            }
            w.raw(&[u8::from(o.coinbase) | (u8::from(o.internal) << 1)]);
            w.raw(&o.onetime_address);
            w.raw(&o.commitment);
            w.u64(o.amount);
            w.raw(&o.blinding);
            w.u32(o.address.major);
            w.u32(o.address.minor);
            w.raw(&o.extension_g);
            w.raw(&o.extension_t);
            w.raw(&o.key_image);
            w.raw(&o.payment_id);
        }
        w.count(self.reserved.len(), 0, MAX_RESERVED)
            .map_err(enc_err)?;
        for r in &self.reserved {
            w.raw(&r.key_image);
            w.u64(r.until_height);
        }
        if version != STATE_VERSION {
            return Ok(());
        }
        w.count(self.outgoing.len(), 0, MAX_OUTGOING)
            .map_err(enc_err)?;
        for o in &self.outgoing {
            w.u64(o.height);
            w.u64(o.time);
            w.count(o.spends.len(), 1, MAX_OUTGOING_SPENDS)
                .map_err(enc_err)?;
            for k in &o.spends {
                w.raw(k);
            }
            w.u64(o.spent);
            w.u64(o.returned);
            w.u64(o.fee);
        }
        Ok(())
    }

    fn from_state_bytes(data: &[u8]) -> Result<Wallet, FileError> {
        let mut r = Reader::new(data);
        let w = Wallet::read_state(&mut r)?;
        r.finish().map_err(dec_err)?;
        Ok(w)
    }

    pub(crate) fn read_state(r: &mut Reader) -> Result<Wallet, FileError> {
        let version = r.u16().map_err(dec_err)?;
        match version {
            STATE_VERSION | NO_OUTGOING_STATE_VERSION | SEED_ONLY_STATE_VERSION => {}
            BETA_STATE_VERSION => {
                return Err(FileError::Corrupt(
                    "a beta wallet: open it with the 0.2 programs (its 24 words also restore a gamma wallet)".into(),
                ))
            }
            _ => return Err(FileError::Corrupt("unknown state version".into())),
        }
        // what the wallet holds: its seed (kind 0), or a view-only tier's keys
        let kind = if version == SEED_ONLY_STATE_VERSION {
            0
        } else {
            r.take(1).map_err(dec_err)?[0]
        };
        let keys: Zeroizing<Vec<u8>> = Zeroizing::new(match kind {
            0 => r.take(32).map_err(dec_err)?.to_vec(),
            1 => r.take(64).map_err(dec_err)?.to_vec(),
            2 => r.take(96).map_err(dec_err)?.to_vec(),
            _ => return Err(FileError::Corrupt("unknown kind of wallet".into())),
        });
        let network = match r.take(1).map_err(dec_err)?[0] {
            0 => Network::Gamma,
            1 => Network::Dev,
            2 => Network::Test,
            _ => return Err(FileError::Corrupt("unknown network".into())),
        };
        let watched = r.u32().map_err(dec_err)?;
        let birth = r.u64().map_err(dec_err)?;
        let flag = r.take(1).map_err(dec_err)?[0];
        let scanned_h = r.u64().map_err(dec_err)?;
        let scanned = match flag {
            0 if scanned_h == 0 => None,
            1 => Some(scanned_h),
            _ => return Err(FileError::Corrupt("bad scan marker".into())),
        };
        let recent = r
            .list(0, RECENT_BLOCKS, |r| Ok((r.u64()?, r.array::<32>()?)))
            .map_err(dec_err)?;
        let owned = r
            .list(0, MAX_OWNED, |r| {
                let global_index = r.u64()?;
                let height = r.u64()?;
                // a state before version 4 has no times (it is scanned again: see below)
                let time = if version == STATE_VERSION {
                    r.u64()?
                } else {
                    0
                };
                let flags = r.take(1)?[0];
                if flags > 3 {
                    return Err(DecodeError::CountOutOfRange);
                }
                Ok(Owned {
                    global_index,
                    height,
                    time,
                    coinbase: flags & 1 == 1,
                    internal: flags & 2 == 2,
                    onetime_address: r.array()?,
                    commitment: r.array()?,
                    amount: r.u64()?,
                    blinding: r.array()?,
                    address: AddressIndex {
                        major: r.u32()?,
                        minor: r.u32()?,
                    },
                    extension_g: r.array()?,
                    extension_t: r.array()?,
                    key_image: r.array()?,
                    payment_id: r.array()?,
                })
            })
            .map_err(dec_err)?;
        let reserved = r
            .list(0, MAX_RESERVED, |r| {
                Ok(Reserved {
                    key_image: r.array()?,
                    until_height: r.u64()?,
                })
            })
            .map_err(dec_err)?;
        let outgoing = if version == STATE_VERSION {
            r.list(0, MAX_OUTGOING, |r| {
                Ok(Outgoing {
                    height: r.u64()?,
                    time: r.u64()?,
                    spends: r.list(1, MAX_OUTGOING_SPENDS, |r| r.array::<32>())?,
                    spent: r.u64()?,
                    returned: r.u64()?,
                    fee: r.u64()?,
                })
            })
            .map_err(dec_err)?
        } else {
            Vec::new()
        };
        let b32 = |i: usize| -> [u8; 32] { keys[i..i + 32].try_into().expect("32") };
        let mut w = match kind {
            0 => Wallet::from_seed(&b32(0), network, birth),
            1 => Wallet::with_access(
                None,
                crate::wallet::view_all_access(b32(0), b32(32))
                    .ok_or_else(|| FileError::Corrupt("a view key that is not one".into()))?,
                network,
                birth,
            ),
            _ => Wallet::with_access(
                None,
                crate::wallet::view_received_access(b32(0), b32(32), b32(64))
                    .ok_or_else(|| FileError::Corrupt("a view key that is not one".into()))?,
                network,
                birth,
            ),
        };
        w.watch_subaddresses(watched);
        // the records must belong to this seed (a corrupted or swapped state is refused, not trusted)
        for o in &owned {
            if !w.opens(o) {
                return Err(FileError::Corrupt(
                    "an output does not belong to this seed".into(),
                ));
            }
        }
        // the remembered blocks are in order and end at the last scanned height
        if recent.windows(2).any(|w| w[0].0 >= w[1].0) || recent.last().map(|l| l.0) != scanned {
            return Err(FileError::Corrupt(
                "the scanned blocks are not in order".into(),
            ));
        }
        // a transaction found spending the wallet's coins spends coins it holds, and is no later than the scan
        if outgoing.iter().any(|o| {
            Some(o.height) > scanned
                || o.spends
                    .iter()
                    .any(|k| !owned.iter().any(|w| &w.key_image == k))
        }) {
            return Err(FileError::Corrupt(
                "a payment out spends coins the wallet does not hold".into(),
            ));
        }
        w.reserved = reserved;
        if version == STATE_VERSION {
            w.scanned = scanned;
            w.recent = recent;
            w.owned = owned;
            w.outgoing = outgoing;
        }
        // an older state never looked for payments out: it is scanned again from its birth height (what it held comes
        // back with them; the reservations are kept)
        Ok(w)
    }

    /// Writes the wallet to `path`, encrypted, replacing any file there atomically (a crash leaves the old
    /// file or the new one, never half of each).
    pub fn save(
        &self,
        path: &Path,
        passphrase: &[u8],
        kdf: KdfParams,
        rng: &mut (impl RngCore + CryptoRng),
    ) -> Result<(), FileError> {
        let state = self.state_bytes()?;
        seal(path, MAGIC, &state, passphrase, kdf, rng)
    }

    /// Reads a wallet file.
    pub fn load(path: &Path, passphrase: &[u8]) -> Result<Wallet, FileError> {
        let (magic, plain) = open(path, passphrase)?;
        if &magic != MAGIC {
            return Err(FileError::NotAWalletFile);
        }
        Wallet::from_state_bytes(&plain)
    }
}

/// Encrypts `state` under `magic` and writes it to `path`, atomically.
pub(crate) fn network_byte(n: Network) -> u8 {
    match n {
        Network::Gamma => 0,
        Network::Dev => 1,
        Network::Test => 2,
    }
}

pub(crate) fn network_of_byte(b: u8) -> Option<Network> {
    match b {
        0 => Some(Network::Gamma),
        1 => Some(Network::Dev),
        2 => Some(Network::Test),
        _ => None,
    }
}

pub(crate) fn seal(
    path: &Path,
    magic: &[u8; 4],
    state: &[u8],
    passphrase: &[u8],
    kdf: KdfParams,
    rng: &mut (impl RngCore + CryptoRng),
) -> Result<(), FileError> {
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 12];
    rng.fill_bytes(&mut salt);
    rng.fill_bytes(&mut nonce);
    let key = derive(passphrase, &salt, &kdf)?;
    let mut header = Vec::with_capacity(HEADER);
    header.extend_from_slice(magic);
    header.extend_from_slice(&kdf.memory_kib.to_le_bytes());
    header.extend_from_slice(&kdf.iterations.to_le_bytes());
    header.extend_from_slice(&kdf.lanes.to_le_bytes());
    header.extend_from_slice(&salt);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&*key));
    let sealed = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: state,
                aad: &header,
            },
        )
        .map_err(|_| FileError::Corrupt("encryption failed".into()))?;
    let mut out = header;
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&sealed);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let io = |e: std::io::Error| FileError::Io(e.to_string());
    {
        let mut f = std::fs::File::create(&tmp).map_err(io)?;
        f.write_all(&out).map_err(io)?;
        f.sync_all().map_err(io)?;
    }
    std::fs::rename(&tmp, path).map_err(io)
}

/// Reads and decrypts a wallet file of either format: which one it is (the magic) and the plaintext.
pub(crate) fn open(
    path: &Path,
    passphrase: &[u8],
) -> Result<([u8; 4], Zeroizing<Vec<u8>>), FileError> {
    let data = std::fs::read(path).map_err(|e| FileError::Io(e.to_string()))?;
    if data.len() < HEADER + 12 + 16 || (&data[..4] != MAGIC && &data[..4] != MAGIC_PURSE) {
        return Err(FileError::NotAWalletFile);
    }
    let u32_at = |i: usize| u32::from_le_bytes(data[i..i + 4].try_into().expect("4 bytes"));
    let kdf = KdfParams {
        memory_kib: u32_at(4),
        iterations: u32_at(8),
        lanes: u32_at(12),
    };
    if kdf.memory_kib > KdfParams::MAX_MEMORY_KIB
        || kdf.iterations > KdfParams::MAX_ITERATIONS
        || kdf.lanes > KdfParams::MAX_LANES
    {
        return Err(FileError::BadParams);
    }
    let salt: [u8; 16] = data[16..32].try_into().expect("16 bytes");
    let nonce = &data[32..44];
    let key = derive(passphrase, &salt, &kdf)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&*key));
    let plain = cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: &data[44..],
                aad: &data[..32],
            },
        )
        .map_err(|_| FileError::WrongPassphraseOrCorrupt)?;
    let magic: [u8; 4] = data[..4].try_into().expect("4 bytes");
    Ok((magic, Zeroizing::new(plain)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use curve25519_dalek::edwards::EdwardsPoint;
    use curve25519_dalek::scalar::Scalar;

    /// An output of `w`'s main address with the sender extensions `n`, `n + 1` (as scanning would have recorded it).
    fn belonging(w: &Wallet, n: u8, global_index: u64) -> Owned {
        let a = w.account();
        let (g, t) = (Scalar::from(u64::from(n)), Scalar::from(u64::from(n) + 1));
        let x = a.k_generate_image + g;
        let y = a.k_prove_spend + t;
        let ko = (EdwardsPoint::mul_base(&x) + *tenero_carrot::points::T * y)
            .compress()
            .to_bytes();
        Owned {
            global_index,
            height: 3,
            time: 1_700_000_180,
            coinbase: false,
            onetime_address: ko,
            commitment: [5; 32],
            amount: 77,
            blinding: [4; 32],
            address: AddressIndex::MAIN,
            extension_g: g.to_bytes(),
            extension_t: t.to_bytes(),
            key_image: tenero_carrot::scan::key_image(&x, &ko),
            payment_id: [0; 8],
            internal: n.is_multiple_of(2),
        }
    }

    fn sample() -> Wallet {
        let mut w = Wallet::from_seed(&[1; 32], Network::Test, 2);
        w.owned = vec![belonging(&w, 5, 10), belonging(&w, 6, 11)];
        w.recent = vec![(2, [9; 32]), (3, [8; 32])];
        w.scanned = Some(3);
        w.reserved = vec![Reserved {
            key_image: [7; 32],
            until_height: 40,
        }];
        w
    }

    #[test]
    fn the_state_round_trips() {
        let mut w = sample();
        w.subaddress(120).unwrap();
        let back = Wallet::from_state_bytes(&w.state_bytes().unwrap()).unwrap();
        assert_eq!(back.owned, w.owned);
        assert_eq!(back.recent, w.recent);
        assert_eq!(back.reserved, w.reserved);
        assert_eq!((back.scanned, back.birth_height), (Some(3), 2));
        assert_eq!(back.address(), w.address());
        assert_eq!(back.network(), Network::Test);
        assert_eq!(back.watched_subaddresses(), w.watched_subaddresses());
    }

    #[test]
    fn payments_out_round_trip_and_an_older_state_is_scanned_again_to_find_them() {
        let mut w = sample();
        let spends = vec![w.owned[0].key_image];
        w.outgoing = vec![Outgoing {
            height: 3,
            time: 1_700_000_180,
            spends,
            spent: 77,
            returned: 70,
            fee: 2,
        }];
        let bytes = w.state_bytes().unwrap();
        let back = Wallet::from_state_bytes(&bytes).unwrap();
        assert_eq!(back.outgoing, w.outgoing);
        assert_eq!(back.outgoing[0].sent(), 5);
        // a payment out that spends a coin the wallet does not hold, or after its scan, is refused
        let mut bad = sample();
        bad.outgoing = vec![Outgoing {
            height: 3,
            time: 1_700_000_180,
            spends: vec![[1; 32]],
            spent: 1,
            returned: 0,
            fee: 0,
        }];
        assert!(Wallet::from_state_bytes(&bad.state_bytes().unwrap()).is_err());
        let mut late = sample();
        late.outgoing = vec![Outgoing {
            height: 4,
            time: 1_700_000_240,
            spends: vec![late.owned[0].key_image],
            spent: 77,
            returned: 0,
            fee: 1,
        }];
        assert!(Wallet::from_state_bytes(&late.state_bytes().unwrap()).is_err());
        // version 3 (no times, no payments out) is read, and its scan starts again from the birth height
        let mut v3 = Writer::new();
        sample()
            .write_state_as(&mut v3, NO_OUTGOING_STATE_VERSION)
            .unwrap();
        let v3 = v3.into_bytes();
        assert_eq!(v3[..2], 3u16.to_le_bytes());
        let old = Wallet::from_state_bytes(&v3).unwrap();
        assert_eq!((old.scanned, old.owned.len()), (None, 0));
        assert!(old.recent.is_empty() && old.outgoing.is_empty());
        assert_eq!(old.birth_height, 2);
        assert_eq!(old.reserved, sample().reserved, "the reservations are kept");
        assert_eq!(old.address(), sample().address());
    }

    #[test]
    fn trailing_bytes_are_refused() {
        let mut bytes = sample().state_bytes().unwrap().to_vec();
        bytes.push(0);
        assert!(matches!(
            Wallet::from_state_bytes(&bytes),
            Err(FileError::Corrupt(_))
        ));
    }

    #[test]
    fn a_beta_wallet_state_is_refused_with_a_reason() {
        let mut bytes = sample().state_bytes().unwrap().to_vec();
        bytes[..2].copy_from_slice(&1u16.to_le_bytes());
        match Wallet::from_state_bytes(&bytes) {
            Err(FileError::Corrupt(why)) => assert!(why.contains("beta"), "{why}"),
            other => panic!("{:?}", other.err()),
        }
    }

    #[test]
    fn an_output_that_is_not_this_seeds_is_refused() {
        let mut w = sample();
        let other = Wallet::from_seed(&[2; 32], Network::Test, 0);
        w.owned.push(belonging(&other, 7, 12));
        assert!(matches!(
            Wallet::from_state_bytes(&w.state_bytes().unwrap()),
            Err(FileError::Corrupt(_))
        ));
        // right key, wrong key image
        let mut w = sample();
        w.owned[0].key_image = [1; 32];
        assert!(matches!(
            Wallet::from_state_bytes(&w.state_bytes().unwrap()),
            Err(FileError::Corrupt(_))
        ));
        // right key image, wrong one-time address
        let mut w = sample();
        w.owned[0].onetime_address = [1; 32];
        assert!(matches!(
            Wallet::from_state_bytes(&w.state_bytes().unwrap()),
            Err(FileError::Corrupt(_))
        ));
    }

    #[test]
    fn remembered_blocks_must_be_in_order_and_end_where_the_scan_did() {
        let mut w = sample();
        w.recent = vec![(3, [9; 32]), (2, [8; 32])];
        assert!(Wallet::from_state_bytes(&w.state_bytes().unwrap()).is_err());
        let mut w = sample();
        w.scanned = Some(4);
        assert!(Wallet::from_state_bytes(&w.state_bytes().unwrap()).is_err());
        let mut w = sample();
        w.scanned = None;
        assert!(Wallet::from_state_bytes(&w.state_bytes().unwrap()).is_err());
    }
}
