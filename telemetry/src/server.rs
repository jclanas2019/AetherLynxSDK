//! Servidores HTTP del colector.
//!
//! - **Ingesta OTLP/HTTP** (`/v1/traces`, `/v1/metrics`, `/v1/logs`), con el
//!   token de ingesta. Solo protobuf (`application/x-protobuf`), con o sin gzip.
//! - **Panel y API de consulta** (`/`, `/api/...`), con el token de consulta.
//!   El panel en sí (HTML/JS/CSS) es público; los datos no.

use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use subtle::ConstantTimeEq;

use crate::alerts::{self, AlertConfig, AlertState};
use crate::otlp;
use crate::series::{self, Mode};
use crate::store::{Batch, LogFilter, SpanFilter, Store};

pub const MAX_BODY: usize = 8 * 1024 * 1024;
/// Límite tras descomprimir: protege contra "bombas" gzip.
pub const MAX_DECOMPRESSED: u64 = 32 * 1024 * 1024;

#[derive(Default)]
pub struct IngestStats {
    pub spans: AtomicU64,
    pub logs: AtomicU64,
    pub points: AtomicU64,
    pub rejected: AtomicU64,
    /// Lotes de servicios nuevos descartados por el tope de servicios.
    pub rejected_services: AtomicU64,
    /// Filas antiguas descartadas porque la base llegó a su tamaño máximo.
    pub shed_rows: AtomicU64,
}

pub struct AppState {
    pub store: Arc<Store>,
    pub alerts: Mutex<AlertState>,
    pub alert_config: AlertConfig,
    pub retention: Duration,
    pub ingest_token: Option<Vec<u8>>,
    pub query_token: Option<Vec<u8>>,
    pub stats: IngestStats,
    pub started_ns: i64,
}

pub fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn bearer_ok(headers: &HeaderMap, expected: &Option<Vec<u8>>) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| bool::from(t.as_bytes().ct_eq(expected)))
        .unwrap_or(false)
}

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({ "error": msg.into() }))).into_response()
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, rusqlite::Error> + Send + 'static,
) -> Result<T, Response> {
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => {
            tracing::error!(error = %e, "error de base de datos");
            Err(err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "error interno de almacenamiento",
            ))
        }
        Err(_) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, "error interno")),
    }
}

// --------------------------------------------------------------------------
// Ingesta OTLP
// --------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Signal {
    Traces,
    Metrics,
    Logs,
}

// El error ya es la respuesta HTTP final; encajarla no aporta nada.
#[allow(clippy::result_large_err)]
fn decode_body(headers: &HeaderMap, body: Bytes) -> Result<Vec<u8>, Response> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !ct.starts_with("application/x-protobuf") {
        return Err(err(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "solo se acepta OTLP protobuf (application/x-protobuf)",
        ));
    }
    match headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
    {
        None | Some("identity") => Ok(body.to_vec()),
        Some("gzip") => {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(&body[..])
                .take(MAX_DECOMPRESSED + 1)
                .read_to_end(&mut out)
                .map_err(|_| err(StatusCode::BAD_REQUEST, "gzip inválido"))?;
            if out.len() as u64 > MAX_DECOMPRESSED {
                return Err(err(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "el cuerpo descomprimido es demasiado grande",
                ));
            }
            Ok(out)
        }
        Some(other) => Err(err(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("codificación no soportada: {other}"),
        )),
    }
}

/// Lotes que se descomprimen y decodifican a la vez. Cada uno puede ocupar
/// hasta 32 MiB descomprimido: así la memoria queda acotada aunque lleguen
/// muchos a la vez (los demás esperan su turno).
static INGEST_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(8);

