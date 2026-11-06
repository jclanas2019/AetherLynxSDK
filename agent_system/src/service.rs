//! Implementación del servicio gRPC sobre el registro.

use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use opentelemetry::KeyValue;
use tonic::{Request, Response, Status};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::integration::HealthSnapshot;
use crate::proto::aether_lynx_server::AetherLynx;
use crate::proto::*;
use crate::reconciler::ReconcileStats;
use crate::registry::{Registry, RegistryError};
use crate::telemetry;

pub struct AetherLynxService {
    registry: Arc<Registry>,
    stats: Arc<ReconcileStats>,
    health: Option<Arc<RwLock<HealthSnapshot>>>,
    started: Instant,
    runtime_kind: &'static str,
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
            runtime_kind: "simulated",
        }
    }

    pub fn with_runtime_kind(mut self, kind: &'static str) -> Self {
        self.runtime_kind = kind;
        self
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

/// Códigos que indican un fallo del servidor (convención OpenTelemetry para
/// servidores gRPC). Los errores del cliente, como un argumento inválido,
/// no marcan el span como error ni disparan alertas de tasa de error.
fn is_server_error(code: tonic::Code) -> bool {
    use tonic::Code::*;
    matches!(
        code,
        Unknown | DeadlineExceeded | Unimplemented | Internal | Unavailable | DataLoss
    )
}

impl AetherLynxService {
    /// Ejecuta una RPC dentro de un span de servidor que continúa la traza
    /// del cliente (cabecera `traceparent`) y registra sus métricas.
    fn traced<T, R>(
        &self,
        method: &'static str,
        req: Request<T>,
        f: impl FnOnce(T) -> Result<R, Status>,
    ) -> Result<Response<R>, Status> {
        let span = tracing::info_span!(
            "grpc.request",
            otel.name = method,
            otel.kind = "server",
            rpc.system = "grpc",
            rpc.service = "aetherlynx.v1.AetherLynx",
            rpc.method = method,
            rpc.grpc.status_code = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
            otel.status_message = tracing::field::Empty,
        );
        let _ = span.set_parent(telemetry::remote_context(req.metadata()));
        let start = Instant::now();
        let out = span.in_scope(|| f(req.into_inner()));
        let code = match &out {
            Ok(_) => tonic::Code::Ok,
            Err(s) => s.code(),
        };
        span.record("rpc.grpc.status_code", code as i32);
        if let Err(s) = &out {
            if is_server_error(code) {
                span.record("otel.status_code", "ERROR");
                span.record("otel.status_message", s.message());
            }
        }
        let m = telemetry::metrics();
        let code_name = format!("{code:?}");
        m.grpc_requests.add(
            1,
            &[
                KeyValue::new("method", method),
                KeyValue::new("code", code_name),
            ],
        );
        m.grpc_duration.record(
            start.elapsed().as_secs_f64(),
            &[KeyValue::new("method", method)],
        );
        out.map(Response::new)
    }

    fn status(&self) -> OrchestratorStatus {
        let agents = self.registry.list();
        let health = self
            .health
            .as_ref()
            .map(|h| h.read().unwrap_or_else(|p| p.into_inner()).clone())
            .unwrap_or_default();
        OrchestratorStatus {
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
            runtime: self.runtime_kind.into(),
        }
    }
}

#[tonic::async_trait]
impl AetherLynx for AetherLynxService {
    async fn create_agent(
        &self,
        req: Request<CreateAgentRequest>,
    ) -> Result<Response<Agent>, Status> {
        self.traced("CreateAgent", req, |r| {
            let rec = self.registry.create(&r.name, &r.model, r.replicas).map_err(to_status)?;
            // Los campos ya están validados: no pueden inyectar líneas en el log.
            tracing::info!(agent = %rec.name, model = %rec.model, replicas = rec.desired_replicas, "agente creado");
            Ok(rec.to_proto())
        })
    }

    async fn scale_agent(
        &self,
        req: Request<ScaleAgentRequest>,
    ) -> Result<Response<Agent>, Status> {
        self.traced("ScaleAgent", req, |r| {
            let rec = self.registry.scale(&r.name, r.replicas).map_err(to_status)?;
            tracing::info!(agent = %rec.name, replicas = rec.desired_replicas, "escalado solicitado");
            Ok(rec.to_proto())
        })
    }

    async fn delete_agent(
        &self,
        req: Request<DeleteAgentRequest>,
    ) -> Result<Response<Agent>, Status> {
        self.traced("DeleteAgent", req, |r| {
            let rec = self.registry.delete(&r.name).map_err(to_status)?;
            tracing::info!(agent = %rec.name, "borrado solicitado");
            Ok(rec.to_proto())
        })
    }

    async fn get_agent(&self, req: Request<GetAgentRequest>) -> Result<Response<Agent>, Status> {
        self.traced("GetAgent", req, |r| {
            Ok(self.registry.get(&r.name).map_err(to_status)?.to_proto())
        })
    }

    async fn list_agents(
        &self,
        req: Request<ListAgentsRequest>,
    ) -> Result<Response<ListAgentsResponse>, Status> {
        self.traced("ListAgents", req, |_| {
            Ok(ListAgentsResponse {
                agents: self.registry.list().iter().map(|r| r.to_proto()).collect(),
            })
        })
    }

    async fn get_status(
        &self,
        req: Request<GetStatusRequest>,
    ) -> Result<Response<OrchestratorStatus>, Status> {
        self.traced("GetStatus", req, |_| Ok(self.status()))
    }
}
