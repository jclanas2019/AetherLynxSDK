//! Almacenamiento de telemetría en SQLite.
//!
//! Tres tablas de datos (spans, logs, puntos de métricas) más una de
//! servicios con la última vez que se supo de cada uno. Todo lo que se
//! guarda vence según la retención configurada.

use std::sync::{Mutex, MutexGuard};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

pub type DbResult<T> = Result<T, rusqlite::Error>;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS services (
    service       TEXT PRIMARY KEY,
    first_seen_ns INTEGER NOT NULL,
    last_seen_ns  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS spans (
    trace_id       TEXT NOT NULL,
    span_id        TEXT NOT NULL,
    parent_span_id TEXT NOT NULL,
    service        TEXT NOT NULL,
    name           TEXT NOT NULL,
    kind           INTEGER NOT NULL,
    start_ns       INTEGER NOT NULL,
    end_ns         INTEGER NOT NULL,
    duration_ns    INTEGER NOT NULL,
    status_code    INTEGER NOT NULL,
    status_message TEXT NOT NULL,
    attributes     TEXT NOT NULL,
    events         TEXT NOT NULL,
    PRIMARY KEY (trace_id, span_id)
);
CREATE INDEX IF NOT EXISTS spans_by_time ON spans (start_ns);
CREATE INDEX IF NOT EXISTS spans_by_service ON spans (service, start_ns);
CREATE TABLE IF NOT EXISTS logs (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_ns         INTEGER NOT NULL,
    service       TEXT NOT NULL,
    severity      INTEGER NOT NULL,
    severity_text TEXT NOT NULL,
    body          TEXT NOT NULL,
    trace_id      TEXT NOT NULL,
    span_id       TEXT NOT NULL,
    attributes    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS logs_by_time ON logs (ts_ns);
CREATE INDEX IF NOT EXISTS logs_by_trace ON logs (trace_id);
CREATE TABLE IF NOT EXISTS metric_points (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_ns       INTEGER NOT NULL,
    start_ns    INTEGER NOT NULL,
    service     TEXT NOT NULL,
    name        TEXT NOT NULL,
    unit        TEXT NOT NULL,
    kind        TEXT NOT NULL,
    monotonic   INTEGER NOT NULL,
    temporality INTEGER NOT NULL,
    attributes  TEXT NOT NULL,
    value       REAL,
    count       INTEGER,
    sum         REAL,
    bounds      TEXT,
    buckets     TEXT
);
CREATE INDEX IF NOT EXISTS metrics_by_name ON metric_points (name, ts_ns);
";

/// Un span listo para guardar.
#[derive(Debug, Clone, Serialize)]
pub struct SpanRow {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: String,
    pub service: String,
    pub name: String,
    pub kind: i32,
    pub start_ns: i64,
    pub end_ns: i64,
    pub duration_ns: i64,
    pub status_code: i32,
    pub status_message: String,
    pub attributes: serde_json::Value,
    pub events: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogRow {
    pub ts_ns: i64,
    pub service: String,
    pub severity: i32,
    pub severity_text: String,
    pub body: String,
    pub trace_id: String,
    pub span_id: String,
    pub attributes: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MetricKind {
    Gauge,
    Sum,
    Histogram,
}

impl MetricKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Gauge => "gauge",
            Self::Sum => "sum",
            Self::Histogram => "histogram",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "sum" => Self::Sum,
            "histogram" => Self::Histogram,
            _ => Self::Gauge,
        }
    }
}

/// Temporalidad OTLP: 1 = delta, 2 = acumulativa.
pub const TEMPORALITY_DELTA: i32 = 1;
pub const TEMPORALITY_CUMULATIVE: i32 = 2;

#[derive(Debug, Clone, Serialize)]
pub struct MetricPoint {
    pub ts_ns: i64,
    pub start_ns: i64,
    pub service: String,
    pub name: String,
    pub unit: String,
    pub kind: MetricKind,
    pub monotonic: bool,
    pub temporality: i32,
    /// Atributos en JSON canónico (claves ordenadas): identifican la serie.
    pub attributes: String,
    pub value: Option<f64>,
    pub count: Option<i64>,
    pub sum: Option<f64>,
    pub bounds: Vec<f64>,
    pub buckets: Vec<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct Batch {
    pub spans: Vec<SpanRow>,
    pub logs: Vec<LogRow>,
    pub metrics: Vec<MetricPoint>,
}

/// Margen de reloj admitido hacia el futuro.
pub const MAX_CLOCK_SKEW_NS: i64 = 5 * 60 * 1_000_000_000;

impl Batch {
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty() && self.logs.is_empty() && self.metrics.is_empty()
    }

    /// Las marcas de tiempo vienen del cliente. Una en el futuro lejano (reloj
    /// desajustado o datos manipulados) nunca saldría de la retención, así
    /// que se trae a `now_ns`, conservando la duración de los spans.
    pub fn clamp_future(&mut self, now_ns: i64) {
        let limit = now_ns.saturating_add(MAX_CLOCK_SKEW_NS);
        for s in &mut self.spans {
            if s.start_ns > limit || s.end_ns > limit {
                s.start_ns = now_ns.saturating_sub(s.duration_ns);
                s.end_ns = now_ns;
            }
        }
        for l in &mut self.logs {
            if l.ts_ns > limit {
                l.ts_ns = now_ns;
            }
        }
        for m in &mut self.metrics {
            if m.ts_ns > limit {
                m.ts_ns = now_ns;
            }
            if m.start_ns > m.ts_ns {
                m.start_ns = m.ts_ns;
            }
        }
    }

    fn services(&self) -> std::collections::BTreeSet<&str> {
        self.spans
            .iter()
            .map(|s| s.service.as_str())
            .chain(self.logs.iter().map(|l| l.service.as_str()))
            .chain(self.metrics.iter().map(|m| m.service.as_str()))
            .collect()
    }

    fn retain_services(&mut self, keep: &std::collections::BTreeSet<String>) {
        self.spans.retain(|s| keep.contains(&s.service));
        self.logs.retain(|l| keep.contains(&l.service));
        self.metrics.retain(|m| keep.contains(&m.service));
    }
}

/// Límites que protegen al colector de un cliente con el token de ingesta
/// que envíe demasiado (por error o a propósito).
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Servicios distintos. Los datos de servicios nuevos por encima del
    /// límite se descartan hasta que alguno salga de la retención.
    pub max_services: usize,
    /// Tamaño máximo de la base. Al llenarse se descarta el 10 % más antiguo.
    pub max_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_services: 256,
            max_bytes: 1536 * 1024 * 1024,
        }
    }
}

