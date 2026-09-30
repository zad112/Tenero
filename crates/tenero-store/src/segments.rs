//! Flat segment files for the prunable data (`docs/CONSENSUS_V2.md` 14.3).
//!
//! The rings and proofs of a transaction are large, written once, never changed, and deleted in whole
//! ranges of blocks. A B-tree is the wrong home for that (measured: rows of about 2 kB do not pack into its
//! 4 KiB pages, and deleting them leaves free pages that only a compaction returns), so they live here: one file per range of
//! `segment_blocks` heights, `seg-<id>.dat`, where `id = height / segment_blocks`. The database keeps only
//! where each transaction's bytes are. Pruning deletes rows, and a file once every block in it is pruned.
//!
//! Crash safety rests on one rule, kept by the caller: data is written and synced HERE before the database
//! transaction that refers to it commits, and it is always written at the last COMMITTED length of the
//! segment. So a crash between the two leaves bytes nothing refers to, which the next write simply
//! overwrites, and a segment file the database knows nothing about is an orphan that `open` deletes.

use crate::error::{Result, StoreError};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

/// The directory of segment files.
pub struct Segments {
    dir: PathBuf,
}

impl Segments {
    pub fn open(dir: PathBuf) -> Result<Segments> {
        fs::create_dir_all(&dir)?;
        Ok(Segments { dir })
    }

    fn file(&self, id: u64) -> PathBuf {
        self.dir.join(format!("seg-{id:010}.dat"))
    }

    /// Writes `bytes` at `offset` of segment `id` (creating the file), makes the file end exactly there
    /// (dropping any leftover bytes of an earlier crashed write), and syncs it to disk.
    pub fn write_at(&self, id: u64, offset: u64, bytes: &[u8]) -> Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.file(id))?;
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(bytes)?;
        f.set_len(offset + bytes.len() as u64)?;
        f.sync_data()?;
        Ok(())
    }

    /// Reads several `(offset, length)` ranges of one segment with one open. A range past the end of the
    /// file, or a missing file, is `Corrupt`: the database said the bytes are there.
    pub fn read_many(&self, id: u64, ranges: &[(u64, u32)]) -> Result<Vec<Vec<u8>>> {
        if ranges.is_empty() {
            return Ok(vec![]);
        }
        let mut f = File::open(self.file(id))
            .map_err(|e| StoreError::Corrupt(format!("segment {id} cannot be opened: {e}")))?;
        let mut out = Vec::with_capacity(ranges.len());
        for &(offset, len) in ranges {
            f.seek(SeekFrom::Start(offset))?;
            let mut buf = vec![0u8; len as usize];
            f.read_exact(&mut buf).map_err(|e| {
                StoreError::Corrupt(format!(
                    "segment {id}: {len} bytes at {offset} are not there: {e}"
                ))
            })?;
            out.push(buf);
        }
        Ok(out)
    }

    /// The ids of the segment files that exist.
    pub fn ids(&self) -> Result<Vec<u64>> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let name = entry?.file_name();
            let name = name.to_string_lossy();
            if let Some(n) = name
                .strip_prefix("seg-")
                .and_then(|s| s.strip_suffix(".dat"))
            {
                if let Ok(id) = n.parse::<u64>() {
                    ids.push(id);
                }
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }

    /// Deletes segment `id` and returns its size in bytes (0 if there was no such file).
    pub fn remove(&self, id: u64) -> Result<u64> {
        let path = self.file(id);
        let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        match fs::remove_file(&path) {
            Ok(()) => Ok(size),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e.into()),
        }
    }

    /// The total size of all segment files.
    pub fn total_size(&self) -> Result<u64> {
        let mut total = 0;
        for id in self.ids()? {
            total += fs::metadata(self.file(id))?.len();
        }
        Ok(total)
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }
}
