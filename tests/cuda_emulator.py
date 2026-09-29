"""Runs the REAL CUDA kernel source (toycoin.gpubackend.KERNEL_SOURCE) on the CPU, for tests.

The source is compiled with a C++ compiler behind a tiny shim that defines what CUDA provides
(threadIdx, blockIdx, atomicAdd, ...). A launch loops over every block and thread one after
another, exactly as many times as the GPU would run them. That checks the kernel LOGIC (integer
maths, indexing, grid-stride loops, the data-dependent picks, the fold's atomics) against the
numpy reference. It does not check CUDA compilation or GPU speed: NVRTC compiles this same source
for sm_120, sm_89 and sm_86, and the self-test on your GPU covers the rest.
"""
import ctypes
import shutil
import subprocess
import types

from toycoin import gpubackend as g

SHIM = r"""
struct Dim3 { unsigned x, y, z; };
static Dim3 blockIdx, threadIdx, blockDim, gridDim;
#define __global__
#define __device__
#define __forceinline__ inline
static inline unsigned long long atomicAdd(unsigned long long* a, unsigned long long v) {
    unsigned long long old = *a; *a = old + v; return old;
}
"""

LAUNCHERS = r"""
extern "C" void launch_keystream(unsigned gx, unsigned bx, unsigned long long keys,
                                 unsigned long long out, unsigned long long blocks_per_key,
                                 unsigned long long start, unsigned long long total) {
    gridDim = {gx, 1, 1}; blockDim = {bx, 1, 1};
    for (unsigned b = 0; b < gx; ++b)
        for (unsigned t = 0; t < bx; ++t) {
            blockIdx = {b, 0, 0}; threadIdx = {t, 0, 0};
            keystream_kernel(keys, out, blocks_per_key, start, total);
        }
}
extern "C" void launch_fill(unsigned gx, unsigned bx, unsigned long long data,
                            unsigned long long blocks_per_slice, unsigned slice_j) {
    gridDim = {gx, 1, 1}; blockDim = {bx, 1, 1};
    for (unsigned b = 0; b < gx; ++b)
        for (unsigned t = 0; t < bx; ++t) {
            blockIdx = {b, 0, 0}; threadIdx = {t, 0, 0};
            fill_kernel(data, blocks_per_slice, slice_j);
        }
}
extern "C" void launch_fold(unsigned gx, unsigned gy, unsigned bx, unsigned long long c,
                            unsigned long long sums, unsigned long long chunks) {
    gridDim = {gx, gy, 1}; blockDim = {bx, 1, 1};
    for (unsigned y = 0; y < gy; ++y)
        for (unsigned x = 0; x < gx; ++x)
            for (unsigned t = 0; t < bx; ++t) {
                blockIdx = {x, y, 0}; threadIdx = {t, 0, 0};
                fold_kernel(c, sums, chunks);
            }
}
"""


def compiler():
    return shutil.which("g++") or shutil.which("c++") or shutil.which("clang++")


def build(directory, source=None):
    """Compiles the kernels (or a modified `source`, for mutation tests) into a loadable library."""
    cpp = directory / "emu.cpp"
    cpp.write_text(SHIM + (source or g.KERNEL_SOURCE) + LAUNCHERS)
    lib = directory / "emu.so"
    subprocess.run([compiler(), "-O2", "-shared", "-fPIC", "-o", str(lib), str(cpp)],
                   check=True, capture_output=True, text=True)
    dll = ctypes.CDLL(str(lib))
    u, ull = ctypes.c_uint, ctypes.c_ulonglong
    dll.launch_keystream.argtypes = [u, u, ull, ull, ull, ull, ull]
    dll.launch_fill.argtypes = [u, u, ull, ull, u]
    dll.launch_fold.argtypes = [u, u, u, ull, ull, ull]
    return dll


class EmulatedKernel:
    def __init__(self, dll, name):
        self.dll, self.name = dll, name

    def __call__(self, grid, block, args):
        a = [int(x) for x in args]
        if self.name == "keystream_kernel":
            self.dll.launch_keystream(grid[0], block[0], *a)
        elif self.name == "fill_kernel":
            self.dll.launch_fill(grid[0], block[0], *a)
        elif self.name == "fold_kernel":
            self.dll.launch_fold(grid[0], grid[1], block[0], *a)
        else:
            raise KeyError(self.name)


class _Stream:
    def __init__(self, ptr):
        pass

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False


class FakeCupy:
    """Just enough of cupy for FusedKernels: RawKernel and the stream API.
    new_streams=True offers Stream.from_external (current CuPy); False only the old
    ExternalStream, which real CuPy now warns about."""

    def __init__(self, dll, new_streams=True, source=None):
        self.dll = dll
        self.source = source or g.KERNEL_SOURCE
        self.used = []

        def from_external(ptr):
            self.used.append("from_external")
            return _Stream(ptr)

        def external_stream(ptr):
            self.used.append("ExternalStream")
            return _Stream(ptr)

        stream_cls = types.SimpleNamespace(from_external=from_external)
        self.cuda = types.SimpleNamespace(ExternalStream=external_stream)
        if new_streams:
            self.cuda.Stream = stream_cls

    def RawKernel(self, source, name):
        assert source == g.KERNEL_SOURCE, "the kernel source must be the one in the package"
        return EmulatedKernel(self.dll, name)
