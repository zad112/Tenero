"""Imports real Monero mainnet data from a checkout of monero-oxide: Bulletproofs+ range proofs into
tests/vectors/upstream_monero_bpp.json and a CLSAG transaction (with its rings) into
tests/vectors/upstream_monero_clsag.json.

NOT a reference implementation and not part of `make_vectors.py --check`: these are third-party data, copied
verbatim, so that our pinned copy of the proof libraries can be shown to accept real Monero proofs. Provenance
is recorded in the file. The source repository is MIT-licensed; the transactions themselves are public
blockchain data.

    python reference/tools/import_upstream_vectors.py <path to a monero-oxide checkout>
"""
import json
import subprocess
import sys
from pathlib import Path

if len(sys.argv) != 2:
    sys.exit(__doc__)
repo = Path(sys.argv[1])
commit = subprocess.check_output(["git", "-C", str(repo), "rev-parse", "HEAD"], text=True).strip()
vec = repo / "monero-oxide" / "src" / "tests" / "vectors"
sources = [("transactions.json", None), ("clsag_tx.json", None)]

proofs = []
txs = json.loads((vec / "transactions.json").read_text())
txs = [(t["id"], t["tx"], "transactions.json") for t in txs]
c = json.loads((vec / "clsag_tx.json").read_text())
txs.append(("clsag_tx", c["tx"], "clsag_tx.json"))
for txid, tx, src in txs:
    bpp = (tx.get("rctsig_prunable") or {}).get("bpp")
    if not bpp:
        continue
    assert tx["rct_signatures"]["type"] == 6, "type 6 is RCT with Bulletproofs+"
    assert len(bpp) == 1
    p = bpp[0]
    proofs.append({
        "tx": txid,
        "source_file": src,
        "out_pk": tx["rct_signatures"]["outPk"],
        "A": p["A"], "A1": p["A1"], "B": p["B"], "r1": p["r1"], "s1": p["s1"], "d1": p["d1"],
        "L": p["L"], "R": p["R"],
    })

out = {
    "schema": 1,
    "name": "upstream_monero_bpp",
    "description": "Real Monero mainnet Bulletproofs+ range proofs (RCT type 6), verbatim from monero-oxide's "
                   "test vectors. Every proof is valid for its out_pk commitments. Used to check that the "
                   "pinned monero-bulletproofs verifies real Monero proofs, not only proofs it made itself.",
    "source": {"repo": "https://github.com/monero-oxide/monero-oxide", "commit": commit,
               "path": "monero-oxide/src/tests/vectors/", "licence": "MIT"},
    "proofs": proofs,
}
dest = Path(__file__).resolve().parent.parent.parent / "tests" / "vectors" / "upstream_monero_bpp.json"
dest.write_text(json.dumps(out, indent=2) + "\n", newline="\n")
print(f"wrote {dest} with {len(proofs)} proofs from {commit[:12]}")

# --- a real CLSAG transaction: the raw transaction, and what a verifier needs beside it ---
ring_data = json.loads((vec / "ring_data.json").read_text())
ctx = c["tx"]
assert ctx["rct_signatures"]["type"] == 6
inputs = ctx["vin"]
assert len(ring_data) == len(inputs)
clsag_out = {
    "schema": 1,
    "name": "upstream_monero_clsag",
    "description": "A real Monero mainnet transaction with CLSAG ring signatures (RCT type 6), with the rings "
                   "its inputs were signed against: for each input the ring as [key, commitment] pairs, the "
                   "key image and the pseudo-output commitment. Verbatim from monero-oxide's tests. The "
                   "signatures verify against Monero's own signature hash of the transaction.",
    "source": out["source"],
    "tx_hex": c["hex"],
    "inputs": [
        {
            "key_image": inp["key"]["k_image"],
            "pseudo_out": ctx["rctsig_prunable"]["pseudoOuts"][i],
            "ring": [[m["key"], m["mask"]] for m in ring_data[i]],
        }
        for i, inp in enumerate(inputs)
    ],
}
dest = Path(__file__).resolve().parent.parent.parent / "tests" / "vectors" / "upstream_monero_clsag.json"
dest.write_text(json.dumps(clsag_out, indent=2) + "\n", newline="\n")
print(f"wrote {dest} with {len(inputs)} inputs")
