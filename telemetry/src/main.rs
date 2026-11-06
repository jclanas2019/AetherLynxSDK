use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use clap::{Args, Parser, Subcommand, ValueEnum};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

use aetherlynx_telemetry::alerts::{AlertConfig, AlertState};
use aetherlynx_telemetry::server::{self, now_ns, AppState, IngestStats};
use aetherlynx_telemetry::store::{Limits, Store};

const MIN_TOKEN: usize = 32;

#[derive(Parser)]
#[command(
    name = "aetherlynx-telemetry",
    version,
    about = "Colector de telemetría de AetherLynx (OTLP/HTTP + panel web)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Inicia el receptor OTLP, la API de consulta y el panel web
    Start(StartArgs),
}

#[derive(Clone, Copy, ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

#[derive(Args)]
struct StartArgs {
    /// Dirección del receptor OTLP/HTTP (puerto estándar 4318)
    #[arg(
        long,
        env = "AETHERLYNX_TELEMETRY_OTLP_LISTEN",
        default_value = "127.0.0.1:4318"
    )]
    otlp_listen: SocketAddr,

    /// Dirección del panel web y la API de consulta
    #[arg(
        long,
        env = "AETHERLYNX_TELEMETRY_UI_LISTEN",
        default_value = "127.0.0.1:8088"
    )]
    ui_listen: SocketAddr,

    /// Base de datos SQLite
    #[arg(long, env = "AETHERLYNX_TELEMETRY_DB", default_value = "telemetry.db")]
    db: String,

    /// Cuánto tiempo se guarda la telemetría
    #[arg(long, env = "AETHERLYNX_TELEMETRY_RETENTION", default_value = "24h", value_parser = humantime::parse_duration)]
    retention: Duration,

    /// Token que deben enviar los exportadores OTLP (mínimo 32 caracteres)
    #[arg(
        long,
        env = "AETHERLYNX_TELEMETRY_INGEST_TOKEN",
        hide_env_values = true
    )]
    ingest_token: Option<String>,

    /// Token del panel y la API de consulta (mínimo 32 caracteres)
    #[arg(long, env = "AETHERLYNX_TELEMETRY_TOKEN", hide_env_values = true)]
    query_token: Option<String>,

    /// Sin tokens. Solo se permite si ambos puertos escuchan en loopback.
    #[arg(long)]
    allow_unauthenticated: bool,

    /// Certificado TLS (PEM), para ambos puertos
    #[arg(long, env = "AETHERLYNX_TELEMETRY_TLS_CERT", requires = "tls_key")]
    tls_cert: Option<PathBuf>,

    /// Clave privada TLS (PEM)
    #[arg(long, env = "AETHERLYNX_TELEMETRY_TLS_KEY", requires = "tls_cert")]
    tls_key: Option<PathBuf>,

    /// Permite escuchar fuera de loopback sin TLS (p. ej. con un service mesh mTLS)
    #[arg(long)]
    allow_insecure_transport: bool,

    /// Tamaño máximo de la base en MiB. Al llenarse se descarta el 10 % más
    /// antiguo. Déjele margen respecto del volumen (WAL y temporales).
    #[arg(long, env = "AETHERLYNX_TELEMETRY_MAX_DB_MIB", default_value_t = 1536)]
    max_db_mib: u64,

    /// Máximo de servicios distintos (protege de nombres de servicio sin fin)
    #[arg(long, default_value_t = 256)]
    max_services: usize,

    /// Cada cuánto se evalúan las alertas
    #[arg(long, default_value = "30s", value_parser = humantime::parse_duration)]
    alert_interval: Duration,

    /// Silencio que dispara la alerta ServiceSilent
    #[arg(long, default_value = "5m", value_parser = humantime::parse_duration)]
    alert_silent_after: Duration,

    /// Antigüedad del último ciclo que dispara ReconcileStalled
    #[arg(long, default_value = "3m", value_parser = humantime::parse_duration)]
    alert_reconcile_stale: Duration,

    /// Proporción de peticiones con error que dispara HighErrorRate (0-1)
    #[arg(long, default_value_t = 0.05)]
    alert_error_ratio: f64,

    #[arg(
        long,
        env = "AETHERLYNX_LOG_FORMAT",
        value_enum,
        default_value = "text"
    )]
    log_format: LogFormat,
}

