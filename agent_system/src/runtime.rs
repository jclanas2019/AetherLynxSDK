//! Runtime: el lugar donde las réplicas de un agente existen de verdad.
//!
//! El reconciliador solo habla con el trait [`Runtime`]. Hay dos
//! implementaciones:
//!
//! - [`SimulatedRuntime`]: réplicas en memoria que suben o bajan `step` por
//!   ciclo. Para desarrollo y pruebas.
//! - [`crate::kubernetes::KubernetesRuntime`]: un Deployment y un Service por
//!   agente en un namespace de Kubernetes.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

/// Lo que el reconciliador quiere para un agente.
#[derive(Debug, Clone)]
pub struct DesiredState<'a> {
    pub name: &'a str,
    pub model: &'a str,
    pub desired_replicas: u32,
    /// El agente se está eliminando: el runtime debe retirar sus recursos.
    pub deleting: bool,
}

/// Lo que el runtime observa tras aplicar el estado deseado.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    /// Réplicas listas para atender.
    pub ready: u32,
    /// `true` cuando el rollout terminó: no quedan réplicas de versiones
    /// anteriores ni cambios pendientes de observar. Al eliminar, `true`
    /// significa que los recursos ya no existen.
    pub converged: bool,
}

/// Un agente encontrado en el runtime al arrancar el orquestador.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredAgent {
    pub name: String,
    pub model: String,
    pub desired_replicas: u32,
    /// Fecha de creación en el runtime, si la conoce.
    pub created_at_unix: Option<i64>,
}

#[async_trait]
pub trait Runtime: Send + Sync {
    /// Acerca el agente a su estado deseado y devuelve lo observado.
    /// Un `Err` marca el agente como `Failed` con ese mensaje.
    async fn apply(&self, desired: &DesiredState<'_>) -> Result<Observation, String>;

    /// Agentes que ya existen en el runtime, para reconstruir el registro al
    /// arrancar. Así el estado deseado sobrevive a un reinicio del orquestador.
    async fn recover(&self) -> Result<Vec<RecoveredAgent>, String> {
        Ok(Vec::new())
    }

    /// Elimina recursos gestionados por este orquestador que ya no
    /// corresponden a ningún agente registrado. Devuelve cuántos eliminó.
    async fn garbage_collect(&self, _known: &[String]) -> Result<usize, String> {
        Ok(0)
    }

    /// Nombre corto para logs y estado.
    fn kind(&self) -> &'static str;
}

pub struct SimulatedRuntime {
    step: u32,
    replicas: Mutex<HashMap<String, u32>>,
}

impl SimulatedRuntime {
    pub fn new(step: u32) -> Self {
        Self {
            step: step.max(1),
            replicas: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl Runtime for SimulatedRuntime {
    async fn apply(&self, d: &DesiredState<'_>) -> Result<Observation, String> {
        let mut map = self.replicas.lock().unwrap_or_else(|p| p.into_inner());
        let target = if d.deleting { 0 } else { d.desired_replicas };
        let cur = map.entry(d.name.to_owned()).or_insert(0);
        *cur = if *cur < target {
            (*cur + self.step).min(target)
        } else {
            cur.saturating_sub(self.step).max(target)
        };
        let ready = *cur;
        if d.deleting && ready == 0 {
            map.remove(d.name);
        }
        Ok(Observation {
            ready,
            converged: ready == target,
        })
    }

    fn kind(&self) -> &'static str {
        "simulated"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(n: u32) -> DesiredState<'static> {
        DesiredState {
            name: "a",
            model: "m",
            desired_replicas: n,
            deleting: false,
        }
    }

    #[tokio::test]
    async fn converges_in_steps() {
        let rt = SimulatedRuntime::new(2);
        let steps = [
            (5, 2, false),
            (5, 4, false),
            (5, 5, true),
            (5, 5, true),
            (1, 3, false),
            (1, 1, true),
            (0, 0, true),
        ];
        for (want, ready, converged) in steps {
            assert_eq!(
                rt.apply(&d(want)).await.unwrap(),
                Observation { ready, converged }
            );
        }
    }

    #[tokio::test]
    async fn deleting_drains_to_zero() {
        let rt = SimulatedRuntime::new(10);
        rt.apply(&d(3)).await.unwrap();
        let del = DesiredState {
            deleting: true,
            ..d(3)
        };
        assert_eq!(
            rt.apply(&del).await.unwrap(),
            Observation {
                ready: 0,
                converged: true
            }
        );
    }
}