async fn ingest(state: Arc<AppState>, signal: Signal, headers: HeaderMap, body: Bytes) -> Response {
    if !bearer_ok(&headers, &state.ingest_token) {
        state.stats.rejected.fetch_add(1, Ordering::Relaxed);
        return err(StatusCode::UNAUTHORIZED, "token de ingesta inválido");
    }
    let Ok(_slot) = INGEST_SLOTS.acquire().await else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "colector apagándose");
    };
    let raw = match decode_body(&headers, body) {
        Ok(r) => r,
        Err(e) => {
            state.stats.rejected.fetch_add(1, Ordering::Relaxed);
            return e;
        }
    };
    let decoded: Result<Batch, _> = match signal {
        Signal::Traces => otlp::decode_traces(&raw),
        Signal::Metrics => otlp::decode_metrics(&raw),
        Signal::Logs => otlp::decode_logs(&raw),
    };
    let mut batch = match decoded {
        Ok(b) => b,
        Err(e) => {
            state.stats.rejected.fetch_add(1, Ordering::Relaxed);
            return err(StatusCode::BAD_REQUEST, e.to_string());
        }
    };
    let (ns, nl, nm) = (batch.spans.len(), batch.logs.len(), batch.metrics.len());
    let store = state.store.clone();
    let now = now_ns();
    batch.clamp_future(now);
    let outcome = match blocking(move || store.insert(&batch, now)).await {
        Ok(o) => o,
        Err(e) => return e,
    };
    if outcome.rejected_services > 0 {
        state
            .stats
            .rejected_services
            .fetch_add(outcome.rejected_services as u64, Ordering::Relaxed);
        tracing::warn!(
            services = outcome.rejected_services,
            "se descartaron datos de servicios nuevos: se alcanzó el máximo de servicios"
        );
    }
    state
        .stats
        .shed_rows
        .fetch_add(outcome.shed_rows as u64, Ordering::Relaxed);
    state.stats.spans.fetch_add(ns as u64, Ordering::Relaxed);
    state.stats.logs.fetch_add(nl as u64, Ordering::Relaxed);
    state.stats.points.fetch_add(nm as u64, Ordering::Relaxed);
    // Respuesta OTLP vacía (Export*ServiceResponse sin partial_success).
    (
        [(header::CONTENT_TYPE, "application/x-protobuf")],
        Vec::<u8>::new(),
    )
        .into_response()
}

pub fn otlp_router(state: Arc<AppState>) -> Router {
    let s1 = state.clone();
    let s2 = state.clone();
    let s3 = state;
    Router::new()
        .route(
            "/v1/traces",
            post(move |h: HeaderMap, b: Bytes| ingest(s1, Signal::Traces, h, b)),
        )
        .route(
            "/v1/metrics",
            post(move |h: HeaderMap, b: Bytes| ingest(s2, Signal::Metrics, h, b)),
        )
        .route(
            "/v1/logs",
            post(move |h: HeaderMap, b: Bytes| ingest(s3, Signal::Logs, h, b)),
        )
        .route("/healthz", get(|| async { "ok" }))
        .layer(DefaultBodyLimit::max(MAX_BODY))
}

// --------------------------------------------------------------------------
// Panel y API de consulta
// --------------------------------------------------------------------------

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const APP_CSS: &str = include_str!("../web/app.css");

/// Política estricta: nada se carga de fuera, nada se ejecuta en línea.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
                   connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

async fn security_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

async fn require_query_token(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    if !bearer_ok(req.headers(), &state.query_token) {
        return err(StatusCode::UNAUTHORIZED, "token de consulta inválido");
    }
    next.run(req).await
}

#[derive(Deserialize, Default)]
struct Window {
    since: Option<String>,
}

impl Window {
    /// Inicio de la ventana pedida (por defecto 15 min), acotada a la retención.
    // El error ya es la respuesta HTTP final; encajarla no aporta nada.
    #[allow(clippy::result_large_err)]
    fn start_ns(&self, state: &AppState) -> Result<i64, Response> {
        let d = match self.since.as_deref() {
            None | Some("") => Duration::from_secs(15 * 60),
            Some(s) => humantime::parse_duration(s)
                .map_err(|_| err(StatusCode::BAD_REQUEST, "since inválido (p. ej. 15m, 1h)"))?,
        };
        Ok(now_ns() - d.min(state.retention).as_nanos() as i64)
    }
}

async fn status(State(state): State<Arc<AppState>>) -> Response {
    let store = state.store.clone();
    let counts = match blocking(move || store.counts()).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    let s = &state.stats;
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_s": (now_ns() - state.started_ns) / 1_000_000_000,
        "retention_s": state.retention.as_secs(),
        "stored": {"spans": counts.0, "logs": counts.1, "metric_points": counts.2},
        "ingested": {
            "spans": s.spans.load(Ordering::Relaxed),
            "logs": s.logs.load(Ordering::Relaxed),
            "metric_points": s.points.load(Ordering::Relaxed),
            "rejected_requests": s.rejected.load(Ordering::Relaxed),
            "rejected_services": s.rejected_services.load(Ordering::Relaxed),
            "shed_rows": s.shed_rows.load(Ordering::Relaxed),
        },
    }))
    .into_response()
}

