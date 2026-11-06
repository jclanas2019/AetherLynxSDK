//! Alertas integradas, evaluadas sobre la telemetría almacenada.
//!
//! Cada regla describe un síntoma operativo con un umbral configurable y
//! una indicación de qué revisar primero (ver también docs/RUNBOOK.md).

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use serde::Serialize;

use crate::series::{increase, latest};
use crate::store::{DbResult, Store};

/// Nombres de métricas que usan las reglas. Los componentes de AetherLynx
/// los emiten; un test de extremo a extremo comprueba que coinciden.
pub mod names {
    pub const RECONCILE_LAST_SUCCESS: &str = "aetherlynx.reconcile.last_success";
    pub const AGENTS: &str = "aetherlynx.agents";
    pub const REPLICAS_DESIRED: &str = "aetherlynx.replicas.desired";
    pub const REPLICAS_READY: &str = "aetherlynx.replicas.ready";
    pub const AUTH_LOGINS: &str = "aetherlynx.auth.logins";
    pub const LLM_REQUESTS: &str = "aetherlynx.agent.llm.requests";
    pub const UPSTREAM_REQUESTS: &str = "aetherlynx.agent.upstream.requests";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Warning,
    Critical,
}

#[derive(Debug, Clone, Serialize)]
pub struct Alert {
    /// Identificador estable: regla + servicio.
    pub id: String,
    pub rule: &'static str,
    pub severity: Severity,
    pub service: String,
    pub summary: String,
    pub value: f64,
    /// Qué revisar primero.
    pub hint: &'static str,
    /// Desde cuándo está activa (ns). Lo fija [`AlertState`].
    pub since_ns: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AlertEvent {
    pub ts_ns: i64,
    pub id: String,
    pub rule: &'static str,
    pub service: String,
    pub severity: Severity,
    pub state: &'static str,
    pub summary: String,
}

#[derive(Debug, Clone)]
pub struct AlertConfig {
    pub silent_after: Duration,
    pub error_window: Duration,
    pub error_ratio: f64,
    pub error_min_spans: i64,
    pub reconcile_stale: Duration,
    pub replicas_window: Duration,
    pub login_window: Duration,
    pub login_failures_per_min: f64,
    pub ratio_window: Duration,
    pub dependency_error_ratio: f64,
    pub dependency_min_requests: f64,
}

impl Default for AlertConfig {
    fn default() -> Self {
        Self {
            silent_after: Duration::from_secs(5 * 60),
            error_window: Duration::from_secs(15 * 60),
            error_ratio: 0.05,
            error_min_spans: 20,
            reconcile_stale: Duration::from_secs(3 * 60),
            replicas_window: Duration::from_secs(10 * 60),
            login_window: Duration::from_secs(5 * 60),
            login_failures_per_min: 30.0,
            ratio_window: Duration::from_secs(10 * 60),
            dependency_error_ratio: 0.2,
            dependency_min_requests: 10.0,
        }
    }
}

fn ago(now_ns: i64, d: Duration) -> i64 {
    now_ns - d.as_nanos() as i64
}

fn fmt_dur(ns: i64) -> String {
    let s = (ns / 1_000_000_000).max(0);
    if s < 120 {
        format!("{s} s")
    } else if s < 7200 {
        format!("{} min", s / 60)
    } else {
        format!("{} h", s / 3600)
    }
}

fn alert(
    rule: &'static str,
    severity: Severity,
    service: &str,
    summary: String,
    value: f64,
    hint: &'static str,
) -> Alert {
    Alert {
        id: format!("{rule}:{service}"),
        rule,
        severity,
        service: service.to_owned(),
        summary,
        value,
        hint,
        since_ns: 0,
    }
}

/// Puntos de una métrica agrupados por servicio.
fn by_service(
    store: &Store,
    name: &str,
    since_ns: i64,
) -> DbResult<BTreeMap<String, Vec<crate::store::MetricPoint>>> {
    let mut out: BTreeMap<String, Vec<_>> = BTreeMap::new();
    for p in store.metric_points(name, None, since_ns)? {
        out.entry(p.service.clone()).or_default().push(p);
    }
    Ok(out)
}

/// Evalúa todas las reglas. Las alertas devueltas aún no tienen `since_ns`.
pub fn evaluate(store: &Store, cfg: &AlertConfig, now_ns: i64) -> DbResult<Vec<Alert>> {
    let mut out = Vec::new();

    // Servicio sin telemetría.
    for s in store.services()? {
        let silent = now_ns - s.last_seen_ns;
        if silent > cfg.silent_after.as_nanos() as i64 {
            out.push(alert(
                "ServiceSilent",
                Severity::Critical,
                &s.service,
                format!("sin telemetría desde hace {}", fmt_dur(silent)),
                silent as f64 / 1e9,
                "¿Está caído el pod o no llega al colector? Revise el estado del pod, sus logs y la NetworkPolicy hacia el puerto 4318.",
            ));
        }
    }

    // Tasa de error en las operaciones (spans de entrada).
    for s in store.services()? {
        let st = store.span_stats(&s.service, ago(now_ns, cfg.error_window))?;
        if st.spans >= cfg.error_min_spans {
            let ratio = st.errors as f64 / st.spans as f64;
            if ratio >= cfg.error_ratio {
                out.push(alert(
                    "HighErrorRate",
                    Severity::Warning,
                    &s.service,
                    format!("{:.0} % de peticiones con error ({} de {})", ratio * 100.0, st.errors, st.spans),
                    ratio,
                    "Abra las trazas con error del servicio y mire el mensaje de estado del span de entrada y sus logs.",
                ));
            }
        }
    }

    // Reconciliador detenido.
    let since = ago(now_ns, Duration::from_secs(3600));
    for (svc, pts) in by_service(store, names::RECONCILE_LAST_SUCCESS, since)? {
        if let Some((_, last_s)) = latest(&pts, |_| true).into_iter().max_by_key(|(ts, _)| *ts) {
            let age = now_ns - (last_s * 1e9) as i64;
            if age > cfg.reconcile_stale.as_nanos() as i64 {
                out.push(alert(
                    "ReconcileStalled",
                    Severity::Critical,
                    &svc,
                    format!("último ciclo de reconciliación correcto hace {}", fmt_dur(age)),
                    age as f64 / 1e9,
                    "El orquestador sigue vivo pero no completa ciclos: revise sus logs (errores del runtime, API de Kubernetes lento o sin permisos).",
                ));
            }
        }
    }

    // Agentes en estado Failed.
    for (svc, pts) in by_service(store, names::AGENTS, since)? {
        let failed: f64 = latest(&pts, |a| a["phase"] == "failed")
            .iter()
            .map(|(_, v)| v)
            .sum();
        if failed > 0.0 {
            out.push(alert(
                "AgentsFailed",
                Severity::Warning,
                &svc,
                format!("{failed} agente(s) en estado Failed"),
                failed,
                "Ejecute `aetherlynx-cli agent list`: la columna MENSAJE indica la causa (rollout atascado, cuota, nombre ocupado...).",
            ));
        }
    }

    // Réplicas por debajo de lo pedido de forma sostenida.
    let win = ago(now_ns, cfg.replicas_window);
    let desired = by_service(store, names::REPLICAS_DESIRED, win)?;
    let ready = by_service(store, names::REPLICAS_READY, win)?;
    for (svc, dpts) in &desired {
        let Some(rpts) = ready.get(svc) else { continue };
        let rmap: BTreeMap<i64, f64> = rpts
            .iter()
            .filter_map(|p| p.value.map(|v| (p.ts_ns, v)))
            .collect();
        // Cada instrumento sella su propia hora al exportar, así que las
        // muestras de "deseadas" y "listas" de una misma exportación difieren
        // en micro o milisegundos: se empareja con la más cercana.
        const PAIR_NS: i64 = 5_000_000_000;
        let nearest = |ts: i64| {
            let before = rmap.range(..=ts).next_back();
            let after = rmap.range(ts..).next();
            [before, after]
                .into_iter()
                .flatten()
                .filter(|(t, _)| (**t - ts).abs() <= PAIR_NS)
                .min_by_key(|(t, _)| (**t - ts).abs())
                .map(|(_, v)| *v)
        };
        let pairs: Vec<(f64, f64)> = dpts
            .iter()
            .filter_map(|p| Some((p.value?, nearest(p.ts_ns)?)))
            .collect();
        let span_ns =
            dpts.last().map(|l| l.ts_ns).unwrap_or(0) - dpts.first().map(|f| f.ts_ns).unwrap_or(0);
        let sustained = pairs.len() >= 2 && span_ns >= cfg.replicas_window.as_nanos() as i64 / 2;
        if sustained && pairs.iter().all(|(d, r)| r < d) {
            let (d, r) = *pairs.last().unwrap();
            out.push(alert(
                "ReplicasBelowDesired",
                Severity::Warning,
                svc,
                format!("{r} de {d} réplicas listas durante {}", fmt_dur(span_ns)),
                d - r,
                "Los pods no llegan a Ready: revise eventos del namespace (imagen, recursos, sondas) con `kubectl get events`.",
            ));
        }
    }

    // Picos de logins fallidos.
    for (svc, pts) in by_service(store, names::AUTH_LOGINS, ago(now_ns, cfg.login_window))? {
        let failures = increase(&pts, |a| a["result"] == "failure");
        let per_min = failures / (cfg.login_window.as_secs_f64() / 60.0);
        if per_min >= cfg.login_failures_per_min {
            out.push(alert(
                "LoginFailureSpike",
                Severity::Warning,
                &svc,
                format!("{per_min:.0} logins fallidos por minuto"),
                per_min,
                "Posible fuerza bruta: revise `/audit` de la API de auth (usuarios e IPs) y el límite por IP.",
            ));
        }
    }

    // Errores en dependencias del agente.
    for (rule, metric, what) in [
        ("LlmErrors", names::LLM_REQUESTS, "llamadas al LLM"),
        (
            "UpstreamErrors",
            names::UPSTREAM_REQUESTS,
            "consultas al API de la universidad",
        ),
    ] {
        for (svc, pts) in by_service(store, metric, ago(now_ns, cfg.ratio_window))? {
            let total = increase(&pts, |_| true);
            let errors = increase(&pts, |a| a["result"] == "error");
            if total >= cfg.dependency_min_requests && errors / total >= cfg.dependency_error_ratio
            {
                out.push(alert(
                    rule,
                    Severity::Warning,
                    &svc,
                    format!("{:.0} % de {what} fallan ({errors:.0} de {total:.0})", 100.0 * errors / total),
                    errors / total,
                    if rule == "LlmErrors" {
                        "Revise la clave del proveedor, cuotas y latencia en las trazas del span `llm.complete`."
                    } else {
                        "Revise la disponibilidad y el token del API de la universidad en las trazas `upstream.*`."
                    },
                ));
            }
        }
    }

    Ok(out)
}

/// Estado de las alertas entre evaluaciones: desde cuándo está activa cada
/// una y un historial corto de cambios.
#[derive(Default)]
pub struct AlertState {
    active: BTreeMap<String, Alert>,
    history: VecDeque<AlertEvent>,
}

const HISTORY: usize = 200;

impl AlertState {
    /// Aplica una evaluación y devuelve los cambios (activadas y resueltas).
    pub fn update(&mut self, current: Vec<Alert>, now_ns: i64) -> Vec<AlertEvent> {
        let mut events = Vec::new();
        let mut next = BTreeMap::new();
        for mut a in current {
            match self.active.get(&a.id) {
                Some(prev) => a.since_ns = prev.since_ns,
                None => {
                    a.since_ns = now_ns;
                    events.push(AlertEvent {
                        ts_ns: now_ns,
                        id: a.id.clone(),
                        rule: a.rule,
                        service: a.service.clone(),
                        severity: a.severity,
                        state: "firing",
                        summary: a.summary.clone(),
                    });
                }
            }
            next.insert(a.id.clone(), a);
        }
        for (id, a) in &self.active {
            if !next.contains_key(id) {
                events.push(AlertEvent {
                    ts_ns: now_ns,
                    id: id.clone(),
                    rule: a.rule,
                    service: a.service.clone(),
                    severity: a.severity,
                    state: "resolved",
                    summary: a.summary.clone(),
                });
            }
        }
        self.active = next;
        for e in &events {
            self.history.push_front(e.clone());
        }
        self.history.truncate(HISTORY);
        events
    }

