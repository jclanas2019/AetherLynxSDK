//! Cliente del colector de telemetría (`aetherlynx-cli telemetry ...`).

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

pub struct TelemetryClient {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

fn fmt_ms(ms: f64) -> String {
    if ms < 1.0 {
        format!("{:.0} µs", ms * 1000.0)
    } else if ms < 1000.0 {
        format!("{ms:.1} ms")
    } else {
        format!("{:.2} s", ms / 1000.0)
    }
}

fn fmt_ts(ns: i64) -> String {
    let secs = ns.div_euclid(1_000_000_000);
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02} UTC")
}

fn fmt_ago(ns: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    let s = ((now - ns) / 1_000_000_000).max(0);
    if s < 60 {
        format!("hace {s} s")
    } else if s < 3600 {
        format!("hace {} min", s / 60)
    } else {
        format!("hace {} h", s / 3600)
    }
}

/// Texto de un valor para la terminal. Los datos vienen de lo que enviaron
/// los servicios: se neutralizan los caracteres de control para que una
/// secuencia de escape no pueda manipular la terminal del operador.
fn s(v: &Value) -> String {
    let raw = match v {
        Value::String(s) => s.clone(),
        Value::Null => return "—".into(),
        other => other.to_string(),
    };
    raw.chars()
        .map(|c| if c.is_control() { '·' } else { c })
        .collect()
}

