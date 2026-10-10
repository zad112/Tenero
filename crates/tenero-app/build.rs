//! Records which source the program was built from, so that a bug report and an emergency (`docs/EMERGENCY_PLAN.md` section 3) can say
//! exactly which build it was: `TENERO_COMMIT` from the environment if set (a build from an archive has no git), else `git describe`
//! (`-dirty` if the working tree had changes), else `unknown`.

use std::process::Command;

fn main() {
    let commit = std::env::var("TENERO_COMMIT")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            Command::new("git")
                .args(["describe", "--always", "--dirty", "--abbrev=12"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=TENERO_COMMIT={commit}");
    println!("cargo:rerun-if-env-changed=TENERO_COMMIT");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");

    // The HIP runtime lives outside the default loader path on the supported ROCm install.
    // Record it in AMD-enabled executables so the installed miner is self-contained apart
    // from the system ROCm runtime itself.
    if std::env::var_os("CARGO_FEATURE_AMD_HIP").is_some() {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/opt/rocm/lib");
    }
}
