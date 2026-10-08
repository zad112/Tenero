//! The records the tables hold, in the same canonical fixed-width form as the rest of version 3.

use crate::error::Result;
use tenero_core::v3::{
    Block, BlockHeader, Coinbase, DecodeError, EncodeError, Prunable, PrunedBlock,
    PrunedTransaction, Reader, Transaction, Wire, Writer,
};

/// What the caller (the validator) tells the store about a block, which the store keeps but does not
/// check: the store has no idea what a target or a median is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockMeta {
    /// The chain's total work up to and including this block (big-endian).
    pub cumulative_work: [u8; 32],
    /// The target this block had to meet (big-endian): later blocks' difficulty is computed from these.
    pub target: [u8; 32],
    /// The block's WEIGHT for the median, the limit and the penalty (`docs/CONSENSUS_V2.md` 15.4): its transactions'
    /// weights, without the coinbase.
    pub body_weight: u64,
    /// Its long-term weight (15.4): its weight, at most 1.4 times the long-term median it was judged by.
    pub long_term_weight: u64,
}

/// What is kept about a block besides its transactions: 274 bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockIndex {
    pub block_id: [u8; 32],
    pub header: BlockHeader,
    /// The chain's total work up to and including this block (big-endian, supplied by the caller).
    pub cumulative_work: [u8; 32],
    /// The target the block had to meet (big-endian, supplied by the caller). All zeros for the genesis block.
    pub target: [u8; 32],
    /// The weight of the block's transactions (supplied by the caller). 0 for the genesis block.
    pub body_weight: u64,
    /// Its long-term weight (supplied by the caller). 0 for the genesis block.
    pub long_term_weight: u64,
    /// The global index of the block's first output (its coinbase's first output).
    pub first_output_index: u64,
    /// How many outputs the block created: the coinbase's and every transaction's.
    pub output_count: u32,
    /// How many transactions besides the coinbase.
    pub tx_count: u32,
}

impl Wire for BlockIndex {
    fn write(&self, w: &mut Writer) -> std::result::Result<(), EncodeError> {
        w.raw(&self.block_id);
        self.header.write(w)?;
        w.raw(&self.cumulative_work);
        w.raw(&self.target);
        w.u64(self.body_weight);
        w.u64(self.long_term_weight);
        w.u64(self.first_output_index);
        w.u32(self.output_count);
        w.u32(self.tx_count);
        Ok(())
    }

    fn read(r: &mut Reader<'_>) -> std::result::Result<Self, DecodeError> {
        Ok(BlockIndex {
            block_id: r.array()?,
            header: BlockHeader::read(r)?,
            cumulative_work: r.array()?,
            target: r.array()?,
            body_weight: r.u64()?,
            long_term_weight: r.u64()?,
            first_output_index: r.u64()?,
            output_count: r.u32()?,
            tx_count: r.u32()?,
        })
    }
}

/// An output as the output table keeps it: what a ring or a curve tree needs, and when it matures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredOutput {
    pub onetime_address: [u8; 32],
    /// The amount commitment. All zeros for a coinbase output, whose amount is public: its commitment is
    /// `1*G + amount*H` (`tenero_tree::coinbase_commitment`).
    pub amount_commitment: [u8; 32],
    /// The plaintext amount of a coinbase output; 0 for an ordinary one.
    pub public_amount: u64,
    pub height: u64,
    pub coinbase: bool,
}

impl Wire for StoredOutput {
    fn write(&self, w: &mut Writer) -> std::result::Result<(), EncodeError> {
        w.raw(&self.onetime_address);
        w.raw(&self.amount_commitment);
        w.u64(self.public_amount);
        w.u64(self.height);
        w.raw(&[u8::from(self.coinbase)]);
        Ok(())
    }

    fn read(r: &mut Reader<'_>) -> std::result::Result<Self, DecodeError> {
        let onetime_address = r.array()?;
        let amount_commitment = r.array()?;
        let public_amount = r.u64()?;
        let height = r.u64()?;
        // One value, one encoding: anything but 0 or 1 is not a stored output.
        let coinbase = match r.take(1)?[0] {
            0 => false,
            1 => true,
            _ => return Err(DecodeError::CountOutOfRange),
        };
        Ok(StoredOutput {
            onetime_address,
            amount_commitment,
            public_amount,
            height,
            coinbase,
        })
    }
}

