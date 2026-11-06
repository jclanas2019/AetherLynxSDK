"""OpenTelemetry para los servicios Python de AetherLynx.

Este archivo se mantiene idéntico en ``agents/`` y en
``tools/pam_ipa_project/`` (un test lo comprueba), para que cada imagen de
contenedor sea autónoma.

Configuración con las variables estándar de OpenTelemetry:

* ``OTEL_EXPORTER_OTLP_ENDPOINT``: URL base del colector (p. ej.
  ``https://aetherlynx-telemetry:4318``). Sin ella no se exporta nada.
* ``OTEL_EXPORTER_OTLP_HEADERS``: ``Authorization=Bearer%20<token>``.
* ``OTEL_EXPORTER_OTLP_CERTIFICATE``: CA en PEM para verificar el colector.
* ``OTEL_SERVICE_NAME`` y ``OTEL_RESOURCE_ATTRIBUTES``.
* ``OTEL_METRIC_EXPORT_INTERVAL``: milisegundos (por defecto 15000).

Privacidad: los spans HTTP usan la plantilla de la ruta (``/users/{username}``),
nunca la URL real, y no registran cuerpos, cabeceras ni parámetros de consulta.
"""

from __future__ import annotations

import logging
import os
import time
from dataclasses import dataclass, field
from typing import Any

from opentelemetry import metrics, propagate, trace
from opentelemetry.trace import SpanKind, Status, StatusCode

log = logging.getLogger("aetherlynx.observability")

# Loggers cuyos mensajes no se envían al colector: evita bucles de
# retroalimentación (el exportador registrando sus propios envíos).
_QUIET_LOGGERS = ("opentelemetry", "urllib3", "requests", "httpx", "httpcore")


@dataclass
class Telemetry:
    enabled: bool = False
    providers: list[Any] = field(default_factory=list)
    handler: logging.Handler | None = None

    def shutdown(self) -> None:
        if self.handler is not None:
            logging.getLogger().removeHandler(self.handler)
        for p in self.providers:
            try:
                p.shutdown()
            except Exception:  # noqa: BLE001 - apagar nunca debe tumbar el proceso
                log.debug("error al apagar un proveedor de telemetría", exc_info=True)
        self.providers.clear()


_STATE: Telemetry | None = None


class _QuietFilter(logging.Filter):
    def filter(self, record: logging.LogRecord) -> bool:
        return not record.name.startswith(_QUIET_LOGGERS)


def setup(default_service: str, version: str) -> Telemetry:
    """Configura los proveedores globales. Idempotente: llamadas posteriores
    devuelven la misma configuración."""
    global _STATE
    if _STATE is not None:
        return _STATE
    if not os.environ.get("OTEL_EXPORTER_OTLP_ENDPOINT", "").strip():
        _STATE = Telemetry(enabled=False)
        return _STATE
    ca = os.environ.get("OTEL_EXPORTER_OTLP_CERTIFICATE", "").strip()
    if ca and not os.path.isfile(ca):
        # Sin la CA cada exportación fallaría y llenaría el log: mejor avisar
        # una vez y seguir sin telemetría. El servicio no depende de ella.
        log.warning("OTEL_EXPORTER_OTLP_CERTIFICATE (%s) no existe: telemetría desactivada", ca)
        _STATE = Telemetry(enabled=False)
        return _STATE

    from opentelemetry._logs import set_logger_provider
    from opentelemetry.exporter.otlp.proto.http._log_exporter import OTLPLogExporter
    from opentelemetry.exporter.otlp.proto.http.metric_exporter import OTLPMetricExporter
    from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter
    from opentelemetry.sdk._logs import LoggerProvider, LoggingHandler
    from opentelemetry.sdk._logs.export import BatchLogRecordProcessor
    from opentelemetry.sdk.metrics import MeterProvider
    from opentelemetry.sdk.metrics.export import PeriodicExportingMetricReader
    from opentelemetry.sdk.resources import Resource
    from opentelemetry.sdk.trace import TracerProvider
    from opentelemetry.sdk.trace.export import BatchSpanProcessor

    attrs = {"service.name": default_service, "service.version": version}
    agent = os.environ.get("AETHERLYNX_AGENT_NAME", "").strip()
    if agent:
        attrs["aetherlynx.agent"] = agent
    # OTEL_SERVICE_NAME y OTEL_RESOURCE_ATTRIBUTES, si existen, tienen prioridad.
    resource = Resource.create(attrs)

    tracer_provider = TracerProvider(resource=resource)
    tracer_provider.add_span_processor(BatchSpanProcessor(OTLPSpanExporter()))
    trace.set_tracer_provider(tracer_provider)

    interval = int(os.environ.get("OTEL_METRIC_EXPORT_INTERVAL", "15000") or 15000)
    reader = PeriodicExportingMetricReader(OTLPMetricExporter(), export_interval_millis=max(1000, interval))
    meter_provider = MeterProvider(resource=resource, metric_readers=[reader])
    metrics.set_meter_provider(meter_provider)

    logger_provider = LoggerProvider(resource=resource)
    logger_provider.add_log_record_processor(BatchLogRecordProcessor(OTLPLogExporter()))
    set_logger_provider(logger_provider)
    handler = LoggingHandler(level=logging.INFO, logger_provider=logger_provider)
    handler.addFilter(_QuietFilter())
    logging.getLogger().addHandler(handler)
    if logging.getLogger().level > logging.INFO or logging.getLogger().level == logging.NOTSET:
        logging.getLogger().setLevel(logging.INFO)

    _STATE = Telemetry(enabled=True, providers=[tracer_provider, meter_provider, logger_provider], handler=handler)
    log.info("exportando telemetría por OTLP a %s", os.environ["OTEL_EXPORTER_OTLP_ENDPOINT"])
    return _STATE


