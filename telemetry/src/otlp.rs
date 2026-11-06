//! Decodificación de peticiones OTLP (protobuf) a filas del almacén.
//!
//! Se aplican límites defensivos: textos, atributos y eventos se recortan
//! para que un cliente defectuoso o malicioso no pueda inflar la base.

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};
use opentelemetry_proto::tonic::metrics::v1::{metric, number_data_point};
use opentelemetry_proto::tonic::resource::v1::Resource;
use prost::Message;
use serde_json::{Map, Value};

use crate::store::{Batch, LogRow, MetricKind, MetricPoint, SpanRow};

pub const MAX_STRING: usize = 4096;
pub const MAX_BODY: usize = 16 * 1024;
pub const MAX_ATTRIBUTES: usize = 128;
pub const MAX_EVENTS: usize = 64;
const MAX_DEPTH: usize = 4;

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    Protobuf(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protobuf(e) => write!(f, "protobuf OTLP inválido: {e}"),
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn any_to_json(v: &AnyValue, depth: usize) -> Value {
    let Some(inner) = &v.value else {
        return Value::Null;
    };
    match inner {
        any_value::Value::StringValue(s) => Value::String(truncate(s, MAX_STRING)),
        any_value::Value::BoolValue(b) => Value::Bool(*b),
        any_value::Value::IntValue(i) => Value::from(*i),
        any_value::Value::DoubleValue(d) => serde_json::Number::from_f64(*d)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        any_value::Value::BytesValue(b) => Value::String(format!("<{} bytes>", b.len())),
        _ if depth >= MAX_DEPTH => Value::String("<anidado>".into()),
        any_value::Value::ArrayValue(a) => Value::Array(
            a.values
                .iter()
                .take(MAX_ATTRIBUTES)
                .map(|x| any_to_json(x, depth + 1))
                .collect(),
        ),
        any_value::Value::KvlistValue(kv) => Value::Object(attrs_to_map(&kv.values, depth + 1)),
    }
}

fn attrs_to_map(attrs: &[KeyValue], depth: usize) -> Map<String, Value> {
    let mut m = Map::new();
    for kv in attrs.iter().take(MAX_ATTRIBUTES) {
        let v = kv
            .value
            .as_ref()
            .map(|v| any_to_json(v, depth))
            .unwrap_or(Value::Null);
        m.insert(truncate(&kv.key, 256), v);
    }
    m
}

/// Atributos como objeto JSON con las claves ordenadas (serde_json::Map es
/// un BTreeMap): la misma serie produce siempre el mismo texto.
pub fn attrs_json(attrs: &[KeyValue]) -> Value {
    Value::Object(attrs_to_map(attrs, 0))
}

fn body_to_string(v: &Option<AnyValue>) -> String {
    match v.as_ref().and_then(|v| v.value.as_ref()) {
        Some(any_value::Value::StringValue(s)) => truncate(s, MAX_BODY),
        Some(_) => truncate(
            &any_to_json(v.as_ref().unwrap_or(&AnyValue::default()), 0).to_string(),
            MAX_BODY,
        ),
        None => String::new(),
    }
}

/// `service.name` del recurso; si falta o es inválido, "desconocido".
pub fn service_name(resource: &Option<Resource>) -> String {
    let name = resource
        .as_ref()
        .and_then(|r| r.attributes.iter().find(|kv| kv.key == "service.name"))
        .and_then(|kv| kv.value.as_ref())
        .and_then(|v| match &v.value {
            Some(any_value::Value::StringValue(s)) => Some(s.trim().to_owned()),
            _ => None,
        })
        .unwrap_or_default();
    let ok = !name.is_empty() && name.len() <= 128 && name.chars().all(|c| !c.is_control());
    if ok {
        name
    } else {
        "desconocido".into()
    }
}

fn ns(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn id_hex(b: &[u8]) -> String {
    if b.iter().all(|x| *x == 0) {
        String::new()
    } else {
        hex::encode(b)
    }
}

pub fn decode_traces(body: &[u8]) -> Result<Batch, DecodeError> {
    let req = ExportTraceServiceRequest::decode(body)
        .map_err(|e| DecodeError::Protobuf(e.to_string()))?;
    let mut batch = Batch::default();
    for rs in req.resource_spans {
        let service = service_name(&rs.resource);
        for ss in rs.scope_spans {
            for s in ss.spans {
                let (start, end) = (ns(s.start_time_unix_nano), ns(s.end_time_unix_nano));
                let events: Vec<Value> = s
                    .events
                    .iter()
                    .take(MAX_EVENTS)
                    .map(|e| {
                        serde_json::json!({
                            "name": truncate(&e.name, 256),
                            "ts_ns": ns(e.time_unix_nano),
                            "attributes": attrs_json(&e.attributes),
                        })
                    })
                    .collect();
                let (code, msg) = s
                    .status
                    .as_ref()
                    .map(|st| (st.code, truncate(&st.message, MAX_STRING)))
                    .unwrap_or((0, String::new()));
                batch.spans.push(SpanRow {
                    trace_id: id_hex(&s.trace_id),
                    span_id: id_hex(&s.span_id),
                    parent_span_id: id_hex(&s.parent_span_id),
                    service: service.clone(),
                    name: truncate(&s.name, 256),
                    kind: s.kind,
                    start_ns: start,
                    end_ns: end,
                    duration_ns: end.saturating_sub(start).max(0),
                    status_code: code,
                    status_message: msg,
                    attributes: attrs_json(&s.attributes),
                    events: Value::Array(events),
                });
            }
        }
    }
    // Spans sin identificadores no se pueden relacionar: se descartan.
    batch
        .spans
        .retain(|s| !s.trace_id.is_empty() && !s.span_id.is_empty());
    Ok(batch)
}

pub fn decode_logs(body: &[u8]) -> Result<Batch, DecodeError> {
    let req =
        ExportLogsServiceRequest::decode(body).map_err(|e| DecodeError::Protobuf(e.to_string()))?;
    let mut batch = Batch::default();
    for rl in req.resource_logs {
        let service = service_name(&rl.resource);
        for sl in rl.scope_logs {
            for l in sl.log_records {
                let ts = if l.time_unix_nano != 0 {
                    l.time_unix_nano
                } else {
                    l.observed_time_unix_nano
                };
                batch.logs.push(LogRow {
                    ts_ns: ns(ts),
                    service: service.clone(),
                    severity: l.severity_number,
                    severity_text: truncate(&l.severity_text, 32),
                    body: body_to_string(&l.body),
                    trace_id: id_hex(&l.trace_id),
                    span_id: id_hex(&l.span_id),
                    attributes: attrs_json(&l.attributes),
                });
            }
        }
    }
    Ok(batch)
}

pub fn decode_metrics(body: &[u8]) -> Result<Batch, DecodeError> {
    let req = ExportMetricsServiceRequest::decode(body)
        .map_err(|e| DecodeError::Protobuf(e.to_string()))?;
    let mut batch = Batch::default();
    for rm in req.resource_metrics {
        let service = service_name(&rm.resource);
        for sm in rm.scope_metrics {
            for m in sm.metrics {
                let name = truncate(&m.name, 256);
                let unit = truncate(&m.unit, 64);
                let base =
                    |kind, monotonic, temporality, ts: u64, start: u64, attrs: &[KeyValue]| {
                        MetricPoint {
                            ts_ns: ns(ts),
                            start_ns: ns(start),
                            service: service.clone(),
                            name: name.clone(),
                            unit: unit.clone(),
                            kind,
                            monotonic,
                            temporality,
                            attributes: attrs_json(attrs).to_string(),
                            value: None,
                            count: None,
                            sum: None,
                            bounds: Vec::new(),
                            buckets: Vec::new(),
                        }
                    };
                let number = |v: &Option<number_data_point::Value>| match v {
                    Some(number_data_point::Value::AsDouble(d)) => Some(*d),
                    Some(number_data_point::Value::AsInt(i)) => Some(*i as f64),
                    None => None,
                };
                match &m.data {
                    Some(metric::Data::Gauge(g)) => {
                        for p in &g.data_points {
                            let mut mp = base(
                                MetricKind::Gauge,
                                false,
                                0,
                                p.time_unix_nano,
                                p.start_time_unix_nano,
                                &p.attributes,
                            );
                            mp.value = number(&p.value);
                            batch.metrics.push(mp);
                        }
                    }
                    Some(metric::Data::Sum(s)) => {
                        for p in &s.data_points {
                            let mut mp = base(
                                MetricKind::Sum,
                                s.is_monotonic,
                                s.aggregation_temporality,
                                p.time_unix_nano,
                                p.start_time_unix_nano,
                                &p.attributes,
                            );
                            mp.value = number(&p.value);
                            batch.metrics.push(mp);
                        }
                    }
                    Some(metric::Data::Histogram(h)) => {
                        for p in &h.data_points {
                            let mut mp = base(
                                MetricKind::Histogram,
                                true,
                                h.aggregation_temporality,
                                p.time_unix_nano,
                                p.start_time_unix_nano,
                                &p.attributes,
                            );
                            mp.count = i64::try_from(p.count).ok();
                            mp.sum = p.sum;
                            // Un histograma válido tiene una cubeta más que
                            // límites y límites finitos y crecientes; si no,
                            // se descarta el punto (no se puede interpretar).
                            let b = &p.explicit_bounds;
                            if b.len() > 256
                                || p.bucket_counts.len() != b.len() + 1
                                || !b.iter().all(|x| x.is_finite())
                                || b.windows(2).any(|w| w[0] >= w[1])
                            {
                                continue;
                            }
                            mp.bounds = b.clone();
                            mp.buckets = p.bucket_counts.clone();
                            batch.metrics.push(mp);
                        }
                    }
                    // Histogramas exponenciales y resúmenes: no los emite
                    // ningún componente de AetherLynx; se ignoran.
                    _ => {}
                }
            }
        }
    }
    batch
        .metrics
        .retain(|m| m.value.is_some() || m.count.is_some());
    Ok(batch)
}

#[cfg(any(test, feature = "test-fixtures"))]
pub mod fixtures {
    //! Constructores de peticiones OTLP para tests.
    use super::*;
    use opentelemetry_proto::tonic::common::v1::{InstrumentationScope, KeyValue};
    use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
    use opentelemetry_proto::tonic::metrics::v1::{
        Gauge, Histogram, HistogramDataPoint, Metric, NumberDataPoint, ResourceMetrics,
        ScopeMetrics, Sum,
    };
    use opentelemetry_proto::tonic::trace::v1::{
        span::Event, ResourceSpans, ScopeSpans, Span, Status,
    };

    pub fn kv(k: &str, v: &str) -> KeyValue {
        KeyValue {
            key: k.into(),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue(v.into())),
            }),
        }
    }

    pub fn resource(service: &str) -> Option<Resource> {
        Some(Resource {
            attributes: vec![kv("service.name", service)],
            ..Default::default()
        })
    }

    pub fn span(
        trace: u8,
        id: u8,
        parent: u8,
        name: &str,
        start_ms: u64,
        dur_ms: u64,
        error: bool,
    ) -> Span {
        Span {
            trace_id: vec![trace; 16],
            span_id: vec![id; 8],
            parent_span_id: if parent == 0 { vec![] } else { vec![parent; 8] },
            name: name.into(),
            kind: 2,
            start_time_unix_nano: start_ms * 1_000_000,
            end_time_unix_nano: (start_ms + dur_ms) * 1_000_000,
            attributes: vec![kv("rpc.method", name)],
            events: vec![Event {
                name: "evento".into(),
                time_unix_nano: start_ms * 1_000_000,
                ..Default::default()
            }],
            status: Some(Status {
                code: if error { 2 } else { 1 },
                message: if error {
                    "falló".into()
                } else {
                    String::new()
                },
            }),
            ..Default::default()
        }
    }

    pub fn traces(service: &str, spans: Vec<Span>) -> Vec<u8> {
        ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: resource(service),
                scope_spans: vec![ScopeSpans {
                    scope: Some(InstrumentationScope::default()),
                    spans,
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
        .encode_to_vec()
    }

    pub fn logs(service: &str, records: Vec<(u64, i32, &str, Option<u8>)>) -> Vec<u8> {
        ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: resource(service),
                scope_logs: vec![ScopeLogs {
                    log_records: records
                        .into_iter()
                        .map(|(ms, sev, body, trace)| LogRecord {
                            time_unix_nano: ms * 1_000_000,
                            severity_number: sev,
                            severity_text: if sev >= 17 {
                                "ERROR".into()
                            } else {
                                "INFO".into()
                            },
                            body: Some(AnyValue {
                                value: Some(any_value::Value::StringValue(body.into())),
                            }),
                            trace_id: trace.map(|t| vec![t; 16]).unwrap_or_default(),
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
        .encode_to_vec()
    }

    pub enum M {
        Gauge(&'static str, Vec<(&'static str, &'static str)>, f64),
        Counter(&'static str, Vec<(&'static str, &'static str)>, f64),
        /// nombre, límites, conteos por cubeta, suma
        Hist(&'static str, Vec<f64>, Vec<u64>, f64),
    }

    pub fn metrics(service: &str, ts_ms: u64, items: Vec<M>) -> Vec<u8> {
        let t = ts_ms * 1_000_000;
        let attrs = |a: &Vec<(&str, &str)>| a.iter().map(|(k, v)| kv(k, v)).collect::<Vec<_>>();
        let metrics = items
            .into_iter()
            .map(|it| match it {
                M::Gauge(n, a, v) => Metric {
                    name: n.into(),
                    data: Some(metric::Data::Gauge(Gauge {
                        data_points: vec![NumberDataPoint {
                            time_unix_nano: t,
                            attributes: attrs(&a),
                            value: Some(number_data_point::Value::AsDouble(v)),
                            ..Default::default()
                        }],
                    })),
                    ..Default::default()
                },
                M::Counter(n, a, v) => Metric {
                    name: n.into(),
                    data: Some(metric::Data::Sum(Sum {
                        data_points: vec![NumberDataPoint {
                            start_time_unix_nano: 1,
                            time_unix_nano: t,
                            attributes: attrs(&a),
                            value: Some(number_data_point::Value::AsInt(v as i64)),
                            ..Default::default()
                        }],
                        aggregation_temporality: 2,
                        is_monotonic: true,
                    })),
                    ..Default::default()
                },
                M::Hist(n, bounds, buckets, sum) => Metric {
                    name: n.into(),
                    unit: "s".into(),
                    data: Some(metric::Data::Histogram(Histogram {
                        data_points: vec![HistogramDataPoint {
                            start_time_unix_nano: 1,
                            time_unix_nano: t,
                            count: buckets.iter().sum(),
                            sum: Some(sum),
                            bucket_counts: buckets,
                            explicit_bounds: bounds,
                            ..Default::default()
                        }],
                        aggregation_temporality: 2,
                    })),
                    ..Default::default()
                },
            })
            .collect();
        ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                resource: resource(service),
                scope_metrics: vec![ScopeMetrics {
                    metrics,
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
        .encode_to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn decodes_spans_with_ids_status_and_events() {
        let b = decode_traces(&traces(
            "orq",
            vec![span(1, 2, 0, "CreateAgent", 1000, 30, true)],
        ))
        .unwrap();
        let s = &b.spans[0];
        assert_eq!(s.trace_id, "01".repeat(16));
        assert_eq!(s.parent_span_id, "");
        assert_eq!(
            (s.service.as_str(), s.duration_ns, s.status_code),
            ("orq", 30_000_000, 2)
        );
        assert_eq!(s.attributes["rpc.method"], "CreateAgent");
        assert_eq!(s.events[0]["name"], "evento");
    }

    #[test]
    fn rejects_garbage_and_drops_spans_without_ids() {
        assert!(decode_traces(b"\xff\xff\xff not protobuf").is_err());
        let mut sp = span(1, 2, 0, "x", 0, 1, false);
        sp.trace_id = vec![0; 16];
        assert!(decode_traces(&traces("s", vec![sp]))
            .unwrap()
            .spans
            .is_empty());
    }

    #[test]
    fn long_values_and_bad_service_names_are_contained() {
        let mut sp = span(1, 2, 0, &"n".repeat(1000), 0, 1, false);
        sp.attributes = (0..500)
            .map(|i| kv(&format!("k{i}"), &"v".repeat(10_000)))
            .collect();
        let b = decode_traces(&traces("bad\nname", vec![sp])).unwrap();
        let s = &b.spans[0];
        assert_eq!(s.service, "desconocido");
        assert!(s.name.len() <= 260);
        assert_eq!(s.attributes.as_object().unwrap().len(), MAX_ATTRIBUTES);
        assert!(s.attributes["k0"].as_str().unwrap().len() <= MAX_STRING + 4);
    }

    #[test]
    fn decodes_logs_and_metrics() {
        let b = decode_logs(&logs("svc", vec![(5, 17, "boom", Some(7))])).unwrap();
        assert_eq!((b.logs[0].severity, b.logs[0].body.as_str()), (17, "boom"));
        assert_eq!(b.logs[0].trace_id, "07".repeat(16));

        let b = decode_metrics(&metrics(
            "svc",
            10,
            vec![
                M::Gauge("g", vec![("phase", "ready")], 3.0),
                M::Counter("c", vec![], 7.0),
                M::Hist("h", vec![0.1, 1.0], vec![1, 2, 3], 4.5),
            ],
        ))
        .unwrap();
        assert_eq!(b.metrics.len(), 3);
        assert_eq!(b.metrics[0].attributes, r#"{"phase":"ready"}"#);
        assert!(b.metrics[1].monotonic && b.metrics[1].value == Some(7.0));
        assert_eq!(
            (b.metrics[2].count, b.metrics[2].buckets.len()),
            (Some(6), 3)
        );

        // Histogramas mal formados: se descartan en vez de romper las gráficas.
        let b = decode_metrics(&metrics(
            "svc",
            10,
            vec![
                M::Hist(
                    "cubetas-de-mas",
                    vec![1.0, 2.0, 3.0],
                    vec![0, 0, 0, 0, 5],
                    1.0,
                ),
                M::Hist("cubetas-de-menos", vec![1.0, 2.0], vec![1, 2], 1.0),
                M::Hist("desordenado", vec![2.0, 1.0], vec![1, 2, 3], 1.0),
                M::Hist("infinito", vec![1.0, f64::INFINITY], vec![1, 2, 3], 1.0),
            ],
        ))
        .unwrap();
        assert!(b.metrics.is_empty());
    }
}
