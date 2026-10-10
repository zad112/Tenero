#!/bin/bash
# Installs what building the AMD GPU backend needs (the `amd-hip` feature) on a GitHub-hosted Ubuntu 22.04 runner: AMD's HIP
# compiler (hipcc), its GPU device library (rocm-device-libs), the HIP headers and runtime, and rocWMMA, from AMD's own apt repository, at ONE pinned ROCm version. It is
# not the whole ROCm SDK (several GiB of maths libraries this backend does not use). Used by release.yml (the Linux AMD package)
# and amd.yml (the check of AMD changes). No GPU is needed to build; nothing here can RUN the AMD backend.
#
# Licences: HIP and rocWMMA are MIT (AMD). rocWMMA is header-only and the HIP headers are compiled into the miner, so release.yml
# copies their licence texts into the AMD package; the HIP runtime (libamdhip64) is NOT shipped: users install ROCm themselves.
set -euo pipefail

ROCM_VERSION="6.2.4"
CODENAME="jammy" # Ubuntu 22.04: the runner the release packages are built on (the oldest glibc)

sudo mkdir --parents --mode=0755 /etc/apt/keyrings
curl -fsSL https://repo.radeon.com/rocm/rocm.gpg.key | gpg --dearmor | sudo tee /etc/apt/keyrings/rocm.gpg > /dev/null
echo "deb [arch=amd64 signed-by=/etc/apt/keyrings/rocm.gpg] https://repo.radeon.com/rocm/apt/${ROCM_VERSION} ${CODENAME} main" \
  | sudo tee /etc/apt/sources.list.d/rocm.list > /dev/null
# prefer AMD's packages over Ubuntu's for anything both have
printf 'Package: *\nPin: release o=repo.radeon.com\nPin-Priority: 600\n' | sudo tee /etc/apt/preferences.d/rocm-pin-600 > /dev/null
sudo apt-get update -q
sudo apt-get install -y -q --no-install-recommends hipcc hip-dev rocm-device-libs rocwmma-dev
/opt/rocm/bin/hipcc --version
ls /opt/rocm/lib/libamdhip64.so*
