//! Who else on this computer can read the node's data directory (M9, threat model G1 and G5).
//!
//! The data directory holds the control interface's **cookie** (whoever reads it can stop the node and drive it), the node's
//! key, the chain database and the peer files. A program that is run as the same user can read all of it whatever we do; but
//! *another account on the same computer* must not be able to, and must not be able to write there either (replace the cookie,
//! the key or the chain).
//!
//! * **A new data directory is made private** (only the owner, and on Windows SYSTEM and the Administrators).
//! * **An existing one is checked.** If it is open to other accounts, the node **refuses to start**, says why and says the
//!   exact command that fixes it; `allow_open_data_dir = yes` starts it anyway with a warning (for a test machine).
//! * If the check itself cannot be done (a tool is missing, an output cannot be read) the node says so and carries on: a check
//!   that cannot run must not be mistaken for a pass, and must not stop a node that is probably fine.
//!
//! **How.** On Unix the directory's mode: private means no permission bits for group or others, and `chmod 700` makes it so
//! (nothing but the standard library; **not yet run on Linux**: no Linux machine has run any of this). On Windows `icacls`
//! (part of Windows) is asked for the directory's ACL as SDDL, which names accounts by SID and so reads the same in every
//! language, and a directory that grants anything to Everyone, Users, Authenticated Users, Interactive or Anonymous is open;
//! making it private removes inherited permissions and grants only SYSTEM, the Administrators and the current user.
//!
//! **What this does not do.** It does not stop anything running as the same user, or as an Administrator or root, and it does not
//! look at the files inside an existing directory (one that was private and has had a world-readable file copied in is not
//! noticed); it does not protect a wallet file kept elsewhere (the wallet file is encrypted, but its location is the user's
//! choice).

use std::path::Path;

use crate::log::Logger;

/// What the check found.
#[derive(Debug, PartialEq, Eq)]
pub enum Exposure {
    /// Nobody but the owner (and the system) can get in.
    Private,
    /// Other accounts can: the reason, in words.
    Open(String),
    /// The check could not be made: why.
    Unknown(String),
}

/// Whether a Unix mode gives anything to group or others.
pub fn mode_is_open(mode: u32) -> bool {
    mode & 0o077 != 0
}

/// The accounts, among those an ACL in SDDL form grants access to, that mean "other people": Everyone (`WD`, `S-1-1-0`), Users
/// (`BU`, `S-1-5-32-545`), Authenticated Users (`AU`, `S-1-5-11`), Interactive (`IU`, `S-1-5-4`), Network (`NU`, `S-1-5-2`),
/// Anonymous (`AN`, `S-1-5-7`), and Guests (`BG`, `S-1-5-32-546`). Only *allow* entries count.
pub fn open_trustees_in_sddl(sddl: &str) -> Vec<String> {
    const OPEN: [&str; 14] = [
        "WD",
        "S-1-1-0",
        "BU",
        "S-1-5-32-545",
        "AU",
        "S-1-5-11",
        "IU",
        "S-1-5-4",
        "NU",
        "S-1-5-2",
        "AN",
        "S-1-5-7",
        "BG",
        "S-1-5-32-546",
    ];
    let mut found = Vec::new();
    // every parenthesised entry after the "D:" of the discretionary ACL: (type;flags;rights;object;inherit object;trustee)
    let dacl = match sddl.find("D:") {
        Some(i) => &sddl[i + 2..],
        None => return found,
    };
    // the system access list (audit entries) follows, after "S:", and is not about who may get in
    let dacl = dacl.split("S:").next().unwrap_or("");
    let mut rest = dacl;
    while let Some(start) = rest.find('(') {
        let Some(end) = rest[start..].find(')') else {
            break;
        };
        let ace = &rest[start + 1..start + end];
        rest = &rest[start + end + 1..];
        let fields: Vec<&str> = ace.split(';').collect();
        if fields.len() >= 6 && fields[0] == "A" && OPEN.contains(&fields[5]) {
            found.push(fields[5].to_string());
        }
    }
    found
}

#[cfg(windows)]
fn describe_trustee(t: &str) -> &'static str {
    match t {
        "WD" | "S-1-1-0" => "Everyone",
        "BU" | "S-1-5-32-545" => "all Users",
        "AU" | "S-1-5-11" => "all Authenticated Users",
        "IU" | "S-1-5-4" => "all Interactive users",
        "NU" | "S-1-5-2" => "network logons",
        "AN" | "S-1-5-7" => "Anonymous",
        _ => "Guests",
    }
}

/// Looks at who can get into `dir`.
pub fn check(dir: &Path) -> Exposure {
    check_impl(dir)
}

#[cfg(unix)]
fn check_impl(dir: &Path) -> Exposure {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(dir) {
        Ok(m) => {
            let mode = m.permissions().mode() & 0o777;
            if mode_is_open(mode) {
                Exposure::Open(format!(
                    "its mode is {mode:03o}, so other accounts on this computer can get into it"
                ))
            } else {
                Exposure::Private
            }
        }
        Err(e) => Exposure::Unknown(format!("cannot look at it: {e}")),
    }
}

