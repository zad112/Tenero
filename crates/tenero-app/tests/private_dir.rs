//! Who can read the node's data directory (`private_dir.rs`, threat model G1 and G5). The ACL reading is tested on the real
//! strings `icacls` gave on the owner's machine; the Windows tests use real directories (one made under `C:\`, which Windows
//! opens to every user, as a place that really is open).

use tenero_app::log::{Level, Logger};
use tenero_app::private_dir::{
    check, ensure_private, fix_command, mode_is_open, open_trustees_in_sddl, Exposure,
};

/// What `icacls /save` printed for a folder inside the user's profile: SYSTEM, Administrators and the user, all inherited.
const PROFILE_DIR: &str = "n1\nD:(A;OICIID;FA;;;SY)(A;OICIID;FA;;;BA)(A;OICIID;FA;;;S-1-5-21-3875827837-2291611180-1020715461-1001)";
/// ...and for a folder made directly under `C:\`.
const C_ROOT_DIR: &str = "tenero-acl-test2\nD:(A;OICIID;FA;;;BA)(A;OICIID;FA;;;SY)(A;OICIID;0x1200a9;;;BU)(A;ID;0x1301bf;;;AU)(A;OICIIOID;SDGXGWGR;;;AU)";

fn quiet() -> Logger {
    Logger::new(Level::Warn, None, false).unwrap()
}

#[test]
fn a_folder_in_the_profile_is_private_and_one_under_c_is_not() {
    assert!(open_trustees_in_sddl(PROFILE_DIR).is_empty());
    assert_eq!(open_trustees_in_sddl(C_ROOT_DIR), vec!["BU", "AU", "AU"]);
}

#[test]
fn every_account_that_means_other_people_is_found() {
    for t in [
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
    ] {
        let sddl = format!("D:(A;;FA;;;SY)(A;;GR;;;{t})");
        assert_eq!(open_trustees_in_sddl(&sddl), vec![t.to_string()], "{t}");
    }
}

#[test]
fn only_allow_entries_count_and_only_the_discretionary_list() {
    // a deny entry for Everyone is not an opening
    assert!(open_trustees_in_sddl("D:(D;;FA;;;WD)(A;;FA;;;SY)").is_empty());
    // the system access list (S:) is not the permissions
    assert!(open_trustees_in_sddl("D:(A;;FA;;;SY)S:(A;;FA;;;WD)").is_empty());
    // named accounts that are not "other people" are not an opening
    assert!(
        open_trustees_in_sddl("D:(A;;FA;;;S-1-5-21-1-2-3-1001)(A;;FA;;;BA)(A;;FA;;;SY)").is_empty()
    );
}

#[test]
fn nonsense_is_not_an_opening_and_does_not_panic() {
    for s in [
        "",
        "D:",
        "(",
        ")",
        "D:(",
        "D:(A;;)",
        "D:(A;;;;;WD",
        "x(A;;;;;WD)",
        "D:((((",
        "D:)A;;;;;WD(",
    ] {
        let _ = open_trustees_in_sddl(s);
    }
    assert!(open_trustees_in_sddl("").is_empty());
    assert!(
        open_trustees_in_sddl("D:(A;;FA;;;WD").is_empty(),
        "an entry without its closing bracket is not read"
    );
}

#[test]
fn a_unix_mode_is_open_if_group_or_others_have_any_permission() {
    for (mode, open) in [
        (0o700, false),
        (0o600, false),
        (0o500, false),
        (0o750, true),
        (0o710, true),
        (0o701, true),
        (0o707, true),
        (0o770, true),
        (0o755, true),
        (0o777, true),
        (0o000, false),
    ] {
        assert_eq!(mode_is_open(mode), open, "{mode:o}");
    }
}

#[test]
fn the_fix_command_names_the_directory() {
    let c = fix_command(std::path::Path::new("some dir"));
    assert!(c.contains("some dir"));
}

#[test]
fn a_directory_that_does_not_exist_is_unknown_not_private() {
    let missing = std::env::temp_dir().join("tenero-no-such-dir-for-the-acl-check");
    assert!(
        matches!(check(&missing), Exposure::Unknown(_) | Exposure::Open(_)),
        "{:?}",
        check(&missing)
    );
}

#[test]
fn a_new_data_directory_is_created_private_and_stays_acceptable() {
    let dir = std::env::temp_dir().join(format!("tenero-private-new-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    ensure_private(&dir, false, &quiet()).unwrap();
    assert!(dir.is_dir());
    assert_eq!(check(&dir), Exposure::Private);
    // and a second start finds it fine
    ensure_private(&dir, false, &quiet()).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::path::PathBuf;
    use tenero_app::private_dir::make_private;

    /// A directory made directly under `C:\`, where Windows gives every user read access (and every signed-in user the
    /// right to write), removed again when this is dropped. `None` where one cannot be made.
    struct OpenDir(PathBuf);

    impl OpenDir {
        fn make(tag: &str) -> Option<OpenDir> {
            let p = PathBuf::from(format!("C:\\tenero-acl-test-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir(&p).ok()?;
            Some(OpenDir(p))
        }
    }

    impl Drop for OpenDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_directory_under_c_is_found_open_and_the_message_says_how_to_fix_it() {
        let Some(d) = OpenDir::make("open") else {
            eprintln!("cannot make a directory under C:\\ here: test skipped");
            return;
        };
        match check(&d.0) {
            Exposure::Open(why) => assert!(why.contains("Users"), "{why}"),
            other => panic!("expected Open, got {other:?}"),
        }
        let err = ensure_private(&d.0, false, &quiet()).unwrap_err();
        assert!(err.contains("open to other accounts"), "{err}");
        assert!(err.contains("icacls"), "{err}");
        assert!(err.contains("allow_open_data_dir"), "{err}");
    }

    #[test]
    fn the_override_lets_an_open_directory_through_with_a_warning() {
        let Some(d) = OpenDir::make("override") else {
            return;
        };
        ensure_private(&d.0, true, &quiet()).unwrap();
        // (the override changes nothing: it is still open afterwards)
        assert!(matches!(check(&d.0), Exposure::Open(_)));
    }

    #[test]
    fn making_a_directory_private_closes_it_and_a_file_made_in_it_is_closed_too() {
        let Some(d) = OpenDir::make("close") else {
            return;
        };
        assert!(matches!(check(&d.0), Exposure::Open(_)));
        make_private(&d.0).unwrap();
        assert_eq!(check(&d.0), Exposure::Private);
        ensure_private(&d.0, false, &quiet()).unwrap();
        // what is created inside inherits it: no entry for other accounts
        let f = d.0.join("control.cookie");
        std::fs::write(&f, "secret").unwrap();
        assert_eq!(
            check(&f),
            Exposure::Private,
            "a new file inherits the privacy"
        );
        // and the owner can still read and write there
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "secret");
        std::fs::write(&f, "more").unwrap();
    }

    #[test]
    fn a_new_directory_under_c_is_made_private_by_ensure_private() {
        let p = PathBuf::from(format!("C:\\tenero-acl-test-{}-new", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        if std::fs::create_dir(&p).is_err() {
            return;
        }
        let _ = std::fs::remove_dir_all(&p);
        struct Gone(PathBuf);
        impl Drop for Gone {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _gone = Gone(p.clone());
        // it does not exist now, so ensure_private makes it, and makes it private (unlike a plain create_dir there)
        ensure_private(&p, false, &quiet()).unwrap();
        assert_eq!(check(&p), Exposure::Private);
    }
}
