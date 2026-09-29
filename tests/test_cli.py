import cli
from toycoin.chain import Blockchain
from toycoin.mempool import Mempool
from toycoin.units import UNIT


def make_app(tmp_path, monkeypatch):
    # point the CLI at a scratch folder so it never touches your real chain
    monkeypatch.setattr(cli, "WALLET_DIR", str(tmp_path / "wallets"))
    import toycoin.chain as chain_mod
    import toycoin.mempool as mempool_mod
    chain_path = str(tmp_path / "chain.json")
    mempool_path = str(tmp_path / "mempool.json")
    monkeypatch.setattr(chain_mod, "DEFAULT_PATH", chain_path)
    monkeypatch.setattr(mempool_mod, "DEFAULT_MEMPOOL", mempool_path)
    monkeypatch.setattr(Blockchain.load_or_new.__func__, "__defaults__",
                        (chain_path, None, None))   # path, searcher, algorithm
    monkeypatch.setattr(Blockchain.save, "__defaults__", (chain_path,))
    monkeypatch.setattr(Mempool.__init__, "__defaults__", (mempool_path,))
    app = cli.App("alice")
    bc = Blockchain(target=2**248, initial_reward=50 * UNIT, halving_interval=10_000,
                    max_supply=10**9 * UNIT, tail_reward=0, difficulty_window=0)
    bc.mine_block(app.wallet.address)
    bc.save()
    return app


def test_send_accepts_four_decimals(tmp_path, monkeypatch, capsys):
    app = make_app(tmp_path, monkeypatch)
    other = cli.Wallet().address
    app.run(["send", other, "1.2345", "slow"])
    out = capsys.readouterr().out
    assert "queued: 1.2345" in out
    assert app.mempool.load()[0].amount == 12_345


def test_send_rejects_five_decimals(tmp_path, monkeypatch, capsys):
    app = make_app(tmp_path, monkeypatch)
    other = cli.Wallet().address
    app.run(["send", other, "1.23456", "slow"])
    assert "at most 4 decimal places" in capsys.readouterr().out
    assert app.mempool.load() == []


def test_balance_shows_four_decimals(tmp_path, monkeypatch, capsys):
    app = make_app(tmp_path, monkeypatch)
    app.run(["balance"])
    assert "confirmed: 50.0000" in capsys.readouterr().out


def test_difficulty_command(tmp_path, monkeypatch, capsys):
    app = make_app(tmp_path, monkeypatch)
    app.run(["difficulty"])
    out = capsys.readouterr().out
    from toycoin.config import TARGET_BLOCK_TIME
    assert f"target block time: {TARGET_BLOCK_TIME}s" in out
    assert "hashes per block" in out


# ---- chains with an expensive proof of work ----

def matmul_chain_in_place(app_bc_saver, blocks=3):
    from toycoin import matmulhash as mh
    from toycoin.pow import MatmulPow
    from toycoin.units import UNIT
    bc = Blockchain(target=2**256 // 8, initial_reward=50 * UNIT, halving_interval=10_000,
                    max_supply=10**9 * UNIT, tail_reward=0, difficulty_window=0,
                    pow=MatmulPow(mh.Params(m=8, k=64, nb=32, num_blocks=4), 2))
    for i in range(blocks):
        bc.mine_block("d" * 40, timestamp=1_000_000 + 30 * (i + 1))
    bc.save()
    return bc


def test_chain_command_is_honest_about_expensive_proof_of_work(tmp_path, monkeypatch, capsys):
    app = make_app(tmp_path, monkeypatch)
    matmul_chain_in_place(app)
    app.run(["chain"])
    out = capsys.readouterr().out
    assert "structure valid: True" in out and "NOT recomputed" in out and "verify" in out
    assert "catches any tampering" in out            # the cheap check is stronger now


def test_the_chain_command_now_spots_tampering_without_the_expensive_check(tmp_path, monkeypatch, capsys):
    import json
    app = make_app(tmp_path, monkeypatch)
    matmul_chain_in_place(app)
    path = str(tmp_path / "chain.json")
    data = json.load(open(path))
    data["chain"][2]["transactions"][0]["amount"] += 1
    with open(path, "w") as f:
        json.dump(data, f)
    app.run(["chain"])
    assert "structure valid: False" in capsys.readouterr().out