/// Resultado de una inserción: lo que no se guardó por los límites.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InsertOutcome {
    pub rejected_services: usize,
    pub shed_rows: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceRow {
    pub service: String,
    pub first_seen_ns: i64,
    pub last_seen_ns: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricInfo {
    pub service: String,
    pub name: String,
    pub unit: String,
    pub kind: MetricKind,
    pub monotonic: bool,
    pub points: i64,
}

#[derive(Debug, Default, Clone)]
pub struct SpanFilter {
    /// Solo spans de entrada a un servicio (ver [`ENTRY_SPAN`]).
    pub entry_only: bool,
    pub service: Option<String>,
    pub name: Option<String>,
    pub errors_only: bool,
    pub since_ns: i64,
    pub limit: u32,
}

/// Condición SQL (sobre el alias `s`) de "span de entrada": el que inicia el
/// trabajo de un servicio. Es la raíz de la traza o un span cuyo padre no es
/// de ese mismo servicio (p. ej. viene de otro proceso por `traceparent`, o de
/// un cliente que no exporta). Cada uno es una operación del servicio.
pub const ENTRY_SPAN: &str = "(s.parent_span_id = '' OR NOT EXISTS (
        SELECT 1 FROM spans p
        WHERE p.trace_id = s.trace_id AND p.span_id = s.parent_span_id AND p.service = s.service))";

/// Una operación, para estadísticas y alertas: un span de entrada que
/// atiende a alguien (servidor o consumidor). Excluye el trabajo interno con
/// raíz propia, como los ciclos del reconciliador, que si no diluirían la
/// tasa de error y las latencias de las peticiones reales.
pub const OPERATION_SPAN: &str = "(s.kind IN (2, 5) AND (s.parent_span_id = '' OR NOT EXISTS (
        SELECT 1 FROM spans p
        WHERE p.trace_id = s.trace_id AND p.span_id = s.parent_span_id AND p.service = s.service)))";

