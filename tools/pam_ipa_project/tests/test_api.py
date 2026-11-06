"""Tests de la API de autenticación contra el módulo Rust compilado."""

import importlib
import secrets
import sys
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import pam_ipa_project as pamlib  # noqa: E402

PW = "correct horse battery"
ADMIN_PW = "admin password 123456"


@pytest.fixture
def env(tmp_path, monkeypatch):
    monkeypatch.setenv("AETHERLYNX_JWT_SECRET", secrets.token_urlsafe(48))
    monkeypatch.setenv("AETHERLYNX_DB_PATH", str(tmp_path / "auth.db"))
    monkeypatch.setenv("AETHERLYNX_BOOTSTRAP_ADMIN_USER", "root")
    monkeypatch.setenv("AETHERLYNX_BOOTSTRAP_ADMIN_PASSWORD", ADMIN_PW)
    return tmp_path


def make_client():
    import app as app_module

    importlib.reload(app_module)
    return TestClient(app_module.app)


@pytest.fixture
def client(env):
    with make_client() as c:
        yield c


def login(c, user, pw, **extra):
    return c.post("/authenticate", json={"username": user, "password": pw, **extra})


def bearer(c, user, pw):
    r = login(c, user, pw)
    assert r.status_code == 200, r.text
    return {"Authorization": f"Bearer {r.json()['access_token']}"}


def test_refuses_to_start_without_strong_secret(env, monkeypatch):
    monkeypatch.setenv("AETHERLYNX_JWT_SECRET", "secret")
    with pytest.raises(RuntimeError, match="32 bytes"):
        with make_client():
            pass


def test_register_assigns_server_side_role_only(client):
    r = client.post("/register", json={"username": "Alice", "password": PW, "role": "admin"})
    assert r.status_code == 201
    assert r.json()["roles"] == ["user"]
    me = client.get("/me", headers=bearer(client, "alice", PW)).json()
    assert me == {"username": "alice", "roles": ["user"]}


def test_re_register_cannot_overwrite_password(client):
    client.post("/register", json={"username": "alice", "password": PW})
    r = client.post("/register", json={"username": "alice", "password": "attacker password!!"})
    assert r.status_code == 409
    assert login(client, "alice", PW).status_code == 200
    assert login(client, "alice", "attacker password!!").status_code == 401


def test_login_errors_are_indistinguishable(client):
    client.post("/register", json={"username": "alice", "password": PW})
    a = login(client, "alice", "wrong password!!")
    b = login(client, "nobody", "wrong password!!")
    assert a.status_code == b.status_code == 401
    assert a.json() == b.json()


def test_weak_password_rejected(client):
    r = client.post("/register", json={"username": "bob", "password": "short"})
    assert r.status_code == 422


def test_protected_routes_need_a_valid_token(client):
    assert client.get("/me").status_code == 401
    assert client.get("/me", headers={"Authorization": "Bearer nonsense"}).status_code == 401


def test_token_from_another_deployment_is_rejected(client, tmp_path):
    client.post("/register", json={"username": "alice", "password": PW})
    other = pamlib.PyAuthManager(str(tmp_path / "other.db"), secrets.token_urlsafe(48))
    other.register_user("alice", PW)
    foreign = other.authenticate_user("alice", PW)
    assert client.get("/me", headers={"Authorization": f"Bearer {foreign}"}).status_code == 401
    assert client.post("/verify-token", json={"token": foreign}).json() == {"is_valid": False}


def test_non_admin_cannot_manage_roles_or_resources(client):
    client.post("/register", json={"username": "alice", "password": PW})
    h = bearer(client, "alice", PW)
    assert client.post("/assign-role/alice", json={"role": "admin"}, headers=h).status_code == 403
    assert client.post("/add-resource", json={"resource": "grades", "allowed_role": "user"}, headers=h).status_code == 403
    assert client.get("/users/root", headers=h).status_code == 403
    assert client.get("/audit", headers=h).status_code == 403
    assert client.get("/me", headers=h).json()["roles"] == ["user"]


