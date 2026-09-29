//! Storage errors.

use tenero_core::v2::{DecodeError, EncodeError};

#[derive(Debug, PartialEq, Eq)]
pub enum StoreError {
    /// The database itself failed (I/O, corruption, a full disk).
    Db(String),
    /// The file belongs to another network or another proof of work than the one asked for.
    WrongChain,
    /// The block's `prev_id` is not the current tip.
    BadParent,
    /// The coinbase names a height other than the next one.
    BadHeight {
        expected: u64,
        got: u64,
    },
    /// The header's rules version is not the one this store writes.
    BadVersion(u16),
    /// The header's `tx_root` is not the Merkle root of the block's transactions.
    BadTxRoot,
    /// A block or a transaction with this id is already stored.
    Duplicate(&'static str),
    /// A key image is already spent (in the chain, or earlier in this same block).
    DoubleSpend([u8; 32]),
    /// The genesis block cannot be removed.
    CannotPopGenesis,
    /// Pruning below a height above the tip.
    PruneBeyondTip {
        tip: u64,
        asked: u64,
    },
    /// A stored record is not what the layout says it must be. Never expected; means the file is damaged.
    Corrupt(String),
    Encode(EncodeError),
    Decode(DecodeError),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Db(e) => write!(f, "database error: {e}"),
            StoreError::WrongChain => write!(
                f,
                "this database belongs to another network or proof of work"
            ),
            StoreError::BadParent => write!(f, "the block's parent is not the tip"),
            StoreError::BadHeight { expected, got } => {
                write!(
                    f,
                    "the coinbase says height {got}, the next height is {expected}"
                )
            }
            StoreError::BadVersion(v) => write!(f, "unsupported rules version {v}"),
            StoreError::BadTxRoot => write!(f, "the header's tx_root does not match the block"),
            StoreError::Duplicate(what) => write!(f, "duplicate {what}"),
            StoreError::DoubleSpend(_) => write!(f, "a key image is spent twice"),
            StoreError::CannotPopGenesis => write!(f, "the genesis block cannot be removed"),
            StoreError::PruneBeyondTip { tip, asked } => {
                write!(f, "cannot prune below height {asked}: the tip is {tip}")
            }
            StoreError::Corrupt(what) => write!(f, "corrupt database: {what}"),
            StoreError::Encode(e) => write!(f, "cannot encode: {e}"),
            StoreError::Decode(e) => write!(f, "cannot decode a stored record: {e}"),
        }
    }
}

impl std::error::Error for StoreError {}

macro_rules! from_db {
    ($($t:ty),*) => {
        $(impl From<$t> for StoreError {
            fn from(e: $t) -> StoreError {
                StoreError::Db(e.to_string())
            }
        })*
    };
}

from_db!(
    redb::DatabaseError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
    redb::CompactionError
);

impl From<EncodeError> for StoreError {
    fn from(e: EncodeError) -> StoreError {
        StoreError::Encode(e)
    }
}

impl From<DecodeError> for StoreError {
    fn from(e: DecodeError) -> StoreError {
        StoreError::Decode(e)
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;
