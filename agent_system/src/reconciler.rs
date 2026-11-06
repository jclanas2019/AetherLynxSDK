//! Bucle de reconciliación: compara estado deseado y observado y pide al
//! runtime que converja.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::registry::Registry;
use crate::runtime::{DesiredState, Runtime};

#[derive(Default)]
pub struct ReconcileStats {
    pub cycles: AtomicU64,
    pub last_unix: AtomicI64,
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

    /// Un ciclo completo sobre todos los agentes. Devuelve cuántos cambiaron.
    pub fn reconcile_once(&self) -> usize {
        let mut changed = 0;
        for rec in self.registry.list() {
            let desired = DesiredState {
                name: &rec.name,
                model: &rec.model,
                desired_replicas: rec.desired_replicas,
            };
            let result = self.runtime.apply(&desired);
            match &result {
                Ok(ready) if *ready != rec.ready_replicas => {
                    changed += 1;
                    tracing::info!(
                        agent = %rec.name,
                        from = rec.ready_replicas,
                        to = ready,
                        desired = rec.desired_replicas,
                        "réplicas actualizadas"
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    changed += 1;
                    tracing::warn!(agent = %rec.name, error = %e, "el runtime falló");
                }
            }
            self.registry.observe(&rec.name, rec.generation, result);
        }
        self.stats.cycles.fetch_add(1, Ordering::Relaxed);
        self.stats
            .last_unix
            .store(crate::unix_now(), Ordering::Relaxed);
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
                _ = ticker.tick() => { self.reconcile_once(); }
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
    use crate::runtime::SimulatedRuntime;

    #[test]
    fn converges_to_desired_and_ready() {
        let reg = Arc::new(Registry::new());
        let rec = Reconciler::new(reg.clone(), Arc::new(SimulatedRuntime::new(2)));
        reg.create("a", "m", 5).unwrap();
        for _ in 0..5 {
            rec.reconcile_once();
        }
        let a = reg.get("a").unwrap();
        assert_eq!((a.ready_replicas, a.phase), (5, AgentPhase::Ready));
        assert_eq!(rec.reconcile_once(), 0, "estado estable, sin cambios");
    }

    #[test]
    fn delete_drains_and_removes() {
        let reg = Arc::new(Registry::new());
        let rec = Reconciler::new(reg.clone(), Arc::new(SimulatedRuntime::new(10)));
        reg.create("a", "m", 3).unwrap();
        rec.reconcile_once();
        reg.delete("a").unwrap();
        rec.reconcile_once();
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
}
