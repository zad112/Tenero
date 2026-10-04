"""The Rust GPU miner (crates/tenero-gpu) compiles its own copy of the CUDA kernels. They must stay
byte for byte the same source as the Python one, or the two miners could disagree with the vectors.
"""
import pathlib

from tenero.gpubackend import KERNEL_SOURCE

RUST_COPY = pathlib.Path(__file__).resolve().parents[2] / "crates" / "tenero-gpu" / "kernels" / "matmulhash.cu"


def test_the_rust_copy_of_the_cuda_kernels_is_identical():
    rust = RUST_COPY.read_bytes().decode("utf-8").replace("\r\n", "\n")
    assert rust == KERNEL_SOURCE, (
        "crates/tenero-gpu/kernels/matmulhash.cu differs from reference/tenero/gpubackend.py KERNEL_SOURCE; "
        "regenerate it: python -c \"from tenero.gpubackend import KERNEL_SOURCE; "
        "open('crates/tenero-gpu/kernels/matmulhash.cu','w',newline='\\n').write(KERNEL_SOURCE)\""
    )
