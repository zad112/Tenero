"""Version 3 wallet keys and addresses (docs/CONSENSUS_V2.md 15.9): the reference against its committed vectors, and the
properties the format promises, checked here on random keys."""
import json
import os
import random

from tools import make_vectors_address as a


def load():
    with open(os.path.join(a.VECTOR_DIR, "v3_address.json"), encoding="utf-8") as f:
        return json.load(f)


def test_the_committed_address_vectors_are_current(capsys):
    assert a.main(["--check"]) == 0, capsys.readouterr().out


def test_every_address_of_a_network_starts_with_its_four_letters_whatever_its_keys():
    rng = random.Random(20261008)
    for net, prefix in a.NETWORKS.items():
        for kind in a.KINDS:
            for _ in range(300):
                spend, view = rng.randbytes(32), rng.randbytes(32)
                pid = rng.randbytes(8) if kind == "integrated" else None
                text = a.encode(net, kind, spend, view, pid)
                assert text.startswith(prefix)
                assert len(text) == (110 if kind == "integrated" else 99)
                assert a.decode(text, net) == (kind, spend, view, pid)


def test_the_tags_are_distinct_four_byte_varints_and_no_other_coin_s():
    tags = list(a.TAGS.values())
    assert len(set(tags)) == 9
    for t in tags:
        assert len(a.varint(t)) == 4
        assert t not in a.OTHER_TAGS


def test_any_single_changed_character_is_caught():
    v = load()
    text = v["valid"][0]["text"]
    for i in range(len(text)):
        for c in "2Zz":
            if c == text[i]:
                continue
            changed = text[:i] + c + text[i + 1:]
            try:
                a.decode(changed, "gamma")
            except a.AddressError:
                continue
            raise AssertionError(f"position {i}: {changed}")


def test_the_master_secret_is_the_documented_hash():
    for k in load()["keys"]:
        seed = bytes.fromhex(k["seed"])
        acct = a.account_seed(seed, k["account"])
        assert acct.hex() == k["account_seed"]
        assert a.sha256(b"tenero carrot master v1", acct).hex() == k["carrot_master"]
