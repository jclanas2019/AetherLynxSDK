//! Convierte puntos de métricas OTLP en series para gráficos y alertas.
//!
//! - Gauge y sumas no monótonas: el valor tal cual.
//! - Contadores (sumas monótonas): tasa por segundo entre puntos
//!   consecutivos. Un reinicio del proceso (nuevo `start_ns` o un valor que
//!   baja) se detecta y no produce tasas negativas.
//! - Histogramas: percentil o media de lo observado entre puntos.

use serde::Serialize;
use serde_json::Value;

use crate::store::{MetricKind, MetricPoint, TEMPORALITY_DELTA};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    /// Lo razonable según el tipo: tasa para contadores, p95 para
    /// histogramas, valor para gauges.
    Auto,
    Value,
    Rate,
    Quantile(f64),
    Mean,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "" | "auto" => Self::Auto,
            "value" => Self::Value,
            "rate" => Self::Rate,
            "mean" => Self::Mean,
            "p50" => Self::Quantile(0.5),
            "p90" => Self::Quantile(0.9),
            "p95" => Self::Quantile(0.95),
            "p99" => Self::Quantile(0.99),
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Series {
    pub service: String,
    pub attributes: Value,
    /// Pares `[ms desde epoch, valor]`.
    pub points: Vec<(i64, f64)>,
}

fn group(points: Vec<MetricPoint>) -> Vec<((String, String), Vec<MetricPoint>)> {
    let mut groups: Vec<((String, String), Vec<MetricPoint>)> = Vec::new();
    let mut index = std::collections::HashMap::new();
    for p in points {
        let key = (p.service.clone(), p.attributes.clone());
        let i = *index.entry(key.clone()).or_insert_with(|| {
            groups.push((key, Vec::new()));
            groups.len() - 1
        });
        groups[i].1.push(p);
    }
    groups
}

/// Diferencias entre dos puntos acumulativos de una misma serie, con
/// detección de reinicio. Devuelve (delta, segundos) o `None` si no se puede
/// calcular.
fn delta(
    prev: Option<&MetricPoint>,
    cur: &MetricPoint,
    f: impl Fn(&MetricPoint) -> f64,
) -> Option<(f64, f64)> {
    if cur.temporality == TEMPORALITY_DELTA {
        let secs = (cur.ts_ns - cur.start_ns) as f64 / 1e9;
        return (secs > 0.0).then(|| (f(cur), secs));
    }
    let prev = prev?;
    let reset = cur.start_ns != prev.start_ns || f(cur) < f(prev);
    if reset {
        let secs = (cur.ts_ns - cur.start_ns) as f64 / 1e9;
        return (cur.start_ns > 0 && secs > 0.0).then(|| (f(cur), secs));
    }
    let secs = (cur.ts_ns - prev.ts_ns) as f64 / 1e9;
    (secs > 0.0).then(|| (f(cur) - f(prev), secs))
}

fn hist_delta(prev: Option<&MetricPoint>, cur: &MetricPoint) -> Option<(Vec<u64>, u64, f64)> {
    let cumulative = cur.temporality != TEMPORALITY_DELTA;
    let use_prev = match prev {
        Some(p) if cumulative => {
            let reset = p.start_ns != cur.start_ns
                || p.count.unwrap_or(0) > cur.count.unwrap_or(0)
                || p.buckets.len() != cur.buckets.len()
                || p.bounds != cur.bounds;
            if reset {
                None
            } else {
                Some(p)
            }
        }
        None if cumulative => return None,
        _ => None,
    };
    let buckets: Vec<u64> = match use_prev {
        Some(p) => cur
            .buckets
            .iter()
            .zip(&p.buckets)
            .map(|(c, pr)| c.saturating_sub(*pr))
            .collect(),
        None => cur.buckets.clone(),
    };
    let count =
        cur.count.unwrap_or(0) as u64 - use_prev.map(|p| p.count.unwrap_or(0) as u64).unwrap_or(0);
    let sum = cur.sum.unwrap_or(0.0) - use_prev.and_then(|p| p.sum).unwrap_or(0.0);
    Some((buckets, count, sum))
}

/// Percentil estimado por interpolación lineal dentro de la cubeta.
pub fn quantile(bounds: &[f64], buckets: &[u64], q: f64) -> Option<f64> {
    let total: u64 = buckets.iter().sum();
    if total == 0 || buckets.is_empty() {
        return None;
    }
    let target = q.clamp(0.0, 1.0) * total as f64;
    let mut cum = 0.0;
    for (i, c) in buckets.iter().enumerate() {
        let c = *c as f64;
        if c > 0.0 && cum + c >= target {
            let lower = if i == 0 {
                0.0_f64.min(bounds.first().copied().unwrap_or(0.0))
            } else {
                *bounds.get(i - 1)?
            };
            // La última cubeta llega a +Inf: no se puede interpolar, se usa su límite inferior.
            let Some(upper) = bounds.get(i).copied() else {
                return bounds.last().copied();
            };
            return Some(lower + (upper - lower) * ((target - cum) / c));
        }
        cum += c;
    }
    bounds.last().copied()
}

