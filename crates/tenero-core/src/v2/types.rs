//! The version 2 objects (`docs/CONSENSUS_V2.md` sections 5 and 6) and their wire forms.
//!
//! A transaction is a PREFIX (what a node needs after verifying it: key images, outputs, the fee) and
//! a PRUNABLE part (the proofs, needed only to verify it once). Its id covers the prefix and a hash of
//! the prunable part, so a node that has discarded the proofs still computes the same id.

use super::codec::{DecodeError, EncodeError, Reader, Wire, Writer};

/// The rules version these types describe.
pub const VERSION: u16 = 2;

// The consensus limits. PROVISIONAL (docs/CONSENSUS_V2.md 6.2): to be re-measured once real proofs exist.
pub const MIN_INPUTS: usize = 1;
/// The most bytes one transaction may take, in its full serialized form (CONSENSUS_V2.md 6.2). **This is what limits the
/// number of inputs**: there is no rule about how many a transaction has, and each one costs about 780 bytes (a key image, a
/// ring of 16 indexes, a CLSAG and a pseudo-output commitment), so about 95 fit (93 with 16 outputs). Half the block-size floor (8.2), so that
/// a block always has room for several of them.
pub const MAX_TX_SIZE: usize = 75_000;
/// A bound on the number of inputs a decoder will believe before it reads them, so that it never reserves memory for a count
/// no transaction could have. **Not a rule**: an input is at least 32 bytes, so more than this cannot fit in `MAX_TX_SIZE`.
pub const INPUT_COUNT_GUARD: usize = MAX_TX_SIZE / 32;
pub const MIN_OUTPUTS: usize = 2;
pub const MAX_OUTPUTS: usize = 16;
pub const MIN_COINBASE_OUTPUTS: usize = 1;
pub const MAX_COINBASE_OUTPUTS: usize = 16;
pub const MAX_EXTRA: usize = 128;
pub const MAX_PROOF: usize = 64 * 1024;
pub const MAX_RING: usize = 16;
pub const MAX_BLOCK_TXS: usize = 8192;

/// A Carrot-style enote: 123 bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    pub onetime_address: [u8; 32],
    pub amount_commitment: [u8; 32],
    pub amount_enc: [u8; 8],
    pub view_tag: [u8; 3],
    pub ephemeral_pubkey: [u8; 32],
    pub anchor_enc: [u8; 16],
}

impl Wire for Output {
    fn write(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.raw(&self.onetime_address);
        w.raw(&self.amount_commitment);
        w.raw(&self.amount_enc);
        w.raw(&self.view_tag);
        w.raw(&self.ephemeral_pubkey);
        w.raw(&self.anchor_enc);
        Ok(())
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Output {
            onetime_address: r.array()?,
            amount_commitment: r.array()?,
            amount_enc: r.array()?,
            view_tag: r.array()?,
            ephemeral_pubkey: r.array()?,
            anchor_enc: r.array()?,
        })
    }
}

/// A spend, as the prefix records it: only its key image (the spend-once tag). The ring it was signed
/// against is in the prunable part, because nothing needs it once the transaction has been verified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    pub key_image: [u8; 32],
}

impl Wire for Input {
    fn write(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.raw(&self.key_image);
        Ok(())
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Input {
            key_image: r.array()?,
        })
    }
}

/// Everything in a transaction except the proofs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxPrefix {
    pub version: u16,
    pub inputs: Vec<Input>,
    pub outputs: Vec<Output>,
    pub fee: u64,
    pub extra: Vec<u8>,
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
        w.u64(self.fee);
        w.var(&self.extra, MAX_EXTRA)
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let version = r.u16()?;
        let inputs = r.list(MIN_INPUTS, INPUT_COUNT_GUARD, Input::read)?;
        let outputs = r.list(MIN_OUTPUTS, MAX_OUTPUTS, Output::read)?;
        Ok(TxPrefix {
            version,
            inputs,
            outputs,
            fee: r.u64()?,
            extra: r.var(MAX_EXTRA)?,
        })
    }
}

/// What is needed only to VERIFY a transaction, once: the ring of each input and the proofs. It is the
/// bulk of a transaction's bytes and the part a pruned node throws away.
///
/// Its wire form depends on the prefix (there is exactly one ring per input), so it is read with
/// [`Prunable::read`], given the number of inputs, not through `Wire`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prunable {
    /// One ring per input, in the order of the inputs: global output indexes (`MAX_RING` at most each).
    pub rings: Vec<Vec<u64>>,
    /// The range proof, the pseudo-output commitments and the membership proofs.
    pub proof_data: Vec<u8>,
}

impl Prunable {
    /// Writes the rings then the proof bytes. `n_inputs` is the prefix's input count: a different
    /// number of rings can never be read back, so it is refused here too.
    pub fn write(&self, w: &mut Writer, n_inputs: usize) -> Result<(), EncodeError> {
        // exactly one ring per input (the count is checked against `n_inputs` on both sides)
        w.count(self.rings.len(), n_inputs, n_inputs)?;
        for ring in &self.rings {
            w.count(ring.len(), 0, MAX_RING)?;
            for index in ring {
                w.u64(*index);
            }
        }
        w.var(&self.proof_data, MAX_PROOF)
    }