    pub fn active(&self) -> Vec<Alert> {
        let mut v: Vec<_> = self.active.values().cloned().collect();
        v.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then(a.since_ns.cmp(&b.since_ns))
        });
        v
    }

    pub fn history(&self) -> Vec<AlertEvent> {
        self.history.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::otlp::fixtures::{self, M};
    use crate::otlp::{decode_metrics, decode_traces};

    const S: i64 = 1_000_000_000;

    fn store() -> Store {
        Store::open(":memory:").unwrap()
    }

    fn rules(alerts: &[Alert]) -> Vec<&str> {
        let mut r: Vec<_> = alerts.iter().map(|a| a.rule).collect();
        r.sort();
        r
    }

    #[test]
    fn healthy_system_has_no_alerts() {
        let st = store();
        let now = 10_000 * S;
        let ms = (now / 1_000_000) as u64;
        st.insert(
            &decode_traces(&fixtures::traces(
                "orq",
                vec![fixtures::span(1, 1, 0, "ok", ms - 1000, 5, false)],
            ))
            .unwrap(),
            now,
        )
        .unwrap();
        st.insert(
            &decode_metrics(&fixtures::metrics(
                "orq",
                ms,
                vec![
                    M::Gauge(names::RECONCILE_LAST_SUCCESS, vec![], (now / S - 5) as f64),
                    M::Gauge(names::AGENTS, vec![("phase", "ready")], 3.0),
                    M::Gauge(names::AGENTS, vec![("phase", "failed")], 0.0),
                ],
            ))
            .unwrap(),
            now,
        )
        .unwrap();
        assert!(evaluate(&st, &AlertConfig::default(), now)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn detects_silence_stalls_failures_and_error_rates() {
        let st = store();
        let t0 = 10_000 * S;
        let ms0 = (t0 / 1_000_000) as u64;
        // 30 peticiones, 6 con error.
        let spans = (0..30u8)
            .map(|i| fixtures::span(i + 1, 1, 0, "q", ms0 + i as u64, 5, i < 6))
            .collect();
        st.insert(
            &decode_traces(&fixtures::traces("agent", spans)).unwrap(),
            t0,
        )
        .unwrap();
        st.insert(
            &decode_metrics(&fixtures::metrics(
                "orq",
                ms0,
                vec![
                    M::Gauge(names::RECONCILE_LAST_SUCCESS, vec![], (t0 / S - 600) as f64),
                    M::Gauge(names::AGENTS, vec![("phase", "failed")], 2.0),
                ],
            ))
            .unwrap(),
            t0,
        )
        .unwrap();
        let a = evaluate(&st, &AlertConfig::default(), t0 + 10 * S).unwrap();
        assert_eq!(
            rules(&a),
            vec!["AgentsFailed", "HighErrorRate", "ReconcileStalled"]
        );

        // Diez minutos después sin más datos: ambos servicios callados.
        let later = evaluate(&st, &AlertConfig::default(), t0 + 600 * S).unwrap();
        assert_eq!(
            later.iter().filter(|a| a.rule == "ServiceSilent").count(),
            2
        );
    }

    #[test]
    fn dependency_error_ratio_and_login_spikes() {
        let st = store();
        let t = 10_000 * S;
        for (i, (ok, err, fail)) in [(0.0, 0.0, 0.0), (10.0, 10.0, 400.0)]
            .into_iter()
            .enumerate()
        {
            let ms = (t / 1_000_000) as u64 + i as u64 * 60_000;
            st.insert(
                &decode_metrics(&fixtures::metrics(
                    "agent",
                    ms,
                    vec![
                        M::Counter(names::LLM_REQUESTS, vec![("result", "ok")], ok),
                        M::Counter(names::LLM_REQUESTS, vec![("result", "error")], err),
                    ],
                ))
                .unwrap(),
                t,
            )
            .unwrap();
            st.insert(
                &decode_metrics(&fixtures::metrics(
                    "auth",
                    ms,
                    vec![M::Counter(
                        names::AUTH_LOGINS,
                        vec![("result", "failure")],
                        fail,
                    )],
                ))
                .unwrap(),
                t,
            )
            .unwrap();
        }
        let a = evaluate(&st, &AlertConfig::default(), t + 61 * S).unwrap();
        assert_eq!(rules(&a), vec!["LlmErrors", "LoginFailureSpike"]);
        let llm = a.iter().find(|a| a.rule == "LlmErrors").unwrap();
        assert!((llm.value - 0.5).abs() < 1e-9);
    }

    #[test]
    fn replicas_below_desired_must_be_sustained() {
        let st = store();
        let t = 10_000 * S;
        // Como el SDK real: cada instrumento con su propia marca de tiempo
        // (aquí, 3 ms de diferencia).
        let push = |minute: u64, d: f64, r: f64| {
            let ms = (t / 1_000_000) as u64 + minute * 60_000;
            for (offset, name, v) in [
                (0, names::REPLICAS_DESIRED, d),
                (3, names::REPLICAS_READY, r),
            ] {
                st.insert(
                    &decode_metrics(&fixtures::metrics(
                        "orq",
                        ms + offset,
                        vec![M::Gauge(name, vec![], v)],
                    ))
                    .unwrap(),
                    t,
                )
                .unwrap();
            }
        };
        push(0, 5.0, 3.0);
        let now = t + 60 * S;
        assert!(
            evaluate(&st, &AlertConfig::default(), now)
                .unwrap()
                .iter()
                .all(|a| a.rule != "ReplicasBelowDesired"),
            "un solo punto no basta"
        );
        for m in 1..=8 {
            push(m, 5.0, 3.0);
        }
        let now = t + 8 * 60 * S + S;
        assert!(evaluate(&st, &AlertConfig::default(), now)
            .unwrap()
            .iter()
            .any(|a| a.rule == "ReplicasBelowDesired"));
        push(9, 5.0, 5.0);
        let now = t + 9 * 60 * S + S;
        assert!(
            evaluate(&st, &AlertConfig::default(), now)
                .unwrap()
                .iter()
                .all(|a| a.rule != "ReplicasBelowDesired"),
            "se recuperó"
        );
    }

    #[test]
    fn state_tracks_since_and_history() {
        let mut s = AlertState::default();
        let a = alert("R", Severity::Critical, "svc", "x".into(), 1.0, "");
        let ev = s.update(vec![a.clone()], 100);
        assert_eq!((ev.len(), ev[0].state), (1, "firing"));
        assert!(s.update(vec![a.clone()], 200).is_empty());
        assert_eq!(s.active()[0].since_ns, 100);
        let ev = s.update(vec![], 300);
        assert_eq!(ev[0].state, "resolved");
        assert!(s.active().is_empty());
        assert_eq!(s.history().len(), 2);
    }
}