fn token(name: &str, v: &Option<String>) -> Result<Option<Vec<u8>>, String> {
    match v {
        Some(t) if t.len() >= MIN_TOKEN => Ok(Some(t.as_bytes().to_vec())),
        Some(_) => Err(format!("{name} debe tener al menos {MIN_TOKEN} caracteres")),
        None => Ok(None),
    }
}

fn tls_acceptor(cert: &PathBuf, key: &PathBuf) -> Result<TlsAcceptor, String> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .map_err(|e| format!("no se pudo leer el certificado: {e}"))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("certificado inválido: {e}"))?;
    let key = PrivateKeyDer::from_pem_file(key).map_err(|e| format!("clave TLS inválida: {e}"))?;
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let mut cfg = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("configuración TLS inválida: {e}"))?;
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(cfg)))
}

/// Conexiones simultáneas por puerto. Las que excedan esperan en la cola del
/// sistema operativo hasta que se libere una.
const MAX_CONNECTIONS: usize = 64;

async fn serve(
    listener: TcpListener,
    app: Router,
    tls: Option<TlsAcceptor>,
    mut stop: watch::Receiver<bool>,
) -> Result<(), String> {
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        let permit = tokio::select! {
            p = slots.clone().acquire_owned() => p.map_err(|e| e.to_string())?,
            _ = stop.wait_for(|s| *s) => return Ok(()),
        };
        let (tcp, peer) = tokio::select! {
            r = listener.accept() => r.map_err(|e| e.to_string())?,
            _ = stop.wait_for(|s| *s) => return Ok(()),
        };
        let (tls, app) = (tls.clone(), app.clone());
        tokio::spawn(async move {
            let _permit = permit;
            let svc = TowerToHyperService::new(app);
            // Cabeceras en 10 s como máximo: una conexión lenta a propósito
            // no retiene su hueco indefinidamente.
            let mut builder = auto::Builder::new(TokioExecutor::new());
            builder
                .http1()
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(10));
            let result = match tls {
                None => builder.serve_connection(TokioIo::new(tcp), svc).await,
                Some(tls) => {
                    let Ok(stream) =
                        tokio::time::timeout(Duration::from_secs(10), tls.accept(tcp)).await
                    else {
                        tracing::debug!(%peer, "TLS: tiempo agotado en el saludo");
                        return;
                    };
                    match stream {
                        Ok(s) => builder.serve_connection(TokioIo::new(s), svc).await,
                        Err(e) => {
                            tracing::debug!(%peer, error = %e, "TLS: saludo fallido");
                            return;
                        }
                    }
                }
            };
            if let Err(e) = result {
                tracing::debug!(%peer, error = %e, "conexión cerrada con error");
            }
        });
    }
}

