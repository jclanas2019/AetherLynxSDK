//! Tests de la capa HTTP del colector: ingesta OTLP y API de consulta.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use aetherlynx_telemetry::alerts::{AlertConfig, AlertState};
use aetherlynx_telemetry::otlp::fixtures::{self, M};
use aetherlynx_telemetry::server::{self, now_ns, AppState, IngestStats};
use aetherlynx_telemetry::store::Store;

const INGEST: &str = "ingest-token-0123456789abcdef0123456789";
const QUERY: &str = "query-token-0123456789abcdef01234567890";

fn state() -> Arc<AppState> {
    Arc::new(AppState {
        store: Arc::new(Store::open(":memory:").unwrap()),
        alerts: Mutex::new(AlertState::default()),
        alert_config: AlertConfig::default(),
        retention: Duration::from_secs(24 * 3600),
        ingest_token: Some(INGEST.as_bytes().to_vec()),
        query_token: Some(QUERY.as_bytes().to_vec()),
        stats: IngestStats::default(),
        started_ns: now_ns(),
    })
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let res = app.clone().oneshot(req).await.unwrap();
    let (parts, body) = res.into_parts();
    (
        parts.status,
        parts.headers,
        body.collect().await.unwrap().to_bytes().to_vec(),
    )
}

fn post(path: &str, token: Option<&str>, ct: &str, body: Vec<u8>) -> Request<Body> {
    let mut b = Request::post(path).header("content-type", ct);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::from(body)).unwrap()
}

fn get(path: &str, token: Option<&str>) -> Request<Body> {
    let mut b = Request::get(path);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::empty()).unwrap()
}

async fn json(app: &Router, path: &str) -> Value {
    let (code, _, body) = send(app, get(path, Some(QUERY))).await;
    assert_eq!(
        code,
        StatusCode::OK,
        "{path}: {}",
        String::from_utf8_lossy(&body)
    );
    serde_json::from_slice(&body).unwrap()
}

fn now_ms() -> u64 {
    (now_ns() / 1_000_000) as u64
}