def test_verify_checks_every_block_and_catches_tampering(tmp_path, monkeypatch, capsys):
    app = make_app(tmp_path, monkeypatch)
    matmul_chain_in_place(app, blocks=3)
    app.run(["verify"])
    out = capsys.readouterr().out
    assert "block 3/3 checked" in out and "valid: True" in out
    assert "dataset is built on the CPU first" in out and "fraction of a second" in out

    import json
    path = str(tmp_path / "chain.json")
    data = json.load(open(path))
    data["chain"][2]["transactions"][0]["amount"] += 1
    json.dump(data, open(path, "w"))
    app.run(["verify"])
    assert "valid: False" in capsys.readouterr().out


def test_verify_on_a_sha256_chain_is_quick_and_quiet(tmp_path, monkeypatch, capsys):
    app = make_app(tmp_path, monkeypatch)          # the fixture's chain is SHA-256
    app.run(["verify"])
    out = capsys.readouterr().out
    assert "valid: True" in out and "a few seconds" not in out


def test_difficulty_names_the_proof_of_work(tmp_path, monkeypatch, capsys):
    app = make_app(tmp_path, monkeypatch)
    matmul_chain_in_place(app)
    app.run(["difficulty"])
    out = capsys.readouterr().out
    assert "proof of work: matmul int8 v2" in out


def test_difficulty_explains_units_and_shows_tops_for_a_matmul_chain(tmp_path, monkeypatch, capsys):
    app = make_app(tmp_path, monkeypatch)
    from toycoin import matmulhash as mh
    from toycoin.pow import MatmulPow
    from toycoin.units import UNIT
    bc = Blockchain(target=2**256 // 8, initial_reward=50 * UNIT, halving_interval=10_000,
                    max_supply=10**9 * UNIT, tail_reward=0, difficulty_window=4,
                    pow=MatmulPow(mh.Params(m=8, k=64, nb=32, num_blocks=4), 2))
    for i in range(6):
        bc.mine_block("d" * 40, timestamp=1_000_000 + 30 * (i + 1))
    bc.save()
    app.run(["difficulty"])
    out = capsys.readouterr().out
    assert "one attempt = " in out and "billion int8 operations" in out
    assert "attempts per block" in out and "hashes per block" not in out
    assert "TOPS" in out and "of int8 matmul" in out


def test_difficulty_on_a_sha256_chain_has_no_tops_line(tmp_path, monkeypatch, capsys):
    app = make_app(tmp_path, monkeypatch)
    app.run(["difficulty"])
    assert "TOPS" not in capsys.readouterr().out


def test_supply_reads_well_at_the_real_scale(tmp_path, monkeypatch, capsys):
    app = make_app(tmp_path, monkeypatch)
    Blockchain(target=2**248).save()                    # every economic setting from config.py
    app.run(["supply"])
    out = capsys.readouterr().out
    assert "main emission cap: 20000000.0000" in out
    assert "next block reward: 20.0000" in out
    assert "MAIN EMISSION - 2,334,400 more block(s)" in out
    assert "about 4.4 years at 60s blocks" in out
    assert "tail emission of 0.5000 per block, forever" in out


def test_supply_uses_the_chains_own_block_time(tmp_path, monkeypatch, capsys):
    app = make_app(tmp_path, monkeypatch)
    Blockchain(target=2**248, block_time=30, initial_reward=10 * UNIT, halving_interval=1_000,
               max_supply=1_000_000 * UNIT, tail_reward=UNIT // 2).save()
    app.run(["supply"])
    out = capsys.readouterr().out
    assert "at 30s blocks" in out                          # not the config default of 60


def test_the_tail_phase_uses_the_chains_own_block_time(tmp_path, monkeypatch, capsys):
    app = make_app(tmp_path, monkeypatch)
    bc = Blockchain(target=2**248, block_time=30, initial_reward=UNIT, halving_interval=1,
                    max_supply=UNIT, tail_reward=UNIT // 2)
    bc.mine_block("d" * 40)
    bc.mine_block("d" * 40)
    bc.save()
    app.run(["supply"])
    out = capsys.readouterr().out
    assert "TAIL EMISSION" in out and "at 30s blocks" in out
    assert "1440.0000 coins/day" in out                    # 0.5 coins x 2880 blocks a day
