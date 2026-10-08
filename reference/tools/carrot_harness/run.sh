#!/bin/bash
# Builds Tenero's Carrot vector harness inside a Monero checkout and writes the vectors. Run in WSL (docs: CLAUDE.md):
#   wsl -d Ubuntu -u test -- bash run.sh <monero checkout> <harness dir> <output directory>
# It writes carrot_monero.json (Carrot) and curve_tree_monero.json (the FCMP++ curve tree) there.
# The checkout must be seraphis-migration/monero at tag v0.19.0.0-beta.3.0 (commit d816367c), configured in build/
# (cmake .. -DCMAKE_BUILD_TYPE=Release -DBUILD_TESTS=ON; with CMake 4 add -DCMAKE_POLICY_VERSION_MINIMUM=3.5).
set -euo pipefail
monero="$1"; harness="$2"; out_dir="$3"
test "$(git -C "$monero" rev-parse HEAD)" = d816367cb1aa405bfa68a20ac3e034d0759d968e || { echo "wrong Monero commit"; exit 1; }
mkdir -p "$monero/tests/tenero_carrot_harness"
cp "$harness/carrot_vectors.cpp" "$harness/tree_vectors.cpp" "$harness/CMakeLists.txt" "$monero/tests/tenero_carrot_harness/"
grep -q tenero_carrot_harness "$monero/tests/CMakeLists.txt" || echo 'add_subdirectory(tenero_carrot_harness)' >> "$monero/tests/CMakeLists.txt"
export PATH="$HOME/.cargo/bin:$PATH"
cd "$monero/build"
cmake .. > /dev/null
make -j"$(nproc)" tenero_carrot_vectors tenero_tree_vectors
./tests/tenero_carrot_harness/tenero_carrot_vectors "$out_dir/carrot_monero.json"
./tests/tenero_carrot_harness/tenero_tree_vectors "$out_dir/curve_tree_monero.json"