def test_admin_flow_and_resource_check_returns_403_not_401(client):
    client.post("/register", json={"username": "alice", "password": PW})
    admin = bearer(client, "root", ADMIN_PW)
    assert client.post("/add-resource", json={"resource": "grades", "allowed_role": "student"}, headers=admin).status_code == 201
    assert login(client, "alice", PW, resource="grades").status_code == 403
    assert client.post("/assign-role/alice", json={"role": "student"}, headers=admin).status_code == 200
    r = login(client, "alice", PW, resource="grades")
    assert r.status_code == 200 and r.json()["resource"] == "grades"
    assert client.post("/assign-role/ghost", json={"role": "x"}, headers=admin).status_code == 404


def test_admin_cannot_remove_own_admin_role(client):
    admin = bearer(client, "root", ADMIN_PW)
    assert client.delete("/assign-role/root/admin", headers=admin).status_code == 409


def test_roles_and_resources_survive_restart(env):
    with make_client() as c:
        c.post("/register", json={"username": "alice", "password": PW})
        admin = bearer(c, "root", ADMIN_PW)
        c.post("/add-resource", json={"resource": "grades", "allowed_role": "student"}, headers=admin)
        c.post("/assign-role/alice", json={"role": "student"}, headers=admin)
    with make_client() as c:
        h = bearer(c, "alice", PW)
        assert c.get("/accessible-resources/alice", headers=h).json()["accessible_resources"] == ["grades"]


def test_account_locks_after_repeated_failures(client):
    client.post("/register", json={"username": "alice", "password": PW})
    for _ in range(5):
        assert login(client, "alice", "wrong password!!").status_code == 401
    assert login(client, "alice", PW).status_code == 401


def test_audit_log_is_persisted_and_admin_only(client):
    client.post("/register", json={"username": "alice", "password": PW})
    login(client, "alice", "wrong password!!")
    events = client.get("/audit", headers=bearer(client, "root", ADMIN_PW)).json()["events"]
    actions = {(e["username"], e["action"], e["success"]) for e in events}
    assert ("alice", "login", False) in actions
    assert ("root", "login", True) in actions


def test_bootstrap_never_promotes_a_preexisting_account(env, monkeypatch):
    monkeypatch.setenv("AETHERLYNX_BOOTSTRAP_ADMIN_USER", "")
    with make_client() as c:
        # Alguien registra el nombre del futuro admin antes de configurarlo.
        assert c.post("/register", json={"username": "root", "password": "squatter password!!"}).status_code == 201
    monkeypatch.setenv("AETHERLYNX_BOOTSTRAP_ADMIN_USER", "root")
    with make_client() as c:
        h = bearer(c, "root", "squatter password!!")
        assert c.get("/me", headers=h).json()["roles"] == ["user"]
        assert c.get("/audit", headers=h).status_code == 403


def test_admin_revocation_survives_restart(env):
    with make_client() as c:
        c.post("/register", json={"username": "alice", "password": PW})
        admin = bearer(c, "root", ADMIN_PW)
        c.post("/assign-role/alice", json={"role": "admin"}, headers=admin)
        c.delete("/assign-role/root/admin", headers=bearer(c, "alice", PW))
    with make_client() as c:
        assert "admin" not in c.get("/me", headers=bearer(c, "root", ADMIN_PW)).json()["roles"]


def test_rate_limit_on_login(env, monkeypatch):
    monkeypatch.setenv("AETHERLYNX_RATE_LIMIT_PER_MINUTE", "3")
    with make_client() as c:
        codes = [login(c, "nobody", "whatever password").status_code for _ in range(5)]
    assert codes == [401, 401, 401, 429, 429]


def test_logout_all_invalidates_tokens(client):
    client.post("/register", json={"username": "alice", "password": PW})
    h = bearer(client, "alice", PW)
    assert client.post("/me/logout-all", headers=h).status_code == 200
    assert client.get("/me", headers=h).status_code == 401


def test_change_password(client):
    client.post("/register", json={"username": "alice", "password": PW})
    h = bearer(client, "alice", PW)
    r = client.post("/me/password", json={"old_password": PW, "new_password": "brand new password"}, headers=h)
    assert r.status_code == 200
    assert client.get("/me", headers=h).status_code == 401, "el token anterior ya no sirve"
    assert login(client, "alice", "brand new password").status_code == 200
    assert login(client, "alice", PW).status_code == 401
