//! OpenTelemetry del orquestador: trazas, métricas y logs por OTLP/HTTP.
//!
//! Se configura con las variables estándar de OpenTelemetry:
//!
//! - `OTEL_EXPORTER_OTLP_ENDPOINT`: URL base del colector (p. ej.
//!   `https://aetherlynx-telemetry:4318`). Sin ella, no se exporta nada y
//!   solo se escriben logs locales.
//! - `OTEL_EXPORTER_OTLP_HEADERS`: cabeceras `clave=valor` separadas por
//!   comas y codificadas como URL, p. ej. `Authorization=Bearer%20<token>`.
//! - `OTEL_EXPORTER_OTLP_CERTIFICATE`: CA en PEM para verificar el colector.
//! - `OTEL_SERVICE_NAME`, `OTEL_RESOURCE_ATTRIBUTES`: identidad del recurso.
//! - `OTEL_METRIC_EXPORT_INTERVAL`: milisegundos entre exportaciones de
//!   métricas (por defecto 15000).

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use opentelemetry::metrics::{Counter, Histogram, Meter};
use opentelemetry::propagation::{Extractor, Injector};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::{global, KeyValue};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{
    LogExporter, MetricExporter, SpanExporter, WithExportConfig, WithHttpConfig,
};
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

pub const SERVICE_NAME: &str = "aetherlynx-orchestrator";

/// Proveedores activos; al soltarlo (o con [`Telemetry::shutdown`]) se
/// vacían los búferes pendientes hacia el colector.
#[derive(Default)]
pub struct Telemetry {
    tracer: Option<SdkTracerProvider>,
    meter: Option<SdkMeterProvider>,
    logger: Option<SdkLoggerProvider>,
    pub endpoint: Option<String>,
}

impl Telemetry {
    pub fn enabled(&self) -> bool {
        self.endpoint.is_some()
    }

    pub fn shutdown(&mut self) {
        if let Some(t) = self.tracer.take() {
            let _ = t.shutdown();
        }
        if let Some(m) = self.meter.take() {
            let _ = m.shutdown();
        }
        if let Some(l) = self.logger.take() {
            let _ = l.shutdown();
        }
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // Se trabaja sobre bytes: cortar el &str podría caer en mitad de un
        // carácter multibyte y entrar en pánico.
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Interpreta `OTEL_EXPORTER_OTLP_HEADERS` (`k=v,k2=v2`, valores codificados como URL).
pub fn parse_headers(raw: &str) -> HashMap<String, String> {
    raw.split(',')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            let k = percent_decode(k.trim());
            (!k.is_empty()).then(|| (k, percent_decode(v.trim())))
        })
        .collect()
}

fn http_client(ca: Option<&str>) -> Result<reqwest::blocking::Client, String> {
    let ca = match ca {
        Some(path) => Some(
            std::fs::read(path)
                .map_err(|e| {
                    format!("no se pudo leer OTEL_EXPORTER_OTLP_CERTIFICATE ({path}): {e}")
                })
                .and_then(|pem| {
                    reqwest::Certificate::from_pem(&pem).map_err(|e| format!("CA inválida: {e}"))
                })?,
        ),
        None => None,
    };
    // El cliente bloqueante de reqwest crea su propio runtime y no puede
    // construirse dentro de uno async: se construye en un hilo aparte.
    std::thread::spawn(move || {
        let mut b = reqwest::blocking::Client::builder().timeout(Duration::from_secs(10));
        if let Some(ca) = ca {
            b = b.add_root_certificate(ca);
        }
        b.build().map_err(|e| e.to_string())
    })
    .join()
    .map_err(|_| "no se pudo crear el cliente HTTP de telemetría".to_string())?
}

fn resource(default_name: &str) -> Resource {
    let b = Resource::builder();
    let b = if std::env::var("OTEL_SERVICE_NAME")
        .map(|v| v.trim().is_empty())
        .unwrap_or(true)
    {
        b.with_service_name(default_name.to_owned())
    } else {
        b
    };
    b.with_attribute(KeyValue::new("service.version", crate::VERSION))
        .build()
}