#[tokio::test]
async fn ingest_requires_token_and_protobuf() {
    let st = state();
    let otlp = server::otlp_router(st.clone());
    let body = fixtures::traces(
        "svc",
        vec![fixtures::span(1, 1, 0, "x", now_ms(), 1, false)],
    );

    let (c, _, _) = send(
        &otlp,
        post("/v1/traces", None, "application/x-protobuf", body.clone()),
    )
    .await;
    assert_eq!(c, StatusCode::UNAUTHORIZED);
    let (c, _, _) = send(
        &otlp,
        post(
            "/v1/traces",
            Some(QUERY),
            "application/x-protobuf",
            body.clone(),
        ),
    )
    .await;
    assert_eq!(
        c,
        StatusCode::UNAUTHORIZED,
        "el token de consulta no sirve para ingerir"
    );
    let (c, _, _) = send(
        &otlp,
        post(
            "/v1/traces",
            Some(INGEST),
            "application/json",
            b"{}".to_vec(),
        ),
    )
    .await;
    assert_eq!(c, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let (c, _, _) = send(
        &otlp,
        post(
            "/v1/traces",
            Some(INGEST),
            "application/x-protobuf",
            b"\xff\xfe junk".to_vec(),
        ),
    )
    .await;
    assert_eq!(c, StatusCode::BAD_REQUEST);
    let (c, h, b) = send(
        &otlp,
        post("/v1/traces", Some(INGEST), "application/x-protobuf", body),
    )
    .await;
    assert_eq!(
        (c, h["content-type"].to_str().unwrap(), b.len()),
        (StatusCode::OK, "application/x-protobuf", 0)
    );
    assert_eq!(
        st.stats.rejected.load(std::sync::atomic::Ordering::Relaxed),
        4
    );
}

#[tokio::test]
async fn gzip_is_accepted_and_bombs_are_rejected() {
    let st = state();
    let otlp = server::otlp_router(st);
    let gz = |data: &[u8]| {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    };
    let body = fixtures::logs("svc", vec![(now_ms(), 9, "hola", None)]);
    let mut req = post(
        "/v1/logs",
        Some(INGEST),
        "application/x-protobuf",
        gz(&body),
    );
    req.headers_mut()
        .insert("content-encoding", "gzip".parse().unwrap());
    assert_eq!(send(&otlp, req).await.0, StatusCode::OK);

    // 64 MiB de ceros comprimen a ~64 KiB: debe rechazarse tras descomprimir 32 MiB.
    let bomb = gz(&vec![0u8; 64 * 1024 * 1024]);
    assert!(bomb.len() < 1024 * 1024);
    let mut req = post("/v1/logs", Some(INGEST), "application/x-protobuf", bomb);
    req.headers_mut()
        .insert("content-encoding", "gzip".parse().unwrap());
    assert_eq!(send(&otlp, req).await.0, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn ingested_data_is_queryable_end_to_end() {
    let st = state();
    let otlp = server::otlp_router(st.clone());
    let ui = server::ui_router(st.clone());
    let t = now_ms() - 5_000;
    let spans = vec![
        fixtures::span(7, 1, 0, "POST /query", t, 120, true),
        fixtures::span(7, 2, 1, "llm.complete", t + 10, 100, true),
    ];
    for (path, body) in [
        ("/v1/traces", fixtures::traces("agent", spans)),
        (
            "/v1/logs",
            fixtures::logs("agent", vec![(t + 50, 17, "el LLM no respondió", Some(7))]),
        ),
        (
            "/v1/metrics",
            fixtures::metrics(
                "agent",
                t,
                vec![M::Counter(
                    "aetherlynx.agent.llm.requests",
                    vec![("result", "error")],
                    3.0,
                )],
            ),
        ),
        (
            "/v1/metrics",
            fixtures::metrics(
                "agent",
                t + 4000,
                vec![M::Counter(
                    "aetherlynx.agent.llm.requests",
                    vec![("result", "error")],
                    7.0,
                )],
            ),
        ),
    ] {
        assert_eq!(
            send(
                &otlp,
                post(path, Some(INGEST), "application/x-protobuf", body)
            )
            .await
            .0,
            StatusCode::OK
        );
    }

    // Sin token, la API no responde.
    assert_eq!(
        send(&ui, get("/api/services", None)).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&ui, get("/api/services", Some(INGEST))).await.0,
        StatusCode::UNAUTHORIZED
    );

    let svcs = json(&ui, "/api/services?since=1h").await;
    let agent = &svcs["services"][0];
    assert_eq!(
        (
            agent["service"].as_str(),
            agent["requests"].as_i64(),
            agent["errors"].as_i64()
        ),
        (Some("agent"), Some(1), Some(1))
    );

    let list = json(&ui, "/api/traces?errors=true&since=1h").await;
    assert_eq!(
        list["traces"].as_array().unwrap().len(),
        1,
        "solo spans raíz"
    );
    let id = list["traces"][0]["trace_id"].as_str().unwrap().to_owned();
    let tr = json(&ui, &format!("/api/traces/{id}")).await;
    assert_eq!(tr["spans"].as_array().unwrap().len(), 2);
    assert_eq!(
        tr["logs"][0]["body"], "el LLM no respondió",
        "los logs de la traza vienen con ella"
    );

    let logs = json(&ui, "/api/logs?severity=17&q=LLM&since=1h").await;
    assert_eq!(logs["logs"].as_array().unwrap().len(), 1);

    let names = json(&ui, "/api/metrics?since=1h").await;
    assert_eq!(names["metrics"][0]["name"], "aetherlynx.agent.llm.requests");
    let series = json(
        &ui,
        "/api/metrics/series?name=aetherlynx.agent.llm.requests&since=1h",
    )
    .await;
    let rate = series["series"][0]["points"][0][1].as_f64().unwrap();
    assert!(
        (rate - 1.0).abs() < 1e-9,
        "4 errores en 4 s = 1/s, obtuve {rate}"
    );

    assert_eq!(
        send(&ui, get("/api/traces/not-hex", Some(QUERY))).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(
            &ui,
            get(&format!("/api/traces/{}", "ab".repeat(16)), Some(QUERY))
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&ui, get("/api/logs?since=forever", Some(QUERY)))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(
            &ui,
            get("/api/metrics/series?name=x&mode=bogus", Some(QUERY))
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn panel_is_served_with_strict_security_headers() {
    let ui = server::ui_router(state());
    for (path, ct) in [
        ("/", "text/html"),
        ("/app.js", "text/javascript"),
        ("/app.css", "text/css"),
    ] {
        let (c, h, _) = send(&ui, get(path, None)).await;
        assert_eq!(c, StatusCode::OK, "{path}");
        assert!(h["content-type"].to_str().unwrap().starts_with(ct));
        let csp = h["content-security-policy"].to_str().unwrap();
        assert!(
            csp.contains("default-src 'none'")
                && csp.contains("script-src 'self'")
                && !csp.contains("unsafe")
        );
        assert_eq!(h["x-frame-options"], "DENY");
    }
    assert_eq!(send(&ui, get("/readyz", None)).await.0, StatusCode::OK);
}