async fn services(State(state): State<Arc<AppState>>, Query(w): Query<Window>) -> Response {
    let since = match w.start_ns(&state) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let store = state.store.clone();
    let res = blocking(move || {
        let mut out = Vec::new();
        for s in store.services()? {
            let st = store.span_stats(&s.service, since)?;
            out.push(json!({
                "service": s.service, "first_seen_ns": s.first_seen_ns, "last_seen_ns": s.last_seen_ns,
                "requests": st.spans, "errors": st.errors, "p50_ms": st.p50_ms, "p95_ms": st.p95_ms,
            }));
        }
        Ok(out)
    })
    .await;
    match res {
        Ok(v) => Json(json!({ "services": v })).into_response(),
        Err(e) => e,
    }
}

async fn alerts_api(State(state): State<Arc<AppState>>) -> Response {
    let a = state.alerts.lock().unwrap_or_else(|p| p.into_inner());
    Json(json!({ "active": a.active(), "history": a.history() })).into_response()
}

#[derive(Deserialize, Default)]
struct TraceQuery {
    since: Option<String>,
    service: Option<String>,
    name: Option<String>,
    errors: Option<bool>,
    limit: Option<u32>,
}

async fn traces(State(state): State<Arc<AppState>>, Query(q): Query<TraceQuery>) -> Response {
    let since = match (Window {
        since: q.since.clone(),
    })
    .start_ns(&state)
    {
        Ok(s) => s,
        Err(e) => return e,
    };
    // Cada fila del listado es una operación: el span de entrada a un
    // servicio (la raíz, o el que continúa una traza que llega de fuera).
    let f = SpanFilter {
        entry_only: true,
        service: q.service.filter(|s| !s.is_empty()),
        name: q.name.filter(|s| !s.is_empty()),
        errors_only: q.errors.unwrap_or(false),
        since_ns: since,
        limit: q.limit.unwrap_or(100),
    };
    let store = state.store.clone();
    match blocking(move || store.spans(&f)).await {
        Ok(spans) => Json(json!({ "traces": spans })).into_response(),
        Err(e) => e,
    }
}

async fn trace(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    if id.len() != 32 || !id.bytes().all(|c| c.is_ascii_hexdigit()) {
        return err(
            StatusCode::BAD_REQUEST,
            "trace_id debe tener 32 caracteres hexadecimales",
        );
    }
    let id = id.to_ascii_lowercase();
    let store = state.store.clone();
    let res = blocking(move || {
        let spans = store.trace(&id)?;
        let logs = store.logs(&LogFilter {
            trace_id: Some(id),
            since_ns: 0,
            limit: 500,
            ..Default::default()
        })?;
        Ok((spans, logs))
    })
    .await;
    match res {
        Ok((spans, _)) if spans.is_empty() => err(
            StatusCode::NOT_FOUND,
            "traza no encontrada (¿venció la retención?)",
        ),
        Ok((spans, logs)) => Json(json!({ "spans": spans, "logs": logs })).into_response(),
        Err(e) => e,
    }
}

#[derive(Deserialize, Default)]
struct LogQuery {
    since: Option<String>,
    service: Option<String>,
    severity: Option<i32>,
    trace_id: Option<String>,
    q: Option<String>,
    limit: Option<u32>,
}

async fn logs(State(state): State<Arc<AppState>>, Query(q): Query<LogQuery>) -> Response {
    let since = match (Window {
        since: q.since.clone(),
    })
    .start_ns(&state)
    {
        Ok(s) => s,
        Err(e) => return e,
    };
    let f = LogFilter {
        service: q.service.filter(|s| !s.is_empty()),
        min_severity: q.severity.unwrap_or(0),
        trace_id: q
            .trace_id
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase()),
        contains: q.q.filter(|s| !s.is_empty()),
        since_ns: since,
        limit: q.limit.unwrap_or(200),
    };
    let store = state.store.clone();
    match blocking(move || store.logs(&f)).await {
        Ok(v) => Json(json!({ "logs": v })).into_response(),
        Err(e) => e,
    }
}