/// Inicializa logs locales y, si hay endpoint OTLP, la exportación a él.
pub fn init(json_logs: bool) -> Result<Telemetry, String> {
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt_layer = if json_logs {
        tracing_subscriber::fmt::layer().json().boxed()
    } else {
        tracing_subscriber::fmt::layer().boxed()
    };

    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .ok()
        .map(|e| e.trim().trim_end_matches('/').to_owned())
        .filter(|e| !e.is_empty());
    // Una CA configurada pero ausente (p. ej. el ConfigMap opcional aún no
    // existe) desactiva la exportación en vez de impedir el arranque: el
    // orquestador no depende de su telemetría.
    let ca = std::env::var("OTEL_EXPORTER_OTLP_CERTIFICATE")
        .ok()
        .filter(|p| !p.trim().is_empty());
    let missing_ca = ca
        .as_deref()
        .filter(|p| endpoint.is_some() && !std::path::Path::new(p).is_file())
        .map(str::to_owned);
    let Some(base) = endpoint.clone().filter(|_| missing_ca.is_none()) else {
        tracing_subscriber::registry()
            .with(fmt_layer.with_filter(filter()))
            .init();
        if let Some(p) = missing_ca {
            tracing::warn!(path = %p, "OTEL_EXPORTER_OTLP_CERTIFICATE no existe: telemetría desactivada");
        }
        return Ok(Telemetry::default());
    };
    if !base.starts_with("https://") && !base.starts_with("http://") {
        return Err("OTEL_EXPORTER_OTLP_ENDPOINT debe empezar con http:// o https://".into());
    }

    let headers = parse_headers(&std::env::var("OTEL_EXPORTER_OTLP_HEADERS").unwrap_or_default());
    let client = http_client(ca.as_deref())?;
    let res = resource(SERVICE_NAME);

    let spans = SpanExporter::builder()
        .with_http()
        .with_http_client(client.clone())
        .with_endpoint(format!("{base}/v1/traces"))
        .with_headers(headers.clone())
        .build()
        .map_err(|e| format!("exportador de trazas: {e}"))?;
    let tracer = SdkTracerProvider::builder()
        .with_batch_exporter(spans)
        .with_resource(res.clone())
        .build();

    let metrics = MetricExporter::builder()
        .with_http()
        .with_http_client(client.clone())
        .with_endpoint(format!("{base}/v1/metrics"))
        .with_headers(headers.clone())
        .build()
        .map_err(|e| format!("exportador de métricas: {e}"))?;
    let every = std::env::var("OTEL_METRIC_EXPORT_INTERVAL")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(15_000)
        .clamp(1_000, 600_000);
    let reader = PeriodicReader::builder(metrics)
        .with_interval(Duration::from_millis(every))
        .build();
    let meter = SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(res.clone())
        .build();

    let logs = LogExporter::builder()
        .with_http()
        .with_http_client(client)
        .with_endpoint(format!("{base}/v1/logs"))
        .with_headers(headers)
        .build()
        .map_err(|e| format!("exportador de logs: {e}"))?;
    let logger = SdkLoggerProvider::builder()
        .with_batch_exporter(logs)
        .with_resource(res)
        .build();

    global::set_text_map_propagator(TraceContextPropagator::new());
    global::set_tracer_provider(tracer.clone());
    global::set_meter_provider(meter.clone());

    // Los logs del propio exportador y de las librerías HTTP no se envían
    // al colector: evita bucles de retroalimentación.
    let quiet = || {
        EnvFilter::new("info")
            .add_directive("opentelemetry=off".parse().expect("directiva válida"))
            .add_directive("opentelemetry_sdk=off".parse().expect("directiva válida"))
            .add_directive("hyper=off".parse().expect("directiva válida"))
            .add_directive("reqwest=off".parse().expect("directiva válida"))
            .add_directive("h2=off".parse().expect("directiva válida"))
            .add_directive("tonic=off".parse().expect("directiva válida"))
            .add_directive("kube_client=warn".parse().expect("directiva válida"))
    };
    tracing_subscriber::registry()
        .with(fmt_layer.with_filter(filter()))
        .with(
            tracing_opentelemetry::layer()
                // Sin busy_ns/idle_ns ni datos del hilo: ruido que ocupa almacenamiento.
                .with_tracked_inactivity(false)
                .with_threads(false)
                .with_tracer(tracer.tracer(SERVICE_NAME))
                .with_filter(quiet()),
        )
        .with(OpenTelemetryTracingBridge::new(&logger).with_filter(quiet()))
        .init();

    Ok(Telemetry {
        tracer: Some(tracer),
        meter: Some(meter),
        logger: Some(logger),
        endpoint,
    })
}

// ---------------------------------------------------------------- métricas

pub mod names {
    pub const RECONCILE_CYCLES: &str = "aetherlynx.reconcile.cycles";
    pub const RECONCILE_DURATION: &str = "aetherlynx.reconcile.duration";
    pub const RECONCILE_LAST_SUCCESS: &str = "aetherlynx.reconcile.last_success";
    pub const AGENTS: &str = "aetherlynx.agents";
    pub const REPLICAS_DESIRED: &str = "aetherlynx.replicas.desired";
    pub const REPLICAS_READY: &str = "aetherlynx.replicas.ready";
    pub const RUNTIME_OPERATIONS: &str = "aetherlynx.runtime.operations";
    pub const RUNTIME_DURATION: &str = "aetherlynx.runtime.duration";
    pub const GRPC_REQUESTS: &str = "aetherlynx.grpc.requests";
    pub const GRPC_DURATION: &str = "aetherlynx.grpc.duration";
    pub const AUTH_REJECTED: &str = "aetherlynx.grpc.auth_rejected";
}

