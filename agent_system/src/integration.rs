//! Sonda de salud opcional hacia los sistemas transaccionales (p. ej. el API
//! de la universidad). Sustituye al antiguo chequeo que pedía los datos
//! personales y financieros del estudiante 12345 cada 10 segundos: aquí solo
//! se consulta un endpoint de salud configurable, sin datos personales.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::watch;

#[derive(Debug, Clone, Default)]
pub struct HealthSnapshot {
    pub enabled: bool,
    pub healthy: bool,
    pub detail: String,
}

pub struct HealthProbe {
    client: reqwest::Client,
    url: reqwest::Url,
    state: Arc<RwLock<HealthSnapshot>>,
}

impl HealthProbe {
    pub fn new(url: &str, timeout: Duration) -> Result<Self, String> {
        let url =
            reqwest::Url::parse(url).map_err(|e| format!("URL de integración inválida: {e}"))?;
        if url.scheme() != "https" && !is_loopback_host(&url) {
            return Err("la URL de integración debe usar https".into());
        }
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout)
            .user_agent(concat!(
                "aetherlynx-orchestrator/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .map_err(|e| e.to_string())?;
        let state = Arc::new(RwLock::new(HealthSnapshot {
            enabled: true,
            healthy: false,
            detail: "sin comprobar todavía".into(),
        }));
        Ok(Self { client, url, state })
    }

    pub fn state(&self) -> Arc<RwLock<HealthSnapshot>> {
        self.state.clone()
    }

    pub async fn check_once(&self) {
        let (healthy, detail) = match self.client.get(self.url.clone()).send().await {
            Ok(r) if r.status().is_success() => (true, format!("HTTP {}", r.status().as_u16())),
            Ok(r) => (false, format!("HTTP {}", r.status().as_u16())),
            Err(e) if e.is_timeout() => (false, "tiempo de espera agotado".into()),
            Err(e) if e.is_connect() => (false, "no se pudo conectar".into()),
            Err(_) => (false, "error de red".into()),
        };
        let mut s = self.state.write().unwrap_or_else(|p| p.into_inner());
        if s.healthy != healthy {
            if healthy {
                tracing::info!(url = %self.url, %detail, "integración disponible");
            } else {
                tracing::warn!(url = %self.url, %detail, "integración no disponible");
            }
        }
        s.healthy = healthy;
        s.detail = detail;
    }

    pub async fn run(self, interval: Duration, mut shutdown: watch::Receiver<bool>) {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => self.check_once().await,
                _ = shutdown.changed() => break,
            }
            if *shutdown.borrow() {
                break;
            }
        }
    }
}

fn is_loopback_host(url: &reqwest::Url) -> bool {
    matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_https_except_loopback() {
        let t = Duration::from_secs(1);
        assert!(HealthProbe::new("http://api.example.edu/health", t).is_err());
        assert!(HealthProbe::new("https://api.example.edu/health", t).is_ok());
        assert!(HealthProbe::new("http://127.0.0.1:8080/health", t).is_ok());
        assert!(HealthProbe::new("not a url", t).is_err());
    }

    #[tokio::test]
    async fn unreachable_endpoint_is_unhealthy_not_a_crash() {
        let p = HealthProbe::new("http://127.0.0.1:9/health", Duration::from_millis(300)).unwrap();
        p.check_once().await;
        let s = p.state().read().unwrap().clone();
        assert!(s.enabled && !s.healthy);
    }
}
