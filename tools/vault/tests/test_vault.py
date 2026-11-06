import subprocess
import sys
import textwrap

import pytest

import rust_vault

PASS = "correct horse battery staple"
# Mínimo permitido (OWASP: 19 MiB, 2 pasadas) para que los tests sean rápidos.
FAST = dict(m_cost_kib=19456, t_cost=2, p_cost=1)


@pytest.fixture
def db(tmp_path):
    return str(tmp_path / "vault.db")


def test_secret_survives_process_restart(db):
    """El fallo original: tras reiniciar, ningún secreto se podía descifrar."""
    code = textwrap.dedent(
        f"""
        import rust_vault, sys
        v = rust_vault.RustVault({db!r}, {PASS!r}, m_cost_kib=19456, t_cost=2, p_cost=1)
        if sys.argv[1] == "write":
            v.add_secret("api_key", "hello")
        else:
            print(v.get_secret("api_key"))
        """
    )
    subprocess.run([sys.executable, "-c", code, "write"], check=True)
    out = subprocess.run([sys.executable, "-c", code, "read"], check=True, capture_output=True, text=True)
    assert out.stdout.strip() == "hello"


def test_wrong_passphrase(db):
    rust_vault.RustVault(db, PASS, **FAST)
    with pytest.raises(rust_vault.WrongPassphraseError):
        rust_vault.RustVault(db, "a different passphrase")


def test_short_passphrase(db):
    with pytest.raises(rust_vault.InvalidInputError):
        rust_vault.RustVault(db, "short")


def test_from_env(db, monkeypatch):
    monkeypatch.setenv("AETHERLYNX_VAULT_PASSPHRASE", PASS)
    rust_vault.RustVault(db, PASS, **FAST).add_secret("k", "v")
    assert rust_vault.RustVault.from_env(db).get_secret("k") == "v"


def test_crud(db):
    v = rust_vault.RustVault(db, PASS, **FAST)
    v.add_secret("a", "1")
    v.add_secret("a", "2")
    assert v.get_secret("a") == "2"
    assert v.list_secrets() == ["a"]
    assert v.delete_secret("a") is True
    assert v.delete_secret("a") is False
    with pytest.raises(rust_vault.SecretNotFoundError):
        v.get_secret("a")


def test_encrypt_requires_same_context(db):
    v = rust_vault.RustVault(db, PASS, **FAST)
    t = v.encrypt(b"datos", context="ctx")
    assert bytes(v.decrypt(t, context="ctx")) == b"datos"
    with pytest.raises(rust_vault.DecryptionError):
        v.decrypt(t, context="otro")


def test_certificate_includes_private_key(db):
    v = rust_vault.RustVault(db, PASS, **FAST)
    c = v.generate_certificate("example.com", days=30, store_key_as="tls/key")
    assert c["certificate_pem"].startswith("-----BEGIN CERTIFICATE-----")
    assert v.get_secret("tls/key") == c["private_key_pem"]


def test_change_passphrase(db):
    v = rust_vault.RustVault(db, PASS, **FAST)
    v.add_secret("k", "v")
    v.change_passphrase("a brand new passphrase")
    del v
    with pytest.raises(rust_vault.WrongPassphraseError):
        rust_vault.RustVault(db, PASS)
    assert rust_vault.RustVault(db, "a brand new passphrase").get_secret("k") == "v"


def test_weak_kdf_params_rejected(db):
    with pytest.raises(rust_vault.InvalidInputError):
        rust_vault.RustVault(db, PASS, m_cost_kib=8, t_cost=1)


def test_second_handle_detects_rotation(db):
    a = rust_vault.RustVault(db, PASS, **FAST)
    b = rust_vault.RustVault(db, PASS)
    a.change_passphrase("a brand new passphrase")
    with pytest.raises(rust_vault.StaleVaultError):
        b.add_secret("x", "1")