/// The curve tree after a block (`docs/CONSENSUS_V2.md` 15.6): how many leaves, how many layers, and the root (all
/// zeros for an empty tree). 41 bytes, kept for every height: a transaction names one as its reference block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeState {
    pub n_leaves: u64,
    pub n_layers: u8,
    pub root: [u8; 32],
}

impl Wire for TreeState {
    fn write(&self, w: &mut Writer) -> std::result::Result<(), EncodeError> {
        w.u64(self.n_leaves);
        w.raw(&[self.n_layers]);
        w.raw(&self.root);
        Ok(())
    }

    fn read(r: &mut Reader<'_>) -> std::result::Result<Self, DecodeError> {
        Ok(TreeState {
            n_leaves: r.u64()?,
            n_layers: r.take(1)?[0],
            root: r.array()?,
        })
    }
}

/// A transaction row: the height it is in, then the pruned form (prefix and prunable hash).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TxRow {
    pub height: u64,
    pub tx: PrunedTransaction,
}

impl Wire for TxRow {
    fn write(&self, w: &mut Writer) -> std::result::Result<(), EncodeError> {
        w.u64(self.height);
        self.tx.write(w)
    }

    fn read(r: &mut Reader<'_>) -> std::result::Result<Self, DecodeError> {
        Ok(TxRow {
            height: r.u64()?,
            tx: PrunedTransaction::read(r)?,
        })
    }
}

/// A transaction as stored: always the pruned form (prefix and prunable hash), and the prunable part
/// (the reference height and the proofs) while it has not been pruned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredTx {
    pub tx: PrunedTransaction,
    pub prunable: Option<Prunable>,
}

impl StoredTx {
    /// Decodes a stored prunable part.
    pub(crate) fn with_prunable_bytes(
        tx: PrunedTransaction,
        bytes: Option<Vec<u8>>,
    ) -> Result<StoredTx> {
        let prunable = bytes.map(|b| Prunable::from_bytes(&b)).transpose()?;
        Ok(StoredTx { tx, prunable })
    }
}

/// A block as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredBlock {
    pub index: BlockIndex,
    pub coinbase: Coinbase,
    pub transactions: Vec<StoredTx>,
}

impl StoredBlock {
    /// The full block, if every transaction still has its proofs.
    pub fn into_full(self) -> Option<Block> {
        let mut txs = Vec::with_capacity(self.transactions.len());
        for t in self.transactions {
            txs.push(Transaction {
                prefix: t.tx.prefix,
                prunable: t.prunable?,
            });
        }
        Some(Block {
            header: self.index.header,
            coinbase: self.coinbase,
            transactions: txs,
        })
    }

    /// The pruned form: always available.
    pub fn to_pruned(&self) -> PrunedBlock {
        PrunedBlock {
            header: self.index.header.clone(),
            coinbase: self.coinbase.clone(),
            transactions: self.transactions.iter().map(|t| t.tx.clone()).collect(),
        }
    }

    /// Whether any transaction has lost its proofs.
    pub fn is_pruned(&self) -> bool {
        self.transactions.iter().any(|t| t.prunable.is_none())
    }
}

/// What `append_block` reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppendInfo {
    pub height: u64,
    pub block_id: [u8; 32],
    pub first_output_index: u64,
    pub output_count: u32,
}

/// What `prune_below` reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PruneStats {
    pub transactions_pruned: u64,
    /// Bytes of prunable parts forgotten (the proofs): logically freed at once.
    pub prunable_bytes_freed: u64,
    /// Segment files deleted because every block in them is now pruned, and their size on disk.
    pub segments_deleted: u64,
    pub segment_bytes_freed: u64,
    pub pruned_below: u64,
}

pub(crate) fn to_bytes<T: Wire>(x: &T) -> Result<Vec<u8>> {
    Ok(x.to_bytes()?)
}

pub(crate) fn from_bytes<T: Wire>(b: &[u8]) -> Result<T> {
    Ok(T::from_bytes(b)?)
}
