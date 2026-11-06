//! Implementación del servicio gRPC sobre el registro.

use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use tonic::{Request, Response, Status};

use crate::integration::HealthSnapshot;
use crate::proto::aether_lynx_server::AetherLynx;
use crate::proto::*;
use crate::reconciler::ReconcileStats;
use crate::registry::{Registry, RegistryError};

pub struct AetherLynxService {
    registry: Arc<Registry>,
    stats: Arc<ReconcileStats>,
    health: Option<Arc<RwLock<HealthSnapshot>>>,
    started: Instant,
}

impl AetherLynxService {
    pub fn new(
        registry: Arc<Registry>,
        stats: Arc<ReconcileStats>,
        health: Option<Arc<RwLock<HealthSnapshot>>>,
    ) -> Self {
        Self {
            registry,
            stats,
            health,
            started: Instant::now(),
        }
    }
}

fn to_status(e: RegistryError) -> Status {
    match e {
        RegistryError::Invalid(v) => Status::invalid_argument(v.0),
        RegistryError::AlreadyExists(_) => Status::already_exists(e.to_string()),
        RegistryError::NotFound(_) => Status::not_found(e.to_string()),
        RegistryError::Terminating(_) => Status::failed_precondition(e.to_string()),
        RegistryError::Capacity => Status::resource_exhausted(e.to_string()),
    }
}

#[tonic::async_trait]
impl AetherLynx for AetherLynxService {
    async fn create_agent(
        &self,
        req: Request<CreateAgentRequest>,
    ) -> Result<Response<Agent>, Status> {
        let r = req.into_inner();
        let rec = self
            .registry
            .create(&r.name, &r.model, r.replicas)
            .map_err(to_status)?;
        // Los campos ya están validados: no pueden inyectar líneas en el log.
        tracing::info!(agent = %rec.name, model = %rec.model, replicas = rec.desired_replicas, "agente creado");
        Ok(Response::new(rec.to_proto()))
    }

    async fn scale_agent(
        &self,
        req: Request<ScaleAgentRequest>,
    ) -> Result<Response<Agent>, Status> {
        let r = req.into_inner();
        let rec = self
            .registry
            .scale(&r.name, r.replicas)
            .map_err(to_status)?;
        tracing::info!(agent = %rec.name, replicas = rec.desired_replicas, "escalado solicitado");
        Ok(Response::new(rec.to_proto()))
    }

    async fn delete_agent(
        &self,
        req: Request<DeleteAgentRequest>,
    ) -> Result<Response<Agent>, Status> {
        let r = req.into_inner();
        let rec = self.registry.delete(&r.name).map_err(to_status)?;
        tracing::info!(agent = %rec.name, "borrado solicitado");
        Ok(Response::new(rec.to_proto()))
    }

    async fn get_agent(&self, req: Request<GetAgentRequest>) -> Result<Response<Agent>, Status> {
        let rec = self
            .registry
            .get(&req.into_inner().name)
            .map_err(to_status)?;
        Ok(Response::new(rec.to_proto()))
    }

    async fn list_agents(
        &self,
        _req: Request<ListAgentsRequest>,
    ) -> Result<Response<ListAgentsResponse>, Status> {
        let agents = self.registry.list().iter().map(|r| r.to_proto()).collect();
        Ok(Response::new(ListAgentsResponse { agents }))
    }

    async fn get_status(
        &self,
        _req: Request<GetStatusRequest>,
    ) -> Result<Response<OrchestratorStatus>, Status> {
        let agents = self.registry.list();
        let health = self
            .health
            .as_ref()
            .map(|h| h.read().unwrap_or_else(|p| p.into_inner()).clone())
            .unwrap_or_default();
        Ok(Response::new(OrchestratorStatus {
            version: crate::VERSION.into(),
            uptime_seconds: self.started.elapsed().as_secs(),
            agents: agents.len() as u32,
            desired_replicas_total: agents.iter().map(|a| a.desired_replicas).sum(),
            ready_replicas_total: agents.iter().map(|a| a.ready_replicas).sum(),
            reconcile_cycles: self.stats.cycles.load(Ordering::Relaxed),
            last_reconcile_unix: self.stats.last_unix.load(Ordering::Relaxed),
            integration_enabled: health.enabled,
            integration_healthy: health.healthy,
            integration_detail: health.detail,
        }))
    }
}
