//! `--version` on every command-line program: the version and the source commit, so a bug report and an emergency (`docs/EMERGENCY_PLAN.md` section 3) can name the exact build.

use std::process::Command;
use tenero_app::daemon::{version_line, wants_version, COMMIT, VERSION};

fn run(exe: &str, arg: &str) -> (bool, String) {
    let out = Command::new(exe)
        .arg(arg)
        .output()
        .expect("the program starts");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
    )
}

#[test]
fn every_program_says_what_build_it_is() {
    let programs = [
        ("tenerod", env!("CARGO_BIN_EXE_tenerod")),
        ("tenero-miner", env!("CARGO_BIN_EXE_tenero-miner")),
        ("tenero-wallet", env!("CARGO_BIN_EXE_tenero-wallet")),
        ("tenero-seedcheck", env!("CARGO_BIN_EXE_tenero-seedcheck")),
    ];
    for (name, exe) in programs {
        for flag in ["--version", "-V", "version"] {
            let (ok, said) = run(exe, flag);
            assert!(ok, "{name} {flag} must succeed");
            assert_eq!(said, version_line(name), "{name} {flag}");
        }
        let (_, said) = run(exe, "--version");
        assert!(
            said.starts_with(&format!("{name} v{VERSION} (commit {COMMIT})")),
            "{said}"
        );
        assert!(said.contains("EXPERIMENTAL, UNAUDITED"), "{said}");
    }
}

#[test]
fn the_version_is_the_release_version_and_the_commit_is_never_empty() {
    // the tag of a release is `v` + this; a release is built from that tag
    assert_eq!(VERSION, "0.3.0-gamma.1");
    assert!(!COMMIT.is_empty());
}

#[test]
fn only_the_version_words_ask_for_the_version() {
    for yes in ["version", "--version", "-V"] {
        assert!(wants_version(Some(yes)), "{yes}");
    }
    for no in ["", "-v", "--versions", "help", "ver", "Version"] {
        assert!(!wants_version(Some(no)), "{no}");
    }
    assert!(!wants_version(None));
}
