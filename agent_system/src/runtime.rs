//! Runtime: el lugar donde las réplicas de un agente existen de verdad.
//!
//! El reconciliador solo habla con este trait. La implementación incluida,
//! [`SimulatedRuntime`], mantiene las réplicas en memoria y sube o baja como
//! máximo `step` réplicas por ciclo, igual que un rollout gradual. Para
//! producción se implementa el trait contra Kubernetes (escalando un
//! Deployment por agente) u otro planificador.

use std::collections::HashMap;
use std::sync::Mutex;

pub struct DesiredState<'a> {
    pub name: &'a str,
    pub model: &'a str,
    pub desired_replicas: u32,
}

pub trait Runtime: Send + Sync {
    /// Acerca el agente a su estado deseado y devuelve las réplicas listas.
    fn apply(&self, desired: &DesiredState<'_>) -> Result<u32, String>;
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

impl Runtime for SimulatedRuntime {
    fn apply(&self, d: &DesiredState<'_>) -> Result<u32, String> {
        let mut map = self.replicas.lock().unwrap_or_else(|p| p.into_inner());
        let cur = map.entry(d.name.to_owned()).or_insert(0);
        *cur = if *cur < d.desired_replicas {
            (*cur + self.step).min(d.desired_replicas)
        } else {
            cur.saturating_sub(self.step).max(d.desired_replicas)
        };
        let ready = *cur;
        if ready == 0 && d.desired_replicas == 0 {
            map.remove(d.name);
        }
        Ok(ready)
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
        }
    }

    #[test]
    fn converges_in_steps() {
        let rt = SimulatedRuntime::new(2);
        assert_eq!(rt.apply(&d(5)).unwrap(), 2);
        assert_eq!(rt.apply(&d(5)).unwrap(), 4);
        assert_eq!(rt.apply(&d(5)).unwrap(), 5);
        assert_eq!(rt.apply(&d(5)).unwrap(), 5);
        assert_eq!(rt.apply(&d(1)).unwrap(), 3);
        assert_eq!(rt.apply(&d(1)).unwrap(), 1);
        assert_eq!(rt.apply(&d(0)).unwrap(), 0);
    }
}
