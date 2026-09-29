//! The store: redb tables laid out so the proofs can be deleted without touching anything else.
//!
//! | table | key -> value |
//! |---|---|
//! | `index` | height -> `BlockIndex` (id, header, cumulative work, output range) |
//! | `block_ids` | block id -> height |
//! | `coinbase` | height -> coinbase bytes |
//! | `block_txs` | height -> the transaction ids, 32 bytes each, in order |
//! | `tx_prefix` | transaction id -> height and the PRUNED transaction (prefix and prunable hash) |
//! | `tx_prunable` | transaction id -> the proof bytes. **The only table pruning touches.** |
//! | `outputs` | global output index -> `StoredOutput` |
//! | `key_images` | key image -> height (the spent set) |
//! | `meta` | format, chain id, proof of work, `pruned_below`, output count |
//!
//! Every mutation is one redb transaction: it commits whole or not at all.

use crate::error::{Result, StoreError};
use crate::records::{
    from_bytes, to_bytes, AppendInfo, BlockIndex, PruneStats, StoredBlock, StoredOutput, StoredTx,
    TxRow,
};
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use std::path::{Path, PathBuf};
use tenero_core::hash::{hex_lower, Sha256Stream};
use tenero_core::v2::ids::{self, PowKind};
use tenero_core::v2::{Block, Coinbase, PrunedTransaction, Wire, VERSION};

const INDEX: TableDefinition<u64, &[u8]> = TableDefinition::new("index");
const BLOCK_IDS: TableDefinition<&[u8], u64> = TableDefinition::new("block_ids");
const COINBASE: TableDefinition<u64, &[u8]> = TableDefinition::new("coinbase");
const BLOCK_TXS: TableDefinition<u64, &[u8]> = TableDefinition::new("block_txs");
const TX_PREFIX: TableDefinition<&[u8], &[u8]> = TableDefinition::new("tx_prefix");
const TX_PRUNABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("tx_prunable");
const OUTPUTS: TableDefinition<u64, &[u8]> = TableDefinition::new("outputs");
const KEY_IMAGES: TableDefinition<&[u8], u64> = TableDefinition::new("key_images");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

/// The on-disk layout version, in `meta`.
pub const FORMAT_VERSION: u32 = 1;

const STATE_TAG: &[u8] = b"tenero state v2";

fn pow_byte(p: PowKind) -> u8 {
    match p {
        PowKind::Matmul => 0,
        PowKind::Sha256 => 1,
    }
}

fn u64_of(b: &[u8], what: &str) -> Result<u64> {
    let a: [u8; 8] = b
        .try_into()
        .map_err(|_| StoreError::Corrupt(format!("{what} is not 8 bytes")))?;
    Ok(u64::from_le_bytes(a))
}

fn ids_of(b: &[u8]) -> Result<Vec<[u8; 32]>> {
    if !b.len().is_multiple_of(32) {
        return Err(StoreError::Corrupt(
            "a transaction id list is not a multiple of 32 bytes".into(),
        ));
    }
    Ok((0..b.len() / 32)
        .map(|i| <[u8; 32]>::try_from(&b[32 * i..32 * i + 32]).expect("32 bytes"))
        .collect())
}

/// An open chain database.
pub struct Store {
    db: Database,
    path: PathBuf,
    pow: PowKind,
    chain_id: [u8; 32],
}

