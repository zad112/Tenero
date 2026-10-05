//! Moving the node's data (the chain, the node key, the peers file) to another folder or drive, for the wallet app.
//!
//! **A copy, never a delete.** The old folder is not touched: after a good move the app says where it is and the person deletes it once the node has
//! run from the new place. Nothing here can lose the chain: the originals are only read.
//!
//! * **Checked.** After copying, every file is read back and compared byte for byte with its original. A copy that fails, does not match or is
//!   cancelled removes only the files and folders *it* made, and the setting stays where it was.
//! * **The new folder is made private first** (only the owner and the system, as the node itself makes a new data folder: `private_dir`), so the node
//!   accepts it and the files inherit that.
//! * **Not copied:** `control.cookie` (the node makes a new one when it starts; an old one would be wrong). Links are refused (they could point
//!   anywhere); the data folder has none.
//! * **Not checked:** free space (the standard library cannot tell). A full disk is an error from the write, and everything made is removed again.
//!
//! The node and the miner must be stopped (the core checks that); this module only copies.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

const BUF: usize = 4 * 1024 * 1024;

/// Files at the top of the data folder that are not copied.
const SKIP: &[&str] = &["control.cookie"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Listing what there is and adding up its size.
    Measuring,
    Copying,
    /// Reading the copy back and comparing it with the original.
    Checking,
}

/// What a running move reports, and the way to stop it. Shared between the copying thread and the window.
#[derive(Default)]
pub struct Progress {
    phase: AtomicU8,
    done: AtomicU64,
    total: AtomicU64,
    cancel: AtomicBool,
}

impl Progress {
    pub fn phase(&self) -> Phase {
        match self.phase.load(Ordering::Relaxed) {
            0 => Phase::Measuring,
            1 => Phase::Copying,
            _ => Phase::Checking,
        }
    }
    /// Bytes copied (or compared, in the checking phase) so far.
    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }
    /// Bytes to copy in all (0 until measured).
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
    /// Asks the copy to stop; it removes what it made and reports `cancelled`.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
    fn set_phase(&self, p: Phase) {
        self.phase.store(
            match p {
                Phase::Measuring => 0,
                Phase::Copying => 1,
                Phase::Checking => 2,
            },
            Ordering::Relaxed,
        );
        self.done.store(0, Ordering::Relaxed);
    }
}

/// Whether `to` may receive the data of `from` (which may not exist: then there is nothing to move, and this still checks `to`).
pub fn check_destination(from: &Path, to: &Path) -> Result<(), String> {
    if !to.is_absolute() {
        return Err(
            "the new folder must be a full path, for example D:\\TeneroData (a drive letter and a folder)"
                .into(),
        );
    }
    let (Some(parent), Some(name)) = (to.parent(), to.file_name()) else {
        return Err(
            "that is a whole drive: choose a folder on it, for example D:\\TeneroData".into(),
        );
    };
    if !parent.is_dir() {
        return Err(format!(
            "{} does not exist: create it first, or choose a folder inside one that does",
            parent.display()
        ));
    }
    if to.exists() && !to.is_dir() {
        return Err(format!("{} is a file, not a folder", to.display()));
    }
    // the same folder, or one inside it, is no move at all (and would copy into itself): said before "not empty", which it also is
    if let Ok(from_c) = fs::canonicalize(from) {
        let to_c = if to.exists() {
            fs::canonicalize(to)
        } else {
            fs::canonicalize(parent).map(|p| p.join(name))
        }
        .map_err(|e| format!("cannot resolve {}: {e}", to.display()))?;
        if to_c == from_c || to_c.starts_with(&from_c) {
            return Err("the new folder is the node's data folder itself, or inside it".into());
        }
    }
    if to.exists() {
        let mut entries =
            fs::read_dir(to).map_err(|e| format!("cannot look in {}: {e}", to.display()))?;
        if entries.next().is_some() {
            return Err(format!(
                "{} is not empty: choose a new folder, or an empty one",
                to.display()
            ));
        }
    }
    Ok(())
}

/// A file under the data folder: its path relative to it, and its size.
type Entry = (PathBuf, u64);

/// Every folder (parents first) and file under `root`, as paths relative to it, with the files' sizes. Links are refused.
fn list(root: &Path) -> Result<(Vec<PathBuf>, Vec<Entry>), String> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let here = root.join(&rel);
        let entries =
            fs::read_dir(&here).map_err(|e| format!("cannot read {}: {e}", here.display()))?;
        let mut names: Vec<_> = entries
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("cannot read {}: {e}", here.display()))?;
        names.sort_by_key(|e| e.file_name());
        for e in names {
            let name = e.file_name();
            let child = rel.join(&name);
            if rel.as_os_str().is_empty() && SKIP.iter().any(|s| name == **s) {
                continue;
            }
            let meta = fs::symlink_metadata(e.path())
                .map_err(|e2| format!("cannot look at {}: {e2}", e.path().display()))?;
            if meta.file_type().is_symlink() {
                return Err(format!(
                    "{} is a link: it is not copied (move this folder by hand)",
                    e.path().display()
                ));
            }
            if meta.is_dir() {
                dirs.push(child.clone());
                stack.push(child);
            } else if meta.is_file() {
                files.push((child, meta.len()));
            } else {
                return Err(format!(
                    "{} is not an ordinary file or folder",
                    e.path().display()
                ));
            }
        }
    }
    dirs.sort();
    files.sort();
    Ok((dirs, files))
}