#[cfg(windows)]
fn check_impl(dir: &Path) -> Exposure {
    let out_file = std::env::temp_dir().join(format!(
        "tenero-acl-{}-{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let run = std::process::Command::new("icacls")
        .arg(dir)
        .arg("/save")
        .arg(&out_file)
        .output();
    let result = match run {
        Err(e) => Exposure::Unknown(format!("cannot run icacls: {e}")),
        Ok(o) if !o.status.success() => Exposure::Unknown(format!(
            "icacls failed: {}",
            String::from_utf8_lossy(&o.stdout).trim()
        )),
        Ok(_) => match std::fs::read(&out_file) {
            Err(e) => Exposure::Unknown(format!("cannot read icacls's answer: {e}")),
            Ok(bytes) => match decode_icacls_save(&bytes) {
                None => Exposure::Unknown("icacls's answer is not text".into()),
                Some(text) => {
                    if !text.contains("D:") {
                        Exposure::Unknown("icacls gave no ACL".into())
                    } else {
                        let open = open_trustees_in_sddl(&text);
                        if open.is_empty() {
                            Exposure::Private
                        } else {
                            let mut names: Vec<&str> =
                                open.iter().map(|t| describe_trustee(t)).collect();
                            names.sort_unstable();
                            names.dedup();
                            Exposure::Open(format!(
                                "its permissions give access to {}",
                                names.join(", ")
                            ))
                        }
                    }
                }
            },
        },
    };
    let _ = std::fs::remove_file(&out_file);
    result
}

#[cfg(not(any(unix, windows)))]
fn check_impl(_dir: &Path) -> Exposure {
    Exposure::Unknown("this platform has no check".into())
}

/// `icacls /save` writes UTF-16 (little-endian, with or without a byte-order mark).
#[cfg(windows)]
fn decode_icacls_save(bytes: &[u8]) -> Option<String> {
    let bytes = bytes.strip_prefix(&[0xFF, 0xFE]).unwrap_or(bytes);
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let (pairs, _) = bytes.as_chunks::<2>();
    let units: Vec<u16> = pairs.iter().map(|c| u16::from_le_bytes(*c)).collect();
    String::from_utf16(&units).ok()
}

/// Makes `dir` private to its owner (see the module's notes). `dir` must exist.
pub fn make_private(dir: &Path) -> Result<(), String> {
    make_private_impl(dir)
}

#[cfg(unix)]
fn make_private_impl(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("cannot set the mode of {}: {e}", dir.display()))
}

#[cfg(windows)]
fn make_private_impl(dir: &Path) -> Result<(), String> {
    let sid = current_user_sid()?;
    let me = format!("*{sid}:(OI)(CI)F");
    let out = std::process::Command::new("icacls")
        .arg(dir)
        // no permissions inherited from the folder above, and the ones it had are kept as copies only for the accounts named
        .args(["/inheritance:r", "/grant:r"])
        .arg(me)
        .args([
            "/grant:r",
            "*S-1-5-18:(OI)(CI)F",
            "/grant:r",
            "*S-1-5-32-544:(OI)(CI)F",
        ])
        .output()
        .map_err(|e| format!("cannot run icacls: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "icacls could not restrict {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stdout).trim()
        ))
    }
}

#[cfg(not(any(unix, windows)))]
fn make_private_impl(_dir: &Path) -> Result<(), String> {
    Err("this platform cannot restrict a directory".into())
}

/// The current user's SID, from `whoami /user`.
#[cfg(windows)]
fn current_user_sid() -> Result<String, String> {
    let out = std::process::Command::new("whoami")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .map_err(|e| format!("cannot run whoami: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    // "domain\user","S-1-5-21-..."
    let sid = text
        .trim()
        .rsplit(',')
        .next()
        .map(|s| s.trim().trim_matches('"').to_string())
        .unwrap_or_default();
    if sid.starts_with("S-1-")
        && sid
            .chars()
            .all(|c| c.is_ascii_digit() || c == 'S' || c == '-')
    {
        Ok(sid)
    } else {
        Err(format!(
            "cannot read the current user's SID from `{}`",
            text.trim()
        ))
    }
}

/// The command an operator can run to make `dir` private, for the message that tells them it is open.
pub fn fix_command(dir: &Path) -> String {
    if cfg!(windows) {
        format!(
            "icacls \"{}\" /inheritance:r /grant:r \"%USERDOMAIN%\\%USERNAME%:(OI)(CI)F\" /grant:r \"SYSTEM:(OI)(CI)F\" /grant:r \"Administrators:(OI)(CI)F\" /T",
            dir.display()
        )
    } else {
        format!("chmod -R go-rwx '{}'", dir.display())
    }
}

/// The node's start-up step: makes a missing `dir` (private), checks an existing one, and refuses an open one unless
/// `allow_open` says to carry on.
pub fn ensure_private(dir: &Path, allow_open: bool, log: &Logger) -> Result<(), String> {
    if !dir.exists() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        return match make_private(dir) {
            Ok(()) => Ok(()),
            Err(why) if allow_open => {
                log.warn(&format!(
                    "the data directory {} could not be made private ({why}); carrying on because allow_open_data_dir is set",
                    dir.display()
                ));
                Ok(())
            }
            Err(why) => Err(format!(
                "the new data directory {} could not be made private: {why}. Fix that, or set allow_open_data_dir = yes to run anyway on a computer only you use",
                dir.display()
            )),
        };
    }
    match check(dir) {
        Exposure::Private => Ok(()),
        Exposure::Unknown(why) => {
            log.warn(&format!(
                "could not check who can read the data directory {} ({why}); make sure only you can",
                dir.display()
            ));
            Ok(())
        }
        Exposure::Open(why) if allow_open => {
            log.warn(&format!(
                "the data directory {} is open to other accounts on this computer: {why}. Anyone who can read the control cookie in it can control this node, and anyone who can write there can replace its files. Carrying on because allow_open_data_dir is set",
                dir.display()
            ));
            Ok(())
        }
        Exposure::Open(why) => Err(format!(
            "the data directory {} is open to other accounts on this computer: {why}. Anyone who can read the control cookie in it can control this node, and anyone who can write there can replace its files. Make it private with:\n  {}\nor set allow_open_data_dir = yes to run anyway on a computer only you use",
            dir.display(),
            fix_command(dir)
        )),
    }
}
