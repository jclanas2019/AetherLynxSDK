//! Orquestador de agentes LLM de AetherLynx.
//!
//! Componentes:
//! - [`registry`]: estado deseado y observado de cada agente.
//! - [`runtime`]: dónde viven realmente las réplicas (trait enchufable).
//! - [`reconciler`]: bucle que hace converger el estado observado al deseado.
//! - [`service`]: implementación gRPC.
//! - [`auth`]: autenticación por token bearer.
//! - [`integration`]: sonda de salud opcional hacia sistemas transaccionales.

pub mod auth;
pub mod integration;
pub mod reconciler;
pub mod registry;
pub mod runtime;
pub mod service;
pub mod validation;

pub mod proto {
    tonic::include_proto!("aetherlynx.v1");
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
