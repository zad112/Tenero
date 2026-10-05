//! Moving the node's data to another folder: it copies, checks the copy, never touches the original, and removes what it made when it fails.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tenero_gui::movedata::{check_destination, move_data, verify, Phase, Progress};

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tenero-move-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// Bytes that are not all the same, so that a copy that shifts or repeats a block does not pass by luck.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// A data folder like a node's: a big file (more than one copying block), a folder inside a folder, an empty file, and a cookie.
fn make_data(root: &Path) {
    fs::create_dir_all(root.join("chain.redb.segments").join("deep")).unwrap();
    fs::write(root.join("chain.redb"), noise(9 * 1024 * 1024 + 123, 1)).unwrap();
    fs::write(
        root.join("chain.redb.segments").join("a.seg"),
        noise(70_000, 2),
    )
    .unwrap();
    fs::write(
        root.join("chain.redb.segments").join("deep").join("b.seg"),
        noise(5, 3),
    )
    .unwrap();
    fs::write(root.join("node.key"), noise(32, 4)).unwrap();
    fs::write(root.join("empty"), b"").unwrap();
    fs::write(root.join("control.cookie"), b"old cookie").unwrap();
}

fn same_tree(a: &Path, b: &Path, skip_cookie: bool) {
    let mut names: Vec<_> = fs::read_dir(a)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names.sort();
    for n in names {
        if skip_cookie && n == "control.cookie" {
            assert!(!b.join(&n).exists(), "the cookie is not copied");
            continue;
        }
        let (pa, pb) = (a.join(&n), b.join(&n));
        if pa.is_dir() {
            same_tree(&pa, &pb, false);
        } else {
            assert_eq!(
                fs::read(&pa).unwrap(),
                fs::read(&pb).unwrap(),
                "{}",
                pa.display()
            );
        }
    }
}

