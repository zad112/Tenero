# Mining on an AMD GPU (Linux, RX 7000 and RX 9000 series)

**Contributed, and not tested by the project's author.** The AMD backend was written by an outside contributor and runs the
whole proof of work (the dataset, the attempt and the fold) on an AMD card through AMD's HIP and its 8-bit matrix units. The
author has no AMD card, and no CI machine has one: CI only **builds** it. Whether it is right was checked by its contributor on
their own RX 7000 card (its bit-for-bit test, below), before it was built for more chips and for ROCm 7; the RX 9000 series has
been run by nobody yet. The node checks every block it is given on the CPU anyway, so a wrong attempt from
this backend could only waste its own work: the block would be refused, never accepted. Experimental and unaudited, like the
rest; nothing on any Tenero network has value.

## What it needs

| | |
|---|---|
| **A card** | AMD **RDNA3** (the **Radeon RX 7000 series**, "gfx11") or **RDNA4** (the **Radeon RX 9000 series**, "gfx12"), and the workstation and laptop chips of those families: see the table below. Older cards (RX 6000 and earlier) are refused with a message that names the chips the miner was built for. About 5 GiB of video memory is used: the 4 GiB dataset and the attempts. |
| **A system** | **Linux** only, 64-bit, glibc 2.34 or newer (Ubuntu 22.04 or newer). There is no Windows build. |
| **ROCm 7** | AMD's compute runtime, **version 7** (7.0.2 or newer for an RX 9000 card), installed in its default place, `/opt/rocm` (AMD's instructions: https://rocm.docs.amd.com). The miner links to its HIP runtime (`libamdhip64.so.7`), which is **not** in the package, so **ROCm 6 will not do**. It was built against ROCm 7.2.1. |

**Which cards, and what is known of each.** AMD's compiled GPU code runs only on the chip it was compiled for, so the package
carries one copy per chip. Nobody on the project has an AMD card, so "built" means CI compiled it, nothing more:

| Cards | Chip | Status |
|---|---|---|
| RX 7900 XTX / XT / GRE | gfx1100 | the contributor's own kind of card: written and checked bit for bit there (the version before ROCm 7; check again) |
| RX 7800 XT, RX 7700 XT | gfx1101 | built, the same code as gfx1100; **not run by anyone yet** |
| RX 7600 / 7600 XT | gfx1102 | built, the same code; **not run yet** |
| Radeon 780M / 760M (laptop) | gfx1103 | **not built**: AMD's rocWMMA does not support this chip |
| Ryzen AI 300 / Strix Halo graphics | gfx1150 / gfx1151 | built, the same code; **not run yet** |
| **RX 9070 XT / 9070 / 9070 GRE** | gfx1201 | built, through **rocWMMA's own path** (RDNA4's matrix instruction and layout differ from RDNA3's, so the hand-written RDNA3 fast path is not used): **not run yet** |
| **RX 9060 XT / 9060** | gfx1200 | as gfx1201: **not run yet** |

**Until someone runs the bit-for-bit test on a card, treat that card as untested.** A wrong result cannot cost you a bad block
(the node checks every block on the CPU and would refuse it) but it would waste your mining: see below, and please report.

## How to mine

From the **`tenero-...-linux-x64-amd`** package (the ordinary Linux package has no AMD backend):

* **The wallet app:** Settings, Mining, Backend: choose **amd**, and the GPU number if the machine has more than one card. Then
  Start on the Mining tab, alone or for a pool, as with any backend.
* **The command line:** `tenero-miner ... --backend amd` (with `--gpu-device N` for another card), for a node or for a pool
  (`--pool default --network gamma --address TENg... --backend amd`).

**The mining fee:** as with the NVIDIA backend, 1% of the mining time (one minute in every hundred) on `gamma` is mined for the
author of the backend, here its contributor, qcvibes1 (`docs/RUNNING.md`, "The mining fee"). It is shown on the screen and in the
app, and cannot be turned off.

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

On Linux with ROCm 7 (`hipcc`, the HIP headers, the device libraries and rocWMMA: `.github/rocm-install.sh` lists the packages):

```
cargo build --release -p tenero-app -p tenero-gui --features tenero-app/amd-hip,tenero-gui/amd-hip
```

`TENERO_ROCM` points the build at a ROCm installed elsewhere; `TENERO_HIP_ARCH` (a comma-separated list; by default every chip
in the table above) builds for fewer chips, which is faster (for example `TENERO_HIP_ARCH=gfx1201` for an RX 9070 alone). The kernel is `crates/tenero-gpu/kernels/amd_hip.cpp`; its tuning switches (`TENERO_AMD_*`) are read at build time.

## Licences

HIP and rocWMMA are MIT-licensed (AMD). rocWMMA and the HIP headers are compiled into the AMD package's miner, and their licence
texts are in its `THIRD-PARTY-LICENCES.txt`; the HIP runtime is your own ROCm install.
