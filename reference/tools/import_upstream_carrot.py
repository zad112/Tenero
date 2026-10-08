"""Imports Monero's Carrot "convergence" values from a checkout of Monero's FCMP++ stressnet release
(seraphis-migration/monero, tag v0.19.0.0-beta.3.0) into tests/vectors/upstream_monero_carrot_convergence.json.

NOT a reference implementation and not part of `make_vectors.py --check`: these are third-party data, copied
verbatim. `tests/unit_tests/carrot_convergence.cpp` fixes one master secret and the expected result of every Carrot
derivation that follows from it (account keys, a subaddress, the ephemeral keys, the shared secret, the commitment,
the one-time addresses, the view tag, the encryption masks, the special Janus anchor): values for another
implementation to converge on. `crates/tenero-carrot` must reproduce every one (docs/FCMP_CARROT_PLAN.md, G2).
Which derivation gives which value is in that test file and in `crates/tenero-carrot/tests/upstream_convergence.rs`.
The source repository is BSD-3-Clause.

    python reference/tools/import_upstream_carrot.py <path to a checkout of that tag>
"""
import json
import re
import subprocess
import sys
from pathlib import Path

if len(sys.argv) != 2:
    sys.exit(__doc__)
repo = Path(sys.argv[1])
commit = subprocess.check_output(["git", "-C", str(repo), "rev-parse", "HEAD"], text=True).strip()
SOURCE = "tests/unit_tests/carrot_convergence.cpp"
text = (repo / SOURCE).read_text()

HEX = re.compile(r'static const hex_value_t<([\w:]+)> (\w+)\("([0-9a-f]+)"\);')
INT = re.compile(r'static const (std::uint32_t|rct::xmr_amount) (\w+) = (\d+);')

values = {}
for type_name, name, hex_value in HEX.findall(text):
    if name in values:
        raise ValueError(f"{name} twice")
    values[name] = {"type": type_name, "hex": hex_value}
for type_name, name, number in INT.findall(text):
    if name in values:
        raise ValueError(f"{name} twice")
    values[name] = {"type": type_name, "int": int(number)}

# the set the Rust test reads: refuse to write a file that silently lost one
EXPECTED = {
    "s_master", "k_prove_spend", "partial_spend_pubkey", "s_view_balance", "s_generate_image_preimage",
    "k_generate_image", "k_view_incoming", "s_generate_address", "account_spend_pubkey", "account_view_pubkey",
    "address_index_major", "address_index_minor", "address_index_preimage_1", "address_index_preimage_2",
    "subaddress_scalar", "subaddress_spend_pubkey", "subaddress_view_pubkey", "anchor_norm", "anchor_special",
    "input_context", "payment_id", "enote_ephemeral_privkey", "enote_ephemeral_pubkey_cryptonote",
    "enote_ephemeral_pubkey_subaddress", "s_sender_receiver", "s_sender_receiver_ctx", "amount",
    "amount_blinding_factor_payment", "amount_blinding_factor_change", "amount_commitment",
    "onetime_address_coinbase", "onetime_address", "view_tag", "anchor_encryption_mask", "amount_encryption_mask",
    "payment_id_encryption_mask",
}
if set(values) != EXPECTED:
    raise ValueError(f"unexpected names: missing {EXPECTED - set(values)}, extra {set(values) - EXPECTED}")

out = {
    "schema": 1,
    "name": "upstream_monero_carrot_convergence",
    "description": "Monero's Carrot convergence values, verbatim: one master secret (s_master) and the expected "
                   "result of each Carrot derivation from it, as checked by Monero's own "
                   "tests/unit_tests/carrot_convergence.cpp. Secret keys and scalars are 32 bytes little endian, "
                   "points are compressed Ed25519, X25519 values are Montgomery u, the input context is 'R' and a "
                   "key image (33 bytes).",
    "source": {"repo": "https://github.com/seraphis-migration/monero", "tag": "v0.19.0.0-beta.3.0",
               "commit": commit, "path": SOURCE, "licence": "BSD-3-Clause"},
    "values": values,
}
dest = Path(__file__).resolve().parent.parent.parent / "tests" / "vectors" / "upstream_monero_carrot_convergence.json"
dest.write_text(json.dumps(out, indent=2) + "\n", newline="\n")
print(f"wrote {dest} with {len(values)} values from {commit[:12]}")