impl TelemetryClient {
    pub fn new(base: &str, token: Option<String>, ca: Option<&Path>) -> Result<Self, String> {
        let base = base.trim_end_matches('/').to_owned();
        if !base.starts_with("http://") && !base.starts_with("https://") {
            return Err("la URL del colector debe empezar con http:// o https://".into());
        }
        let mut b = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5));
        if base.starts_with("http://") && token.is_some() {
            let host = reqwest::Url::parse(&base)
                .ok()
                .and_then(|u| u.host_str().map(str::to_owned));
            if !crate::grpc_client::is_loopback(host.as_deref()) {
                eprintln!("aviso: enviando el token de consulta sin TLS a {base}");
            }
        }
        if let Some(ca) = ca {
            if !base.starts_with("https://") {
                return Err("--telemetry-ca-cert requiere una URL https://".into());
            }
            let pem = std::fs::read(ca)
                .map_err(|e| format!("no se pudo leer el CA '{}': {e}", ca.display()))?;
            b = b.add_root_certificate(
                reqwest::Certificate::from_pem(&pem).map_err(|e| format!("CA inválida: {e}"))?,
            );
        }
        Ok(Self {
            http: b.build().map_err(|e| e.to_string())?,
            base,
            token,
        })
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, String> {
        let mut req = self.http.get(format!("{}{path}", self.base));
        let q: Vec<_> = query.iter().filter(|(_, v)| !v.is_empty()).collect();
        req = req.query(&q);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let res = req
            .send()
            .await
            .map_err(|e| format!("no se pudo conectar al colector en {}: {e}", self.base))?;
        let status = res.status();
        let body: Value = res.json().await.unwrap_or(Value::Null);
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err("el colector rechazó el token (revise AETHERLYNX_TELEMETRY_TOKEN)".into());
        }
        if !status.is_success() {
            return Err(body["error"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("HTTP {status}")));
        }
        Ok(body)
    }

    pub async fn status(&self, json: bool) -> Result<(), String> {
        let v = self.get("/api/status", &[]).await?;
        if json {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            return Ok(());
        }
        println!(
            "Colector:       {} (en marcha {} s)",
            s(&v["version"]),
            s(&v["uptime_s"])
        );
        println!(
            "Guardado:       {} spans, {} logs, {} puntos de métricas",
            s(&v["stored"]["spans"]),
            s(&v["stored"]["logs"]),
            s(&v["stored"]["metric_points"])
        );
        println!(
            "Rechazadas:     {} peticiones de ingesta, {} servicios nuevos por encima del máximo",
            s(&v["ingested"]["rejected_requests"]),
            s(&v["ingested"]["rejected_services"])
        );
        println!(
            "Descartadas:    {} filas antiguas por base llena",
            s(&v["ingested"]["shed_rows"])
        );
        println!(
            "Retención:      {} h",
            v["retention_s"].as_i64().unwrap_or(0) / 3600
        );
        Ok(())
    }

    pub async fn alerts(&self, json: bool) -> Result<bool, String> {
        let v = self.get("/api/alerts", &[]).await?;
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&v["active"]).unwrap_or_default()
            );
        } else if v["active"].as_array().is_none_or(|a| a.is_empty()) {
            println!("Sin alertas activas.");
        } else {
            for a in v["active"].as_array().into_iter().flatten() {
                let sev = if a["severity"] == "critical" {
                    "CRÍTICA"
                } else {
                    "AVISO  "
                };
                println!(
                    "{sev} {} · {}: {}",
                    s(&a["rule"]),
                    s(&a["service"]),
                    s(&a["summary"])
                );
                println!(
                    "        desde {} · {}",
                    fmt_ago(a["since_ns"].as_i64().unwrap_or(0)),
                    s(&a["hint"])
                );
            }
        }
        // `true` si hay alguna crítica: permite usar la CLI en scripts.
        Ok(v["active"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|a| a["severity"] == "critical"))
    }

    pub async fn services(&self, since: &str, json: bool) -> Result<(), String> {
        let v = self
            .get("/api/services", &[("since", since.into())])
            .await?;
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&v["services"]).unwrap_or_default()
            );
            return Ok(());
        }
        let rows = v["services"].as_array().cloned().unwrap_or_default();
        if rows.is_empty() {
            println!("Aún no llegó telemetría.");
            return Ok(());
        }
        let w = rows
            .iter()
            .map(|r| s(&r["service"]).len())
            .max()
            .unwrap_or(8)
            .max(8);
        println!(
            "{:<w$}  {:<12}  {:>10}  {:>8}  {:>9}  {:>9}",
            "SERVICIO", "ÚLTIMA SEÑAL", "OPERACIONES", "ERRORES", "P50", "P95"
        );
        for r in rows {
            let req = r["requests"].as_i64().unwrap_or(0);
            let err = r["errors"].as_i64().unwrap_or(0);
            let pct = if req > 0 {
                format!("{:.1} %", 100.0 * err as f64 / req as f64)
            } else {
                "—".into()
            };
            let p = |k: &str| r[k].as_f64().map(fmt_ms).unwrap_or_else(|| "—".into());
            println!(
                "{:<w$}  {:<12}  {:>10}  {:>8}  {:>9}  {:>9}",
                s(&r["service"]),
                fmt_ago(r["last_seen_ns"].as_i64().unwrap_or(0)),
                req,
                pct,
                p("p50_ms"),
                p("p95_ms")
            );
        }
        Ok(())
    }

    pub async fn traces(
        &self,
        since: &str,
        service: Option<String>,
        errors: bool,
        limit: u32,
        json: bool,
    ) -> Result<(), String> {
        let v = self
            .get(
                "/api/traces",
                &[
                    ("since", since.into()),
                    ("service", service.unwrap_or_default()),
                    ("errors", if errors { "true".into() } else { String::new() }),
                    ("limit", limit.to_string()),
                ],
            )
            .await?;
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&v["traces"]).unwrap_or_default()
            );
            return Ok(());
        }
        let rows = v["traces"].as_array().cloned().unwrap_or_default();
        if rows.is_empty() {
            println!("No hay trazas que coincidan.");
            return Ok(());
        }
        println!(
            "{:<12}  {:<32}  {:<24}  {:<26}  {:>10}  ESTADO",
            "HORA", "TRAZA", "SERVICIO", "OPERACIÓN", "DURACIÓN"
        );
        for t in rows {
            let err = t["status_code"] == 2;
            println!(
                "{:<12}  {:<32}  {:<24}  {:<26}  {:>10}  {}",
                fmt_ts(t["start_ns"].as_i64().unwrap_or(0)),
                s(&t["trace_id"]),
                s(&t["service"]),
                s(&t["name"]),
                fmt_ms(t["duration_ns"].as_f64().unwrap_or(0.0) / 1e6),
                if err {
                    format!("ERROR {}", s(&t["status_message"]))
                } else {
                    "ok".into()
                }
            );
        }
        Ok(())
    }

    pub async fn trace(&self, id: &str, json: bool) -> Result<(), String> {
        let v = self.get(&format!("/api/traces/{id}"), &[]).await?;
        if json {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            return Ok(());
        }
        let spans = v["spans"].as_array().cloned().unwrap_or_default();
        let t0 = spans
            .iter()
            .filter_map(|x| x["start_ns"].as_i64())
            .min()
            .unwrap_or(0);
        let t1 = spans
            .iter()
            .filter_map(|x| x["end_ns"].as_i64())
            .max()
            .unwrap_or(t0);
        let total = (t1 - t0).max(1) as f64;
        println!(
            "Traza {id} · {} · {} spans · {}",
            fmt_ts(t0),
            spans.len(),
            fmt_ms(total / 1e6)
        );
        let ids: std::collections::HashSet<_> = spans.iter().map(|x| s(&x["span_id"])).collect();
        let width = 30.0;
        fn walk(
            spans: &[Value],
            ids: &std::collections::HashSet<String>,
            parent: &str,
            depth: usize,
            t0: i64,
            total: f64,
            width: f64,
        ) {
            let mut kids: Vec<&Value> = spans
                .iter()
                .filter(|x| {
                    let p = s(&x["parent_span_id"]);
                    if parent.is_empty() {
                        p == "—" || p.is_empty() || !ids.contains(&p)
                    } else {
                        p == parent
                    }
                })
                .collect();
            kids.sort_by_key(|x| x["start_ns"].as_i64().unwrap_or(0));
            for x in kids {
                let start = (x["start_ns"].as_i64().unwrap_or(0) - t0) as f64;
                let dur = x["duration_ns"].as_f64().unwrap_or(0.0);
                let off = ((start / total) * width).round() as usize;
                let len = (((dur / total) * width).round() as usize)
                    .max(1)
                    .min(width as usize - off.min(width as usize - 1));
                let bar = format!(
                    "{}{}",
                    " ".repeat(off.min(width as usize - 1)),
                    "█".repeat(len)
                );
                let err = if x["status_code"] == 2 {
                    format!("  ✗ {}", s(&x["status_message"]))
                } else {
                    String::new()
                };
                let label = format!(
                    "{}{} [{}]",
                    "  ".repeat(depth),
                    s(&x["name"]),
                    s(&x["service"])
                );
                println!("  {label:<48} {bar:<30} {:>10}{err}", fmt_ms(dur / 1e6));
                walk(spans, ids, &s(&x["span_id"]), depth + 1, t0, total, width);
            }
        }
        walk(&spans, &ids, "", 0, t0, total, width);
        let logs = v["logs"].as_array().cloned().unwrap_or_default();
        if !logs.is_empty() {
            println!("Logs:");
            let mut logs = logs;
            logs.sort_by_key(|l| l["ts_ns"].as_i64().unwrap_or(0));
            for l in logs {
                println!(
                    "  +{:>9}  {:<5}  {}",
                    fmt_ms((l["ts_ns"].as_i64().unwrap_or(t0) - t0) as f64 / 1e6),
                    s(&l["severity_text"]),
                    s(&l["body"])
                );
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn logs(
        &self,
        since: &str,
        service: Option<String>,
        severity: i32,
        trace: Option<String>,
        q: Option<String>,
        limit: u32,
        json: bool,
    ) -> Result<(), String> {
        let v = self
            .get(
                "/api/logs",
                &[
                    ("since", since.into()),
                    ("service", service.unwrap_or_default()),
                    ("severity", severity.to_string()),
                    ("trace_id", trace.unwrap_or_default()),
                    ("q", q.unwrap_or_default()),
                    ("limit", limit.to_string()),
                ],
            )
            .await?;
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&v["logs"]).unwrap_or_default()
            );
            return Ok(());
        }
        let mut rows = v["logs"].as_array().cloned().unwrap_or_default();
        if rows.is_empty() {
            println!("No hay logs que coincidan.");
            return Ok(());
        }
        rows.reverse(); // del más antiguo al más reciente, como un log
        for l in rows {
            let trace = l["trace_id"]
                .as_str()
                .filter(|t| !t.is_empty())
                .map(|t| {
                    format!(
                        "  [traza {}]",
                        s(&Value::from(t.chars().take(8).collect::<String>()))
                    )
                })
                .unwrap_or_default();
            println!(
                "{}  {:<5}  {:<24}  {}{trace}",
                fmt_ts(l["ts_ns"].as_i64().unwrap_or(0)),
                s(&l["severity_text"]),
                s(&l["service"]),
                s(&l["body"])
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::s;
    use serde_json::Value;

    #[test]
    fn control_characters_never_reach_the_terminal() {
        let v = Value::from("ok\u{1b}]0;título\u{7}\u{1b}[2Jfin\n");
        let out = s(&v);
        assert!(!out.chars().any(char::is_control), "{out:?}");
        assert!(out.starts_with("ok·") && out.contains("fin"));
        assert_eq!(s(&Value::Null), "—");
    }
}
