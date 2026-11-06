import json
import sys
from pathlib import Path

import pytest
import requests

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import student_queries_agent as sqa  # noqa: E402

STUDENT = {
    "name": "María Pérez",
    "rut": "12.345.678-9",
    "email": "maria@uni.cl",
    "address": "Av. Siempre Viva 742",
    "program": "Ingeniería Civil",
    "enrollment_status": "regular",
    "current_term": "2026-2",
}
FINANCE = {"balance_due": 1250000, "card_number": "4111111111111111", "registration_hold": True}


class FakeUniversity:
    def __init__(self, student=STUDENT, finance=FINANCE):
        self._s, self._f = student, finance
        self.calls = []

    def student(self, sid):
        self.calls.append(("students", sid))
        return self._s

    def finances(self, sid):
        self.calls.append(("finances", sid))
        return self._f


class RecordingLLM:
    def __init__(self, reply="respuesta", fail=False):
        self.reply, self.fail, self.prompts = reply, fail, []

    def complete(self, system, user):
        self.prompts.append((system, user))
        if self.fail:
            raise RuntimeError("proveedor caído")
        return self.reply


def test_only_allowlisted_fields_reach_the_llm():
    llm = RecordingLLM()
    sqa.StudentQueryAgent(FakeUniversity(), llm).answer("¿Cuándo me inscribo?", "12345")
    _, user = llm.prompts[0]
    for leaked in ("María", "12.345.678-9", "maria@uni.cl", "Siempre Viva", "1250000", "4111"):
        assert leaked not in user
    ctx = json.loads(user.split("<<<\n", 1)[1].split("\n>>>", 1)[0])
    assert ctx["estudiante"]["program"] == "Ingeniería Civil"
    assert ctx["estado_financiero"] == {"registration_hold": True}


def test_context_is_framed_as_data():
    llm = RecordingLLM()
    sqa.StudentQueryAgent(FakeUniversity(), llm).answer("hola", "12345")
    system, _ = llm.prompts[0]
    assert "ignora" in system and "instrucciones" in system


@pytest.mark.parametrize("sid", ["", "../../admin", "1 OR 1=1", "a" * 33, "id/../x"])
def test_invalid_student_ids_never_reach_the_api(sid):
    uni = FakeUniversity()
    with pytest.raises(sqa.InvalidRequest):
        sqa.StudentQueryAgent(uni, RecordingLLM()).answer("hola", sid)
    assert uni.calls == []


def test_overlong_question_rejected():
    with pytest.raises(sqa.InvalidRequest):
        sqa.StudentQueryAgent(FakeUniversity(), RecordingLLM()).answer("x" * 5000, "12345")


def test_backend_down_gives_friendly_reply_without_calling_llm():
    llm = RecordingLLM()
    agent = sqa.StudentQueryAgent(FakeUniversity(None, None), llm)
    assert agent.answer("hola", "12345") == agent.unavailable_reply
    assert llm.prompts == []


def test_llm_failure_is_contained():
    agent = sqa.StudentQueryAgent(FakeUniversity(), RecordingLLM(fail=True))
    assert agent.answer("hola", "12345") == agent.unavailable_reply


def test_config_requires_https():
    with pytest.raises(RuntimeError):
        sqa.Config(api_url="http://api.university.edu")
    assert sqa.Config(api_url="https://api.university.edu/").api_url == "https://api.university.edu"


def test_client_errors_return_none_and_quote_ids(monkeypatch):
    cfg = sqa.Config(api_url="https://api.example.edu", api_token="t")
    client = sqa.UniversityClient(cfg)
    seen = {}

    def fake_get(url, timeout, headers=None):
        seen["url"], seen["timeout"] = url, timeout
        raise requests.ConnectionError("boom")

    monkeypatch.setattr(client.s, "get", fake_get)
    assert client.student("12345") is None
    assert seen == {"url": "https://api.example.edu/students/12345", "timeout": 5.0}
    assert client.s.headers["Authorization"] == "Bearer t"


TOKEN = "k3J9-xQ2v_Lp8ZmA4tR7wE1nC6bY0uH5sD"


@pytest.mark.parametrize("bad", ["x" * 40, "REEMPLAZAR-con-al-menos-32-caracteres-aleatorios", "short"])
def test_service_refuses_placeholder_or_weak_tokens(monkeypatch, bad):
    from fastapi.testclient import TestClient

    import service

    monkeypatch.setenv("AETHERLYNX_AGENT_TOKEN", bad)
    service.state.agent = sqa.StudentQueryAgent(FakeUniversity(), RecordingLLM("ok"))
    with pytest.raises(RuntimeError):
        with TestClient(service.app):
            pass


def test_service_requires_token(monkeypatch):
    from fastapi.testclient import TestClient

    import service

    monkeypatch.setenv("AETHERLYNX_AGENT_TOKEN", TOKEN)
    service.state.agent = sqa.StudentQueryAgent(FakeUniversity(), RecordingLLM("ok"))
    with TestClient(service.app) as c:
        body = {"student_id": "12345", "question": "hola"}
        assert c.post("/query", json=body).status_code == 401
        assert c.post("/query", json=body, headers={"Authorization": "Bearer wrong"}).status_code == 401
        r = c.post("/query", json=body, headers={"Authorization": "Bearer " + TOKEN})
        assert r.status_code == 200 and r.json() == {"answer": "ok"}
        bad = c.post("/query", json={**body, "student_id": "../x"}, headers={"Authorization": "Bearer " + TOKEN})
        assert bad.status_code == 422
        assert c.get("/readyz").status_code == 200
