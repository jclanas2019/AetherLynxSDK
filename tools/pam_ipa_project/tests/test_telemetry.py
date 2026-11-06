"""Telemetría de la API de autenticación: contadores de inicio de sesión,
spans con plantilla de ruta y ausencia de nombres de usuario y contraseñas."""

import json
import sys
from pathlib import Path

import pytest
from opentelemetry import metrics, trace
from opentelemetry.sdk.metrics import Counter, Histogram, MeterProvider, UpDownCounter
from opentelemetry.sdk.metrics.export import AggregationTemporality, InMemoryMetricReader
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import SimpleSpanProcessor
from opentelemetry.sdk.trace.export.in_memory_span_exporter import InMemorySpanExporter

sys.path.insert(0, str(Path(__file__).resolve().parent))

SPANS = InMemorySpanExporter()
READER = InMemoryMetricReader(
    preferred_temporality={
        Counter: AggregationTemporality.DELTA,
        UpDownCounter: AggregationTemporality.DELTA,
        Histogram: AggregationTemporality.DELTA,
    }
)
_tp = TracerProvider()
_tp.add_span_processor(SimpleSpanProcessor(SPANS))
trace.set_tracer_provider(_tp)
metrics.set_meter_provider(MeterProvider(metric_readers=[READER]))

from test_api import PW, env, login, make_client  # noqa: E402,F401

USER = "telemetria-alice"


@pytest.fixture
def client(env):  # noqa: F811
    SPANS.clear()
    READER.get_metrics_data()
    with make_client() as c:
        yield c


def points(data, name):
    out = []
    for rm in data.resource_metrics if data else []:
        for sm in rm.scope_metrics:
            for m in sm.metrics:
                if m.name == name:
                    out.extend((dict(p.attributes), getattr(p, "value", None)) for p in m.data.data_points)
    return out


def total(data, name, **attrs):
    return sum(v for a, v in points(data, name) if all(a.get(k) == x for k, x in attrs.items()))


def test_logins_are_counted_by_result(client):
    assert client.post("/register", json={"username": USER, "password": PW}).status_code == 201
    assert login(client, USER, PW).status_code == 200
    assert login(client, USER, "contraseña equivocada").status_code == 401
    assert login(client, "nadie-existe", PW).status_code == 401
    data = READER.get_metrics_data()
    assert total(data, "aetherlynx.auth.logins", result="success") == 1
    assert total(data, "aetherlynx.auth.logins", result="failure") == 2
    assert total(data, "aetherlynx.auth.registrations", source="self") == 1


def test_rate_limited_logins_are_counted(env, monkeypatch):  # noqa: F811
    monkeypatch.setenv("AETHERLYNX_RATE_LIMIT_PER_MINUTE", "2")
    READER.get_metrics_data()
    with make_client() as c:
        codes = [login(c, "x-user", "y password long").status_code for _ in range(4)]
    assert codes.count(429) == 2
    data = READER.get_metrics_data()
    assert total(data, "aetherlynx.auth.logins", result="rate_limited") == 2
    assert total(data, "aetherlynx.auth.rate_limited", route="/authenticate") == 2


def test_server_spans_use_route_template_and_carry_no_credentials(client):
    client.post("/register", json={"username": USER, "password": PW})
    token = login(client, USER, PW).json()["access_token"]
    r = client.get(f"/users/{USER}", headers={"Authorization": f"Bearer {token}"})
    assert r.status_code == 200 and len(r.headers["X-Trace-Id"]) == 32
    servers = [s for s in SPANS.get_finished_spans() if s.kind == trace.SpanKind.SERVER]
    names = {s.name for s in servers}
    assert {"POST /register", "POST /authenticate", "GET /users/{username}"} <= names
    assert not [s for s in SPANS.get_finished_spans() if s.name.startswith("fastapi.")]
    auth = next(s for s in servers if s.name == "POST /authenticate")
    assert auth.attributes["aetherlynx.auth.result"] == "success"

    dump = json.dumps(
        [{"n": s.name, "a": dict(s.attributes), "e": [dict(e.attributes) for e in s.events]} for s in SPANS.get_finished_spans()]
    )
    dump += json.dumps([a for m in ("aetherlynx.http.requests", "aetherlynx.auth.logins") for a, _ in points(READER.get_metrics_data(), m)])
    for secret in (USER, PW, token):
        assert secret not in dump