fn series_values(
    kind: MetricKind,
    monotonic: bool,
    mode: Mode,
    pts: &[MetricPoint],
) -> Vec<(i64, f64)> {
    let mode = match mode {
        Mode::Auto => match (kind, monotonic) {
            (MetricKind::Histogram, _) => Mode::Quantile(0.95),
            (MetricKind::Sum, true) => Mode::Rate,
            _ => Mode::Value,
        },
        m => m,
    };
    let ms = |p: &MetricPoint| p.ts_ns / 1_000_000;
    let mut out = Vec::new();
    for (i, cur) in pts.iter().enumerate() {
        let prev = i.checked_sub(1).map(|j| &pts[j]);
        let v = match (kind, mode) {
            (MetricKind::Histogram, Mode::Quantile(q)) => {
                hist_delta(prev, cur).and_then(|(b, _, _)| quantile(&cur.bounds, &b, q))
            }
            (MetricKind::Histogram, Mode::Mean) => {
                hist_delta(prev, cur).and_then(|(_, n, s)| (n > 0).then(|| s / n as f64))
            }
            (MetricKind::Histogram, Mode::Rate) => {
                delta(prev, cur, |p| p.count.unwrap_or(0) as f64).map(|(d, s)| d / s)
            }
            (MetricKind::Histogram, _) => cur.count.map(|c| c as f64),
            (_, Mode::Rate) => delta(prev, cur, |p| p.value.unwrap_or(0.0)).map(|(d, s)| d / s),
            _ => cur.value,
        };
        if let Some(v) = v.filter(|v| v.is_finite()) {
            out.push((ms(cur), v));
        }
    }
    out
}

fn downsample(points: Vec<(i64, f64)>, max: usize) -> Vec<(i64, f64)> {
    if max == 0 || points.len() <= max {
        return points;
    }
    let step = points.len() as f64 / max as f64;
    let last = *points.last().unwrap();
    let mut out: Vec<_> = (0..max - 1)
        .map(|i| points[(i as f64 * step) as usize])
        .collect();
    out.push(last);
    out
}

pub fn build(points: Vec<MetricPoint>, mode: Mode, max_points: usize) -> Vec<Series> {
    group(points)
        .into_iter()
        .map(|((service, attrs), pts)| {
            let (kind, monotonic) = (pts[0].kind, pts[0].monotonic);
            Series {
                service,
                attributes: serde_json::from_str(&attrs).unwrap_or(Value::Null),
                points: downsample(series_values(kind, monotonic, mode, &pts), max_points),
            }
        })
        .filter(|s| !s.points.is_empty())
        .collect()
}

/// Cuánto creció un contador en la ventana, sumando todas las series que
/// cumplan `filter` (con reinicios tenidos en cuenta).
pub fn increase(points: &[MetricPoint], filter: impl Fn(&Value) -> bool) -> f64 {
    // Inicio de la ventana: la muestra más antigua recibida (la consulta ya
    // vino acotada a la ventana).
    let window_start = points.iter().map(|p| p.ts_ns).min().unwrap_or(0);
    let mut total = 0.0;
    for ((_, attrs), pts) in group(points.to_vec()) {
        let a: Value = serde_json::from_str(&attrs).unwrap_or(Value::Null);
        if !filter(&a) {
            continue;
        }
        for (i, cur) in pts.iter().enumerate() {
            let prev = i.checked_sub(1).map(|j| &pts[j]);
            if cur.temporality == TEMPORALITY_DELTA {
                total += cur.value.or(cur.count.map(|c| c as f64)).unwrap_or(0.0);
            } else if let Some(p) = prev {
                let (vc, vp) = (cur.value.unwrap_or(0.0), p.value.unwrap_or(0.0));
                total += if cur.start_ns != p.start_ns || vc < vp {
                    vc
                } else {
                    vc - vp
                };
            } else if cur.start_ns > 0 && cur.start_ns >= window_start {
                // Primera muestra de una serie que nació dentro de la ventana
                // (p. ej. el primer error): todo su valor es incremento.
                total += cur.value.unwrap_or(0.0);
            }
        }
    }
    total
}