/// Copies the data of `from` into `to` (new or empty: see [`check_destination`]), checks the copy, and leaves `from` alone. On any failure, and on a
/// cancel, what this made is removed again.
pub fn move_data(from: &Path, to: &Path, progress: &Progress) -> Result<(), String> {
    let root_made = !to.exists();
    let mut made_files: Vec<PathBuf> = Vec::new();
    let mut made_dirs: Vec<PathBuf> = Vec::new();
    let result = copy_and_check(
        from,
        to,
        progress,
        root_made,
        &mut made_files,
        &mut made_dirs,
    );
    if result.is_err() {
        for f in &made_files {
            let _ = fs::remove_file(f);
        }
        for d in made_dirs.iter().rev() {
            let _ = fs::remove_dir(d);
        }
        if root_made {
            let _ = fs::remove_dir(to);
        }
    }
    result
}

fn copy_and_check(
    from: &Path,
    to: &Path,
    p: &Progress,
    root_made: bool,
    made_files: &mut Vec<PathBuf>,
    made_dirs: &mut Vec<PathBuf>,
) -> Result<(), String> {
    p.set_phase(Phase::Measuring);
    let (dirs, files) = list(from)?;
    p.total
        .store(files.iter().map(|(_, n)| *n).sum(), Ordering::Relaxed);
    if p.is_cancelled() {
        return Err("cancelled".into());
    }
    if root_made {
        fs::create_dir(to).map_err(|e| format!("cannot create {}: {e}", to.display()))?;
    }
    // private before anything goes in, so the files inherit it and the node accepts the folder
    tenero_app::private_dir::make_private(to)?;
    for d in &dirs {
        let path = to.join(d);
        fs::create_dir(&path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
        made_dirs.push(path);
    }

    p.set_phase(Phase::Copying);
    let mut buf = vec![0u8; BUF];
    for (rel, _) in &files {
        let (src_path, dst_path) = (from.join(rel), to.join(rel));
        let mut src = File::open(&src_path)
            .map_err(|e| format!("cannot open {}: {e}", src_path.display()))?;
        let mut dst = File::create_new(&dst_path)
            .map_err(|e| format!("cannot create {}: {e}", dst_path.display()))?;
        made_files.push(dst_path.clone());
        loop {
            if p.is_cancelled() {
                return Err("cancelled".into());
            }
            let n = src
                .read(&mut buf)
                .map_err(|e| format!("cannot read {}: {e}", src_path.display()))?;
            if n == 0 {
                break;
            }
            dst.write_all(&buf[..n])
                .map_err(|e| format!("cannot write {}: {e}", dst_path.display()))?;
            p.done.fetch_add(n as u64, Ordering::Relaxed);
        }
        // on the disk, not only in the system's cache: the drive may be one that is unplugged
        dst.sync_all()
            .map_err(|e| format!("cannot finish writing {}: {e}", dst_path.display()))?;
    }

    verify_listed(from, to, &files, p)
}

/// Reads the copy in `to` back and compares every file of `from` with it (sizes, then the bytes): `Ok` only if they are all the same.
pub fn verify(from: &Path, to: &Path, p: &Progress) -> Result<(), String> {
    let (_, files) = list(from)?;
    p.total
        .store(files.iter().map(|(_, n)| *n).sum(), Ordering::Relaxed);
    verify_listed(from, to, &files, p)
}

fn verify_listed(
    from: &Path,
    to: &Path,
    files: &[(PathBuf, u64)],
    p: &Progress,
) -> Result<(), String> {
    p.set_phase(Phase::Checking);
    let mut buf = vec![0u8; BUF];
    let mut other = vec![0u8; BUF];
    for (rel, len) in files {
        let (src_path, dst_path) = (from.join(rel), to.join(rel));
        let dst_len = fs::metadata(&dst_path)
            .map_err(|e| format!("cannot look at {}: {e}", dst_path.display()))?
            .len();
        if dst_len != *len {
            return Err(format!(
                "the copy of {} is {dst_len} bytes, the original {len}: the copy was removed",
                rel.display()
            ));
        }
        let mut a = File::open(&src_path)
            .map_err(|e| format!("cannot open {}: {e}", src_path.display()))?;
        let mut b = File::open(&dst_path)
            .map_err(|e| format!("cannot open {}: {e}", dst_path.display()))?;
        loop {
            if p.is_cancelled() {
                return Err("cancelled".into());
            }
            let n = a
                .read(&mut buf)
                .map_err(|e| format!("cannot read {}: {e}", src_path.display()))?;
            if n == 0 {
                break;
            }
            b.read_exact(&mut other[..n])
                .map_err(|e| format!("cannot read back {}: {e}", dst_path.display()))?;
            if buf[..n] != other[..n] {
                return Err(format!(
                    "the copy of {} does not match the original: the copy was removed",
                    rel.display()
                ));
            }
            p.done.fetch_add(n as u64, Ordering::Relaxed);
        }
    }
    Ok(())
}