#[derive(Debug, Default, Clone)]
pub struct LogFilter {
    pub service: Option<String>,
    pub min_severity: i32,
    pub trace_id: Option<String>,
    pub contains: Option<String>,
    pub since_ns: i64,
    pub limit: u32,
}

/// Resumen de un servicio en una ventana de tiempo.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SpanStats {
    pub spans: i64,
    pub errors: i64,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
}

pub struct Store {
    conn: Mutex<Connection>,
    limits: Limits,
}

fn parse_json(s: String) -> serde_json::Value {
    serde_json::from_str(&s).unwrap_or(serde_json::Value::Null)
}

fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

impl Store {
    pub fn open(path: &str) -> DbResult<Self> {
        Self::open_with(path, Limits::default())
    }

    pub fn open_with(path: &str, limits: Limits) -> DbResult<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // Debe fijarse antes de crear tablas para que surta efecto.
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        if path != ":memory:" {
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
        }
        // Temporales en memoria: en el contenedor el sistema de archivos es de
        // solo lectura y no hay /tmp donde SQLite pueda volcar ordenaciones.
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        conn.execute_batch(SCHEMA)?;
        let page_size: u64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        let pages = (limits.max_bytes / page_size.max(512)).max(64);
        conn.pragma_update(None, "max_page_count", pages)?;
        Ok(Self {
            conn: Mutex::new(conn),
            limits,
        })
    }

    fn c(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Guarda un lote completo en una transacción y actualiza la última
    /// vez que se supo de cada servicio. Aplica los [`Limits`].
    pub fn insert(&self, batch: &Batch, now_ns: i64) -> DbResult<InsertOutcome> {
        let mut out = InsertOutcome::default();
        if batch.is_empty() {
            return Ok(out);
        }
        let mut conn = self.c();

        // Tope de servicios distintos.
        let incoming = batch.services();
        let mut known = std::collections::BTreeSet::new();
        {
            let mut st = conn.prepare_cached("SELECT 1 FROM services WHERE service = ?1")?;
            for svc in &incoming {
                if st.exists(params![svc])? {
                    known.insert((*svc).to_owned());
                }
            }
        }
        let new: Vec<&str> = incoming
            .iter()
            .copied()
            .filter(|s| !known.contains(*s))
            .collect();
        let filtered;
        let batch = if new.is_empty() {
            batch
        } else {
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM services", [], |r| r.get(0))?;
            let room = self.limits.max_services.saturating_sub(count as usize);
            let mut keep = known.clone();
            keep.extend(new.iter().take(room).map(|s| (*s).to_owned()));
            out.rejected_services = new.len().saturating_sub(room);
            if out.rejected_services == 0 {
                batch
            } else {
                let mut b = batch.clone();
                b.retain_services(&keep);
                filtered = b;
                &filtered
            }
        };
        if batch.is_empty() {
            return Ok(out);
        }

        match Self::insert_tx(&mut conn, batch, now_ns) {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::DiskFull =>
            {
                // Base llena: se descarta lo más antiguo y se reintenta una vez.
                out.shed_rows = Self::shed_oldest(&conn, 0.1)?;
                Self::insert_tx(&mut conn, batch, now_ns)?;
            }
            r => r?,
        }
        Ok(out)
    }

    /// Borra la fracción más antigua de cada tabla de datos.
    fn shed_oldest(conn: &Connection, fraction: f64) -> DbResult<usize> {
        let mut n = 0;
        for (table, col) in [
            ("spans", "start_ns"),
            ("logs", "ts_ns"),
            ("metric_points", "ts_ns"),
        ] {
            let total: i64 =
                conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
            let skip = ((total as f64) * fraction).ceil() as i64;
            if skip == 0 {
                continue;
            }
            let cutoff: Option<i64> = conn
                .query_row(
                    &format!("SELECT {col} FROM {table} ORDER BY {col} LIMIT 1 OFFSET ?1"),
                    params![skip - 1],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(c) = cutoff {
                n += conn.execute(
                    &format!("DELETE FROM {table} WHERE {col} <= ?1"),
                    params![c],
                )?;
            }
        }
        tracing::warn!(
            rows = n,
            "base de telemetría llena: se descartaron los datos más antiguos"
        );
        Ok(n)
    }

    fn insert_tx(conn: &mut Connection, batch: &Batch, now_ns: i64) -> DbResult<()> {
        let tx = conn.transaction()?;
        {
            let mut seen = std::collections::BTreeSet::new();
            let mut st = tx.prepare_cached(
                "INSERT OR REPLACE INTO spans VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            )?;
            for s in &batch.spans {
                st.execute(params![
                    s.trace_id,
                    s.span_id,
                    s.parent_span_id,
                    s.service,
                    s.name,
                    s.kind,
                    s.start_ns,
                    s.end_ns,
                    s.duration_ns,
                    s.status_code,
                    s.status_message,
                    s.attributes.to_string(),
                    s.events.to_string()
                ])?;
                seen.insert(s.service.as_str());
            }
            let mut st = tx.prepare_cached(
                "INSERT INTO logs (ts_ns, service, severity, severity_text, body, trace_id, span_id, attributes)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            )?;
            for l in &batch.logs {
                st.execute(params![
                    l.ts_ns,
                    l.service,
                    l.severity,
                    l.severity_text,
                    l.body,
                    l.trace_id,
                    l.span_id,
                    l.attributes.to_string()
                ])?;
                seen.insert(l.service.as_str());
            }
            let mut st = tx.prepare_cached(
                "INSERT INTO metric_points (ts_ns, start_ns, service, name, unit, kind, monotonic,
                     temporality, attributes, value, count, sum, bounds, buckets)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            )?;
            for m in &batch.metrics {
                let (bounds, buckets) = if m.kind == MetricKind::Histogram {
                    (
                        Some(serde_json::to_string(&m.bounds).unwrap_or_default()),
                        Some(serde_json::to_string(&m.buckets).unwrap_or_default()),
                    )
                } else {
                    (None, None)
                };
                st.execute(params![
                    m.ts_ns,
                    m.start_ns,
                    m.service,
                    m.name,
                    m.unit,
                    m.kind.as_str(),
                    m.monotonic,
                    m.temporality,
                    m.attributes,
                    m.value,
                    m.count,
                    m.sum,
                    bounds,
                    buckets
                ])?;
                seen.insert(m.service.as_str());
            }
            let mut st = tx.prepare_cached(
                "INSERT INTO services (service, first_seen_ns, last_seen_ns) VALUES (?1, ?2, ?2)
                 ON CONFLICT(service) DO UPDATE SET last_seen_ns = max(last_seen_ns, excluded.last_seen_ns)",
            )?;
            for svc in seen {
                st.execute(params![svc, now_ns])?;
            }
        }
        tx.commit()
    }

    /// Borra lo anterior a `cutoff_ns` y libera espacio. Devuelve filas borradas.
    pub fn prune(&self, cutoff_ns: i64) -> DbResult<usize> {
        let conn = self.c();
        let mut n = conn.execute("DELETE FROM spans WHERE start_ns < ?1", params![cutoff_ns])?;
        n += conn.execute("DELETE FROM logs WHERE ts_ns < ?1", params![cutoff_ns])?;
        n += conn.execute(
            "DELETE FROM metric_points WHERE ts_ns < ?1",
            params![cutoff_ns],
        )?;
        n += conn.execute(
            "DELETE FROM services WHERE last_seen_ns < ?1",
            params![cutoff_ns],
        )?;
        conn.execute_batch("PRAGMA incremental_vacuum(2000)")?;
        Ok(n)
    }

    pub fn services(&self) -> DbResult<Vec<ServiceRow>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT service, first_seen_ns, last_seen_ns FROM services ORDER BY service",
        )?;
        let rows = st.query_map([], |r| {
            Ok(ServiceRow {
                service: r.get(0)?,
                first_seen_ns: r.get(1)?,
                last_seen_ns: r.get(2)?,
            })
        })?;
        rows.collect()
    }

    fn span_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SpanRow> {
        Ok(SpanRow {
            trace_id: r.get(0)?,
            span_id: r.get(1)?,
            parent_span_id: r.get(2)?,
            service: r.get(3)?,
            name: r.get(4)?,
            kind: r.get(5)?,
            start_ns: r.get(6)?,
            end_ns: r.get(7)?,
            duration_ns: r.get(8)?,
            status_code: r.get(9)?,
            status_message: r.get(10)?,
            attributes: parse_json(r.get(11)?),
            events: parse_json(r.get(12)?),
        })
    }

    /// Spans que encajan con el filtro, del más reciente al más antiguo.
    pub fn spans(&self, f: &SpanFilter) -> DbResult<Vec<SpanRow>> {
        let c = self.c();
        let mut st = c.prepare(&format!(
            "SELECT s.trace_id, s.span_id, s.parent_span_id, s.service, s.name, s.kind,
                    s.start_ns, s.end_ns, s.duration_ns, s.status_code, s.status_message,
                    s.attributes, s.events
             FROM spans s
             WHERE s.start_ns >= ?1
               AND (?2 IS NULL OR s.service = ?2)
               AND (?3 IS NULL OR s.name = ?3)
               AND (?4 = 0 OR s.status_code = 2)
               AND (?6 = 0 OR {ENTRY_SPAN})
             ORDER BY s.start_ns DESC LIMIT ?5"
        ))?;
        let rows = st.query_map(
            params![
                f.since_ns,
                f.service,
                f.name,
                f.errors_only,
                f.limit.clamp(1, 1000),
                f.entry_only
            ],
            Self::span_from_row,
        )?;
        rows.collect()
    }

    pub fn trace(&self, trace_id: &str) -> DbResult<Vec<SpanRow>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT trace_id, span_id, parent_span_id, service, name, kind, start_ns, end_ns,
                    duration_ns, status_code, status_message, attributes, events
             FROM spans WHERE trace_id = ?1 ORDER BY start_ns LIMIT 5000",
        )?;
        let rows = st.query_map(params![trace_id], Self::span_from_row)?;
        rows.collect()
    }

    pub fn logs(&self, f: &LogFilter) -> DbResult<Vec<LogRow>> {
        let c = self.c();
        let contains = f
            .contains
            .as_deref()
            .map(|s| format!("%{}%", like_escape(s)));
        let mut st = c.prepare(
            "SELECT ts_ns, service, severity, severity_text, body, trace_id, span_id, attributes
             FROM logs
             WHERE ts_ns >= ?1
               AND (?2 IS NULL OR service = ?2)
               AND severity >= ?3
               AND (?4 IS NULL OR trace_id = ?4)
               AND (?5 IS NULL OR body LIKE ?5 ESCAPE '\\')
             ORDER BY ts_ns DESC LIMIT ?6",
        )?;
        let rows = st.query_map(
            params![
                f.since_ns,
                f.service,
                f.min_severity,
                f.trace_id,
                contains,
                f.limit.clamp(1, 2000)
            ],
            |r| {
                Ok(LogRow {
                    ts_ns: r.get(0)?,
                    service: r.get(1)?,
                    severity: r.get(2)?,
                    severity_text: r.get(3)?,
                    body: r.get(4)?,
                    trace_id: r.get(5)?,
                    span_id: r.get(6)?,
                    attributes: parse_json(r.get(7)?),
                })
            },
        )?;
        rows.collect()
    }

    pub fn metric_names(&self, since_ns: i64) -> DbResult<Vec<MetricInfo>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT service, name, max(unit), max(kind), max(monotonic), COUNT(*)
             FROM metric_points WHERE ts_ns >= ?1
             GROUP BY service, name ORDER BY name, service",
        )?;
        let rows = st.query_map(params![since_ns], |r| {
            Ok(MetricInfo {
                service: r.get(0)?,
                name: r.get(1)?,
                unit: r.get(2)?,
                kind: MetricKind::parse(&r.get::<_, String>(3)?),
                monotonic: r.get(4)?,
                points: r.get(5)?,
            })
        })?;
        rows.collect()
    }

    /// Puntos de una métrica en orden temporal.
    pub fn metric_points(
        &self,
        name: &str,
        service: Option<&str>,
        since_ns: i64,
    ) -> DbResult<Vec<MetricPoint>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT ts_ns, start_ns, service, name, unit, kind, monotonic, temporality, attributes,
                    value, count, sum, bounds, buckets
             FROM metric_points
             WHERE name = ?1 AND ts_ns >= ?2 AND (?3 IS NULL OR service = ?3)
             ORDER BY ts_ns DESC LIMIT 200000",
        )?;
        let rows = st.query_map(params![name, since_ns, service], |r| {
            let bounds: Option<String> = r.get(12)?;
            let buckets: Option<String> = r.get(13)?;
            Ok(MetricPoint {
                ts_ns: r.get(0)?,
                start_ns: r.get(1)?,
                service: r.get(2)?,
                name: r.get(3)?,
                unit: r.get(4)?,
                kind: MetricKind::parse(&r.get::<_, String>(5)?),
                monotonic: r.get(6)?,
                temporality: r.get(7)?,
                attributes: r.get(8)?,
                value: r.get(9)?,
                count: r.get(10)?,
                sum: r.get(11)?,
                bounds: bounds
                    .and_then(|b| serde_json::from_str(&b).ok())
                    .unwrap_or_default(),
                buckets: buckets
                    .and_then(|b| serde_json::from_str(&b).ok())
                    .unwrap_or_default(),
            })
        })?;
        // Se leen los más recientes (si hay que cortar, que sea lo antiguo)
        // y se devuelven en orden cronológico.
        let mut v: Vec<MetricPoint> = rows.collect::<DbResult<_>>()?;
        v.reverse();
        Ok(v)
    }

    /// Conteo, errores y percentiles de duración de las operaciones
    /// ([`OPERATION_SPAN`]) de un servicio.
    pub fn span_stats(&self, service: &str, since_ns: i64) -> DbResult<SpanStats> {
        let c = self.c();
        let (spans, errors): (i64, i64) = c.query_row(
            &format!(
                "SELECT COUNT(*), COALESCE(SUM(s.status_code = 2), 0) FROM spans s
                 WHERE s.service = ?1 AND s.start_ns >= ?2 AND {OPERATION_SPAN}"
            ),
            params![service, since_ns],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let pct = |p: f64| -> DbResult<Option<f64>> {
            if spans == 0 {
                return Ok(None);
            }
            let offset = ((spans - 1) as f64 * p).round() as i64;
            c.query_row(
                &format!(
                    "SELECT s.duration_ns FROM spans s
                     WHERE s.service = ?1 AND s.start_ns >= ?2 AND {OPERATION_SPAN}
                     ORDER BY s.duration_ns LIMIT 1 OFFSET ?3"
                ),
                params![service, since_ns, offset],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map(|v| v.map(|ns| ns as f64 / 1e6))
        };
        Ok(SpanStats {
            spans,
            errors,
            p50_ms: pct(0.5)?,
            p95_ms: pct(0.95)?,
        })
    }

    pub fn counts(&self) -> DbResult<(i64, i64, i64)> {
        let c = self.c();
        c.query_row(
            "SELECT (SELECT COUNT(*) FROM spans), (SELECT COUNT(*) FROM logs), (SELECT COUNT(*) FROM metric_points)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::otlp::decode_traces;
    use crate::otlp::fixtures::{span, traces};

    #[test]
    fn entry_spans_are_roots_or_continue_another_service() {
        let st = Store::open(":memory:").unwrap();
        let now = 10_000_000_000_000i64;
        let ms = (now / 1_000_000) as u64;
        // Traza 1, toda en "api": raíz (1) con un hijo local (2).
        // Traza 2: "api" llama a "agente" (4 continúa a 3) y el agente tiene
        // un hijo local (5). Traza 3: el agente continúa un padre que nunca
        // llegó (un cliente que no exporta).
        // Un ciclo interno con raíz propia (como reconcile_cycle): aparece en
        // el listado pero no cuenta como operación.
        let mut internal = span(7, 7, 0, "ciclo", ms - 600, 1, true);
        internal.kind = 1;
        let api = traces(
            "api",
            vec![
                internal,
                span(1, 1, 0, "GET /", ms - 900, 10, false),
                span(1, 2, 1, "db", ms - 899, 5, false),
                span(2, 3, 0, "POST /pedir", ms - 800, 20, false),
            ],
        );
        let agent = traces(
            "agente",
            vec![
                span(2, 4, 3, "POST /query", ms - 799, 15, true),
                span(2, 5, 4, "llm", ms - 798, 10, false),
                span(3, 6, 9, "POST /query", ms - 700, 30, false),
            ],
        );
        for b in [api, agent] {
            st.insert(&decode_traces(&b).unwrap(), now).unwrap();
        }
        let since = now - 60_000_000_000;
        let a = st.span_stats("api", since).unwrap();
        assert_eq!((a.spans, a.errors), (2, 0));
        let g = st.span_stats("agente", since).unwrap();
        assert_eq!((g.spans, g.errors), (2, 1));

        let f = |entry_only, service: &str| SpanFilter {
            entry_only,
            service: Some(service.into()),
            since_ns: since,
            limit: 100,
            ..Default::default()
        };
        let names = |v: Vec<SpanRow>| v.into_iter().map(|s| s.name).collect::<Vec<_>>();
        assert_eq!(
            names(st.spans(&f(true, "agente")).unwrap()),
            ["POST /query", "POST /query"]
        );
        assert_eq!(st.spans(&f(false, "agente")).unwrap().len(), 3);
    }

    const NOW: i64 = 10_000_000_000_000;

    fn one_span(service: &str, trace: u8, start_ms: u64) -> Batch {
        decode_traces(&traces(
            service,
            vec![span(trace, 1, 0, "op", start_ms, 1, false)],
        ))
        .unwrap()
    }

    #[test]
    fn new_services_beyond_the_limit_are_dropped() {
        let st = Store::open_with(
            ":memory:",
            Limits {
                max_services: 2,
                ..Limits::default()
            },
        )
        .unwrap();
        let ms = (NOW / 1_000_000) as u64;
        for (i, svc) in ["a", "b", "c"].iter().enumerate() {
            let o = st.insert(&one_span(svc, i as u8 + 1, ms), NOW).unwrap();
            assert_eq!(o.rejected_services, usize::from(*svc == "c"));
        }
        // Los ya conocidos siguen entrando.
        assert_eq!(
            st.insert(&one_span("a", 9, ms), NOW).unwrap(),
            InsertOutcome::default()
        );
        let names: Vec<_> = st
            .services()
            .unwrap()
            .into_iter()
            .map(|s| s.service)
            .collect();
        assert_eq!(names, ["a", "b"]);
    }

    #[test]
    fn future_timestamps_are_brought_to_now() {
        let year_ns = 365 * 24 * 3600 * 1_000_000_000i64;
        let mut b = one_span("a", 1, ((NOW + year_ns) / 1_000_000) as u64);
        b.clamp_future(NOW);
        assert_eq!(b.spans[0].end_ns, NOW);
        assert_eq!(b.spans[0].start_ns, NOW - b.spans[0].duration_ns);
        // Lo normal no se toca.
        let mut ok = one_span("a", 2, (NOW / 1_000_000) as u64 - 10);
        let before = ok.spans[0].start_ns;
        ok.clamp_future(NOW);
        assert_eq!(ok.spans[0].start_ns, before);
    }

    #[test]
    fn a_full_database_sheds_the_oldest_rows() {
        let st = Store::open_with(
            ":memory:",
            Limits {
                max_bytes: 512 * 1024,
                ..Limits::default()
            },
        )
        .unwrap();
        let ms = (NOW / 1_000_000) as u64;
        let mut shed = 0;
        for i in 0..4000u64 {
            let mut b = one_span("a", 1, ms - 4000 + i);
            b.spans[0].span_id = format!("{i:016x}");
            b.spans[0].attributes = serde_json::json!({ "relleno": "x".repeat(200) });
            shed += st.insert(&b, NOW).unwrap().shed_rows;
        }
        assert!(shed > 0, "la base debía llenarse");
        let (spans, _, _) = st.counts().unwrap();
        assert!(spans > 0 && spans < 4000);
        // Lo que queda es lo más reciente.
        let newest = st
            .spans(&SpanFilter {
                since_ns: 0,
                limit: 1,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(newest[0].span_id, format!("{:016x}", 3999));
    }
}