/// Último valor de cada serie que cumpla `filter`.
pub fn latest(points: &[MetricPoint], filter: impl Fn(&Value) -> bool) -> Vec<(i64, f64)> {
    group(points.to_vec())
        .into_iter()
        .filter(|((_, a), _)| filter(&serde_json::from_str(a).unwrap_or(Value::Null)))
        .filter_map(|(_, pts)| pts.last().and_then(|p| p.value.map(|v| (p.ts_ns, v))))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TEMPORALITY_CUMULATIVE;

    fn counter(ts_s: i64, start_s: i64, v: f64, attrs: &str) -> MetricPoint {
        MetricPoint {
            ts_ns: ts_s * 1_000_000_000,
            start_ns: start_s * 1_000_000_000,
            service: "s".into(),
            name: "c".into(),
            unit: String::new(),
            kind: MetricKind::Sum,
            monotonic: true,
            temporality: TEMPORALITY_CUMULATIVE,
            attributes: attrs.into(),
            value: Some(v),
            count: None,
            sum: None,
            bounds: vec![],
            buckets: vec![],
        }
    }

    fn hist(ts_s: i64, buckets: Vec<u64>, sum: f64) -> MetricPoint {
        MetricPoint {
            kind: MetricKind::Histogram,
            count: Some(buckets.iter().sum::<u64>() as i64),
            sum: Some(sum),
            bounds: vec![0.1, 0.5, 1.0],
            buckets,
            value: None,
            ..counter(ts_s, 1, 0.0, "{}")
        }
    }

    #[test]
    fn counter_rate_handles_resets() {
        let pts = vec![
            counter(10, 1, 100.0, "{}"),
            counter(20, 1, 150.0, "{}"),
            counter(30, 25, 10.0, "{}"), // el proceso se reinició en t=25
        ];
        let s = build(pts, Mode::Auto, 0);
        assert_eq!(s[0].points, vec![(20_000, 5.0), (30_000, 2.0)]);
    }

    #[test]
    fn increase_sums_matching_series_only() {
        let pts = vec![
            counter(10, 1, 5.0, r#"{"result":"failure"}"#),
            counter(20, 1, 25.0, r#"{"result":"failure"}"#),
            counter(10, 1, 100.0, r#"{"result":"success"}"#),
            counter(20, 1, 900.0, r#"{"result":"success"}"#),
        ];
        let failures = increase(&pts, |a| a["result"] == "failure");
        assert_eq!(failures, 20.0);
    }

    #[test]
    fn quantile_interpolates_within_buckets() {
        // 10 observaciones: 0 en (0,0.1], 10 en (0.1,0.5]
        let p50 = quantile(&[0.1, 0.5, 1.0], &[0, 10, 0, 0], 0.5).unwrap();
        assert!((p50 - 0.3).abs() < 1e-12, "{p50}");
        assert_eq!(
            quantile(&[0.1, 0.5, 1.0], &[0, 0, 0, 4], 0.9),
            Some(1.0),
            "+Inf usa el último límite"
        );
        assert_eq!(quantile(&[0.1], &[0, 0], 0.5), None);
        // Más cubetas que límites + 1: sin pánico.
        assert_eq!(quantile(&[1.0, 2.0, 3.0], &[0, 0, 0, 0, 5], 0.5), None);
    }

    #[test]
    fn increase_counts_a_series_born_inside_the_window() {
        // "success" ya existía (empezó en t=1); "error" aparece en t=15 con 3.
        let pts = vec![
            counter(10, 1, 100.0, r#"{"result":"success"}"#),
            counter(20, 1, 110.0, r#"{"result":"success"}"#),
            counter(20, 15, 3.0, r#"{"result":"error"}"#),
        ];
        assert_eq!(increase(&pts, |a| a["result"] == "error"), 3.0);
        assert_eq!(increase(&pts, |a| a["result"] == "success"), 10.0);
    }

    #[test]
    fn histogram_percentile_uses_only_new_observations() {
        let pts = vec![
            hist(10, vec![100, 0, 0, 0], 5.0), // 100 rápidas antes de la ventana
            hist(20, vec![100, 0, 10, 0], 12.0), // 10 nuevas entre 0.5 y 1.0
        ];
        let s = build(pts.clone(), Mode::Quantile(0.95), 0);
        let (_, p95) = s[0].points[0];
        assert!((0.5..=1.0).contains(&p95), "{p95}");
        let mean = build(pts, Mode::Mean, 0)[0].points[0].1;
        assert!((mean - 0.7).abs() < 1e-9);
    }

    #[test]
    fn downsampling_keeps_the_last_point() {
        let pts: Vec<_> = (0..1000)
            .map(|i| counter(i, 0, 0.0, "{}"))
            .map(|mut p| {
                p.kind = MetricKind::Gauge;
                p.value = Some(p.ts_ns as f64);
                p
            })
            .collect();
        let s = build(pts, Mode::Value, 100);
        assert_eq!(s[0].points.len(), 100);
        assert_eq!(s[0].points.last().unwrap().0, 999_000);
    }
}
