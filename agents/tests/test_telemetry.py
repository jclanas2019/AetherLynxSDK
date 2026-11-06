"""Telemetría del agente con exportadores en memoria: forma de las trazas,
métricas emitidas y ausencia de datos personales."""

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

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

SPANS = InMemorySpanExporter()
# Delta: cada lectura trae solo lo nuevo desde la anterior.
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

import observability  # noqa: E402
import student_queries_agent as sqa  # noqa: E402
from test_agent import FINANCE, STUDENT, FakeUniversity, RecordingLLM  # noqa: E402

PII = ["María", "12.345.678-9", "maria@uni.cl", "Siempre Viva", "1250000", "4111", "12345"]


@pytest.fixture(autouse=True)
def clear():
    SPANS.clear()
    READER.get_metrics_data()  # vacía lo acumulado por los tests anteriores
    _SNAPSHOT.clear()
    yield


_SNAPSHOT = {}


def metric_points(name):
    """Puntos de una métrica. La primera consulta de cada test lee el lector
    (delta) y guarda la instantánea para las demás consultas del mismo test."""
    if not _SNAPSHOT:
        _SNAPSHOT["data"] = READER.get_metrics_data()
    data = _SNAPSHOT["data"]
    out = []
    for rm in data.resource_metrics if data else []:
        for sm in rm.scope_metrics:
            for m in sm.metrics:
                if m.name == name:
                    out.extend(m.data.data_points)
    return out


def dump_spans():
    return json.dumps([json.loads(s.to_json()) for s in SPANS.get_finished_spans()], ensure_ascii=False)


class TracingUniversity(FakeUniversity):
    """Simula el cliente real creando los mismos spans."""

    def student(self, sid):
        with trace.get_tracer("t").start_as_current_span("upstream.students"):
            return super().student(sid)


def test_answer_produces_span_tree_and_metrics():
    llm = RecordingLLM("ok")
    llm.model = "gpt-4o-mini"
    sqa.StudentQueryAgent(TracingUniversity(), llm).answer("¿Cuándo me inscribo?", "12345")
    spans = {s.name: s for s in SPANS.get_finished_spans()}
    assert {"agent.answer", "llm.complete", "upstream.students"} <= set(spans)
    root = spans["agent.answer"]
    assert spans["llm.complete"].parent.span_id == root.context.span_id
    assert spans["upstream.students"].parent.span_id == root.context.span_id
    assert spans["llm.complete"].attributes["gen_ai.request.model"] == "gpt-4o-mini"
    assert root.attributes["aetherlynx.answer"] == "answered"

    q = metric_points("aetherlynx.agent.queries")
    assert [(p.attributes["result"], p.value) for p in q] == [("answered", 1)]
    llm_pts = metric_points("aetherlynx.agent.llm.requests")
    assert [(p.attributes["result"], p.attributes["model"]) for p in llm_pts] == [("ok", "gpt-4o-mini")]
    assert metric_points("aetherlynx.agent.llm.duration")[0].count == 1


def test_llm_failure_is_an_error_span_and_counted():
    sqa.StudentQueryAgent(FakeUniversity(), RecordingLLM(fail=True)).answer("hola", "12345")
    llm = next(s for s in SPANS.get_finished_spans() if s.name == "llm.complete")
    assert llm.status.status_code == trace.StatusCode.ERROR
    assert [p.attributes["result"] for p in metric_points("aetherlynx.agent.llm.requests")] == ["error"]
    assert [p.attributes["result"] for p in metric_points("aetherlynx.agent.queries")] == ["unavailable"]


def test_no_personal_data_in_telemetry():
    sqa.StudentQueryAgent(TracingUniversity(STUDENT, FINANCE), RecordingLLM("respuesta con 12345")).answer(
        "Soy María, RUT 12.345.678-9", "12345"
    )
    text = dump_spans()
    for leaked in PII + ["Soy María", "respuesta con"]:
        assert leaked not in text, leaked


def test_upstream_span_uses_route_template_and_propagates_context(monkeypatch):
    cfg = sqa.Config(api_url="https://api.example.edu")
    client = sqa.UniversityClient(cfg)
    sent = {}

    class Resp:
        status_code = 200

        def raise_for_status(self):
            pass

        def json(self):
            return {"program": "x"}

    def fake_get(url, timeout, headers=None):
        sent.update(headers or {})
        return Resp()

    monkeypatch.setattr(client.s, "get", fake_get)
    client.student("12345")
    span = next(s for s in SPANS.get_finished_spans() if s.name == "upstream.students")
    assert span.attributes["url.template"] == "/students/{student_id}"
    assert "12345" not in dump_spans()
    assert sent["traceparent"].split("-")[1] == format(span.context.trace_id, "032x")
    pts = metric_points("aetherlynx.agent.upstream.requests")
    assert [(p.attributes["endpoint"], p.attributes["result"]) for p in pts] == [("students", "ok")]


def test_http_middleware_continues_incoming_trace(monkeypatch):
    from fastapi.testclient import TestClient

    import service

    token = "k3J9-xQ2v_Lp8ZmA4tR7wE1nC6bY0uH5sD"
    monkeypatch.setenv("AETHERLYNX_AGENT_TOKEN", token)
    service.state.agent = sqa.StudentQueryAgent(FakeUniversity(), RecordingLLM("ok"))
    incoming = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
    with TestClient(service.app) as c:
        r = c.post(
            "/query",
            json={"student_id": "12345", "question": "hola"},
            headers={"Authorization": f"Bearer {token}", "traceparent": incoming},
        )
    assert r.status_code == 200
    assert r.headers["X-Trace-Id"] == "0af7651916cd43dd8448eb211c80319c"
    servers = [s for s in SPANS.get_finished_spans() if s.kind == trace.SpanKind.SERVER]
    assert len(servers) == 1, "un solo span de servidor (sin duplicar el nativo de FastAPI)"
    server = servers[0]
    assert not [s for s in SPANS.get_finished_spans() if s.name.startswith("fastapi.")]
    assert server.name == "POST /query"
    assert format(server.context.trace_id, "032x") == "0af7651916cd43dd8448eb211c80319c"
    assert server.attributes["http.route"] == "/query"
    answer = next(s for s in SPANS.get_finished_spans() if s.name == "agent.answer")
    assert answer.parent.span_id == server.context.span_id
    http = metric_points("aetherlynx.http.requests")
    assert any(p.attributes["route"] == "/query" and p.attributes["status_code"] == "200" for p in http)


def test_observability_module_is_identical_in_both_services():
    other = ROOT.parent / "tools" / "pam_ipa_project" / "observability.py"
    assert other.read_text() == (ROOT / "observability.py").read_text(), "las copias de observability.py divergieron"


def test_setup_is_a_noop_without_endpoint(monkeypatch):
    monkeypatch.delenv("OTEL_EXPORTER_OTLP_ENDPOINT", raising=False)
    monkeypatch.setattr(observability, "_STATE", None)
    t = observability.setup("x", "0")
    assert not t.enabled
    monkeypatch.setattr(observability, "_STATE", None)


def test_setup_is_disabled_when_the_ca_file_is_missing(monkeypatch, tmp_path):
    monkeypatch.setattr(observability, "_STATE", None)
    monkeypatch.setenv("OTEL_EXPORTER_OTLP_ENDPOINT", "https://colector:4318")
    monkeypatch.setenv("OTEL_EXPORTER_OTLP_CERTIFICATE", str(tmp_path / "no-existe.crt"))
    assert observability.setup("x", "0").enabled is False
