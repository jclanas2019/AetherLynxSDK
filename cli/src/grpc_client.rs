//! Cliente gRPC con token bearer y TLS opcional.

use std::path::Path;
use std::time::Duration;

use tonic::metadata::MetadataValue;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};
use tonic::{Request, Status};

pub mod proto {
    tonic::include_proto!("aetherlynx.v1");
}

use proto::aether_lynx_client::AetherLynxClient;

#[derive(Clone)]
pub struct BearerAuth(Option<MetadataValue<tonic::metadata::Ascii>>);

impl tonic::service::Interceptor for BearerAuth {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        if let Some(v) = &self.0 {
            req.metadata_mut().insert("authorization", v.clone());
        }
        Ok(req)
    }
}

pub type Client = AetherLynxClient<InterceptedService<Channel, BearerAuth>>;

pub async fn connect(
    endpoint: &str,
    token: Option<&str>,
    ca_cert: Option<&Path>,
) -> Result<Client, String> {
    let mut ep = Endpoint::from_shared(endpoint.to_owned())
        .map_err(|e| format!("endpoint inválido '{endpoint}': {e}"))?
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30));
    let uri = ep.uri().clone();

    if uri.scheme_str() == Some("https") {
        let mut tls = ClientTlsConfig::new().with_native_roots();
        if let Some(ca) = ca_cert {
            let pem = std::fs::read(ca)
                .map_err(|e| format!("no se pudo leer el CA '{}': {e}", ca.display()))?;
            tls = tls.ca_certificate(Certificate::from_pem(pem));
        }
        ep = ep
            .tls_config(tls)
            .map_err(|e| format!("configuración TLS inválida: {e}"))?;
    } else {
        if ca_cert.is_some() {
            return Err("--ca-cert requiere un endpoint https://".into());
        }
        if token.is_some() && !is_loopback(uri.host()) {
            eprintln!("aviso: enviando el token sin TLS a {endpoint}");
        }
    }

    let auth = match token {
        Some(t) => {
            let mut v: MetadataValue<tonic::metadata::Ascii> = format!("Bearer {t}")
                .parse()
                .map_err(|_| "el token contiene caracteres no válidos".to_string())?;
            // Evita que el token aparezca en logs o trazas de depuración.
            v.set_sensitive(true);
            BearerAuth(Some(v))
        }
        None => BearerAuth(None),
    };

    let channel = ep
        .connect()
        .await
        .map_err(|e| format!("no se pudo conectar a {endpoint}: {}", error_chain(&e)))?;
    Ok(AetherLynxClient::with_interceptor(channel, auth))
}

/// "transport error" no dice nada: mostramos la cadena de causas completa.
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut parts = vec![e.to_string()];
    let mut cur = e.source();
    while let Some(s) = cur {
        let msg = s.to_string();
        if parts.last() != Some(&msg) {
            parts.push(msg);
        }
        cur = s.source();
    }
    parts.join(": ")
}

/// Compara el host ya parseado, no un prefijo del texto: `localhost.evil.com`
/// o `127.0.0.1.example.com` no cuentan como loopback.
fn is_loopback(host: Option<&str>) -> bool {
    match host {
        Some("localhost") => true,
        Some(h) => h
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false),
        None => false,
    }
}

/// Traduce un error gRPC a un mensaje para la persona que usa la CLI.
pub fn describe(s: &Status) -> String {
    use tonic::Code::*;
    let hint = match s.code() {
        Unauthenticated => " (revise AETHERLYNX_API_TOKEN)",
        Unavailable => " (¿está el orquestador en marcha?)",
        _ => "",
    };
    format!("{}{hint}", s.message())
}

#[cfg(test)]
mod tests {
    use super::is_loopback;

    #[test]
    fn loopback_detection_uses_the_parsed_host() {
        for h in ["localhost", "127.0.0.1", "127.1.2.3", "[::1]", "::1"] {
            assert!(is_loopback(Some(h)), "{h}");
        }
        for h in [
            "localhost.evil.com",
            "127.0.0.1.example.com",
            "10.0.0.1",
            "example.com",
        ] {
            assert!(!is_loopback(Some(h)), "{h}");
        }
        assert!(!is_loopback(None));
    }
}
