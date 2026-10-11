use std::env;
use std::path::PathBuf;
use std::process::Command;

/// The chips the AMD backend is compiled for unless `TENERO_HIP_ARCH` says otherwise (see `main`).
const DEFAULT_ARCHS: &[&str] = &[
    "gfx1100", "gfx1101", "gfx1102", "gfx1103", "gfx1150", "gfx1151", "gfx1200", "gfx1201",
];

fn main() {
    println!("cargo:rerun-if-env-changed=TENERO_HIPCC");
    println!("cargo:rerun-if-env-changed=TENERO_ROCM");
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
    // ROCm, and so this backend, is built and run on Linux only (the NVIDIA build needs none of it)
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        panic!("the amd-hip feature (the AMD GPU backend) builds on Linux with ROCm only");
    }
    // where ROCm is installed (its default place unless TENERO_ROCM says otherwise)
    let rocm = PathBuf::from(env::var_os("TENERO_ROCM").unwrap_or_else(|| "/opt/rocm".into()));

    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    let object = out.join("amd_hip.o");
    let archive = out.join("libtenero_hip.a");
    let hipcc = env::var("TENERO_HIPCC").unwrap_or_else(|_| {
        rocm.join("bin")
            .join("hipcc")
            .to_string_lossy()
            .into_owned()
    });
    // AMD's GPU code runs only on the chip it was compiled for (no later chip can take it, unlike NVIDIA's PTX), so the
    // backend is compiled for every chip it supports, one code object each in the same file: RDNA3 (gfx11: RX 7000 series
    // gfx1100/1101/1102, the Radeon 700M laptop chips gfx1103, Strix gfx1150/1151) and RDNA4 (gfx12: RX 9000 series
    // gfx1200/1201). `TENERO_HIP_ARCH` (a comma-separated list) builds for others. The kernel is told the list, so a card that
    // is not on it is refused at the start with a message, not by a failure in the middle.
    let archs = env::var("TENERO_HIP_ARCH").unwrap_or_else(|_| DEFAULT_ARCHS.join(","));
    let archs: Vec<&str> = archs
        .split(',')
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .collect();
    assert!(
        archs
            .iter()
            .all(|a| a.starts_with("gfx11") || a.starts_with("gfx12")),
        "TENERO_HIP_ARCH: this backend is written for RDNA3 (gfx11) and RDNA4 (gfx12) only: {archs:?}"
    );

    let mut command = Command::new(&hipcc);
    command.args(["-std=c++17", "-O3", "-fPIC", "-c", "kernels/amd_hip.cpp"]);
    for name in TUNABLES {
        if let Some(value) = env::var_os(name) {
            command.arg(format!("-D{name}={}", value.to_string_lossy()));
        }
    }
    command.arg(format!("-DTENERO_AMD_ARCHS=\"{}\"", archs.join(",")));
    for arch in &archs {
        command.arg(format!("--offload-arch={arch}"));
    }
    let status = command
        .arg("-o")
        .arg(&object)
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
    let rocm_lib = rocm.join("lib");
    println!("cargo:rustc-link-search=native={}", rocm_lib.display());
    println!("cargo:rustc-link-lib=dylib=amdhip64");
    println!("cargo:rustc-link-lib=dylib=stdc++");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", rocm_lib.display());
}
