# Mining on an AMD GPU (Linux, RX 7000 series)

**Contributed, and not tested by the project's author.** The AMD backend was written by an outside contributor and runs the
whole proof of work (the dataset, the attempt and the fold) on an AMD card through AMD's HIP and its 8-bit matrix units. The
author has no AMD card, and no CI machine has one: CI only **builds** it. Whether it is right was checked by its contributor on
their own card (its bit-for-bit test, below). The node checks every block it is given on the CPU anyway, so a wrong attempt from
this backend could only waste its own work: the block would be refused, never accepted. Experimental and unaudited, like the
rest; nothing on any Tenero network has value.

## What it needs

| | |
|---|---|
| **A card** | AMD RDNA3: the **Radeon RX 7000 series** and its workstation and laptop versions ("gfx11"). Older cards (RX 6000 and earlier) and newer architectures are refused with a message. About 5 GiB of video memory is used: the 4 GiB dataset and the attempts. |
| **A system** | **Linux** only, 64-bit, glibc 2.34 or newer (Ubuntu 22.04 or newer). There is no Windows build. |
| **ROCm 6** | AMD's compute runtime, installed in its default place, `/opt/rocm` (AMD's instructions: https://rocm.docs.amd.com). The miner links to its HIP runtime (`libamdhip64`), which is **not** in the package. It was built against ROCm 6.2.4. |

## How to mine

From the **`tenero-...-linux-x64-amd`** package (the ordinary Linux package has no AMD backend):

* **The wallet app:** Settings, Mining, Backend: choose **amd**, and the GPU number if the machine has more than one card. Then
  Start on the Mining tab, alone or for a pool, as with any backend.
* **The command line:** `tenero-miner ... --backend amd` (with `--gpu-device N` for another card), for a node or for a pool
  (`--pool default --network gamma --address TENg... --backend amd`).

What is different from the NVIDIA backend: `--gpu-batch auto` (measuring the best batch at start) is not offered, and the miner's
screen shows no temperature, power or clock readings for an AMD card (those come from NVIDIA's NVML).

## Checking that it is right on your card

The bit-for-bit test builds the dataset on the card and compares every slice and a set of attempts, both designs, against the
CPU code, at small and at the real parameters (the real ones need about 4.5 GiB of RAM and of video memory). From the source,
with ROCm installed:

```
cargo test --release -p tenero-gpu --features amd-hip --test amd_selftest -- --ignored --test-threads=1
```

If you run it, the author would like to know the result, your card and your ROCm version (an issue on GitHub). **No speed is
claimed here:** none has been measured by the project (`docs/BENCHMARKS.md`).

## Building it yourself

On Linux with ROCm 6 (`hipcc`, the HIP headers and rocWMMA: `.github/rocm-install.sh` lists the packages):

```
cargo build --release -p tenero-app -p tenero-gui --features tenero-app/amd-hip,tenero-gui/amd-hip
```

`TENERO_ROCM` points the build at a ROCm installed elsewhere; `TENERO_HIP_ARCH` (default `gfx1100`) builds for another gfx11
chip. The kernel is `crates/tenero-gpu/kernels/amd_hip.cpp`; its tuning switches (`TENERO_AMD_*`) are read at build time.

## Licences

HIP and rocWMMA are MIT-licensed (AMD). rocWMMA and the HIP headers are compiled into the AMD package's miner, and their licence
texts are in its `THIRD-PARTY-LICENCES.txt`; the HIP runtime is your own ROCm install.
