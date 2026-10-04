//! The store: a redb database for the index and the state, and flat segment files for the prunable data.
//!
//! | table | key -> value |
//! |---|---|
//! | `index` | height -> `BlockIndex` (id, header, cumulative work, output range) |
//! | `block_ids` | block id -> height |
//! | `coinbase` | height -> coinbase bytes |
//! | `block_txs` | height -> the transaction ids, 32 bytes each, in order |
//! | `tx_prefix` | transaction id -> height and the PRUNED transaction (prefix and prunable hash) |
//! | `tx_loc` | transaction id -> where its prunable bytes are: segment, offset, length (16 bytes). **The only table pruning touches** |
//! | `seg_len` | segment id -> the committed length of that segment file |
//! | `outputs` | global output index -> `StoredOutput` |
//! | `key_images` | key image -> height (the spent set) |
//! | `meta` | format, chain id, proof of work, segment size, `pruned_below`, output count |
//!
//! The rings and proofs themselves are in `<database path>.segments/seg-<id>.dat` (see [`crate::segments`]).
//! Every mutation is one redb transaction, and the segment bytes are written and synced before it commits:
//! the result is whole or not at all, and a crash can only leave bytes nothing refers to.

use crate::error::{Result, StoreError};
use crate::records::{
    from_bytes, to_bytes, AppendInfo, BlockIndex, BlockMeta, PruneStats, StoredBlock, StoredOutput,
    StoredTx, TxRow,
};
use crate::segments::Segments;
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
const TX_LOC: TableDefinition<&[u8], &[u8]> = TableDefinition::new("tx_loc");
const SEG_LEN: TableDefinition<u64, u64> = TableDefinition::new("seg_len");
const OUTPUTS: TableDefinition<u64, &[u8]> = TableDefinition::new("outputs");
const KEY_IMAGES: TableDefinition<&[u8], u64> = TableDefinition::new("key_images");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

/// The on-disk layout version, in `meta`. Version 2: the prunable data is in segment files. Version 3: the
/// block record also holds the block's target and body size. Version 4: every record in a segment file is followed by
/// a 16-byte checksum (`CHECK_LEN`), which every read verifies. A store of an older version is refused (`WrongFormat`), not guessed at.
pub const FORMAT_VERSION: u32 = 4;

/// The bytes of checksum after each record in a segment file: the first 16 bytes of a SHA-256 of the record and of where it is.
const CHECK_LEN: usize = 16;

/// The checksum of a record. It covers the place too (segment, offset, length), so a record that is intact but was read from
/// the wrong place, or a length the database got wrong, is also caught.
fn record_check(segment: u32, offset: u64, payload: &[u8]) -> [u8; CHECK_LEN] {
    let h = tenero_core::hash::sha256(&[
        b"tenero segment record v1",
        &segment.to_le_bytes(),
        &offset.to_le_bytes(),
        &(payload.len() as u64).to_le_bytes(),
        payload,
    ]);
    let mut out = [0u8; CHECK_LEN];
    out.copy_from_slice(&h[..CHECK_LEN]);
    out
}

/// How many block heights one segment file covers, by default (about 17 hours of 60-second blocks; at the
/// worst case of full 150 kB blocks about 130 MB of prunable data).
pub const DEFAULT_SEGMENT_BLOCKS: u64 = 1_000;

/// Where one transaction's prunable bytes are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Loc {
    segment: u32,
    offset: u64,
    len: u32,
}

impl Loc {
    fn to_bytes(self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0..4].copy_from_slice(&self.segment.to_le_bytes());
        b[4..12].copy_from_slice(&self.offset.to_le_bytes());
        b[12..16].copy_from_slice(&self.len.to_le_bytes());
        b
    }

    fn from_bytes(b: &[u8]) -> Result<Loc> {
        let a: [u8; 16] = b
            .try_into()
            .map_err(|_| StoreError::Corrupt("a location is not 16 bytes".into()))?;
        Ok(Loc {
            segment: u32::from_le_bytes([a[0], a[1], a[2], a[3]]),
            offset: u64::from_le_bytes([a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11]]),
            len: u32::from_le_bytes([a[12], a[13], a[14], a[15]]),
        })
    }
}

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
    segments: Segments,
    segment_blocks: u64,
    pow: PowKind,
    chain_id: [u8; 32],
}

impl Store {
    /// Opens the database at `path`, creating it (with the genesis block of the network named `label`)
    /// if it does not exist. Opening a file made for another network or proof of work is an error, so a
    /// node can never mix chains. A new database uses `DEFAULT_SEGMENT_BLOCKS`; an existing one keeps its own.
    pub fn open(path: impl AsRef<Path>, label: &str, pow: PowKind) -> Result<Store> {
        Store::open_with(path, label, pow, None)
    }