async fn metric_names(State(state): State<Arc<AppState>>, Query(w): Query<Window>) -> Response {
    let since = match w.start_ns(&state) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let store = state.store.clone();
    match blocking(move || store.metric_names(since)).await {
        Ok(v) => Json(json!({ "metrics": v })).into_response(),
        Err(e) => e,
    }
}

#[derive(Deserialize, Default)]
struct SeriesQuery {
    name: String,
    service: Option<String>,
    mode: Option<String>,
    since: Option<String>,
}

async fn metric_series(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SeriesQuery>,
) -> Response {
    let since = match (Window {
        since: q.since.clone(),
    })
    .start_ns(&state)
    {
        Ok(s) => s,
        Err(e) => return e,
    };
    let Some(mode) = Mode::parse(q.mode.as_deref().unwrap_or("")) else {
        return err(
            StatusCode::BAD_REQUEST,
            "mode: auto, value, rate, mean, p50, p90, p95 o p99",
        );
    };
    let store = state.store.clone();
    let (name, service) = (q.name.clone(), q.service.filter(|s| !s.is_empty()));
    match blocking(move || store.metric_points(&name, service.as_deref(), since)).await {
        Ok(points) => {
            let unit = points.first().map(|p| p.unit.clone()).unwrap_or_default();
            let kind = points.first().map(|p| p.kind);
            Json(json!({ "name": q.name, "unit": unit, "kind": kind, "series": series::build(points, mode, 300) }))
                .into_response()
        }
        Err(e) => e,
    }
}

pub fn ui_router(state: Arc<AppState>) -> Router {
    let api = Router::new()
        .route("/api/status", get(status))
        .route("/api/services", get(services))
        .route("/api/alerts", get(alerts_api))
        .route("/api/traces", get(traces))
        .route("/api/traces/{id}", get(trace))
        .route("/api/logs", get(logs))
        .route("/api/metrics", get(metric_names))
        .route("/api/metrics/series", get(metric_series))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_query_token,
        ))
        .with_state(state.clone());
    let store = state.store.clone();
    Router::new()
        .route(
            "/",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    INDEX_HTML,
                )
            }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    APP_JS,
                )
            }),
        )
        .route(
            "/app.css",
            get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS) }),
        )
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/readyz",
            get(move || {
                let store = store.clone();
                async move {
                    match tokio::task::spawn_blocking(move || store.counts()).await {
                        Ok(Ok(_)) => (StatusCode::OK, "ready"),
                        _ => (
                            StatusCode::SERVICE_UNAVAILABLE,
                            "almacenamiento no disponible",
                        ),
                    }
                }
            }),
        )
        .merge(api)
        .layer(middleware::from_fn(security_headers))
}

/// Evalúa alertas periódicamente y registra los cambios en el log.
pub async fn alert_loop(
    state: Arc<AppState>,
    every: Duration,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(every);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = stop.changed() => break,
        }
        let (store, cfg) = (state.store.clone(), state.alert_config.clone());
        let now = now_ns();
        let Ok(Ok(current)) =
            tokio::task::spawn_blocking(move || alerts::evaluate(&store, &cfg, now)).await
        else {
            tracing::warn!("no se pudieron evaluar las alertas");
            continue;
        };
        let events = state
            .alerts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .update(current, now);
        for e in events {
            if e.state == "firing" {
                tracing::warn!(alert = %e.rule, service = %e.service, severity = ?e.severity, summary = %e.summary, "alerta activa");
            } else {
                tracing::info!(alert = %e.rule, service = %e.service, "alerta resuelta");
            }
        }
    }
}

/// Borra lo que venció la retención.
pub async fn retention_loop(
    state: Arc<AppState>,
    every: Duration,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(every);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = stop.changed() => break,
        }
        let store = state.store.clone();
        let cutoff = now_ns() - state.retention.as_nanos() as i64;
        match tokio::task::spawn_blocking(move || store.prune(cutoff)).await {
            Ok(Ok(n)) if n > 0 => tracing::info!(rows = n, "datos vencidos eliminados"),
            Ok(Err(e)) => tracing::warn!(error = %e, "falló la limpieza por retención"),
            _ => {}
        }
    }
}
