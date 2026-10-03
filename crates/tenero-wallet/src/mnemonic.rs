//! The seed as 24 words you can write on paper (BIP-39 word list and checksum).
//!
//! **What this is and is not.** The wallet's whole secret is a 32-byte seed. This module writes those 32 bytes as 24
//! words of the BIP-39 English list (the last word carries a checksum, so a mistyped or swapped word is caught) and
//! reads them back. It uses BIP-39 only as a *spelling* of the seed. It does NOT use BIP-39's later step (PBKDF2 into
//! a 64-byte seed, and the optional extra word): a Tenero phrase typed into a Bitcoin wallet gives unrelated keys,
//! and the reverse. The words are the whole wallet: anyone who reads them can spend everything.
//!
//! The word list and the checksum come from the `bip39` crate (CC0, owner-approved 2026-10-03).

use bip39::{Language, Mnemonic};
use zeroize::{Zeroize, Zeroizing};

/// Words in a phrase.
pub const WORDS: usize = 24;

#[derive(Debug, PartialEq, Eq)]
pub enum PhraseError {
    /// Not 24 words (the count found).
    WrongCount(usize),
    /// A word that is not in the list (its position, counting from 1).
    UnknownWord(usize),
    /// All words are real but they do not go together: one is wrong, missing or out of order.
    Checksum,
}

impl std::fmt::Display for PhraseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PhraseError::WrongCount(n) => write!(f, "a phrase has {WORDS} words, this has {n}"),
            PhraseError::UnknownWord(i) => write!(f, "word {i} is not in the word list"),
            PhraseError::Checksum => write!(
                f,
                "the words do not match each other: one is wrong, missing or out of order"
            ),
        }
    }
}

impl std::error::Error for PhraseError {}

/// The 24 words of a seed, separated by single spaces.
pub fn phrase_of(seed: &[u8; 32]) -> Zeroizing<String> {
    let mut m =
        Mnemonic::from_entropy_in(Language::English, seed).expect("32 bytes is a valid size");
    let text = Zeroizing::new(m.to_string());
    m.zeroize();
    text
}

/// The seed of a phrase. Case and spacing do not matter; the words must be exactly the BIP-39 English words.
pub fn seed_of(phrase: &str) -> Result<Zeroizing<[u8; 32]>, PhraseError> {
    let words: Vec<String> = phrase
        .split_whitespace()
        .map(|w| w.to_lowercase())
        .collect();
    if words.len() != WORDS {
        return Err(PhraseError::WrongCount(words.len()));
    }
    let joined = Zeroizing::new(words.join(" "));
    let mut m = Mnemonic::parse_in_normalized(Language::English, &joined).map_err(|e| match e {
        bip39::Error::UnknownWord(i) => PhraseError::UnknownWord(i + 1),
        _ => PhraseError::Checksum,
    })?;
    let entropy = Zeroizing::new(m.to_entropy());
    m.zeroize();
    let mut seed = Zeroizing::new([0u8; 32]);
    if entropy.len() != 32 {
        return Err(PhraseError::Checksum);
    }
    seed.copy_from_slice(&entropy);
    Ok(seed)
}
