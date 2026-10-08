"""Compiles the miner's CUDA kernels AHEAD OF TIME, so the programs need only the NVIDIA driver at run time (no CUDA
Toolkit, no NVRTC). The driver loads the result with cuModuleLoadData (crates/tenero-gpu/src/lib.rs, `KERNELS`).

    python crates/tenero-gpu/kernels/build_kernels.py           rebuild both fat binaries (needs the CUDA 13.x Toolkit)
    python crates/tenero-gpu/kernels/build_kernels.py --check   do the committed binaries match the sources? (no Toolkit)

How (no C++ host compiler needed, so it works on Windows without Visual Studio): NVRTC (the same compiler the miner
used at run time before) turns a module into PTX for each target, `ptxas` turns that into machine code, and
`fatbinary` packs them. The Toolkit's `bin` folder must be on PATH; NVRTC is found there (or in `bin/x64`).

Run it after ANY change to a .cu file in this folder, and commit the .fatbin files with the change: a Rust test
(`the_embedded_kernels_were_built_from_these_sources`, in CI) fails while they are stale.

Two modules, as the program loads them:
  matmulhash.fatbin   matmulhash.cu then fast.cu (fast.cu uses matmulhash.cu's ChaCha20 functions)
  gather.fatbin       gather.cu

Each fat binary holds machine code (SASS) for the GPU generations in TARGETS, which a driver for CUDA 13 (R580 or newer)
loads as it is, and PTX for compute_80, which the driver compiles for a GPU newer than all of them (that needs a driver
at least as new as the nvcc that made it). The source hashes in kernels.sha256 ignore line endings (a Windows checkout
has CRLF, Linux LF).
"""
import ctypes
import glob
import hashlib
import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))

# (compute capability, what it is). sm_80 is the floor: the gathered multiply needs cp.async and int8 mma.sync m16n8k32.
TARGETS = [
    ("80", "A100"),
    ("86", "RTX 30 series, A10, A40"),
    ("89", "RTX 40 series, L4, L40"),
    ("90", "H100, H200"),
    ("100", "B200"),
    ("103", "B300"),
    ("120", "RTX 50 series, RTX PRO 6000 Blackwell"),
]
PTX_FALLBACK = "80"

MODULES = {
    "matmulhash": ["matmulhash.cu", "fast.cu"],
    "gather": ["gather.cu"],
}
HASH_FILE = os.path.join(HERE, "kernels.sha256")


def source_of(module):
    """The module's source as nvcc sees it: its files joined by a newline, with LF line endings."""
    parts = []
    for name in MODULES[module]:
        with open(os.path.join(HERE, name), "rb") as f:
            parts.append(f.read().replace(b"\r\n", b"\n"))
    return b"\n".join(parts)


def hashes():
    return {m: hashlib.sha256(source_of(m)).hexdigest() for m in MODULES}


def write_hash_file(nvcc_version):
    lines = [f"{h}  {m}" for m, h in sorted(hashes().items())]
    lines.append(f"# built by: {nvcc_version}")
    lines.append("# targets: " + " ".join(f"sm_{cc}" for cc, _ in TARGETS) + f" + compute_{PTX_FALLBACK} PTX")
    with open(HASH_FILE, "w", newline="\n") as f:
        f.write("\n".join(lines) + "\n")


def recorded():
    out = {}
    with open(HASH_FILE) as f:
        for line in f:
            if line.strip() and not line.startswith("#"):
                h, m = line.split()
                out[m] = h
    return out


def load_nvrtc():
    ptxas = shutil.which("ptxas")
    if not ptxas:
        sys.exit("ptxas is not on PATH: put the CUDA Toolkit's bin folder on PATH")
    bindir = os.path.dirname(ptxas)
    pattern = "nvrtc64_*.dll" if os.name == "nt" else "libnvrtc.so*"
    found = [f for d in (bindir, os.path.join(bindir, "x64"), os.path.join(bindir, "..", "lib64"))
             for f in sorted(glob.glob(os.path.join(d, pattern))) if "builtins" not in f]
    if not found:
        sys.exit(f"NVRTC ({pattern}) not found next to {ptxas}")
    if os.name == "nt":
        os.add_dll_directory(os.path.dirname(found[0]))
    return ctypes.CDLL(found[0])


def nvrtc_ptx(nvrtc, source, name, arch):
    """PTX for `arch` (compute_XY), exactly as the miner's run-time NVRTC compile made it (only the arch is given)."""
    prog = ctypes.c_void_p()
    if nvrtc.nvrtcCreateProgram(ctypes.byref(prog), source, name.encode(), 0, None, None) != 0:
        sys.exit("nvrtcCreateProgram failed")
    opts = (ctypes.c_char_p * 1)(f"--gpu-architecture={arch}".encode())
    rc = nvrtc.nvrtcCompileProgram(prog, 1, opts)
    size = ctypes.c_size_t()
    nvrtc.nvrtcGetProgramLogSize(prog, ctypes.byref(size))
    log = ctypes.create_string_buffer(size.value)
    nvrtc.nvrtcGetProgramLog(prog, log)
    if rc != 0:
        sys.exit(f"NVRTC failed on {name} for {arch}:\n{log.value.decode(errors='replace')}")
    nvrtc.nvrtcGetPTXSize(prog, ctypes.byref(size))
    ptx = ctypes.create_string_buffer(size.value)
    nvrtc.nvrtcGetPTX(prog, ptx)
    nvrtc.nvrtcDestroyProgram(ctypes.byref(prog))
    return ptx.value


def build():
    nvrtc = load_nvrtc()
    major, minor = ctypes.c_int(), ctypes.c_int()
    nvrtc.nvrtcVersion(ctypes.byref(major), ctypes.byref(minor))
    ptxas_v = subprocess.run(["ptxas", "--version"], capture_output=True, text=True, check=True).stdout
    ptxas_v = [l for l in ptxas_v.splitlines() if "release" in l][0].strip()
    version = f"NVRTC {major.value}.{minor.value}; ptxas {ptxas_v}"
    with tempfile.TemporaryDirectory() as tmp:
        for module in MODULES:
            source = source_of(module)
            images = []
            for cc, _ in TARGETS:
                ptx = os.path.join(tmp, f"{module}_{cc}.ptx")
                with open(ptx, "wb") as f:
                    f.write(nvrtc_ptx(nvrtc, source, module + ".cu", f"compute_{cc}"))
                cubin = os.path.join(tmp, f"{module}_{cc}.cubin")
                subprocess.run(["ptxas", f"-arch=sm_{cc}", "-O3", ptx, "-o", cubin], check=True)
                images.append(f"--image3=kind=elf,sm={cc},file={cubin}")
            fallback = os.path.join(tmp, f"{module}_{PTX_FALLBACK}.ptx")
            images.append(f"--image3=kind=ptx,sm={PTX_FALLBACK},file={fallback}")
            out = os.path.join(HERE, module + ".fatbin")
            subprocess.run(["fatbinary", "-64", f"--create={out}", *images], check=True)
            print(f"wrote {os.path.relpath(out)} ({os.path.getsize(out) // 1024} KiB)")
    write_hash_file(version)
    print(f"wrote {os.path.relpath(HASH_FILE)}")


def check():
    want, have = recorded(), hashes()
    bad = [m for m in MODULES if want.get(m) != have[m] or not os.path.exists(os.path.join(HERE, m + ".fatbin"))]
    for m in MODULES:
        print(f"{'ok      ' if m not in bad else 'STALE   '}{m}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(check() if "--check" in sys.argv else build())
