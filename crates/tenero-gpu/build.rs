use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=TENERO_HIPCC");
    println!("cargo:rerun-if-env-changed=TENERO_HIP_ARCH");
    const TUNABLES: &[&str] = &[
        "TENERO_AMD_GEMM_THREADS",
        "TENERO_AMD_M_TILE",
        "TENERO_AMD_N_TILE",
        "TENERO_AMD_K_STAGE",
        "TENERO_AMD_A_PAD",
        "TENERO_AMD_B_PAD",
        "TENERO_AMD_MIN_BLOCKS_PER_CU",
        "TENERO_AMD_COL_TILES_PER_WAVE",
        "TENERO_AMD_SPECIALIZE_DEFAULT",
        "TENERO_AMD_DIRECT_A",
        "TENERO_AMD_RESTRICT_POINTERS",
        "TENERO_AMD_HOIST_LOADS",
        "TENERO_AMD_PREFETCH_LOADS",
        "TENERO_AMD_PREFETCH_DISTANCE",
        "TENERO_AMD_PREFETCH_A",
        "TENERO_AMD_FUSE_FOLD",
        "TENERO_AMD_LDS_DOUBLE_BUFFER",
        "TENERO_AMD_DIRECT_B_NATIVE",
        "TENERO_AMD_DIRECT_AB_NATIVE",
        "TENERO_AMD_GRID_ATTEMPT_X",
        "TENERO_AMD_X_STAGE_MAJOR",
        "TENERO_AMD_B_NONTEMPORAL",
    ];
    for name in TUNABLES {
        println!("cargo:rerun-if-env-changed={name}");
    }
    println!("cargo:rerun-if-changed=kernels/amd_hip.cpp");
    if env::var_os("CARGO_FEATURE_AMD_HIP").is_none() {
        return;
    }

    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    let object = out.join("amd_hip.o");
    let archive = out.join("libtenero_hip.a");
    let hipcc = env::var("TENERO_HIPCC").unwrap_or_else(|_| "hipcc".to_string());
    let arch = env::var("TENERO_HIP_ARCH").unwrap_or_else(|_| "gfx1100".to_string());

    let mut command = Command::new(&hipcc);
    command.args([
            "-std=c++17",
            "-O3",
            "-fPIC",
            "-c",
            "kernels/amd_hip.cpp",
        ]);
    for name in TUNABLES {
        if let Some(value) = env::var_os(name) {
            command.arg(format!("-D{name}={}", value.to_string_lossy()));
        }
    }
    let status = command
        .arg("-o")
        .arg(&object)
        .arg(format!("--offload-arch={arch}"))
        .status()
        .unwrap_or_else(|e| panic!("cannot run {hipcc}: {e}"));
    assert!(status.success(), "HIP kernel compilation failed");

    let ar = env::var("AR").unwrap_or_else(|_| "ar".to_string());
    let status = Command::new(&ar)
        .arg("crus")
        .arg(&archive)
        .arg(&object)
        .status()
        .unwrap_or_else(|e| panic!("cannot run {ar}: {e}"));
    assert!(status.success(), "cannot archive the HIP object");

    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=tenero_hip");
    let rocm_lib = Path::new("/opt/rocm/lib");
    println!("cargo:rustc-link-search=native={}", rocm_lib.display());
    println!("cargo:rustc-link-lib=dylib=amdhip64");
    println!("cargo:rustc-link-lib=dylib=stdc++");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", rocm_lib.display());
}