async fn start(a: StartArgs) -> Result<(), String> {
    let loopback = a.otlp_listen.ip().is_loopback() && a.ui_listen.ip().is_loopback();
    let ingest = token("AETHERLYNX_TELEMETRY_INGEST_TOKEN", &a.ingest_token)?;
    let query = token("AETHERLYNX_TELEMETRY_TOKEN", &a.query_token)?;
    match (ingest.is_some() && query.is_some(), a.allow_unauthenticated) {
        (true, true) => return Err("los tokens y --allow-unauthenticated son incompatibles".into()),
        (true, false) => {}
        (false, true) if loopback => {
            tracing::warn!("autenticación DESACTIVADA: modo desarrollo, solo loopback")
        }
        (false, true) => {
            return Err("--allow-unauthenticated solo se permite escuchando en loopback".into())
        }
        (false, false) => {
            return Err(
                "defina AETHERLYNX_TELEMETRY_INGEST_TOKEN y AETHERLYNX_TELEMETRY_TOKEN \
                        (o use --allow-unauthenticated en loopback para desarrollo)"
                    .into(),
            )
        }
    }
    if !loopback && a.tls_cert.is_none() && !a.allow_insecure_transport {
        return Err(
            "escuchar fuera de loopback sin TLS expondría los tokens en claro: configure \
                    --tls-cert/--tls-key o use --allow-insecure-transport"
                .into(),
        );
    }
    if a.retention < Duration::from_secs(600) {
        return Err("la retención mínima es de 10 minutos".into());
    }
    let tls = match (&a.tls_cert, &a.tls_key) {
        (Some(c), Some(k)) => Some(tls_acceptor(c, k)?),
        _ => None,
    };

    if a.max_db_mib < 16 || a.max_services == 0 {
        return Err("--max-db-mib debe ser al menos 16 y --max-services al menos 1".into());
    }
    let limits = Limits {
        max_services: a.max_services,
        max_bytes: a.max_db_mib * 1024 * 1024,
    };
    let store = Arc::new(
        Store::open_with(&a.db, limits).map_err(|e| format!("no se pudo abrir {}: {e}", a.db))?,
    );
    let state = Arc::new(AppState {
        store,
        alerts: Mutex::new(AlertState::default()),
        alert_config: AlertConfig {
            silent_after: a.alert_silent_after,
            reconcile_stale: a.alert_reconcile_stale,
            error_ratio: a.alert_error_ratio.clamp(0.0, 1.0),
            ..AlertConfig::default()
        },
        retention: a.retention,
        ingest_token: ingest,
        query_token: query,
        stats: IngestStats::default(),
        started_ns: now_ns(),
    });

    let otlp = TcpListener::bind(a.otlp_listen)
        .await
        .map_err(|e| format!("no se pudo escuchar en {}: {e}", a.otlp_listen))?;
    let ui = TcpListener::bind(a.ui_listen)
        .await
        .map_err(|e| format!("no se pudo escuchar en {}: {e}", a.ui_listen))?;
    tracing::info!(otlp = %a.otlp_listen, ui = %a.ui_listen, tls = tls.is_some(), db = %a.db, retention = ?a.retention, "colector en marcha");

    let (tx, rx) = watch::channel(false);
    let bg = [
        tokio::spawn(server::alert_loop(
            state.clone(),
            a.alert_interval,
            rx.clone(),
        )),
        tokio::spawn(server::retention_loop(
            state.clone(),
            Duration::from_secs(60),
            rx.clone(),
        )),
    ];
    let stopper = tx.clone();
    tokio::spawn(async move {
        let ctrl_c = async {
            let _ = tokio::signal::ctrl_c().await;
        };
        #[cfg(unix)]
        let term = async {
            if let Ok(mut s) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                s.recv().await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        #[cfg(not(unix))]
        let term = std::future::pending::<()>();
        tokio::select! { _ = ctrl_c => {}, _ = term => {} }
        tracing::info!("señal de apagado recibida");
        let _ = stopper.send(true);
    });

    let r = tokio::try_join!(
        serve(
            otlp,
            server::otlp_router(state.clone()),
            tls.clone(),
            rx.clone()
        ),
        serve(ui, server::ui_router(state.clone()), tls, rx.clone()),
    );
    let _ = tx.send(true);
    for h in bg {
        let _ = h.await;
    }
    r.map(|_| ())
}

#[tokio::main]
async fn main() -> ExitCode {
    let Command::Start(args) = Cli::parse().command;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let b = tracing_subscriber::fmt().with_env_filter(filter);
    match args.log_format {
        LogFormat::Text => b.init(),
        LogFormat::Json => b.json().init(),
    }
    match start(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