impl Store {
    /// Opens the database at `path`, creating it (with the genesis block of the network named `label`)
    /// if it does not exist. Opening a file made for another network or proof of work is an error, so a
    /// node can never mix chains.
    pub fn open(path: impl AsRef<Path>, label: &str, pow: PowKind) -> Result<Store> {
        let path = path.as_ref().to_path_buf();
        let db = Database::create(&path)?;
        let chain_id = ids::genesis_id(label);
        let txn = db.begin_write()?;
        {
            // opening a table creates it, so every table exists from the first commit
            let mut index = txn.open_table(INDEX)?;
            let mut block_ids = txn.open_table(BLOCK_IDS)?;
            let mut block_txs = txn.open_table(BLOCK_TXS)?;
            txn.open_table(COINBASE)?;
            txn.open_table(TX_PREFIX)?;
            txn.open_table(TX_PRUNABLE)?;
            txn.open_table(OUTPUTS)?;
            txn.open_table(KEY_IMAGES)?;
            let mut meta = txn.open_table(META)?;

            let existing = meta.get("chain_id")?.map(|g| g.value().to_vec());
            match existing {
                Some(id) => {
                    let format = meta.get("format")?.map(|g| g.value().to_vec());
                    let stored_pow = meta.get("pow")?.map(|g| g.value().to_vec());
                    if id != chain_id
                        || stored_pow.as_deref() != Some(&[pow_byte(pow)][..])
                        || format.as_deref() != Some(&FORMAT_VERSION.to_le_bytes()[..])
                    {
                        return Err(StoreError::WrongChain);
                    }
                }
                None => {
                    let genesis = BlockIndex {
                        block_id: chain_id,
                        header: ids::genesis_header(label),
                        cumulative_work: [0; 32],
                        first_output_index: 0,
                        output_count: 0,
                        tx_count: 0,
                    };
                    index.insert(0u64, to_bytes(&genesis)?.as_slice())?;
                    block_ids.insert(chain_id.as_slice(), 0u64)?;
                    block_txs.insert(0u64, &[][..])?;
                    meta.insert("format", FORMAT_VERSION.to_le_bytes().as_slice())?;
                    meta.insert("chain_id", chain_id.as_slice())?;
                    meta.insert("pow", [pow_byte(pow)].as_slice())?;
                    meta.insert("label", label.as_bytes())?;
                    meta.insert("pruned_below", 0u64.to_le_bytes().as_slice())?;
                    meta.insert("output_count", 0u64.to_le_bytes().as_slice())?;
                }
            }
        }
        txn.commit()?;
        Ok(Store {
            db,
            path,
            pow,
            chain_id,
        })
    }

    /// The chain id (the genesis block id) this database is bound to.
    pub fn chain_id(&self) -> [u8; 32] {
        self.chain_id
    }

    pub fn pow(&self) -> PowKind {
        self.pow
    }

    /// The size of the database file in bytes.
    pub fn file_size(&self) -> std::io::Result<u64> {
        Ok(std::fs::metadata(&self.path)?.len())
    }

    /// Reclaims the space of deleted rows (redb reuses freed pages but does not shrink the file by
    /// itself). Returns whether anything was compacted.
    pub fn compact(&mut self) -> Result<bool> {
        Ok(self.db.compact()?)
    }

    // ------------------------------------------------------------------ reading

    /// The height and index record of the tip. The genesis block is height 0.
    pub fn tip(&self) -> Result<(u64, BlockIndex)> {
        let txn = self.db.begin_read()?;
        let index = txn.open_table(INDEX)?;
        let (k, v) = index
            .last()?
            .ok_or_else(|| StoreError::Corrupt("the index has no genesis".into()))?;
        Ok((k.value(), from_bytes(v.value())?))
    }

    pub fn block_index(&self, height: u64) -> Result<Option<BlockIndex>> {
        let txn = self.db.begin_read()?;
        let index = txn.open_table(INDEX)?;
        let found = index.get(height)?;
        found.map(|g| from_bytes(g.value())).transpose()
    }

