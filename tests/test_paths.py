import os

from toycoin import paths


def test_default_is_the_project_folder(monkeypatch):
    monkeypatch.delenv("TOYCOIN_DATA", raising=False)
    assert paths.data_dir() == paths.PROJECT_DIR
    assert paths.data_dir("") == paths.PROJECT_DIR


def test_the_environment_variable_selects_a_folder_and_creates_it(monkeypatch, tmp_path):
    target = tmp_path / "scratch"
    monkeypatch.setenv("TOYCOIN_DATA", str(target))
    assert paths.data_dir() == str(target)
    assert target.is_dir()


def test_an_explicit_value_wins_over_the_environment(monkeypatch, tmp_path):
    monkeypatch.setenv("TOYCOIN_DATA", str(tmp_path / "from_env"))
    other = tmp_path / "explicit"
    assert paths.data_dir(str(other)) == str(other)


def test_a_relative_folder_is_made_absolute(monkeypatch, tmp_path):
    monkeypatch.chdir(tmp_path)
    got = paths.data_dir("scratch")
    assert os.path.isabs(got) and got == str(tmp_path / "scratch")


def test_all_the_data_files_live_in_the_data_folder():
    assert paths.CHAIN_PATH == os.path.join(paths.DATA_DIR, "chain.json")
    assert paths.MEMPOOL_PATH == os.path.join(paths.DATA_DIR, "mempool.json")
    assert paths.WALLET_DIR == os.path.join(paths.DATA_DIR, "wallets")


def test_the_modules_use_these_paths():
    from toycoin import chain, mempool
    assert chain.DEFAULT_PATH == paths.CHAIN_PATH
    assert mempool.DEFAULT_MEMPOOL == paths.MEMPOOL_PATH
