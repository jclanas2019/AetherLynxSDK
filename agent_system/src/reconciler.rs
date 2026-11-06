//! Bucle de reconciliación: compara estado deseado y observado y pide al
//! runtime que converja.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::{self, StreamExt};
use opentelemetry::KeyValue;
use tokio::sync::watch;
use tracing::Instrument;

use crate::registry::{AgentRecord, Registry};
use crate::runtime::{DesiredState, Observation, Runtime};
use crate::telemetry::metrics;

/// Registra una llamada al runtime en el span actual y en las métricas.
fn record_op(operation: &'static str, ok: bool, timed_out: bool, started: std::time::Instant) {
    let result = if timed_out {
        "timeout"
    } else if ok {
        "ok"
    } else {
        "error"
    };
    let m = metrics();
    m.runtime_operations.add(
        1,
        &[
            KeyValue::new("operation", operation),
            KeyValue::new("result", result),
        ],
    );
    m.runtime_duration.record(
        started.elapsed().as_secs_f64(),
        &[KeyValue::new("operation", operation)],
    );
    let span = tracing::Span::current();
    span.record("result", result);
    if !ok {
        span.record("otel.status_code", "ERROR");
    }
}

/// Tiempo máximo de una llamada al runtime. Un API lento no debe congelar
/// el bucle para el resto de agentes.
const APPLY_TIMEOUT: Duration = Duration::from_secs(15);
/// La limpieza puede tocar varios objetos; cada uno tiene su propio tiempo
/// máximo dentro del cliente, y un fallo no detiene a los demás.
const GC_TIMEOUT: Duration = Duration::from_secs(60);
/// Agentes que se reconcilian a la vez.
pub const CONCURRENCY: usize = 8;

#[derive(Default)]
pub struct ReconcileStats {
    pub cycles: AtomicU64,
    /// Último ciclo terminado, con o sin errores.
    pub last_unix: AtomicI64,
    /// Último ciclo que hizo progreso: el runtime respondió a al menos uno de
    /// los agentes (o no había ninguno) y la limpieza no falló. Si todas las
    /// llamadas fallan (API caído, RBAC retirado) deja de avanzar y salta la
    /// alerta ReconcileStalled; un solo agente roto se ve en AgentsFailed.
    pub last_success_unix: AtomicI64,
}

pub struct Reconciler {
    registry: Arc<Registry>,
    runtime: Arc<dyn Runtime>,
    pub stats: Arc<ReconcileStats>,
}

impl Reconciler {
    pub fn new(registry: Arc<Registry>, runtime: Arc<dyn Runtime>) -> Self {
        Self {
            registry,
            runtime,
            stats: Arc::new(ReconcileStats::default()),
        }
    }

    /// Recupera del runtime los agentes que ya existían. Llamar una vez al
    /// arrancar, antes de servir peticiones.
    pub async fn recover(&self) -> Result<usize, String> {
        let span = tracing::info_span!(
            "runtime.recover",
            otel.name = "runtime.recover",
            result = tracing::field::Empty,
            otel.status_code = tracing::field::Empty
        );
        async {
            let started = std::time::Instant::now();
            let r = tokio::time::timeout(APPLY_TIMEOUT, self.runtime.recover()).await;
            record_op("recover", matches!(r, Ok(Ok(_))), r.is_err(), started);
            let found =
                r.map_err(|_| "tiempo agotado recuperando agentes del runtime".to_string())??;
            Ok(self.registry.restore(found))
        }
        .instrument(span)
        .await
    }