    /// Like `open`, with an explicit segment size (in blocks). For a new database it is recorded; for an
    /// existing one it must equal the recorded size, or the result is `WrongFormat`.
    pub fn open_with(
        path: impl AsRef<Path>,
        label: &str,
        pow: PowKind,
        segment_blocks: Option<u64>,
    ) -> Result<Store> {
        if segment_blocks == Some(0) {
            return Err(StoreError::WrongFormat);
        }
        let path = path.as_ref().to_path_buf();
        let mut segments_dir = path.clone().into_os_string();
        segments_dir.push(".segments");
        let segments = Segments::open(PathBuf::from(segments_dir))?;
        let db = Database::create(&path)?;
        let chain_id = ids::genesis_id(label);
        let mut chosen_segment_blocks = segment_blocks.unwrap_or(DEFAULT_SEGMENT_BLOCKS);
        let txn = db.begin_write()?;
        {
            // opening a table creates it, so every table exists from the first commit
            let mut index = txn.open_table(INDEX)?;
            let mut block_ids = txn.open_table(BLOCK_IDS)?;
            let mut block_txs = txn.open_table(BLOCK_TXS)?;
            txn.open_table(COINBASE)?;
            txn.open_table(TX_PREFIX)?;
            txn.open_table(TX_LOC)?;
            txn.open_table(SEG_LEN)?;
            txn.open_table(OUTPUTS)?;
            txn.open_table(KEY_IMAGES)?;
            let mut meta = txn.open_table(META)?;

            let existing = meta.get("chain_id")?.map(|g| g.value().to_vec());
            match existing {
                Some(id) => {
                    let format = meta.get("format")?.map(|g| g.value().to_vec());
                    let stored_pow = meta.get("pow")?.map(|g| g.value().to_vec());
                    if id != chain_id || stored_pow.as_deref() != Some(&[pow_byte(pow)][..]) {
                        return Err(StoreError::WrongChain);
                    }
                    if format.as_deref() != Some(&FORMAT_VERSION.to_le_bytes()[..]) {
                        return Err(StoreError::WrongFormat);
                    }
                    let stored = {
                        let g = meta
                            .get("segment_blocks")?
                            .ok_or_else(|| StoreError::Corrupt("no segment size".into()))?;
                        u64_of(g.value(), "segment_blocks")?
                    };
                    if segment_blocks.is_some_and(|asked| asked != stored) {
                        return Err(StoreError::WrongFormat);
                    }
                    chosen_segment_blocks = stored;
                }
                None => {
                    meta.insert(
                        "segment_blocks",
                        chosen_segment_blocks.to_le_bytes().as_slice(),
                    )?;
                    let genesis = BlockIndex {
                        block_id: chain_id,
                        header: ids::genesis_header(label),
                        cumulative_work: [0; 32],
                        target: [0; 32],
                        body_size: 0,
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
        let store = Store {
            db,
            path,
            segments,
            segment_blocks: chosen_segment_blocks,
            pow,
            chain_id,
        };
        // a segment file the database does not know (left by a crash) is an orphan
        store.sweep_segments()?;
        Ok(store)
    }

    /// The number of block heights one segment file covers.
    pub fn segment_blocks(&self) -> u64 {
        self.segment_blocks
    }

    /// The directory holding the segment files.
    pub fn segments_dir(&self) -> &Path {
        self.segments.dir()
    }

    /// The ids of the segment files that exist.
    pub fn segment_ids(&self) -> Result<Vec<u64>> {
        self.segments.ids()
    }

    /// Deletes every segment file the database has no committed length for: files whose blocks have all
    /// been pruned, files left behind by a crash between a commit and a deletion, and files written for a
    /// transaction that never committed. Returns how many files and bytes were removed.
    fn sweep_segments(&self) -> Result<(u64, u64)> {
        let known: std::collections::BTreeSet<u64> = {
            let txn = self.db.begin_read()?;
            let t = txn.open_table(SEG_LEN)?;
            let mut s = std::collections::BTreeSet::new();
            for entry in t.iter()? {
                s.insert(entry?.0.value());
            }
            s
        };
        let (mut files, mut bytes) = (0, 0);
        for id in self.segments.ids()? {
            if !known.contains(&id) {
                bytes += self.segments.remove(id)?;
                files += 1;
            }
        }
        Ok((files, bytes))
    }

    /// Reads the prunable bytes at these locations (`None` where the transaction has been pruned),
    /// opening each segment file once.
    fn read_prunables(&self, locs: &[Option<Loc>]) -> Result<Vec<Option<Vec<u8>>>> {
        let mut out: Vec<Option<Vec<u8>>> = vec![None; locs.len()];
        let mut by_segment: std::collections::BTreeMap<u32, Vec<(usize, Loc)>> =
            std::collections::BTreeMap::new();
        for (i, loc) in locs.iter().enumerate() {
            if let Some(l) = loc {
                by_segment.entry(l.segment).or_default().push((i, *l));
            }
        }
        for (segment, items) in by_segment {
            let ranges: Vec<(u64, u32)> = items
                .iter()
                .map(|(_, l)| (l.offset, l.len.saturating_add(CHECK_LEN as u32)))
                .collect();
            for ((i, loc), mut bytes) in items
                .iter()
                .zip(self.segments.read_many(u64::from(segment), &ranges)?)
            {
                let payload_len = bytes.len() - CHECK_LEN;
                let tag = bytes.split_off(payload_len);
                if tag[..] != record_check(segment, loc.offset, &bytes)[..] {
                    return Err(StoreError::Corrupt(format!(
                        "segment {segment}: the record at byte {} ({} bytes) fails its checksum: the file is damaged",
                        loc.offset, loc.len
                    )));
                }
                out[*i] = Some(bytes);
            }
        }
        Ok(out)
    }

    /// The chain id (the genesis block id) this database is bound to.
    pub fn chain_id(&self) -> [u8; 32] {
        self.chain_id
    }

    pub fn pow(&self) -> PowKind {
        self.pow
    }

    /// The size of the database file in bytes (not counting the segment files).
    pub fn file_size(&self) -> std::io::Result<u64> {
        Ok(std::fs::metadata(&self.path)?.len())
    }

    /// The size of the segment files in bytes.
    pub fn segments_size(&self) -> Result<u64> {
        self.segments.total_size()
    }

    /// Everything this store keeps on disk: the database and the segment files.
    pub fn total_size(&self) -> Result<u64> {
        Ok(self.file_size()? + self.segments_size()?)
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
        let locations = txn.open_table(TX_LOC)?;
        let mut rows = Vec::with_capacity(tx_ids.len());
        let mut locs = Vec::with_capacity(tx_ids.len());
        for id in &tx_ids {
            let row = prefixes.get(id.as_slice())?.ok_or_else(|| {
                StoreError::Corrupt(format!("block {height}: a transaction row is missing"))
            })?;
            rows.push(from_bytes::<TxRow>(row.value())?);
            let loc = locations
                .get(id.as_slice())?
                .map(|g| Loc::from_bytes(g.value()))
                .transpose()?;
            locs.push(loc);
        }
        let mut transactions = Vec::with_capacity(rows.len());
        for (row, bytes) in rows.into_iter().zip(self.read_prunables(&locs)?) {
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
        let loc = txn
            .open_table(TX_LOC)?
            .get(id.as_slice())?
            .map(|g| Loc::from_bytes(g.value()))
            .transpose()?;
        let bytes = self.read_prunables(&[loc])?.remove(0);
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
    pub fn append_block(&self, block: &Block, meta_in: BlockMeta) -> Result<AppendInfo> {
        let txn = self.db.begin_write()?;
        let info;
        {
            let mut index = txn.open_table(INDEX)?;
            let mut block_ids = txn.open_table(BLOCK_IDS)?;
            let mut coinbase_t = txn.open_table(COINBASE)?;
            let mut block_txs = txn.open_table(BLOCK_TXS)?;
            let mut prefixes = txn.open_table(TX_PREFIX)?;
            let mut tx_loc = txn.open_table(TX_LOC)?;
            let mut seg_len = txn.open_table(SEG_LEN)?;
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
            // this block's prunable bytes go to one segment, at its last COMMITTED length
            let segment = height / self.segment_blocks;
            let segment32 = u32::try_from(segment)
                .map_err(|_| StoreError::Corrupt("more than 2^32 segments".into()))?;
            let committed = seg_len.get(segment)?.map(|g| g.value()).unwrap_or(0);
            let mut blob: Vec<u8> = Vec::new();
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
                let bytes = t.prunable.to_bytes(t.prefix.inputs.len())?;
                let record_offset = committed + blob.len() as u64;
                let loc = Loc {
                    segment: segment32,
                    offset: record_offset,
                    len: u32::try_from(bytes.len())
                        .map_err(|_| StoreError::Corrupt("prunable data over 4 GiB".into()))?,
                };
                tx_loc.insert(id.as_slice(), loc.to_bytes().as_slice())?;
                blob.extend_from_slice(&bytes);
                blob.extend_from_slice(&record_check(segment32, record_offset, &bytes));
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
                cumulative_work: meta_in.cumulative_work,
                target: meta_in.target,
                body_size: meta_in.body_size,
                first_output_index,
                output_count,
                tx_count: u32::try_from(block.transactions.len()).expect("at most MAX_BLOCK_TXS"),
            };
            index.insert(height, to_bytes(&record)?.as_slice())?;
            meta.insert("output_count", next_output.to_le_bytes().as_slice())?;
            // Every check has passed. Now the bytes reach the disk BEFORE the database refers to them, and
            // at the committed length, so a crash before the commit leaves only bytes nothing refers to.
            if !blob.is_empty() {
                self.segments.write_at(segment, committed, &blob)?;
                seg_len.insert(segment, committed + blob.len() as u64)?;
            }
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
            let mut tx_loc = txn.open_table(TX_LOC)?;
            let mut seg_len = txn.open_table(SEG_LEN)?;
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
            let segment = tip_height / self.segment_blocks;
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
            let mut rows = Vec::with_capacity(tx_ids.len());
            let mut locs: Vec<Option<Loc>> = Vec::with_capacity(tx_ids.len());
            for id in &tx_ids {
                let row: TxRow = {
                    let g = prefixes.remove(id.as_slice())?.ok_or_else(|| {
                        StoreError::Corrupt("a transaction row is missing".into())
                    })?;
                    from_bytes(g.value())?
                };
                let loc = tx_loc
                    .remove(id.as_slice())?
                    .map(|g| Loc::from_bytes(g.value()))
                    .transpose()?;
                locs.push(loc);
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
                rows.push(row);
            }
            // the block's bytes are at the end of its segment: hand them back, and give the space back
            let bytes = self.read_prunables(&locs)?;
            let mut transactions = Vec::with_capacity(rows.len());
            for (row, b) in rows.into_iter().zip(bytes) {
                transactions.push(StoredTx::with_prunable_bytes(row.tx, b)?);
            }
            let mut emptied_segment = None;
            if let Some(start) = locs.iter().flatten().map(|l| l.offset).min() {
                if start == 0 {
                    seg_len.remove(segment)?;
                    emptied_segment = Some(segment);
                } else {
                    seg_len.insert(segment, start)?;
                }
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
            popped = (
                StoredBlock {
                    index: tip,
                    coinbase,
                    transactions,
                },
                emptied_segment,
            );
        }
        txn.commit()?;
        let (block, emptied_segment) = popped;
        if let Some(id) = emptied_segment {
            self.segments.remove(id)?;
        }
        Ok(block)
    }

    /// Deletes the rings and proofs of every block below `height`. Idempotent, and it never moves
    /// backwards: a height at or below the current `pruned_below` does nothing. It changes NO id, no
    /// Merkle root and no part of the state. The locations go from the database at once (exactly, by
    /// height), and a segment file is deleted as soon as every block in it is below `pruned_below`; the
    /// space of a file that still holds some unpruned blocks comes back when the rest are pruned.
    pub fn prune_below(&self, height: u64) -> Result<PruneStats> {
        let txn = self.db.begin_write()?;
        let mut stats;
        let dead_segments: Vec<u64>;
        {
            let index = txn.open_table(INDEX)?;
            let block_txs = txn.open_table(BLOCK_TXS)?;
            let mut tx_loc = txn.open_table(TX_LOC)?;
            let mut seg_len = txn.open_table(SEG_LEN)?;
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
                    if let Some(old) = tx_loc.remove(id.as_slice())? {
                        s.transactions_pruned += 1;
                        s.prunable_bytes_freed +=
                            u64::from(Loc::from_bytes(old.value())?.len) + CHECK_LEN as u64;
                    }
                }
            }
            meta.insert("pruned_below", s.pruned_below.to_le_bytes().as_slice())?;
            // every segment whose blocks are all below `pruned_below` is dead: forget its length now
            // (in this transaction), delete the file once this has committed
            let first_live = s.pruned_below / self.segment_blocks;
            let mut dead = Vec::new();
            for entry in seg_len.range(..first_live)? {
                dead.push(entry?.0.value());
            }
            for id in &dead {
                seg_len.remove(*id)?;
            }
            dead_segments = dead;
            stats = s;
        }
        txn.commit()?;
        for id in dead_segments {
            let bytes = self.segments.remove(id)?;
            stats.segments_deleted += 1;
            stats.segment_bytes_freed += bytes;
        }
        Ok(stats)
    }

    /// Like `prune_below`, but in steps of at most `step_blocks` blocks, each its own transaction, so no
    /// single transaction touches more than that many blocks (a bound on how much a crash can leave
    /// half done and on how long the database is held). The result is the same as one
    /// `prune_below(height)`. (Pruning is cheap either way: what it deletes from the database is a
    /// 16-byte location per transaction, and the big data is whole files.)
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
            total.segments_deleted += s.segments_deleted;
            total.segment_bytes_freed += s.segment_bytes_freed;
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