#[test]
fn the_data_is_copied_checked_and_the_original_is_left_alone() {
    let base = scratch("copy");
    let (from, to) = (base.join("from"), base.join("to"));
    fs::create_dir_all(&from).unwrap();
    make_data(&from);
    check_destination(&from, &to).unwrap();
    let p = Progress::default();
    move_data(&from, &to, &p).unwrap();
    same_tree(&from, &to, true);
    // the original is whole, cookie included
    assert_eq!(
        fs::read(from.join("control.cookie")).unwrap(),
        b"old cookie"
    );
    assert_eq!(
        fs::read(from.join("chain.redb")).unwrap().len(),
        9 * 1024 * 1024 + 123
    );
    // it said what it did: the checking phase ran, and counted every byte
    assert_eq!(p.phase(), Phase::Checking);
    assert_eq!(p.done(), p.total());
    assert!(p.total() > 9 * 1024 * 1024);
    // and the copy passes the check by itself
    verify(&from, &to, &Progress::default()).unwrap();
    // the new folder is private, so the node accepts it (it refuses to start on one that other accounts can open)
    assert_eq!(
        tenero_app::private_dir::check(&to),
        tenero_app::private_dir::Exposure::Private
    );
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn an_empty_folder_that_already_exists_is_a_fine_destination() {
    let base = scratch("emptydest");
    let (from, to) = (base.join("from"), base.join("to"));
    fs::create_dir_all(&from).unwrap();
    make_data(&from);
    fs::create_dir_all(&to).unwrap();
    check_destination(&from, &to).unwrap();
    move_data(&from, &to, &Progress::default()).unwrap();
    same_tree(&from, &to, true);
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn the_check_catches_a_changed_byte_and_a_short_file() {
    let base = scratch("verify");
    let (from, to) = (base.join("from"), base.join("to"));
    fs::create_dir_all(&from).unwrap();
    make_data(&from);
    move_data(&from, &to, &Progress::default()).unwrap();
    // one byte changed, far into the big file (past the first copying block)
    let big = to.join("chain.redb");
    let mut bytes = fs::read(&big).unwrap();
    bytes[6 * 1024 * 1024] ^= 1;
    fs::write(&big, &bytes).unwrap();
    let e = verify(&from, &to, &Progress::default()).unwrap_err();
    assert!(e.contains("does not match"), "{e}");
    // a file that is too short
    fs::write(&big, &bytes[..bytes.len() - 1]).unwrap();
    let e = verify(&from, &to, &Progress::default()).unwrap_err();
    assert!(e.contains("bytes"), "{e}");
    // a file that is missing
    fs::remove_file(&big).unwrap();
    assert!(verify(&from, &to, &Progress::default()).is_err());
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_bad_destination_is_refused_with_a_reason() {
    let base = scratch("refuse");
    let from = base.join("from");
    fs::create_dir_all(&from).unwrap();
    make_data(&from);
    let bad = |to: &Path, needle: &str| {
        let e = check_destination(&from, to).unwrap_err();
        assert!(
            e.contains(needle),
            "`{}` gave `{e}`, expected `{needle}`",
            to.display()
        );
    };
    bad(Path::new("relative\\folder"), "full path");
    bad(
        Path::new(if cfg!(windows) { "C:\\" } else { "/" }),
        "whole drive",
    );
    bad(&base.join("missing").join("to"), "does not exist");
    // not empty
    let full = base.join("full");
    fs::create_dir_all(&full).unwrap();
    fs::write(full.join("x"), b"x").unwrap();
    bad(&full, "not empty");
    // a file
    let file = base.join("afile");
    fs::write(&file, b"x").unwrap();
    bad(&file, "file");
    // the data folder itself, or inside it
    bad(&from, "itself");
    bad(&from.join("inner"), "inside");
    bad(&from.join("chain.redb.segments"), "inside");
    // nothing was made by refusing
    assert!(!from.join("inner").exists());
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_cancel_removes_everything_that_was_made_and_leaves_the_original() {
    let base = scratch("cancel");
    let (from, to) = (base.join("from"), base.join("to"));
    fs::create_dir_all(&from).unwrap();
    make_data(&from);
    // enough to cancel in the middle of
    fs::write(from.join("big"), noise(48 * 1024 * 1024, 9)).unwrap();
    let p = Arc::new(Progress::default());
    let t = {
        let (p, from, to) = (p.clone(), from.clone(), to.clone());
        std::thread::spawn(move || move_data(&from, &to, &p))
    };
    // wait until it has really begun copying, then stop it
    let begun = std::time::Instant::now();
    while p.done() == 0 && begun.elapsed().as_secs() < 30 {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(p.done() > 0, "it never started");
    p.cancel();
    let e = t.join().unwrap().unwrap_err();
    assert_eq!(e, "cancelled");
    assert!(
        !to.exists(),
        "the folder it made is gone, with everything in it"
    );
    // the original is whole
    assert_eq!(fs::read(from.join("big")).unwrap().len(), 48 * 1024 * 1024);
    assert_eq!(
        fs::read(from.join("control.cookie")).unwrap(),
        b"old cookie"
    );
    // an existing empty destination is left empty and in place, not deleted
    fs::create_dir_all(&to).unwrap();
    let p2 = Progress::default();
    p2.cancel();
    assert_eq!(move_data(&from, &to, &p2).unwrap_err(), "cancelled");
    assert!(to.is_dir() && fs::read_dir(&to).unwrap().next().is_none());
    let _ = fs::remove_dir_all(&base);
}

#[cfg(unix)]
#[test]
fn a_link_in_the_data_folder_is_refused_and_nothing_is_left_behind() {
    let base = scratch("link");
    let (from, to) = (base.join("from"), base.join("to"));
    fs::create_dir_all(&from).unwrap();
    make_data(&from);
    std::os::unix::fs::symlink("/etc", from.join("sneaky")).unwrap();
    let e = move_data(&from, &to, &Progress::default()).unwrap_err();
    assert!(e.contains("link"), "{e}");
    assert!(!to.exists());
    let _ = fs::remove_dir_all(&base);
}
