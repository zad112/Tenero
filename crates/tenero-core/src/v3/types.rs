//! The version 3 objects and their canonical wire form (`docs/CONSENSUS_V2.md` 15.2).

use crate::v2::codec::{DecodeError, EncodeError, Reader, Wire, Writer};
use crate::v2::{BlockHeader, Coinbase, Input};

pub const VERSION: u16 = 3;
pub use crate::v2::{
    INPUT_COUNT_GUARD, MAX_BLOCK_TXS, MAX_COINBASE_OUTPUTS, MAX_EXTRA, MAX_OUTPUTS, MAX_PROOF,
    MAX_TX_SIZE, MIN_COINBASE_OUTPUTS, MIN_INPUTS, MIN_OUTPUTS,
};
/// The bytes of one output (no ephemeral key: it is the transaction's).
pub const OUTPUT_SIZE: usize = 91;

/// How many ephemeral keys a transaction with `n_outputs` outputs carries: one when it has two (they share it), else
/// one per output. It is never written: it follows from the outputs.
pub fn n_ephemeral_keys(n_outputs: usize) -> usize {
    if n_outputs == 2 {
        1
    } else {
        n_outputs
    }
}

/// A Carrot output: 91 bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    pub onetime_address: [u8; 32],
    pub amount_commitment: [u8; 32],
    pub amount_enc: [u8; 8],
    pub view_tag: [u8; 3],
    pub anchor_enc: [u8; 16],
}

impl Wire for Output {
    fn write(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.raw(&self.onetime_address);
        w.raw(&self.amount_commitment);
        w.raw(&self.amount_enc);
        w.raw(&self.view_tag);
        w.raw(&self.anchor_enc);
        Ok(())
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Output {
            onetime_address: r.array()?,
            amount_commitment: r.array()?,
            amount_enc: r.array()?,
            view_tag: r.array()?,
            anchor_enc: r.array()?,
        })
    }
}

/// Everything in a transaction except the proofs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxPrefix {
    pub version: u16,
    pub inputs: Vec<Input>,
    pub outputs: Vec<Output>,
    /// [`n_ephemeral_keys`] of them; encoding refuses any other count.
    pub ephemeral_pubkeys: Vec<[u8; 32]>,
    pub fee: u64,
    pub encrypted_payment_id: [u8; 8],
}

impl Wire for TxPrefix {
    fn write(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.u16(self.version);
        w.count(self.inputs.len(), MIN_INPUTS, INPUT_COUNT_GUARD)?;
        for i in &self.inputs {
            i.write(w)?;
        }
        w.count(self.outputs.len(), MIN_OUTPUTS, MAX_OUTPUTS)?;
        for o in &self.outputs {
            o.write(w)?;
        }
        if self.ephemeral_pubkeys.len() != n_ephemeral_keys(self.outputs.len()) {
            return Err(EncodeError::CountOutOfRange);
        }
        for k in &self.ephemeral_pubkeys {
            w.raw(k);
        }
        w.u64(self.fee);
        w.raw(&self.encrypted_payment_id);
        Ok(())
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let version = r.u16()?;
        let inputs = r.list(MIN_INPUTS, INPUT_COUNT_GUARD, Input::read)?;
        let outputs = r.list(MIN_OUTPUTS, MAX_OUTPUTS, Output::read)?;
        let ephemeral_pubkeys = (0..n_ephemeral_keys(outputs.len()))
            .map(|_| r.array())
            .collect::<Result<_, _>>()?;
        Ok(TxPrefix {
            version,
            inputs,
            outputs,
            ephemeral_pubkeys,
            fee: r.u64()?,
            encrypted_payment_id: r.array()?,
        })
    }
}

/// What is needed only to verify a transaction once: the reference height and the proofs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prunable {
    /// The block whose curve-tree root the membership proof is made against.
    pub reference_height: u64,
    /// The pseudo-outputs, the range proof and the FCMP++ proof (`docs/CONSENSUS_V2.md` 15.7).
    pub proof_data: Vec<u8>,
}

impl Wire for Prunable {
    fn write(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.u64(self.reference_height);
        w.var(&self.proof_data, MAX_PROOF)
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Prunable {
            reference_height: r.u64()?,
            proof_data: r.var(MAX_PROOF)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transaction {
    pub prefix: TxPrefix,
    pub prunable: Prunable,
}

impl Wire for Transaction {
    fn write(&self, w: &mut Writer) -> Result<(), EncodeError> {
        let start = w.len();
        self.prefix.write(w)?;
        self.prunable.write(w)?;
        if w.len() - start > MAX_TX_SIZE {
            return Err(EncodeError::LengthOverMaximum);
        }
        Ok(())
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let start = r.position();
        let prefix = TxPrefix::read(r)?;
        let prunable = Prunable::read(r)?;
        if r.position() - start > MAX_TX_SIZE {
            return Err(DecodeError::LengthOverMaximum);
        }
        Ok(Transaction { prefix, prunable })
    }
}

/// A transaction whose proofs were discarded: the prefix and the hash of the prunable part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrunedTransaction {
    pub prefix: TxPrefix,
    pub prunable_hash: [u8; 32],
}

impl Wire for PrunedTransaction {
    fn write(&self, w: &mut Writer) -> Result<(), EncodeError> {
        self.prefix.write(w)?;
        w.raw(&self.prunable_hash);
        Ok(())
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(PrunedTransaction {
            prefix: TxPrefix::read(r)?,
            prunable_hash: r.array()?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub header: BlockHeader,
    pub coinbase: Coinbase,
    pub transactions: Vec<Transaction>,
}

impl Wire for Block {
    fn write(&self, w: &mut Writer) -> Result<(), EncodeError> {
        self.header.write(w)?;
        self.coinbase.write(w)?;
        w.count(self.transactions.len(), 0, MAX_BLOCK_TXS)?;
        for t in &self.transactions {
            t.write(w)?;
        }
        Ok(())
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Block {
            header: BlockHeader::read(r)?,
            coinbase: Coinbase::read(r)?,
            transactions: r.list(0, MAX_BLOCK_TXS, Transaction::read)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrunedBlock {
    pub header: BlockHeader,
    pub coinbase: Coinbase,
    pub transactions: Vec<PrunedTransaction>,
}

impl Wire for PrunedBlock {
    fn write(&self, w: &mut Writer) -> Result<(), EncodeError> {
        self.header.write(w)?;
        self.coinbase.write(w)?;
        w.count(self.transactions.len(), 0, MAX_BLOCK_TXS)?;
        for t in &self.transactions {
            t.write(w)?;
        }
        Ok(())
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(PrunedBlock {
            header: BlockHeader::read(r)?,
            coinbase: Coinbase::read(r)?,
            transactions: r.list(0, MAX_BLOCK_TXS, PrunedTransaction::read)?,
        })
    }
}
