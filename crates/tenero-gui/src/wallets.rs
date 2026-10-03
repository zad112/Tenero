//! Several wallets: each is one encrypted file `NAME.twl` in the wallets folder (`wallets-test` or `wallets-dev` under the app
//! folder by default), with its own seed, password, accounts and history. The window shows the list when the wallet is locked, and
//! "switch wallet" locks the open one and goes back to it. A wallet file from before this (`wallet-<network>.twl` in the app
//! folder, or wherever the settings pointed) is listed too and stays where it is.
//!
//! A name becomes a file name, so it is checked strictly: no path separators, nothing that is special to Windows, no clash with a
//! wallet that is there (compared without regard to case, because Windows does the same).

use std::path::{Path, PathBuf};

use crate::settings::Settings;

pub const EXTENSION: &str = "twl";
pub const MAX_NAME_CHARS: usize = 40;

/// A wallet file in the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalletEntry {
    /// The file's name without `.twl`.
    pub name: String,
    pub path: PathBuf,
}

/// Names Windows will not make a file of (with or without an extension), compared in lower case.
const RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// A name that is safe to make a file of, trimmed, or the reason it is not.
pub fn check_name(name: &str) -> Result<String, String> {
    let n = name.trim();
    if n.is_empty() {
        return Err("give the wallet a name".into());
    }
    if n.chars().count() > MAX_NAME_CHARS {
        return Err(format!(
            "a wallet name is at most {MAX_NAME_CHARS} characters"
        ));
    }
    if !n
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
    {
        return Err("a wallet name may hold letters, digits, spaces, - and _ only".into());
    }
    if RESERVED.contains(&n.to_lowercase().as_str()) {
        return Err("that name is reserved by Windows: choose another".into());
    }
    Ok(n.to_string())
}

fn stem(path: &Path) -> Option<String> {
    path.file_stem().map(|s| s.to_string_lossy().into_owned())
}

/// The wallets there are: every `*.twl` in the wallets folder, the file the settings point at if it is elsewhere, and the single
/// wallet of the first versions of the app if it is still there, sorted by name without regard to case.
pub fn list(settings: &Settings) -> Vec<WalletEntry> {
    let mut out: Vec<WalletEntry> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&settings.wallets_dir) {
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().is_some_and(|x| x == EXTENSION) && path.is_file() {
                if let Some(name) = stem(&path) {
                    out.push(WalletEntry { name, path });
                }
            }
        }
    }
    for extra in [&settings.wallet_file, &settings.legacy_wallet_file] {
        if extra.is_file() && !out.iter().any(|w| &w.path == extra) {
            if let Some(name) = stem(extra) {
                out.push(WalletEntry {
                    name,
                    path: extra.clone(),
                });
            }
        }
    }
    out.sort_by_key(|w| w.name.to_lowercase());
    out
}

/// The path a new wallet of this name will be written to, if the name is acceptable and not in use.
pub fn new_path(settings: &Settings, name: &str) -> Result<(String, PathBuf), String> {
    let name = check_name(name)?;
    let taken = list(settings)
        .iter()
        .any(|w| w.name.to_lowercase() == name.to_lowercase());
    if taken {
        return Err(format!(
            "a wallet called \"{name}\" already exists: choose another name"
        ));
    }
    let path = settings.wallets_dir.join(format!("{name}.{EXTENSION}"));
    if path.exists() {
        return Err(format!(
            "a file for \"{name}\" already exists: choose another name"
        ));
    }
    Ok((name, path))
}

/// A name for the next wallet that is not in use ("Wallet 2", "Wallet 3", ...), for the form to start with.
pub fn suggest_name(settings: &Settings) -> String {
    let have = list(settings);
    if have.is_empty() {
        return "Main wallet".to_string();
    }
    (2..)
        .map(|n| format!("Wallet {n}"))
        .find(|c| {
            !have
                .iter()
                .any(|w| w.name.to_lowercase() == c.to_lowercase())
        })
        .expect("there is always a free number")
}
