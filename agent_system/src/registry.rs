//! Registro en memoria del estado deseado y observado de los agentes.

use std::collections::BTreeMap;
use std::sync::RwLock;

use crate::proto::{Agent, AgentPhase};
use crate::validation::{self, ValidationError, MAX_AGENTS};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRecord {
    pub name: String,
    pub model: String,
    pub desired_replicas: u32,
    pub ready_replicas: u32,
    pub phase: AgentPhase,
    pub created_at_unix: i64,
    pub generation: u64,
    pub message: String,
    pub deleting: bool,
}

impl AgentRecord {
    pub fn to_proto(&self) -> Agent {
        Agent {
            name: self.name.clone(),
            model: self.model.clone(),
            desired_replicas: self.desired_replicas,
            ready_replicas: self.ready_replicas,
            phase: self.phase as i32,
            created_at_unix: self.created_at_unix,
            generation: self.generation,
            message: self.message.clone(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum RegistryError {
    Invalid(ValidationError),
    AlreadyExists(String),
    NotFound(String),
    Terminating(String),
    Capacity,
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(e) => write!(f, "{e}"),
            Self::AlreadyExists(n) => write!(f, "el agente '{n}' ya existe"),
            Self::NotFound(n) => write!(f, "el agente '{n}' no existe"),
            Self::Terminating(n) => write!(f, "el agente '{n}' se está eliminando"),
            Self::Capacity => write!(f, "límite de {MAX_AGENTS} agentes alcanzado"),
        }
    }
}

impl std::error::Error for RegistryError {}

impl From<ValidationError> for RegistryError {
    fn from(e: ValidationError) -> Self {
        Self::Invalid(e)
    }
}

#[derive(Default)]
pub struct Registry {
    agents: RwLock<BTreeMap<String, AgentRecord>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    // Un pánico con el lock tomado no debe dejar el orquestador inservible:
    // recuperamos el estado envenenado (el mapa sigue siendo coherente porque
    // ninguna mutación queda a medias).
    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, AgentRecord>> {
        self.agents.read().unwrap_or_else(|p| p.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<String, AgentRecord>> {
        self.agents.write().unwrap_or_else(|p| p.into_inner())
    }

    pub fn create(
        &self,
        name: &str,
        model: &str,
        replicas: u32,
    ) -> Result<AgentRecord, RegistryError> {
        validation::agent_name(name)?;
        validation::model_id(model)?;
        validation::replicas(replicas)?;
        let mut agents = self.write();
        if agents.contains_key(name) {
            return Err(RegistryError::AlreadyExists(name.to_owned()));
        }
        if agents.len() >= MAX_AGENTS {
            return Err(RegistryError::Capacity);
        }
        let rec = AgentRecord {
            name: name.to_owned(),
            model: model.to_owned(),
            desired_replicas: replicas,
            ready_replicas: 0,
            phase: if replicas == 0 {
                AgentPhase::Ready
            } else {
                AgentPhase::Pending
            },
            created_at_unix: crate::unix_now(),
            generation: 1,
            message: String::new(),
            deleting: false,
        };
        agents.insert(name.to_owned(), rec.clone());
        Ok(rec)
    }

    pub fn scale(&self, name: &str, replicas: u32) -> Result<AgentRecord, RegistryError> {
        validation::agent_name(name)?;
        validation::replicas(replicas)?;
        let mut agents = self.write();
        let rec = agents
            .get_mut(name)
            .ok_or_else(|| RegistryError::NotFound(name.to_owned()))?;
        if rec.deleting {
            return Err(RegistryError::Terminating(name.to_owned()));
        }
        if rec.desired_replicas != replicas {
            rec.desired_replicas = replicas;
            rec.generation += 1;
            if rec.ready_replicas != replicas {
                rec.phase = AgentPhase::Scaling;
            }
        }
        Ok(rec.clone())
    }

    /// Marca el agente para borrado. El reconciliador lo retira del registro
    /// cuando el runtime confirma 0 réplicas.
    pub fn delete(&self, name: &str) -> Result<AgentRecord, RegistryError> {
        validation::agent_name(name)?;
        let mut agents = self.write();
        let rec = agents
            .get_mut(name)
            .ok_or_else(|| RegistryError::NotFound(name.to_owned()))?;
        if !rec.deleting {
            rec.deleting = true;
            rec.desired_replicas = 0;
            rec.generation += 1;
            rec.phase = AgentPhase::Terminating;
        }
        Ok(rec.clone())
    }

    pub fn get(&self, name: &str) -> Result<AgentRecord, RegistryError> {
        validation::agent_name(name)?;
        self.read()
            .get(name)
            .cloned()
            .ok_or_else(|| RegistryError::NotFound(name.to_owned()))
    }

    pub fn list(&self) -> Vec<AgentRecord> {
        self.read().values().cloned().collect()
    }

    /// Aplica una observación del runtime. Ignora observaciones obsoletas
    /// (si el estado deseado cambió mientras el runtime trabajaba).
    pub fn observe(&self, name: &str, generation: u64, result: Result<u32, String>) {
        let mut agents = self.write();
        let Some(rec) = agents.get_mut(name) else {
            return;
        };
        if rec.generation != generation {
            return;
        }
        match result {
            Ok(ready) => {
                rec.ready_replicas = ready;
                rec.message.clear();
                if rec.deleting {
                    if ready == 0 {
                        agents.remove(name);
                    }
                    return;
                }
                rec.phase = if ready == rec.desired_replicas {
                    AgentPhase::Ready
                } else {
                    AgentPhase::Scaling
                };
            }
            Err(msg) => {
                rec.phase = AgentPhase::Failed;
                rec.message = msg;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_duplicate() {
        let r = Registry::new();
        let a = r.create("student-agent", "gpt-4o", 3).unwrap();
        assert_eq!(a.phase, AgentPhase::Pending);
        assert_eq!(
            r.create("student-agent", "gpt-4o", 1),
            Err(RegistryError::AlreadyExists("student-agent".into()))
        );
    }

    #[test]
    fn rejects_invalid_input_before_touching_state() {
        let r = Registry::new();
        assert!(matches!(
            r.create("Bad Name", "m", 1),
            Err(RegistryError::Invalid(_))
        ));
        assert!(matches!(
            r.create("ok", "m", 10_000),
            Err(RegistryError::Invalid(_))
        ));
        assert!(r.list().is_empty());
    }

    #[test]
    fn scale_bumps_generation_only_on_change() {
        let r = Registry::new();
        r.create("a", "m", 1).unwrap();
        assert_eq!(r.scale("a", 5).unwrap().generation, 2);
        assert_eq!(r.scale("a", 5).unwrap().generation, 2);
        assert_eq!(
            r.scale("missing", 1),
            Err(RegistryError::NotFound("missing".into()))
        );
    }

    #[test]
    fn stale_observation_is_ignored() {
        let r = Registry::new();
        r.create("a", "m", 2).unwrap(); // gen 1
        r.scale("a", 4).unwrap(); // gen 2
        r.observe("a", 1, Ok(2));
        assert_eq!(r.get("a").unwrap().ready_replicas, 0);
        r.observe("a", 2, Ok(4));
        let a = r.get("a").unwrap();
        assert_eq!((a.ready_replicas, a.phase), (4, AgentPhase::Ready));
    }

    #[test]
    fn delete_removes_after_zero_replicas() {
        let r = Registry::new();
        r.create("a", "m", 2).unwrap();
        r.observe("a", 1, Ok(2));
        let d = r.delete("a").unwrap();
        assert_eq!(d.phase, AgentPhase::Terminating);
        assert!(matches!(
            r.scale("a", 3),
            Err(RegistryError::Terminating(_))
        ));
        r.observe("a", d.generation, Ok(0));
        assert!(matches!(r.get("a"), Err(RegistryError::NotFound(_))));
    }

    #[test]
    fn runtime_failure_sets_failed_phase() {
        let r = Registry::new();
        r.create("a", "m", 1).unwrap();
        r.observe("a", 1, Err("sin cuota".into()));
        let a = r.get("a").unwrap();
        assert_eq!(a.phase, AgentPhase::Failed);
        assert_eq!(a.message, "sin cuota");
    }
}