def inject(headers: dict[str, str]) -> dict[str, str]:
    """Añade ``traceparent`` a cabeceras salientes para continuar la traza."""
    propagate.inject(headers)
    return headers


def current_trace_id() -> str | None:
    ctx = trace.get_current_span().get_span_context()
    return format(ctx.trace_id, "032x") if ctx.is_valid else None


# Límites de cubetas en segundos, de 5 ms a 60 s.
LATENCY_BUCKETS = (0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0)


def latency_histogram(meter: metrics.Meter, name: str, description: str):
    """Histograma en segundos con cubetas adecuadas para latencias de red."""
    try:
        return meter.create_histogram(name, unit="s", description=description, explicit_bucket_boundaries_advisory=LATENCY_BUCKETS)
    except TypeError:  # versiones del API sin el parámetro de cubetas
        return meter.create_histogram(name, unit="s", description=description)


def fastapi_kwargs() -> dict[str, Any]:
    """Argumentos para ``FastAPI(...)`` que desactivan su telemetría nativa.

    FastAPI 0.16x+ trae instrumentación propia. Se desactiva para que este
    middleware sea la única fuente en cualquier versión (sin spans de
    servidor duplicados) y, sobre todo, porque sus logs de errores de
    validación incluyen el valor recibido, que puede ser una contraseña.
    """
    import inspect

    from fastapi import FastAPI

    if "telemetry" in inspect.signature(FastAPI.__init__).parameters:
        return {"telemetry": {"tracing": False, "metrics": False, "logs": False, "operation_spans": False}}
    return {}


def instrument_fastapi(app, scope_name: str) -> None:
    """Middleware: un span de servidor por petición (continuando la traza
    entrante), métricas HTTP y la cabecera ``X-Trace-Id`` en la respuesta."""
    tracer = trace.get_tracer(scope_name)
    meter = metrics.get_meter(scope_name)
    requests_counter = meter.create_counter("aetherlynx.http.requests", description="Peticiones HTTP por ruta y código")
    duration = latency_histogram(meter, "aetherlynx.http.duration", "Duración de las peticiones HTTP")

    @app.middleware("http")
    async def _otel_middleware(request, call_next):
        method = request.method
        ctx = propagate.extract(dict(request.headers))
        start = time.perf_counter()
        with tracer.start_as_current_span(method, context=ctx, kind=SpanKind.SERVER) as span:
            span.set_attribute("http.request.method", method)
            status_code = 500
            try:
                response = await call_next(request)
                status_code = response.status_code
                return_value = response
            except Exception as exc:
                span.record_exception(exc)
                span.set_status(Status(StatusCode.ERROR, type(exc).__name__))
                raise
            finally:
                route = getattr(request.scope.get("route"), "path", None) or "sin ruta"
                span.update_name(f"{method} {route}")
                span.set_attribute("http.route", route)
                span.set_attribute("http.response.status_code", status_code)
                if status_code >= 500:
                    span.set_status(Status(StatusCode.ERROR, f"HTTP {status_code}"))
                labels = {"method": method, "route": route, "status_code": str(status_code)}
                requests_counter.add(1, labels)
                duration.record(time.perf_counter() - start, {"method": method, "route": route})
            trace_id = format(span.get_span_context().trace_id, "032x")
            if span.get_span_context().is_valid:
                return_value.headers["X-Trace-Id"] = trace_id
            return return_value