/// Límites de cubetas en segundos, de 1 ms a 30 s.
const LATENCY_BUCKETS: [f64; 13] = [
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 10.0, 30.0,
];

pub struct Metrics {
    pub reconcile_cycles: Counter<u64>,
    pub reconcile_duration: Histogram<f64>,
    pub runtime_operations: Counter<u64>,
    pub runtime_duration: Histogram<f64>,
    pub grpc_requests: Counter<u64>,
    pub grpc_duration: Histogram<f64>,
    pub auth_rejected: Counter<u64>,
}

pub fn meter() -> Meter {
    global::meter(SERVICE_NAME)
}

/// Instrumentos del orquestador. Sin proveedor configurado son no-op.
pub fn metrics() -> &'static Metrics {
    static M: OnceLock<Metrics> = OnceLock::new();
    M.get_or_init(|| {
        let m = meter();
        let hist = |name: &'static str, desc: &'static str| {
            m.f64_histogram(name)
                .with_unit("s")
                .with_description(desc)
                .with_boundaries(LATENCY_BUCKETS.to_vec())
                .build()
        };
        Metrics {
            reconcile_cycles: m
                .u64_counter(names::RECONCILE_CYCLES)
                .with_description("Ciclos de reconciliación completados")
                .build(),
            reconcile_duration: hist(
                names::RECONCILE_DURATION,
                "Duración de un ciclo de reconciliación",
            ),
            runtime_operations: m
                .u64_counter(names::RUNTIME_OPERATIONS)
                .with_description("Llamadas al runtime por operación y resultado")
                .build(),
            runtime_duration: hist(
                names::RUNTIME_DURATION,
                "Duración de las llamadas al runtime",
            ),
            grpc_requests: m
                .u64_counter(names::GRPC_REQUESTS)
                .with_description("Peticiones gRPC por método y código")
                .build(),
            grpc_duration: hist(names::GRPC_DURATION, "Duración de las peticiones gRPC"),
            auth_rejected: m
                .u64_counter(names::AUTH_REJECTED)
                .with_description("Peticiones gRPC rechazadas por token")
                .build(),
        }
    })
}

// ------------------------------------------------------------- propagación

/// Lee `traceparent`/`tracestate` de los metadatos gRPC.
pub struct MetadataExtractor<'a>(pub &'a tonic::metadata::MetadataMap);

impl Extractor for MetadataExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }
    fn keys(&self) -> Vec<&str> {
        self.0
            .keys()
            .filter_map(|k| match k {
                tonic::metadata::KeyRef::Ascii(k) => Some(k.as_str()),
                tonic::metadata::KeyRef::Binary(_) => None,
            })
            .collect()
    }
}

/// Escribe el contexto de traza en cabeceras HTTP (para llamadas salientes).
pub struct HeaderInjector<'a>(pub &'a mut reqwest::header::HeaderMap);

impl Injector for HeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let (Ok(k), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(key.as_bytes()),
            reqwest::header::HeaderValue::from_str(&value),
        ) {
            self.0.insert(k, v);
        }
    }
}

/// Contexto OpenTelemetry del padre remoto de una petición gRPC.
pub fn remote_context(meta: &tonic::metadata::MetadataMap) -> opentelemetry::Context {
    global::get_text_map_propagator(|p| p.extract(&MetadataExtractor(meta)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_are_parsed_and_decoded() {
        let h = parse_headers("Authorization=Bearer%20abc%3D, x-tenant = uni ,bad,=empty");
        assert_eq!(
            h.get("Authorization").map(String::as_str),
            Some("Bearer abc=")
        );
        assert_eq!(h.get("x-tenant").map(String::as_str), Some("uni"));
        assert_eq!(h.len(), 2);
        // Un multibyte tras '%' no provoca pánico: se deja tal cual.
        assert_eq!(
            parse_headers("k=%ñx%2").get("k").map(String::as_str),
            Some("%ñx%2")
        );
    }

    #[test]
    fn traceparent_is_extracted_from_metadata() {
        use opentelemetry::trace::TraceContextExt;
        global::set_text_map_propagator(TraceContextPropagator::new());
        let mut md = tonic::metadata::MetadataMap::new();
        md.insert(
            "traceparent",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
                .parse()
                .unwrap(),
        );
        let cx = remote_context(&md);
        let sc = cx.span().span_context().clone();
        assert_eq!(
            sc.trace_id().to_string(),
            "0af7651916cd43dd8448eb211c80319c"
        );
        assert!(sc.is_remote());
    }
}
