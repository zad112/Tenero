"""Imports Monero's own FCMP++ test proofs from a checkout of Monero's FCMP++ stressnet release
(seraphis-migration/monero, tag v0.19.0.0-beta.3.0) into tests/vectors/upstream_monero_fcmp_pp.json.

NOT a reference implementation and not part of `make_vectors.py --check`: these are third-party data, copied
verbatim, so that our pinned copy of the FCMP++ crates can be shown to accept proofs that Monero's own code
produced (docs/FCMP_CARROT_PLAN.md, milestone G1). Provenance is recorded in the file. The source repository is
BSD-3-Clause.

Each `tests/data/fcmp_pp_verify_inputs_<n>in.bin` is Monero's binary serialization of the test's
`SerializableFcmpPpVerify` (tests/unit_tests/unit_tests_utils.cpp): the signable transaction hash (32 bytes),
the number of tree layers (one byte), the proof (a varint length, then its bytes), the tree root (32 bytes), and
the pseudo-outputs and the key images (each a varint count, then 32 bytes each).

    python reference/tools/import_upstream_fcmp_pp.py <path to a checkout of that tag>
"""
import json
import subprocess
import sys
from pathlib import Path

if len(sys.argv) != 2:
    sys.exit(__doc__)
repo = Path(sys.argv[1])
commit = subprocess.check_output(["git", "-C", str(repo), "rev-parse", "HEAD"], text=True).strip()
INPUTS = [1, 2, 4, 8, 128]


class Reader:
    def __init__(self, data):
        self.data = data
        self.pos = 0

    def take(self, n):
        if self.pos + n > len(self.data):
            raise ValueError("short read")
        b = self.data[self.pos:self.pos + n]
        self.pos += n
        return b

    def varint(self):
        value, shift = 0, 0
        while True:
            byte = self.take(1)[0]
            value |= (byte & 0x7F) << shift
            if byte < 0x80:
                return value
            shift += 7
            if shift > 63:
                raise ValueError("varint too long")


cases = []
for n in INPUTS:
    name = f"fcmp_pp_verify_inputs_{n}in.bin"
    r = Reader((repo / "tests" / "data" / name).read_bytes())
    signable_tx_hash = r.take(32)
    n_layers = r.take(1)[0]
    proof = r.take(r.varint())
    tree_root = r.take(32)
    pseudo_outs = [r.take(32) for _ in range(r.varint())]
    key_images = [r.take(32) for _ in range(r.varint())]
    if r.pos != len(r.data):
        raise ValueError(f"{name}: trailing bytes")
    if len(pseudo_outs) != n or len(key_images) != n:
        raise ValueError(f"{name}: expected {n} inputs")
    cases.append({
        "source_file": name,
        "inputs": n,
        "n_layers": n_layers,
        "signable_tx_hash": signable_tx_hash.hex(),
        "tree_root": tree_root.hex(),
        "pseudo_outs": [p.hex() for p in pseudo_outs],
        "key_images": [k.hex() for k in key_images],
        "proof": proof.hex(),
    })

out = {
    "schema": 1,
    "name": "upstream_monero_fcmp_pp",
    "description": "Monero's own FCMP++ test proofs (1, 2, 4, 8 and 128 inputs), verbatim from its FCMP++ "
                   "stressnet release. Every proof is valid for its signable transaction hash, tree root, "
                   "layer count, pseudo-outputs and key images. The tree root is a Selene point when the "
                   "layer count is odd and a Helios point when it is even. Used to check that the pinned "
                   "monero-fcmp-plus-plus verifies proofs made by Monero's code, not only proofs it made itself.",
    "source": {"repo": "https://github.com/seraphis-migration/monero", "tag": "v0.19.0.0-beta.3.0",
               "commit": commit, "path": "tests/data/", "licence": "BSD-3-Clause"},
    "cases": cases,
}
dest = Path(__file__).resolve().parent.parent.parent / "tests" / "vectors" / "upstream_monero_fcmp_pp.json"
dest.write_text(json.dumps(out, indent=2) + "\n", newline="\n")
print(f"wrote {dest} with {len(cases)} proofs from {commit[:12]}")
