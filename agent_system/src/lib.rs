//! Orquestador de agentes LLM de AetherLynx.
//!
//! Componentes:
//! - [`registry`]: estado deseado y observado de cada agente.
//! - [`runtime`]: dónde viven realmente las réplicas (trait enchufable).
//! - [`kubernetes`]: runtime que crea un Deployment y un Service por agente.
//! - [`reconciler`]: bucle que hace converger el estado observado al deseado.
//! - [`service`]: implementación gRPC.
//! - [`auth`]: autenticación por token bearer.
//! - [`integration`]: sonda de salud opcional hacia sistemas transaccionales.
//! - [`telemetry`]: OpenTelemetry (trazas, métricas y logs por OTLP).

pub mod auth;
pub mod integration;
pub mod kubernetes;
pub mod reconciler;
pub mod registry;
pub mod runtime;
pub mod service;
pub mod telemetry;
pub mod validation;

pub mod proto {
    tonic::include_proto!("aetherlynx.v1");
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
