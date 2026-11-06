//! Colector de telemetría de AetherLynx.
//!
//! Recibe trazas, métricas y logs por OTLP/HTTP desde cualquier componente
//! instrumentado con OpenTelemetry, los guarda en SQLite con retención,
//! evalúa alertas operativas y sirve un panel web propio y una API de
//! consulta. No depende de ningún servicio externo.

pub mod alerts;
pub mod otlp;
pub mod series;
pub mod server;
pub mod store;