    /// The height of the block with this id.
    pub fn height_of(&self, block_id: &[u8; 32]) -> Result<Option<u64>> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(BLOCK_IDS)?;
        let found = t.get(block_id.as_slice())?;
        Ok(found.map(|g| g.value()))
    }

    /// The block at `height` with whatever proofs it still has. `None` for the genesis block (which has
    /// no coinbase or transactions: see `block_index`) and for heights above the tip.
    pub fn get_block(&self, height: u64) -> Result<Option<StoredBlock>> {
        if height == 0 {
            return Ok(None);
        }
        let txn = self.db.begin_read()?;
        let index = txn.open_table(INDEX)?;
        let Some(idx) = index
            .get(height)?
            .map(|g| from_bytes::<BlockIndex>(g.value()))
            .transpose()?
        else {
            return Ok(None);
        };
        let coinbase_t = txn.open_table(COINBASE)?;
        let cb_bytes = coinbase_t
            .get(height)?
            .ok_or_else(|| StoreError::Corrupt(format!("block {height} has no coinbase")))?
            .value()
            .to_vec();
        let coinbase = Coinbase::from_bytes(&cb_bytes)?;
        let block_txs = txn.open_table(BLOCK_TXS)?;
        let tx_ids = ids_of(
            block_txs
                .get(height)?
                .ok_or_else(|| {
                    StoreError::Corrupt(format!("block {height} has no transaction list"))
                })?
                .value(),
        )?;
        let prefixes = txn.open_table(TX_PREFIX)?;
        let prunable = txn.open_table(TX_PRUNABLE)?;
        let mut transactions = Vec::with_capacity(tx_ids.len());
        for id in &tx_ids {
            let row = prefixes.get(id.as_slice())?.ok_or_else(|| {
                StoreError::Corrupt(format!("block {height}: a transaction row is missing"))
            })?;
            let row: TxRow = from_bytes(row.value())?;
            let bytes = prunable.get(id.as_slice())?.map(|g| g.value().to_vec());
            transactions.push(StoredTx::with_prunable_bytes(row.tx, bytes)?);
        }
        Ok(Some(StoredBlock {
            index: idx,
            coinbase,
            transactions,
        }))
    }

    /// A transaction by id: the height it is in, and its stored form.
    pub fn tx(&self, id: &[u8; 32]) -> Result<Option<(u64, StoredTx)>> {
        let txn = self.db.begin_read()?;
        let prefixes = txn.open_table(TX_PREFIX)?;
        let Some(row) = prefixes
            .get(id.as_slice())?
            .map(|g| from_bytes::<TxRow>(g.value()))
            .transpose()?
        else {
            return Ok(None);
        };
        let bytes = txn
            .open_table(TX_PRUNABLE)?
            .get(id.as_slice())?
            .map(|g| g.value().to_vec());
        Ok(Some((
            row.height,
            StoredTx::with_prunable_bytes(row.tx, bytes)?,
        )))
    }

    /// How many outputs exist (the next global index).
    pub fn output_count(&self) -> Result<u64> {
        let txn = self.db.begin_read()?;
        let meta = txn.open_table(META)?;
        let g = meta
            .get("output_count")?
            .ok_or_else(|| StoreError::Corrupt("no output count".into()))?;
        u64_of(g.value(), "output_count")
    }

    pub fn output(&self, global_index: u64) -> Result<Option<StoredOutput>> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(OUTPUTS)?;
        let found = t.get(global_index)?;
        found.map(|g| from_bytes(g.value())).transpose()
    }

    /// The height of the block that spent this key image, if any.
    pub fn key_image_height(&self, key_image: &[u8; 32]) -> Result<Option<u64>> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(KEY_IMAGES)?;
        let found = t.get(key_image.as_slice())?;
        Ok(found.map(|g| g.value()))
    }

    /// Every block below this height has lost its proofs (and none at or above it has).
    pub fn pruned_below(&self) -> Result<u64> {
        let txn = self.db.begin_read()?;
        let meta = txn.open_table(META)?;
        let g = meta
            .get("pruned_below")?
            .ok_or_else(|| StoreError::Corrupt("no pruned_below".into()))?;
        u64_of(g.value(), "pruned_below")
    }

    /// A hash of the chain's STATE: every output in index order and every spent key image in order. It
    /// does not depend on which proofs are still stored, so pruning must never change it, and rolling a
    /// block back must restore it exactly. (It is also what a future state snapshot would be checked with.)
    pub fn state_digest(&self) -> Result<[u8; 32]> {
        let txn = self.db.begin_read()?;
        let mut h = Sha256Stream::new();
        h.update(STATE_TAG);
        let outputs = txn.open_table(OUTPUTS)?;
        let n = outputs.len()?;
        h.update(&n.to_le_bytes());
        for entry in outputs.iter()? {
            let (k, v) = entry?;
            h.update(&k.value().to_le_bytes());
            h.update(v.value());
        }
        let images = txn.open_table(KEY_IMAGES)?;
        h.update(&images.len()?.to_le_bytes());
        for entry in images.iter()? {
            let (k, v) = entry?;
            h.update(k.value());
            h.update(&v.value().to_le_bytes());
        }
        Ok(h.finalize())
    }

    // ------------------------------------------------------------------ writing

    /// Adds `block` on top of the tip, in one transaction.
    ///
    /// The store checks what would corrupt its own indexes (the parent, the height, the rules version,
    /// the Merkle root, duplicate ids, and that no key image is spent twice), and nothing else:
    /// proofs, fees, rewards, difficulty and maturity are the validator's job. `cumulative_work` is
    /// supplied by the caller. Global output indexes are assigned in the order of `CONSENSUS_V2.md`
    /// 14.6: the coinbase's outputs, then each transaction's, continuing from the last block.
    pub fn append_block(&self, block: &Block, cumulative_work: [u8; 32]) -> Result<AppendInfo> {
        let txn = self.db.begin_write()?;
        let info;
        {
            let mut index = txn.open_table(INDEX)?;
            let mut block_ids = txn.open_table(BLOCK_IDS)?;
            let mut coinbase_t = txn.open_table(COINBASE)?;
            let mut block_txs = txn.open_table(BLOCK_TXS)?;
            let mut prefixes = txn.open_table(TX_PREFIX)?;
            let mut prunable = txn.open_table(TX_PRUNABLE)?;
            let mut outputs = txn.open_table(OUTPUTS)?;
            let mut images = txn.open_table(KEY_IMAGES)?;
            let mut meta = txn.open_table(META)?;

            let (tip_height, tip) = {
                let (k, v) = index
                    .last()?
                    .ok_or_else(|| StoreError::Corrupt("the index has no genesis".into()))?;
                (k.value(), from_bytes::<BlockIndex>(v.value())?)
            };
            let height = tip_height + 1;
            if block.header.version != VERSION {
                return Err(StoreError::BadVersion(block.header.version));
            }
            if block.header.prev_id != tip.block_id {
                return Err(StoreError::BadParent);
            }
            if block.coinbase.height != height {
                return Err(StoreError::BadHeight {
                    expected: height,
                    got: block.coinbase.height,
                });
            }
            if ids::block_tx_root(&block.coinbase, &block.transactions)? != block.header.tx_root {
                return Err(StoreError::BadTxRoot);
            }
            let block_id = ids::block_id(&block.header, self.pow);
            if block_ids.insert(block_id.as_slice(), height)?.is_some() {
                return Err(StoreError::Duplicate("block"));
            }

            let first_output_index = {
                let g = meta
                    .get("output_count")?
                    .ok_or_else(|| StoreError::Corrupt("no output count".into()))?;
                u64_of(g.value(), "output_count")?
            };
            let mut next_output = first_output_index;

            for o in &block.coinbase.outputs {
                let rec = StoredOutput {
                    onetime_address: o.onetime_address,
                    amount_commitment: [0; 32],
                    public_amount: o.amount,
                    height,
                    coinbase: true,
                };
                outputs.insert(next_output, to_bytes(&rec)?.as_slice())?;
                next_output += 1;
            }
            coinbase_t.insert(height, block.coinbase.to_bytes()?.as_slice())?;

            let mut tx_ids = Vec::with_capacity(32 * block.transactions.len());
            for t in &block.transactions {
                let id = ids::tx_id(t)?;
                let pruned = t.prune()?;
                let row = TxRow { height, tx: pruned };
                if prefixes
                    .insert(id.as_slice(), to_bytes(&row)?.as_slice())?
                    .is_some()
                {
                    return Err(StoreError::Duplicate("transaction"));
                }
                prunable.insert(
                    id.as_slice(),
                    t.prunable.to_bytes(t.prefix.inputs.len())?.as_slice(),
                )?;
                for input in &t.prefix.inputs {
                    if images.insert(input.key_image.as_slice(), height)?.is_some() {
                        return Err(StoreError::DoubleSpend(input.key_image));
                    }
                }
                for o in &t.prefix.outputs {
                    let rec = StoredOutput {
                        onetime_address: o.onetime_address,
                        amount_commitment: o.amount_commitment,
                        public_amount: 0,
                        height,
                        coinbase: false,
                    };
                    outputs.insert(next_output, to_bytes(&rec)?.as_slice())?;
                    next_output += 1;
                }
                tx_ids.extend_from_slice(&id);
            }
            block_txs.insert(height, tx_ids.as_slice())?;

            let output_count = u32::try_from(next_output - first_output_index).map_err(|_| {
                StoreError::Corrupt("a block created more than 2^32 outputs".into())
            })?;
            let record = BlockIndex {
                block_id,
                header: block.header.clone(),
                cumulative_work,
                first_output_index,
                output_count,
                tx_count: u32::try_from(block.transactions.len()).expect("at most MAX_BLOCK_TXS"),
            };
            index.insert(height, to_bytes(&record)?.as_slice())?;
            meta.insert("output_count", next_output.to_le_bytes().as_slice())?;
            info = AppendInfo {
                height,
                block_id,
                first_output_index,
                output_count,
            };
        }
        txn.commit()?;
        Ok(info)
    }

    /// Removes the tip block, for a reorganisation, and returns it (with its proofs if they were still
    /// stored). It restores the state exactly as it was before that block: its outputs and its key
    /// images go, and the global output indexes it used are free again. **It works on pruned blocks**
    /// too, because everything it needs (the key images and the output range) is in the prefix.
    pub fn pop_block(&self) -> Result<StoredBlock> {
        let txn = self.db.begin_write()?;
        let popped;
        {
            let mut index = txn.open_table(INDEX)?;
            let mut block_ids = txn.open_table(BLOCK_IDS)?;
            let mut coinbase_t = txn.open_table(COINBASE)?;
            let mut block_txs = txn.open_table(BLOCK_TXS)?;
            let mut prefixes = txn.open_table(TX_PREFIX)?;
            let mut prunable = txn.open_table(TX_PRUNABLE)?;
            let mut outputs = txn.open_table(OUTPUTS)?;
            let mut images = txn.open_table(KEY_IMAGES)?;
            let mut meta = txn.open_table(META)?;

            let (tip_height, tip) = {
                let (k, v) = index
                    .last()?
                    .ok_or_else(|| StoreError::Corrupt("the index has no genesis".into()))?;
                (k.value(), from_bytes::<BlockIndex>(v.value())?)
            };
            if tip_height == 0 {
                return Err(StoreError::CannotPopGenesis);
            }
            let coinbase = {
                let g = coinbase_t
                    .remove(tip_height)?
                    .ok_or_else(|| StoreError::Corrupt("no coinbase".into()))?;
                Coinbase::from_bytes(g.value())?
            };
            let tx_ids = {
                let g = block_txs
                    .remove(tip_height)?
                    .ok_or_else(|| StoreError::Corrupt("no transaction list".into()))?;
                ids_of(g.value())?
            };
            let mut transactions = Vec::with_capacity(tx_ids.len());
            for id in &tx_ids {
                let row: TxRow = {
                    let g = prefixes.remove(id.as_slice())?.ok_or_else(|| {
                        StoreError::Corrupt("a transaction row is missing".into())
                    })?;
                    from_bytes(g.value())?
                };
                let bytes = prunable.remove(id.as_slice())?.map(|g| g.value().to_vec());
                for input in &row.tx.prefix.inputs {
                    let spent_at = images
                        .remove(input.key_image.as_slice())?
                        .map(|g| g.value());
                    if spent_at != Some(tip_height) {
                        return Err(StoreError::Corrupt(format!(
                            "key image {} is recorded at {spent_at:?}, not at the tip {tip_height}",
                            hex_lower(&input.key_image)
                        )));
                    }
                }
                transactions.push(StoredTx::with_prunable_bytes(row.tx, bytes)?);
            }
            for i in tip.first_output_index..tip.first_output_index + u64::from(tip.output_count) {
                if outputs.remove(i)?.is_none() {
                    return Err(StoreError::Corrupt(format!(
                        "output {i} of the tip block is missing"
                    )));
                }
            }
            block_ids.remove(tip.block_id.as_slice())?;
            index.remove(tip_height)?;
            meta.insert(
                "output_count",
                tip.first_output_index.to_le_bytes().as_slice(),
            )?;
            // every block below `pruned_below` has lost its proofs; with the tip gone, that cannot
            // reach past the new tip
            let below = {
                let g = meta
                    .get("pruned_below")?
                    .ok_or_else(|| StoreError::Corrupt("no pruned_below".into()))?;
                u64_of(g.value(), "pruned_below")?
            };
            meta.insert(
                "pruned_below",
                below.min(tip_height).to_le_bytes().as_slice(),
            )?;
            popped = StoredBlock {
                index: tip,
                coinbase,
                transactions,
            };
        }
        txn.commit()?;
        Ok(popped)
    }

    /// Deletes the proofs of every block below `height`. Idempotent, and it never moves backwards: a
    /// height at or below the current `pruned_below` does nothing. It changes NO id, no Merkle root and no
    /// part of the state; only the `tx_prunable` table shrinks. (`compact` then returns the space.)
    pub fn prune_below(&self, height: u64) -> Result<PruneStats> {
        let txn = self.db.begin_write()?;
        let stats;
        {
            let index = txn.open_table(INDEX)?;
            let block_txs = txn.open_table(BLOCK_TXS)?;
            let mut prunable = txn.open_table(TX_PRUNABLE)?;
            let mut meta = txn.open_table(META)?;
            let tip_height = {
                let (k, _) = index
                    .last()?
                    .ok_or_else(|| StoreError::Corrupt("the index has no genesis".into()))?;
                k.value()
            };
            if height > tip_height + 1 {
                return Err(StoreError::PruneBeyondTip {
                    tip: tip_height,
                    asked: height,
                });
            }
            let current = {
                let g = meta
                    .get("pruned_below")?
                    .ok_or_else(|| StoreError::Corrupt("no pruned_below".into()))?;
                u64_of(g.value(), "pruned_below")?
            };
            let mut s = PruneStats {
                pruned_below: current.max(height),
                ..PruneStats::default()
            };
            for h in current..height {
                let ids = {
                    let g = block_txs.get(h)?.ok_or_else(|| {
                        StoreError::Corrupt(format!("no transaction list at {h}"))
                    })?;
                    ids_of(g.value())?
                };
                for id in ids {
                    if let Some(old) = prunable.remove(id.as_slice())? {
                        s.transactions_pruned += 1;
                        s.prunable_bytes_freed += old.value().len() as u64;
                    }
                }
            }
            meta.insert("pruned_below", s.pruned_below.to_le_bytes().as_slice())?;
            stats = s;
        }
        txn.commit()?;
        Ok(stats)
    }

    /// Like `prune_below`, but in steps of at most `step_blocks` blocks, each its own transaction.
    ///
    /// Measured (`tests/store.rs`, the compaction test): one prune transaction over 149 blocks
    /// roughly DOUBLED the file before compaction (24.7 MB to 49.4 MB), because a copy-on-write database
    /// needs room for the pages it is replacing. Bounding each transaction bounds that. The result is
    /// the same as one `prune_below(height)`, and a crash in the middle just leaves it partly done.
    pub fn prune_below_in_steps(&self, height: u64, step_blocks: u64) -> Result<PruneStats> {
        let (tip, _) = self.tip()?;
        if height > tip + 1 {
            return Err(StoreError::PruneBeyondTip { tip, asked: height });
        }
        let step = step_blocks.max(1);
        let mut at = self.pruned_below()?;
        let mut total = PruneStats {
            pruned_below: at,
            ..PruneStats::default()
        };
        while at < height {
            let next = height.min(at.saturating_add(step));
            let s = self.prune_below(next)?;
            total.transactions_pruned += s.transactions_pruned;
            total.prunable_bytes_freed += s.prunable_bytes_freed;
            total.pruned_below = s.pruned_below;
            at = next;
        }
        Ok(total)
    }

    /// Prunes everything except the most recent `keep_blocks` blocks (the policy of
    /// `CONSENSUS_V2.md` 14.6: `PRUNE_KEEP_BLOCKS`, proposed default 5,500).
    pub fn prune_keeping(&self, keep_blocks: u64) -> Result<PruneStats> {
        let (tip, _) = self.tip()?;
        self.prune_below((tip + 1).saturating_sub(keep_blocks))
    }

    /// Recomputes the transaction Merkle root of the block at `height` from what is STORED (the pruned
    /// forms and the coinbase), and returns it. It equals the header's `tx_root` whether or not the
    /// proofs have been pruned; that is the property the prefix/prunable split exists for.
    pub fn recompute_tx_root(&self, height: u64) -> Result<Option<[u8; 32]>> {
        let Some(b) = self.get_block(height)? else {
            return Ok(None);
        };
        let mut leaves = Vec::with_capacity(1 + b.transactions.len());
        leaves.push(ids::coinbase_id(&b.coinbase)?);
        for t in &b.transactions {
            leaves.push(ids::pruned_tx_id(&PrunedTransaction {
                prefix: t.tx.prefix.clone(),
                prunable_hash: t.tx.prunable_hash,
            })?);
        }
        Ok(Some(ids::merkle_root(&leaves)))
    }
}