    async fn apply_one<'a>(
        &self,
        rec: &'a AgentRecord,
    ) -> (&'a AgentRecord, Result<Observation, String>) {
        let desired = DesiredState {
            name: &rec.name,
            model: &rec.model,
            desired_replicas: rec.desired_replicas,
            deleting: rec.deleting,
        };
        let op = if rec.deleting { "delete" } else { "apply" };
        let span = tracing::info_span!(
            "runtime.apply",
            otel.name = if rec.deleting { "runtime.delete" } else { "runtime.apply" },
            aetherlynx.agent = %rec.name,
            aetherlynx.replicas.desired = i64::from(rec.desired_replicas),
            aetherlynx.replicas.ready = tracing::field::Empty,
            result = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
            otel.status_message = tracing::field::Empty,
        );
        let result = async {
            let started = std::time::Instant::now();
            let r = tokio::time::timeout(APPLY_TIMEOUT, self.runtime.apply(&desired)).await;
            let timed_out = r.is_err();
            let r = r.unwrap_or_else(|_| Err("el runtime no respondió a tiempo".to_string()));
            record_op(op, r.is_ok(), timed_out, started);
            match &r {
                Ok(obs) => {
                    tracing::Span::current()
                        .record("aetherlynx.replicas.ready", i64::from(obs.ready));
                }
                Err(e) => {
                    tracing::Span::current().record("otel.status_message", e.as_str());
                }
            }
            r
        }
        .instrument(span)
        .await;
        (rec, result)
    }

    /// Un ciclo completo sobre todos los agentes, con hasta
    /// [`CONCURRENCY`] llamadas al runtime en paralelo. Devuelve cuántos
    /// cambiaron.
    pub async fn reconcile_once(&self) -> usize {
        let span = tracing::info_span!(
            "reconcile_cycle",
            otel.name = "reconcile_cycle",
            aetherlynx.agents = tracing::field::Empty,
            aetherlynx.changed = tracing::field::Empty,
        );
        let started = std::time::Instant::now();
        let changed = self.reconcile_inner().instrument(span.clone()).await;
        span.record("aetherlynx.changed", changed as i64);
        let m = metrics();
        m.reconcile_cycles.add(1, &[]);
        m.reconcile_duration
            .record(started.elapsed().as_secs_f64(), &[]);
        changed
    }

    async fn reconcile_inner(&self) -> usize {
        let records = self.registry.list();
        tracing::Span::current().record("aetherlynx.agents", records.len() as i64);
        // Las futuras se crean antes de entrar al stream: así el closure no
        // forma parte de su tipo y `run()` sigue siendo `Send` (rustc #102211).
        let pending: Vec<_> = records.iter().map(|rec| self.apply_one(rec)).collect();
        let results: Vec<_> = stream::iter(pending)
            .buffer_unordered(CONCURRENCY)
            .collect()
            .await;

        let mut changed = 0;
        let total = results.len();
        let mut failed = 0;
        for (rec, result) in results {
            match &result {
                Ok(obs) if obs.ready != rec.ready_replicas => {
                    changed += 1;
                    tracing::info!(
                        agent = %rec.name,
                        from = rec.ready_replicas,
                        to = obs.ready,
                        desired = rec.desired_replicas,
                        "réplicas actualizadas"
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    changed += 1;
                    failed += 1;
                    tracing::warn!(agent = %rec.name, error = %e, "el runtime falló");
                }
            }
            self.registry.observe(&rec.name, rec.generation, result);
        }

        // Limpieza de borrados pendientes. Se usa la lista tomada al inicio
        // del ciclo: un agente creado durante el ciclo todavía no tiene
        // recursos, así que no hay carrera.
        let known: Vec<String> = records.into_iter().map(|r| r.name).collect();
        let gc_span = tracing::info_span!(
            "runtime.garbage_collect",
            otel.name = "runtime.garbage_collect",
            result = tracing::field::Empty,
            otel.status_code = tracing::field::Empty
        );
        let gc_ok = async {
            let started = std::time::Instant::now();
            let r = tokio::time::timeout(GC_TIMEOUT, self.runtime.garbage_collect(&known)).await;
            record_op(
                "garbage_collect",
                matches!(r, Ok(Ok(_))),
                r.is_err(),
                started,
            );
            let ok = matches!(r, Ok(Ok(_)));
            match r {
                Ok(Ok(0)) => {}
                Ok(Ok(n)) => tracing::info!(removed = n, "borrados pendientes completados"),
                Ok(Err(e)) => tracing::warn!(error = %e, "falló la limpieza"),
                Err(_) => tracing::warn!("la limpieza no respondió a tiempo"),
            }
            ok
        }
        .instrument(gc_span)
        .await;

        let now = crate::unix_now();
        self.stats.cycles.fetch_add(1, Ordering::Relaxed);
        self.stats.last_unix.store(now, Ordering::Relaxed);
        if gc_ok && (total == 0 || failed < total) {
            self.stats.last_success_unix.store(now, Ordering::Relaxed);
        }
        changed
    }

    /// Ejecuta ciclos cada `interval` hasta que `shutdown` cambie a `true`.
    pub async fn run(self, interval: Duration, mut shutdown: watch::Receiver<bool>) {
        tracing::info!(
            runtime = self.runtime.kind(),
            ?interval,
            "reconciliador iniciado"
        );
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => { self.reconcile_once().await; }
                _ = shutdown.changed() => break,
            }
            if *shutdown.borrow() {
                break;
            }
        }
        tracing::info!("reconciliador detenido");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::AgentPhase;
    use crate::runtime::{Observation, RecoveredAgent, SimulatedRuntime};
    use async_trait::async_trait;

    #[tokio::test]
    async fn converges_to_desired_and_ready() {
        let reg = Arc::new(Registry::new());
        let rec = Reconciler::new(reg.clone(), Arc::new(SimulatedRuntime::new(2)));
        reg.create("a", "m", 5).unwrap();
        for _ in 0..5 {
            rec.reconcile_once().await;
        }
        let a = reg.get("a").unwrap();
        assert_eq!((a.ready_replicas, a.phase), (5, AgentPhase::Ready));
        assert_eq!(rec.reconcile_once().await, 0, "estado estable, sin cambios");
    }

    #[tokio::test]
    async fn delete_drains_and_removes() {
        let reg = Arc::new(Registry::new());
        let rec = Reconciler::new(reg.clone(), Arc::new(SimulatedRuntime::new(10)));
        reg.create("a", "m", 3).unwrap();
        rec.reconcile_once().await;
        reg.delete("a").unwrap();
        rec.reconcile_once().await;
        assert!(reg.list().is_empty());
    }

    #[tokio::test]
    async fn run_stops_on_shutdown() {
        let reg = Arc::new(Registry::new());
        let rec = Reconciler::new(reg, Arc::new(SimulatedRuntime::new(1)));
        let stats = rec.stats.clone();
        let (tx, rx) = watch::channel(false);
        let h = tokio::spawn(rec.run(Duration::from_millis(10), rx));
        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), h)
            .await
            .unwrap()
            .unwrap();
        assert!(stats.cycles.load(Ordering::Relaxed) >= 1);
    }

    /// Runtime que tarda más que el límite: el ciclo debe seguir.
    struct Hanging;

    #[async_trait]
    impl Runtime for Hanging {
        async fn apply(&self, _d: &DesiredState<'_>) -> Result<Observation, String> {
            std::future::pending().await
        }
        async fn recover(&self) -> Result<Vec<RecoveredAgent>, String> {
            Ok(vec![RecoveredAgent {
                name: "old".into(),
                model: "m".into(),
                desired_replicas: 2,
                created_at_unix: None,
            }])
        }
        fn kind(&self) -> &'static str {
            "hanging"
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_runtime_marks_failed_instead_of_blocking() {
        let reg = Arc::new(Registry::new());
        let rec = Reconciler::new(reg.clone(), Arc::new(Hanging));
        assert_eq!(rec.recover().await.unwrap(), 1);
        rec.reconcile_once().await; // con el reloj pausado, el timeout salta al instante
        let a = reg.get("old").unwrap();
        assert_eq!(a.phase, AgentPhase::Failed);
        assert!(a.message.contains("a tiempo"));
        // El ciclo terminó, pero sin progreso: no cuenta como éxito.
        assert!(rec.stats.last_unix.load(Ordering::Relaxed) > 0);
        assert_eq!(rec.stats.last_success_unix.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn a_cycle_with_progress_counts_as_success() {
        let reg = Arc::new(Registry::new());
        let rec = Reconciler::new(reg.clone(), Arc::new(SimulatedRuntime::new(2)));
        reg.create("a", "m", 1).unwrap();
        rec.reconcile_once().await;
        assert!(rec.stats.last_success_unix.load(Ordering::Relaxed) > 0);
    }
}