    /// Reads exactly one ring per input, then the proof bytes.
    pub fn read(r: &mut Reader<'_>, n_inputs: usize) -> Result<Prunable, DecodeError> {
        let rings = r.list(n_inputs, n_inputs, |r| r.list(0, MAX_RING, |r| r.u64()))?;
        Ok(Prunable {
            rings,
            proof_data: r.var(MAX_PROOF)?,
        })
    }

    /// The bytes the `prunable_hash` covers and a store keeps.
    pub fn to_bytes(&self, n_inputs: usize) -> Result<Vec<u8>, EncodeError> {
        let mut w = Writer::new();
        self.write(&mut w, n_inputs)?;
        Ok(w.into_bytes())
    }

    pub fn from_bytes(data: &[u8], n_inputs: usize) -> Result<Prunable, DecodeError> {
        let mut r = Reader::new(data);
        let p = Prunable::read(&mut r, n_inputs)?;
        r.finish()?;
        Ok(p)
    }
}

/// A full transaction: the prefix and the prunable part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transaction {
    pub prefix: TxPrefix,
    pub prunable: Prunable,
}

impl Wire for Transaction {
    fn write(&self, w: &mut Writer) -> Result<(), EncodeError> {
        let start = w.len();
        self.prefix.write(w)?;
        self.prunable.write(w, self.prefix.inputs.len())?;
        if w.len() - start > MAX_TX_SIZE {
            return Err(EncodeError::LengthOverMaximum);
        }
        Ok(())
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let start = r.position();
        let prefix = TxPrefix::read(r)?;
        let prunable = Prunable::read(r, prefix.inputs.len())?;
        if r.position() - start > MAX_TX_SIZE {
            return Err(DecodeError::LengthOverMaximum);
        }
        Ok(Transaction { prefix, prunable })
    }
}

/// A transaction whose rings and proofs have been discarded: the prefix and the 32-byte hash of them.
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

/// One output of a coinbase: its amount is public.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoinbaseOutput {
    pub onetime_address: [u8; 32],
    pub amount: u64,
    pub view_tag: [u8; 3],
    pub ephemeral_pubkey: [u8; 32],
    pub anchor_enc: [u8; 16],
}

impl Wire for CoinbaseOutput {
    fn write(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.raw(&self.onetime_address);
        w.u64(self.amount);
        w.raw(&self.view_tag);
        w.raw(&self.ephemeral_pubkey);
        w.raw(&self.anchor_enc);
        Ok(())
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(CoinbaseOutput {
            onetime_address: r.array()?,
            amount: r.u64()?,
            view_tag: r.array()?,
            ephemeral_pubkey: r.array()?,
            anchor_enc: r.array()?,
        })
    }
}

/// The reward transaction: no inputs, no proofs (so nothing to prune).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Coinbase {
    pub version: u16,
    pub height: u64,
    pub outputs: Vec<CoinbaseOutput>,
    pub extra: Vec<u8>,
}

impl Wire for Coinbase {
    fn write(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.u16(self.version);
        w.u64(self.height);
        w.count(
            self.outputs.len(),
            MIN_COINBASE_OUTPUTS,
            MAX_COINBASE_OUTPUTS,
        )?;
        for o in &self.outputs {
            o.write(w)?;
        }
        w.var(&self.extra, MAX_EXTRA)
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let version = r.u16()?;
        let height = r.u64()?;
        let outputs = r.list(
            MIN_COINBASE_OUTPUTS,
            MAX_COINBASE_OUTPUTS,
            CoinbaseOutput::read,
        )?;
        Ok(Coinbase {
            version,
            height,
            outputs,
            extra: r.var(MAX_EXTRA)?,
        })
    }
}

/// The block header: 146 bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockHeader {
    pub version: u16,
    pub prev_id: [u8; 32],
    pub timestamp: u64,
    pub tx_root: [u8; 32],
    pub nonce: u64,
    /// The matmulhash mix; 64 zero bytes on a SHA-256 test chain.
    pub mix: [u8; 64],
}

impl Wire for BlockHeader {
    fn write(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.u16(self.version);
        w.raw(&self.prev_id);
        w.u64(self.timestamp);
        w.raw(&self.tx_root);
        w.u64(self.nonce);
        w.raw(&self.mix);
        Ok(())
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(BlockHeader {
            version: r.u16()?,
            prev_id: r.array()?,
            timestamp: r.u64()?,
            tx_root: r.array()?,
            nonce: r.u64()?,
            mix: r.array()?,
        })
    }
}

/// A whole block: the header, the coinbase, and the other transactions.
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

/// A block as a pruned node stores it and serves it: every transaction without its proofs.
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
